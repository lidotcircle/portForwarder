use mio::net::{TcpListener, TcpStream};
use mio::{Events, Interest, Poll, Token};
use ntest::timeout;
use portforwarder::forward_config::{ForwardSessionConfig, TcpMode};
use portforwarder::tcp_forwarder::TcpForwarder;
use rand::Rng;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Write};
use std::net::ToSocketAddrs;
use std::rc::Rc;
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

fn tcp_sender<T: ToSocketAddrs>(addr: T, finished: Arc<AtomicBool>) {
    let server_addr = addr.to_socket_addrs().unwrap().next().unwrap();
    let mut stream = TcpStream::connect(server_addr).expect("Failed to connect");

    let mut poll = Poll::new().unwrap();
    let token = Token(0);
    poll.registry()
        .register(&mut stream, token, Interest::WRITABLE | Interest::READABLE)
        .unwrap();

    let mut rng = rand::thread_rng();
    let mut buf_storage = Vec::new();
    let target_bytes = 1024 * 100;
    let mut send_bytes = 0;
    let mut recv_bytes = 0;
    let mut events = Events::with_capacity(1024);
    let mut buffer = vec![0; 1024];

    while recv_bytes < target_bytes {
        poll.poll(&mut events, Some(Duration::from_secs(1)))
            .unwrap();

        for event in events.iter() {
            if event.token() != Token(0) {
                continue;
            }

            if event.is_writable() && send_bytes < target_bytes {
                let to_send = std::cmp::min(buffer.len(), target_bytes - send_bytes);
                rng.fill(&mut buffer[..to_send]);

                match stream.write(&buffer[..to_send]) {
                    Ok(n) => {
                        buf_storage.extend_from_slice(&buffer[..n]);
                        send_bytes += n;
                        println!("TCP sender sent {} bytes", n);
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) => panic!("Failed to send data: {}", e),
                }

                if send_bytes >= target_bytes {
                    stream.shutdown(std::net::Shutdown::Write).unwrap();
                }
            }

            if event.is_readable() {
                let mut recv_buffer = [0; 1024];
                let old_n = recv_bytes;
                loop {
                    match stream.read(&mut recv_buffer) {
                        Ok(0) => {
                            if old_n == recv_bytes {
                                // panic!("Connection closed prematurely");
                            }
                            break;
                        }
                        Ok(n) => {
                            assert!(buf_storage.len() >= recv_bytes + n);
                            assert_eq!(&buf_storage[recv_bytes..recv_bytes + n], &recv_buffer[..n]);
                            recv_bytes += n;
                            println!("TCP sender received {} bytes (total {})", n, recv_bytes);
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(e) => panic!("Failed to read: {}", e),
                    }
                }
            }
        }
    }

    finished.store(true, std::sync::atomic::Ordering::SeqCst);
}

fn tcp_echo<T: ToSocketAddrs>(listen_addr: T, finished: Arc<AtomicBool>) {
    let addr = listen_addr.to_socket_addrs().unwrap().next().unwrap();
    let mut listener = TcpListener::bind(addr).expect("Failed to bind TCP listener");

    let mut poll = Poll::new().unwrap();
    let mut connections = HashMap::new();
    let mut finished_connections = HashSet::new();
    let mut next_token = 1;
    let mut recieved_bytes: u64 = 0;
    let mut send_bytes: u64 = 0;

    poll.registry()
        .register(&mut listener, Token(0), Interest::READABLE)
        .unwrap();

    let mut events = Events::with_capacity(1024);

    while !finished.load(std::sync::atomic::Ordering::SeqCst) {
        poll.poll(&mut events, Some(Duration::from_millis(100)))
            .unwrap();

        for event in events.iter() {
            match event.token() {
                Token(0) => loop {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let token = Token(next_token);
                            next_token += 1;

                            poll.registry()
                                .register(&mut stream, token, Interest::READABLE)
                                .unwrap();

                            connections.insert(token, (Rc::new(RefCell::new(stream)), Vec::new()));
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(e) => panic!("Failed to accept: {}", e),
                    }
                },
                token => {
                    if event.is_readable() {
                        let (stream, write_buf) = match connections.get_mut(&token) {
                            Some(c) => c,
                            None => continue,
                        };

                        let mut read_buf = [0; 1024];
                        let mut remove_stream = false;
                        loop {
                            match stream.borrow_mut().read(&mut read_buf) {
                                Ok(0) => {
                                    remove_stream = true;
                                    break;
                                }
                                Ok(n) => {
                                    recieved_bytes += n as u64;
                                    write_buf.extend_from_slice(&read_buf[..n]);
                                }
                                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                                Err(e) => panic!("Failed to read: {}", e),
                            }
                        }

                        if !write_buf.is_empty() {
                            poll.registry()
                                .reregister(
                                    &mut *stream.borrow_mut(),
                                    token,
                                    Interest::READABLE | Interest::WRITABLE,
                                )
                                .unwrap();
                        }

                        if remove_stream {
                            finished_connections.insert(token);
                            poll.registry()
                                .reregister(&mut *stream.borrow_mut(), token, Interest::WRITABLE)
                                .unwrap();
                        }
                    }

                    if event.is_writable() {
                        let (stream, write_buf) = match connections.get_mut(&token) {
                            Some(c) => c,
                            None => continue,
                        };

                        let mut written = 0;
                        while written < write_buf.len() {
                            match stream.borrow_mut().write(&write_buf[written..]) {
                                Ok(n) => written += n,
                                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                                Err(e) => panic!("Failed to write: {}", e),
                            }
                        }
                        send_bytes += written as u64;
                        write_buf.drain(0..written);

                        if write_buf.is_empty() {
                            poll.registry()
                                .reregister(&mut *stream.borrow_mut(), token, Interest::READABLE)
                                .unwrap();

                            if finished_connections.contains(&token) {
                                connections.remove(&token);
                                finished_connections.remove(&token);
                            }
                        }
                    }
                }
            }
        }
    }

    assert_eq!(recieved_bytes, send_bytes);
}

