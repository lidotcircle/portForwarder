use portforwarder::forward_config::{ForwardSessionConfig, TcpMode, TrafficRecordingConfig};
use portforwarder::tcp_forwarder::TcpForwarder;
use portforwarder::tcp_udp_forwarder::TcpUdpForwarder;
use portforwarder::udp_forwarder::UdpForwarder;
use rusqlite::Connection;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream, UdpSocket};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn database_path(label: &str) -> PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "portforwarder-{label}-{}-{timestamp}.sqlite3",
        std::process::id()
    ))
}

fn unused_tcp_addr() -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap()
}

fn unused_udp_addr() -> std::net::SocketAddr {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket.local_addr().unwrap()
}

fn connect_with_retry(addr: std::net::SocketAddr) -> TcpStream {
    for _ in 0..50 {
        match TcpStream::connect(addr) {
            Ok(stream) => return stream,
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    panic!("forwarder did not start listening at {}", addr);
}

fn remove_database(path: &PathBuf) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path.to_string_lossy()));
    let _ = std::fs::remove_file(format!("{}-shm", path.to_string_lossy()));
}

#[test]
fn records_forwarded_tcp_payload_and_summary() {
    let database = database_path("tcp-recording");
    let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
    let upstream_addr = upstream.local_addr().unwrap();
    let listen_addr = unused_tcp_addr();
    let payload = b"record this TCP payload".to_vec();
    let expected = payload.clone();
    let echo = std::thread::spawn(move || {
        let (mut stream, _) = upstream.accept().unwrap();
        let mut received = vec![0; expected.len()];
        stream.read_exact(&mut received).unwrap();
        assert_eq!(received, expected);
        stream.write_all(&received).unwrap();
    });

    let config = ForwardSessionConfig {
        local: listen_addr.to_string(),
        remoteMap: vec![(".*".to_string(), upstream_addr.to_string())],
        allow_nets: vec!["127.0.0.0/8".to_string()],
        enable_tcp: true,
        enable_udp: false,
        conn_bufsize: 1024 * 1024,
        max_connections: 10,
        tcp_mode: TcpMode::Forward,
        recording: Some(TrafficRecordingConfig::new(database.to_string_lossy())),
    };
    let stopped = Arc::new(AtomicBool::new(false));
    let thread_stopped = stopped.clone();
    let forwarder = TcpForwarder::from(&config).unwrap();
    let forwarder_thread = std::thread::spawn(move || forwarder.listen(thread_stopped).unwrap());

    let mut client = connect_with_retry(listen_addr);
    client.write_all(&payload).unwrap();
    client.shutdown(Shutdown::Write).unwrap();
    let mut response = Vec::new();
    client.read_to_end(&mut response).unwrap();
    assert_eq!(response, payload);
    drop(client);
    echo.join().unwrap();

    stopped.store(true, Ordering::SeqCst);
    forwarder_thread.join().unwrap();

    let db = Connection::open(&database).unwrap();
    let summary: (String, String, i64, i64, i64) = db
        .query_row(
            "SELECT transport, target_addr, client_to_target_bytes, \
                    target_to_client_bytes, recorded_events \
             FROM traffic_sessions",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(summary.0, "tcp");
    assert_eq!(summary.1, upstream_addr.to_string());
    assert_eq!(summary.2, payload.len() as i64);
    assert_eq!(summary.3, payload.len() as i64);
    assert!(summary.4 >= 2);

    let mut statement = db
        .prepare(
            "SELECT direction, payload FROM traffic_events \
             ORDER BY sequence",
        )
        .unwrap();
    let mut uploads = Vec::new();
    let mut downloads = Vec::new();
    let events = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .unwrap();
    for event in events {
        let (direction, bytes) = event.unwrap();
        match direction.as_str() {
            "client_to_target" => uploads.extend(bytes),
            "target_to_client" => downloads.extend(bytes),
            other => panic!("unexpected direction {}", other),
        }
    }
    assert_eq!(uploads, payload);
    assert_eq!(downloads, payload);

    drop(statement);
    drop(db);
    remove_database(&database);
}

#[test]
fn records_socks5_payload_without_proxy_handshake() {
    let database = database_path("socks5-recording");
    let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
    let upstream_addr = upstream.local_addr().unwrap();
    let listen_addr = unused_tcp_addr();
    let payload = b"SOCKS application bytes".to_vec();
    let expected = payload.clone();
    let echo = std::thread::spawn(move || {
        let (mut stream, _) = upstream.accept().unwrap();
        let mut received = vec![0; expected.len()];
        stream.read_exact(&mut received).unwrap();
        stream.write_all(&received).unwrap();
    });

    let config = ForwardSessionConfig {
        local: listen_addr.to_string(),
        remoteMap: vec![],
        allow_nets: vec!["127.0.0.0/8".to_string()],
        enable_tcp: true,
        enable_udp: false,
        conn_bufsize: 1024 * 1024,
        max_connections: 10,
        tcp_mode: TcpMode::Socks5Server,
        recording: Some(TrafficRecordingConfig::new(database.to_string_lossy())),
    };
    let stopped = Arc::new(AtomicBool::new(false));
    let thread_stopped = stopped.clone();
    let forwarder = TcpForwarder::from(&config).unwrap();
    let forwarder_thread = std::thread::spawn(move || forwarder.listen(thread_stopped).unwrap());

    let mut client = connect_with_retry(listen_addr);
    client.write_all(&[0x05, 0x01, 0x00]).unwrap();
    let mut greeting = [0u8; 2];
    client.read_exact(&mut greeting).unwrap();
    assert_eq!(greeting, [0x05, 0x00]);

    let octets = match upstream_addr.ip() {
        std::net::IpAddr::V4(ip) => ip.octets(),
        _ => unreachable!(),
    };
    let mut request = vec![0x05, 0x01, 0x00, 0x01];
    request.extend_from_slice(&octets);
    request.extend_from_slice(&upstream_addr.port().to_be_bytes());
    client.write_all(&request).unwrap();
    let mut connect_reply = [0u8; 10];
    client.read_exact(&mut connect_reply).unwrap();
    assert_eq!(&connect_reply[..2], &[0x05, 0x00]);

    client.write_all(&payload).unwrap();
    client.shutdown(Shutdown::Write).unwrap();
    let mut response = Vec::new();
    client.read_to_end(&mut response).unwrap();
    assert_eq!(response, payload);
    echo.join().unwrap();

    stopped.store(true, Ordering::SeqCst);
    forwarder_thread.join().unwrap();

    let db = Connection::open(&database).unwrap();
    let summary: (String, i64, i64) = db
        .query_row(
            "SELECT application_protocol, client_to_target_bytes, target_to_client_bytes \
             FROM traffic_sessions",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(summary.0, "SOCKS5");
    assert_eq!(summary.1, payload.len() as i64);
    assert_eq!(summary.2, payload.len() as i64);

    let mut statement = db
        .prepare("SELECT payload FROM traffic_events ORDER BY direction, sequence")
        .unwrap();
    let captured_size: usize = statement
        .query_map([], |row| row.get::<_, Vec<u8>>(0))
        .unwrap()
        .map(|row| row.unwrap().len())
        .sum();
    assert_eq!(captured_size, payload.len() * 2);

    drop(statement);
    drop(db);
    remove_database(&database);
}

#[test]
fn records_udp_datagrams_in_both_directions() {
    let database = database_path("udp-recording");
    let upstream = UdpSocket::bind("127.0.0.1:0").unwrap();
    let upstream_addr = upstream.local_addr().unwrap();
    let listen_addr = unused_udp_addr();
    let payload = b"one UDP datagram".to_vec();
    let expected = payload.clone();
    let echo = std::thread::spawn(move || {
        let mut buffer = [0u8; 1024];
        let (size, peer) = upstream.recv_from(&mut buffer).unwrap();
        assert_eq!(&buffer[..size], expected.as_slice());
        upstream.send_to(&buffer[..size], peer).unwrap();
    });

    let config = ForwardSessionConfig {
        local: listen_addr.to_string(),
        remoteMap: vec![(".*".to_string(), upstream_addr.to_string())],
        allow_nets: vec!["127.0.0.0/8".to_string()],
        enable_tcp: false,
        enable_udp: true,
        conn_bufsize: 1024 * 1024,
        max_connections: 10,
        tcp_mode: TcpMode::Forward,
        recording: Some(TrafficRecordingConfig::new(database.to_string_lossy())),
    };
    let stopped = Arc::new(AtomicBool::new(false));
    let thread_stopped = stopped.clone();
    let forwarder = UdpForwarder::from(&config).unwrap();
    let forwarder_thread = std::thread::spawn(move || forwarder.listen(thread_stopped).unwrap());

    std::thread::sleep(Duration::from_millis(100));
    let client = UdpSocket::bind("127.0.0.1:0").unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    client.send_to(&payload, listen_addr).unwrap();
    let mut response = [0u8; 1024];
    let (size, _) = client.recv_from(&mut response).unwrap();
    assert_eq!(&response[..size], payload.as_slice());
    echo.join().unwrap();

    stopped.store(true, Ordering::SeqCst);
    forwarder_thread.join().unwrap();

    let db = Connection::open(&database).unwrap();
    let summary: (String, i64, i64, i64) = db
        .query_row(
            "SELECT transport, client_to_target_bytes, target_to_client_bytes, recorded_events \
             FROM traffic_sessions",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(summary.0, "udp");
    assert_eq!(summary.1, payload.len() as i64);
    assert_eq!(summary.2, payload.len() as i64);
    assert_eq!(summary.3, 2);

    let event_types: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM traffic_events \
             WHERE event_type = 'udp_datagram' AND original_size = ?1 AND stored_size = ?1",
            [payload.len() as i64],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(event_types, 2);

    drop(db);
    remove_database(&database);
}

#[test]
fn combined_forwarder_keeps_recording_after_listener_restart() {
    let database = database_path("restart-recording");
    let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
    let upstream_addr = upstream.local_addr().unwrap();
    let listen_addr = unused_tcp_addr();
    let payload = b"forward after restart".to_vec();
    let expected = payload.clone();
    let echo = std::thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = upstream.accept().unwrap();
            let mut received = vec![0; expected.len()];
            stream.read_exact(&mut received).unwrap();
            assert_eq!(received, expected);
            stream.write_all(&received).unwrap();
        }
    });

    let config = ForwardSessionConfig {
        local: listen_addr.to_string(),
        remoteMap: vec![(".*".to_string(), upstream_addr.to_string())],
        allow_nets: vec!["127.0.0.0/8".to_string()],
        enable_tcp: true,
        enable_udp: false,
        conn_bufsize: 1024 * 1024,
        max_connections: 10,
        tcp_mode: TcpMode::Forward,
        recording: Some(TrafficRecordingConfig::new(database.to_string_lossy())),
    };
    let forwarder = TcpUdpForwarder::from(&config).unwrap();

    for _ in 0..2 {
        let close = forwarder.listen();
        let mut client = connect_with_retry(listen_addr);
        client.write_all(&payload).unwrap();
        client.shutdown(Shutdown::Write).unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).unwrap();
        assert_eq!(response, payload);
        close();
    }
    echo.join().unwrap();

    let db = Connection::open(&database).unwrap();
    let summary: (i64, i64, i64) = db
        .query_row(
            "SELECT COUNT(*), SUM(client_to_target_bytes), SUM(target_to_client_bytes) \
             FROM traffic_sessions",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        summary,
        (2, (payload.len() * 2) as i64, (payload.len() * 2) as i64)
    );

    drop(db);
    drop(forwarder);
    remove_database(&database);
}
