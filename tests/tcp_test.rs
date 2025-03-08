use mio::net::{TcpListener, TcpStream};
use mio::{Events, Interest, Poll, Token};
use ntest::timeout;
use portforwarder::forward_config::ForwardSessionConfig;
use portforwarder::tcp_forwarder::TcpForwarder;
use rand::Rng;
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::ToSocketAddrs;
use std::rc::Rc;
use std::sync::{Arc, atomic::AtomicBool};
use std::time::Duration;

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
    let mut next_token = 1;

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
                            connections.remove(&token);
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
                        write_buf.drain(0..written);

                        if write_buf.is_empty() {
                            poll.registry()
                                .reregister(&mut *stream.borrow_mut(), token, Interest::READABLE)
                                .unwrap();
                        }
                    }
                }
            }
        }
    }
}

fn run_tcp_forwarder(finished: Arc<AtomicBool>) {
    let remote_map = vec![(".*".to_string(), "localhost:32345".to_string())];
    let config = ForwardSessionConfig {
        local: "localhost:33833",
        remoteMap: remote_map,
        enable_tcp: true,
        enable_udp: false,
        conn_bufsize: 1024 * 1024,
        allow_nets: vec!["127.0.0.1/24".to_string()],
        max_connections: 10,
    };
    let forwarder = TcpForwarder::from(&config).unwrap();
    forwarder.listen(finished).unwrap();
}

#[test]
#[timeout(8000)]
fn test_tcp_forwarder() {
    env_logger::init_from_env(
        env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "debug"),
    );

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