fn run_tcp_forwarder(finished: Arc<AtomicBool>) {
    run_tcp_forwarder_at("localhost:33833", "localhost:32345", finished);
}

fn run_tcp_forwarder_at(local: &'static str, remote: &'static str, finished: Arc<AtomicBool>) {
    let remote_map = vec![(".*".to_string(), remote.to_string())];
    let config = ForwardSessionConfig {
        local,
        remoteMap: remote_map,
        enable_tcp: true,
        enable_udp: false,
        conn_bufsize: 1024 * 1024,
        allow_nets: vec!["127.0.0.1/24".to_string(), "::1/128".to_string()],
        max_connections: 256,
        tcp_mode: TcpMode::Forward,
    };
    let forwarder = TcpForwarder::from(&config).unwrap();
    forwarder.listen(finished).unwrap();
}

#[test]
#[timeout(60000)]
fn test_tcp_forwarder() {
    let _guard = test_lock();
    init_log();

    let finished: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let lx1 = finished.clone();
    let forwarder_thread = std::thread::spawn(move || run_tcp_forwarder(lx1));
    std::thread::sleep(Duration::from_millis(200));

    let lx2 = finished.clone();
    let echo_thread = std::thread::spawn(move || tcp_echo("localhost:32345", lx2));
    let lx3 = finished.clone();
    let sender_thread = std::thread::spawn(move || tcp_sender("localhost:33833", lx3));

    sender_thread.join().unwrap();
    echo_thread.join().unwrap();
    forwarder_thread.join().unwrap();

    assert!(finished.load(std::sync::atomic::Ordering::SeqCst));
}

#[test]
#[timeout(90000)]
fn test_tcp_forwarder_congestion_multi_clients() {
    let _guard = test_lock();
    init_log();

    let finished: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));

    let p1 = finished.clone();
    let forwarder_thread =
        std::thread::spawn(move || run_tcp_forwarder_at("127.0.0.1:33841", "127.0.0.1:32355", p1));
    std::thread::sleep(Duration::from_millis(200));

    let p2 = finished.clone();
    let backend_thread = std::thread::spawn(move || {
        let listener = std::net::TcpListener::bind("127.0.0.1:32355").unwrap();
        listener.set_nonblocking(true).unwrap();
        while !p2.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    std::thread::spawn(move || {
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                        let mut buf = [0u8; 4096];
                        loop {
                            match stream.read(&mut buf) {
                                Ok(0) => break,
                                Ok(n) => {
                                    for (chunk_idx, chunk) in buf[..n].chunks(173).enumerate() {
                                        if chunk_idx % 2 == 0 {
                                            std::thread::sleep(Duration::from_millis(1));
                                        } else {
                                            std::thread::sleep(Duration::from_millis(2));
                                        }
                                        if stream.write_all(chunk).is_err() {
                                            return;
                                        }
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                    });
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    });
    std::thread::sleep(Duration::from_millis(100));

    let mut workers = Vec::new();
    for w in 0..10usize {
        workers.push(std::thread::spawn(move || {
            for i in 0..10usize {
                let mut client = std::net::TcpStream::connect("127.0.0.1:33841").unwrap();
                client.set_read_timeout(Some(Duration::from_secs(8))).unwrap();
                client.set_write_timeout(Some(Duration::from_secs(8))).unwrap();

                let mut payload = vec![0u8; 192 * 1024];
                for (idx, b) in payload.iter_mut().enumerate() {
                    *b = ((idx + i + w) % 251) as u8;
                }

                let mut sent = 0usize;
                while sent < payload.len() {
                    let end = (sent + 389).min(payload.len());
                    client.write_all(&payload[sent..end]).unwrap();
                    sent = end;
                    if sent % (4 * 1024) == 0 {
                        if ((sent / 4096) + i + w) % 2 == 0 {
                            std::thread::sleep(Duration::from_millis(1));
                        } else {
                            std::thread::sleep(Duration::from_millis(2));
                        }
                    }
                }
                client.shutdown(std::net::Shutdown::Write).unwrap();

                let mut out = Vec::with_capacity(payload.len());
                let mut buf = [0u8; 1024];
                while out.len() < payload.len() {
                    let n = client.read(&mut buf).unwrap();
                    assert!(n > 0, "unexpected EOF from proxy");
                    out.extend_from_slice(&buf[..n]);
                    if (out.len() / 1024 + i + w) % 3 == 0 {
                        std::thread::sleep(Duration::from_millis(2));
                    } else {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
                out.truncate(payload.len());
                assert_eq!(out, payload, "mismatch worker {w}, iter {i}");
            }
        }));
    }

    for worker in workers {
        worker.join().unwrap();
    }

    finished.store(true, Ordering::SeqCst);
    backend_thread.join().unwrap();
    forwarder_thread.join().unwrap();
}
