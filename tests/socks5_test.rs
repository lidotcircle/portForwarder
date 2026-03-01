use ntest::timeout;
use portforwarder::forward_config::{ForwardSessionConfig, TcpMode};
use portforwarder::tcp_forwarder::TcpForwarder;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

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

fn run_socks5_forwarder(finished: Arc<AtomicBool>) {
    let config = ForwardSessionConfig {
        local: "127.0.0.1:33834",
        remoteMap: vec![],
        enable_tcp: true,
        enable_udp: false,
        conn_bufsize: 1024 * 1024,
        allow_nets: vec!["127.0.0.1/24".to_string()],
        max_connections: 10,
        tcp_mode: TcpMode::Socks5Server,
    };
    let forwarder = TcpForwarder::from(&config).unwrap();
    forwarder.listen(finished).unwrap();
}

#[test]
#[timeout(8000)]
fn test_socks5_server_connect_and_relay() {
    env_logger::init_from_env(
        env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "debug"),
    );

    let finished = Arc::new(AtomicBool::new(false));
    let lx1 = finished.clone();
    let forwarder_thread = std::thread::spawn(move || run_socks5_forwarder(lx1));
    std::thread::sleep(Duration::from_millis(200));

    let lx2 = finished.clone();
    let echo_thread = std::thread::spawn(move || tcp_echo("127.0.0.1:32346", lx2));
    std::thread::sleep(Duration::from_millis(100));

    let mut client = TcpStream::connect("127.0.0.1:33834").unwrap();

    client.write_all(&[0x05, 0x01, 0x00]).unwrap();
    let mut greet_resp = [0u8; 2];
    client.read_exact(&mut greet_resp).unwrap();
    assert_eq!(greet_resp, [0x05, 0x00]);

    let req = [0x05, 0x01, 0x00, 0x01, 127, 0, 0, 1, 126, 90];
    client.write_all(&req).unwrap();

    let mut conn_resp_head = [0u8; 4];
    client.read_exact(&mut conn_resp_head).unwrap();
    assert_eq!(conn_resp_head[0], 0x05);
    assert_eq!(conn_resp_head[1], 0x00);
    assert_eq!(conn_resp_head[2], 0x00);

    let to_read = match conn_resp_head[3] {
        0x01 => 6,
        0x03 => {
            let mut len = [0u8; 1];
            client.read_exact(&mut len).unwrap();
            len[0] as usize + 2
        }
        0x04 => 18,
        _ => panic!("invalid atyp in socks5 reply"),
    };
    let mut skip = vec![0u8; to_read];
    client.read_exact(&mut skip).unwrap();

    let payload = b"socks5-relay-test";
    client.write_all(payload).unwrap();
    let mut echoed = vec![0u8; payload.len()];
    client.read_exact(&mut echoed).unwrap();
    assert_eq!(payload.to_vec(), echoed);

    client.shutdown(Shutdown::Both).unwrap_or(());
    finished.store(true, Ordering::SeqCst);

    echo_thread.join().unwrap();
    forwarder_thread.join().unwrap();
}
