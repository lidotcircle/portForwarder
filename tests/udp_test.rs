use mio::net::UdpSocket;
use mio::{Events, Interest, Poll, Token};
use ntest::timeout;
use portforwarder::forward_config::ForwardSessionConfig;
use portforwarder::udp_forwarder::UdpForwarder;
use rand::Rng;
use std::net::ToSocketAddrs;
use std::sync::{Arc, atomic::AtomicBool};
use std::time::Duration;

fn udp_sender<T: ToSocketAddrs>(addr: T, finished: Arc<AtomicBool>) {
    // Create a UDP socket and bind it to a random local port
    let mut socket = UdpSocket::bind("0.0.0.0:0".to_socket_addrs().unwrap().next().unwrap())
        .expect("Failed to bind UDP socket");

    // Resolve the server address
    let server_addr = match addr
        .to_socket_addrs()
        .expect("Failed to resolve server address")
        .next()
    {
        Some(addr) => addr,
        None => panic!("Failed to resolve server address"),
    };

    let mut poll = Poll::new().expect("Failed to create Poll instance");
    let token = Token(0);
    poll.registry()
        .register(&mut socket, token, Interest::WRITABLE | Interest::READABLE)
        .expect("Failed to register UDP socket with Poll");

    let mut rng = rand::thread_rng();
    let mut buf_storage: Vec<u8> = vec![];
    const RECIEVE_BUFSIZE: usize = 1024;
    let mut buffer = vec![0; RECIEVE_BUFSIZE / 2];

    let target_bytes = RECIEVE_BUFSIZE * 100;
    let mut finish_bytes = 0;
    let mut send_bytes = 0;
    let mut events = Events::with_capacity(1024);
    while finish_bytes < target_bytes {
        poll.poll(&mut events, Some(Duration::from_secs(1)))
            .expect("Failed to poll events");

        for event in events.iter() {
            match event.token() {
                Token(0) => {
                    if event.is_readable() {
                        loop {
                            let mut response_buffer = [0; RECIEVE_BUFSIZE];
                            match socket.recv_from(&mut response_buffer) {
                                Ok((num_bytes, _)) => {
                                    if num_bytes > 0 {
                                        assert!(buf_storage.len() >= num_bytes);
                                        assert_eq!(
                                            &buf_storage[0..num_bytes],
                                            &response_buffer[0..num_bytes]
                                        );
                                        buf_storage.drain(0..num_bytes);
                                        finish_bytes += num_bytes;
                                        println!(
                                            "udpClient: recieve {num_bytes} bytes, totalRecieved/target = {finish_bytes}/{target_bytes}"
                                        );
                                    }
                                }
                                Err(e) => {
                                    if e.kind() == std::io::ErrorKind::WouldBlock {
                                        break;
                                    }
                                    assert!(false);
                                }
                            }
                        }
                    }

                    if event.is_writable() && send_bytes < target_bytes {
                        rng.fill(&mut buffer[..]);
                        let s = socket
                            .send_to(&buffer[..], server_addr)
                            .expect("Failed to send UDP packet");
                        println!("udpClient: sent {} bytes", s);
                        buf_storage.append(&mut buffer.clone()[0..s].to_vec());
                        send_bytes += s;
                    }
                }
                _ => unreachable!(),
            }
        }
    }
    println!("sender has completed his work!!!");
    finished.store(true, std::sync::atomic::Ordering::SeqCst);
}

fn udp_echo<T: ToSocketAddrs>(listen_addr: T, finished: Arc<AtomicBool>) {
    let addr = listen_addr.to_socket_addrs().unwrap().next().unwrap();
    let socket = UdpSocket::bind(addr).expect("Failed to bind UDP socket");
    let mut recieved_bytes = 0;

    while finished.load(std::sync::atomic::Ordering::SeqCst) == false {
        // Create a buffer to store the received data
        let mut buffer = [0; 1024];

        let kk = socket.recv_from(&mut buffer);
        if kk.is_err() {
            std::thread::sleep(std::time::Duration::from_millis(5));
            continue;
        }
        // Receive data from the socket
        let (num_bytes, client_addr) = kk.expect("Failed to receive UDP packet");
        if num_bytes == 0 {
            continue;
        }
        recieved_bytes += num_bytes;

        // Send the received buffer back to the client
        println!(
            "udpEchoServer: Received {} bytes from {}, total = {recieved_bytes}",
            num_bytes, client_addr
        );
        let n = socket
            .send_to(&buffer[..num_bytes], client_addr)
            .expect("Failed to send UDP packet");
        assert_eq!(n, num_bytes);
        println!("udpEchoServer: send {n} bytes");
    }
}

fn run_udp_forwarder(finished: Arc<AtomicBool>) {
    let remote_map: Vec<(String, String)> = vec![(".*".to_string(), "localhost:32345".to_string())];
    let config = ForwardSessionConfig {
        local: "localhost:33833",
        remoteMap: remote_map,
        enable_tcp: false,
        enable_udp: true,
        conn_bufsize: 1024 * 1024,
        allow_nets: ["127.0.0.1/24".to_string()].to_vec(),
        max_connections: 10,
    };
    let forwarder_wrap = UdpForwarder::from(&config);
    assert!(forwarder_wrap.is_ok());
    let forwarder = forwarder_wrap.unwrap();
    let result = forwarder.listen(finished.clone());
    assert!(result.is_ok());
}

#[test]
#[timeout(8000)]
fn test_udp_forwader() {
    env_logger::init_from_env(
        env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "debug"),
    );

    let finished: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let lx1 = finished.clone();
    let fd = std::thread::spawn(move || {
        run_udp_forwarder(lx1);
    });
    std::thread::sleep(std::time::Duration::from_secs(1));

    let lx2 = finished.clone();
    let h1 = std::thread::spawn(move || {
        udp_echo("localhost:32345", lx2);
    });
    let lx3 = finished.clone();
    let h2 = std::thread::spawn(|| {
        udp_sender("localhost:33833", lx3);
    });
    h2.join().unwrap();
    h1.join().unwrap();
    fd.join().unwrap();
    assert!(finished.load(std::sync::atomic::Ordering::SeqCst));
}
