use ntest::timeout;
use portforwarder::forward_config::{ForwardSessionConfig, TcpMode};
use portforwarder::tcp_forwarder::TcpForwarder;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

fn init_log() {
    let _ = env_logger::builder().is_test(true).try_init();
}

fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
}

fn run_proxy(local: &'static str, finished: Arc<AtomicBool>) {
    let config = ForwardSessionConfig {
        local,
        remoteMap: vec![],
        enable_tcp: true,
        enable_udp: false,
        conn_bufsize: 1024 * 1024,
        allow_nets: vec!["127.0.0.1/24".to_string()],
        max_connections: 256,
        tcp_mode: TcpMode::Socks5Server,
    };
    let forwarder = TcpForwarder::from(&config).unwrap();
    forwarder.listen(finished).unwrap();
}

fn tcp_echo(listen_addr: &str, finished: Arc<AtomicBool>) {
    let listener = TcpListener::bind(listen_addr).unwrap();
    listener.set_nonblocking(true).unwrap();

    while !finished.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                std::thread::spawn(move || {
                    let mut buf = [0u8; 4096];
                    loop {
                        match stream.read(&mut buf) {
                            Ok(0) => break,
                            Ok(n) => {
                                if stream.write_all(&buf[..n]).is_err() {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                });
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => break,
        }
    }
}

fn connect_via_socks5(proxy: &str, target: &str) -> TcpStream {
    let mut stream = TcpStream::connect(proxy).unwrap();
    stream
        .write_all(&[0x05, 0x01, 0x00]) // VER, NMETHODS, no-auth
        .unwrap();
    let mut auth_resp = [0u8; 2];
    stream.read_exact(&mut auth_resp).unwrap();
    assert_eq!(auth_resp, [0x05, 0x00]);

    let target_addr: std::net::SocketAddr = target.parse().unwrap();
    match target_addr {
        std::net::SocketAddr::V4(v4) => {
            let mut req = Vec::with_capacity(10);
            req.push(0x05); // VER
            req.push(0x01); // CONNECT
            req.push(0x00); // RSV
            req.push(0x01); // IPv4
            req.extend_from_slice(&v4.ip().octets());
            req.extend_from_slice(&v4.port().to_be_bytes());
            stream.write_all(&req).unwrap();
        }
        std::net::SocketAddr::V6(_) => panic!("test only supports ipv4 target"),
    }

    let mut head = [0u8; 4];
    stream.read_exact(&mut head).unwrap();
    assert_eq!(head[0], 0x05);
    assert_eq!(head[1], 0x00);
    match head[3] {
        0x01 => {
            let mut tail = [0u8; 6];
            stream.read_exact(&mut tail).unwrap();
        }
        0x04 => {
            let mut tail = [0u8; 18];
            stream.read_exact(&mut tail).unwrap();
        }
        _ => panic!("invalid atyp in socks5 reply"),
    }
    stream
}

fn http_backend(listen_addr: &str, finished: Arc<AtomicBool>) {
    let listener = TcpListener::bind(listen_addr).unwrap();
    listener.set_nonblocking(true).unwrap();
    while !finished.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((mut conn, _)) => {
                std::thread::spawn(move || {
                    let _ = conn.set_read_timeout(Some(Duration::from_secs(2)));
                    let mut req = vec![0u8; 8192];
                    let mut total = 0usize;
                    loop {
                        if total >= req.len() {
                            break;
                        }
                        match conn.read(&mut req[total..]) {
                            Ok(0) => break,
                            Ok(n) => {
                                total += n;
                                if req[..total].windows(4).any(|w| w == b"\r\n\r\n") {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    let req_s = String::from_utf8_lossy(&req[..total]);
                    let first_line = req_s.lines().next().unwrap_or("GET / HTTP/1.1");
                    let body = format!("ok:{first_line}");
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = conn.write_all(resp.as_bytes());
                    let _ = conn.shutdown(Shutdown::Both);
                });
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => break,
        }
    }
}

#[test]
#[timeout(8000)]
fn test_http_connect_on_socks5_port() {
    let _guard = test_lock();
    init_log();
    let finished = Arc::new(AtomicBool::new(false));

    let lx1 = finished.clone();
    let proxy_thread = std::thread::spawn(move || run_proxy("127.0.0.1:33835", lx1));
    std::thread::sleep(Duration::from_millis(200));

    let lx2 = finished.clone();
    let echo_thread = std::thread::spawn(move || tcp_echo("127.0.0.1:32347", lx2));
    std::thread::sleep(Duration::from_millis(100));

    let mut client = TcpStream::connect("127.0.0.1:33835").unwrap();
    client
        .write_all(b"CONNECT 127.0.0.1:32347 HTTP/1.1\r\nHost: 127.0.0.1:32347\r\n\r\n")
        .unwrap();

    let mut resp = [0u8; 128];
    let n = client.read(&mut resp).unwrap();
    let status = String::from_utf8_lossy(&resp[..n]);
    assert!(status.starts_with("HTTP/1.1 200"));

    let payload = b"http-connect-through-proxy";
    client.write_all(payload).unwrap();
    let mut echoed = vec![0u8; payload.len()];
    client.read_exact(&mut echoed).unwrap();
    assert_eq!(payload.to_vec(), echoed);

    client.shutdown(Shutdown::Both).unwrap_or(());
    finished.store(true, Ordering::SeqCst);
    echo_thread.join().unwrap();
    proxy_thread.join().unwrap();
}

#[test]
#[timeout(8000)]
fn test_http_forward_on_socks5_port() {
    let _guard = test_lock();
    init_log();
    let finished = Arc::new(AtomicBool::new(false));

    let lx1 = finished.clone();
    let proxy_thread = std::thread::spawn(move || run_proxy("127.0.0.1:33836", lx1));
    std::thread::sleep(Duration::from_millis(200));

    let http_server = TcpListener::bind("127.0.0.1:32348").unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let server_thread = std::thread::spawn(move || {
        let (mut conn, _) = http_server.accept().unwrap();
        let mut req = vec![0u8; 2048];
        let n = conn.read(&mut req).unwrap();
        let req_s = String::from_utf8_lossy(&req[..n]);
        tx.send(req_s.to_string()).unwrap();
        conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            .unwrap();
    });

    let mut client = TcpStream::connect("127.0.0.1:33836").unwrap();
    client
        .write_all(
            b"GET http://127.0.0.1:32348/abc HTTP/1.1\r\nHost: 127.0.0.1:32348\r\nProxy-Connection: keep-alive\r\nConnection: keep-alive\r\n\r\n",
        )
        .unwrap();

    let seen_req = rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(
        seen_req.starts_with("GET /abc HTTP/1.1\r\n"),
        "request line: {}",
        seen_req
    );
    assert!(
        seen_req.contains("\r\nConnection: close\r\n"),
        "missing forced close header: {}",
        seen_req
    );
    assert!(
        !seen_req
            .to_ascii_lowercase()
            .contains("\r\nproxy-connection:"),
        "proxy-connection header should be stripped: {}",
        seen_req
    );
    std::thread::sleep(Duration::from_millis(50));

    client.shutdown(Shutdown::Both).unwrap_or(());
    finished.store(true, Ordering::SeqCst);
    server_thread.join().unwrap();
    proxy_thread.join().unwrap();
}

#[test]
#[timeout(8000)]
fn test_http_forward_absolute_uri_with_query_no_path() {
    let _guard = test_lock();
    init_log();
    let finished = Arc::new(AtomicBool::new(false));

    let lx1 = finished.clone();
    let proxy_thread = std::thread::spawn(move || run_proxy("127.0.0.1:33837", lx1));
    std::thread::sleep(Duration::from_millis(200));

    let http_server = TcpListener::bind("127.0.0.1:32349").unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let server_thread = std::thread::spawn(move || {
        let (mut conn, _) = http_server.accept().unwrap();
        let mut req = vec![0u8; 2048];
        let n = conn.read(&mut req).unwrap();
        let req_s = String::from_utf8_lossy(&req[..n]);
        tx.send(req_s.to_string()).unwrap();
        conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            .unwrap();
    });

    let mut client = TcpStream::connect("127.0.0.1:33837").unwrap();
    client
        .write_all(
            b"GET HTTP://127.0.0.1:32349?x=1 HTTP/1.1\r\nHost: 127.0.0.1:32349\r\nConnection: close\r\n\r\n",
        )
        .unwrap();

    let seen_req = rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(
        seen_req.starts_with("GET /?x=1 HTTP/1.1\r\n"),
        "request line: {}",
        seen_req
    );
    std::thread::sleep(Duration::from_millis(50));

    client.shutdown(Shutdown::Both).unwrap_or(());
    finished.store(true, Ordering::SeqCst);
    server_thread.join().unwrap();
    proxy_thread.join().unwrap();
}

#[test]
#[timeout(20000)]
fn test_mixed_proxy_protocol_stress_switching() {
    let _guard = test_lock();
    init_log();
    let finished = Arc::new(AtomicBool::new(false));

    let p1 = finished.clone();
    let proxy_thread = std::thread::spawn(move || run_proxy("127.0.0.1:33838", p1));
    std::thread::sleep(Duration::from_millis(200));

    let p2 = finished.clone();
    let echo_thread = std::thread::spawn(move || tcp_echo("127.0.0.1:32350", p2));

    let p3 = finished.clone();
    let http_thread = std::thread::spawn(move || http_backend("127.0.0.1:32351", p3));
    std::thread::sleep(Duration::from_millis(150));

    let mut workers = Vec::new();
    for t in 0..8usize {
        workers.push(std::thread::spawn(move || {
            for i in 0..15usize {
                let phase = (t + i) % 3;
                if phase == 0 {
                    let mut s = connect_via_socks5("127.0.0.1:33838", "127.0.0.1:32350");
                    let payload = format!("socks5-{}-{}", t, i).into_bytes();
                    s.write_all(&payload).unwrap();
                    let mut out = vec![0u8; payload.len()];
                    s.read_exact(&mut out).unwrap();
                    assert_eq!(out, payload, "socks5 mismatch at worker {t}, iter {i}");
                    let _ = s.shutdown(Shutdown::Both);
                } else if phase == 1 {
                    let mut s = TcpStream::connect("127.0.0.1:33838").unwrap();
                    s.write_all(
                        b"CONNECT 127.0.0.1:32350 HTTP/1.1\r\nHost: 127.0.0.1:32350\r\n\r\n",
                    )
                    .unwrap();
                    let mut resp = [0u8; 128];
                    let n = s.read(&mut resp).unwrap();
                    let status = String::from_utf8_lossy(&resp[..n]);
                    assert!(
                        status.starts_with("HTTP/1.1 200"),
                        "bad connect status at worker {}, iter {}: {}",
                        t,
                        i,
                        status
                    );
                    let payload = format!("http-connect-{}-{}", t, i).into_bytes();
                    s.write_all(&payload).unwrap();
                    let mut out = vec![0u8; payload.len()];
                    s.read_exact(&mut out).unwrap();
                    assert_eq!(out, payload, "http connect mismatch at worker {t}, iter {i}");
                    let _ = s.shutdown(Shutdown::Both);
                } else {
                    let mut s = TcpStream::connect("127.0.0.1:33838").unwrap();
                    s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                    let req = format!(
                        "GET http://127.0.0.1:32351/r/{}/{}?q={} HTTP/1.1\r\nHost: 127.0.0.1:32351\r\nProxy-Connection: keep-alive\r\nConnection: keep-alive\r\n\r\n",
                        t, i, i
                    );
                    s.write_all(req.as_bytes()).unwrap();
                    let mut buf = vec![0u8; 4096];
                    let n = s.read(&mut buf).unwrap();
                    let resp = String::from_utf8_lossy(&buf[..n]);
                    assert!(
                        resp.starts_with("HTTP/1.1 200"),
                        "bad http status at worker {}, iter {}: {}",
                        t,
                        i,
                        resp
                    );
                    assert!(
                        resp.contains("ok:GET /r/"),
                        "unexpected http body at worker {}, iter {}: {}",
                        t,
                        i,
                        resp
                    );
                    let _ = s.shutdown(Shutdown::Both);
                }
            }
        }));
    }

    for w in workers {
        w.join().unwrap();
    }

    finished.store(true, Ordering::SeqCst);
    echo_thread.join().unwrap();
    http_thread.join().unwrap();
    proxy_thread.join().unwrap();
}
