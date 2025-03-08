use mio::net::UdpSocket;
use mio::{Events, Interest, Poll, Token};
use portforwarder::forward_config::ForwardSessionConfig;
use portforwarder::udp_forwarder::UdpForwarder;
use rand::Rng;
use std::net::ToSocketAddrs;
use std::sync::{Arc, atomic::AtomicBool};
use std::time::Duration;

fn udp_sender<T: ToSocketAddrs>(addr: T) {
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
    let mut events = Events::with_capacity(1024);
    let mut write_to_socket = true;
    while finish_bytes < target_bytes {
        poll.poll(&mut events, Some(Duration::from_secs(1)))
            .expect("Failed to poll events");

        for event in events.iter() {
            match event.token() {
                Token(0) => {
                    if event.is_readable() {
                        let mut response_buffer = [0; RECIEVE_BUFSIZE];
                        let (num_bytes, _) = socket
                            .recv_from(&mut response_buffer)
                            .expect("Failed to receive UDP packet");
                        if num_bytes > 0 {
                            assert!(buf_storage.len() >= num_bytes);
                            assert_eq!(&buf_storage[0..num_bytes], &response_buffer[0..num_bytes]);
                            buf_storage.drain(0..num_bytes);
                            finish_bytes += num_bytes;
                        }
                        write_to_socket = buf_storage.is_empty();
                    }

                    if event.is_writable() && write_to_socket {
                        rng.fill(&mut buffer[..]);
                        let s = socket
                            .send_to(&buffer[..], server_addr)
                            .expect("Failed to send UDP packet");
                        buf_storage.append(&mut buffer.clone()[0..s].to_vec());
                    }
                }
                _ => unreachable!(),
            }
        }
    }
}

fn udp_echo<T: ToSocketAddrs>(listen_addr: T) {
    let addr = listen_addr.to_socket_addrs().unwrap().next().unwrap();
    let socket = UdpSocket::bind(addr).expect("Failed to bind UDP socket");

    loop {
        // Create a buffer to store the received data
        let mut buffer = [0; 1024];

        let kk = socket.recv_from(&mut buffer);
        if kk.is_err() {
            continue;
        }
        // Receive data from the socket
        let (num_bytes, client_addr) = kk.expect("Failed to receive UDP packet");
        if num_bytes == 0 {
            continue;
        }

        // Send the received buffer back to the client
        let _ = socket
            .send_to(&buffer[..num_bytes], client_addr)
            .expect("Failed to send UDP packet");
    }
}

#[test]
fn test_udp_forwader() {
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
    let h1 = std::thread::spawn(|| {
        udp_echo("localhost:32345");
    });
    std::thread::sleep(std::time::Duration::from_secs(1));
    let h2 = std::thread::spawn(|| {
        udp_sender("localhost:33833");
    });
    let result = forwarder.listen(Arc::new(AtomicBool::from(false)));
    assert!(result.is_ok());
    h2.join().unwrap();
    h1.join().unwrap();
}
