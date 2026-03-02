use mio::net::UdpSocket;
use mio::{Events, Interest, Poll, Token};
use ntest::timeout;
use portforwarder::forward_config::{ForwardSessionConfig, TcpMode};
use portforwarder::udp_forwarder::UdpForwarder;
use rand::Rng;
use std::collections::HashSet;
use std::convert::TryInto;
use std::net::ToSocketAddrs;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

fn init_log() {
    let _ = env_logger::builder().is_test(true).try_init();
}

fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    match LOCK.get_or_init(|| Mutex::new(())).lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    }
}

fn udp_sender<T: ToSocketAddrs>(addr: T, finished: Arc<AtomicBool>) {
    // Create a UDP socket and bind it to a random local port
    let mut socket = UdpSocket::bind("localhost:0".to_socket_addrs().unwrap().next().unwrap())
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
                        let mut ermsg = String::from("Failed to send UDP packet, addr = ");
                        ermsg.push_str(format!("{:?}", server_addr).as_str());
                        let s = socket.send_to(&buffer[..], server_addr).expect(&ermsg);
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
    run_udp_forwarder_at("localhost:33833", "localhost:32345", finished);
}

fn run_udp_forwarder_at(local: &'static str, remote: &'static str, finished: Arc<AtomicBool>) {
    let remote_map: Vec<(String, String)> = vec![(".*".to_string(), remote.to_string())];
    let config = ForwardSessionConfig {
        local,
        remoteMap: remote_map,
        enable_tcp: false,
        enable_udp: true,
        conn_bufsize: 1024 * 1024,
        allow_nets: ["127.0.0.1/24".to_string(), "::1/128".to_string()].to_vec(),
        max_connections: 256,
        tcp_mode: TcpMode::Forward,
    };
    let forwarder_wrap = UdpForwarder::from(&config);
    assert!(forwarder_wrap.is_ok());
    let forwarder = forwarder_wrap.unwrap();
    let result = forwarder.listen(finished.clone());
    assert!(result.is_ok());
}

#[test]
#[timeout(60000)]
fn test_udp_forwader() {
    let _guard = test_lock();
    init_log();

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

#[test]
#[timeout(90000)]
fn test_udp_forwarder_congestion_burst() {
    let _guard = test_lock();
    init_log();

    let finished: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));

    let p1 = finished.clone();
    let fd = std::thread::spawn(move || {
        run_udp_forwarder_at("127.0.0.1:33842", "127.0.0.1:32356", p1);
    });
    std::thread::sleep(Duration::from_millis(200));

    let p2 = finished.clone();
    let echo_thread = std::thread::spawn(move || {
        let socket = std::net::UdpSocket::bind("127.0.0.1:32356").unwrap();
        socket.set_nonblocking(true).unwrap();
        let mut buf = [0u8; 2048];
        while !p2.load(Ordering::SeqCst) {
            match socket.recv_from(&mut buf) {
                Ok((n, addr)) => {
                    if n % 3 == 0 {
                        std::thread::sleep(Duration::from_millis(2));
                    } else {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    let _ = socket.send_to(&buf[..n], addr);
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(_) => break,
            }
        }
    });
    std::thread::sleep(Duration::from_millis(100));

    let mut workers = Vec::new();
    let worker_count = 8u64;
    let packets_per_worker = 220u64;
    for worker_id in 0..worker_count {
        workers.push(std::thread::spawn(move || {
            let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            socket.set_nonblocking(true).unwrap();

            let packets = packets_per_worker;
            let mut next_send = 0u64;
            let mut seen = HashSet::new();
            let deadline = Instant::now() + Duration::from_secs(20);
            let mut recv_buf = [0u8; 512];

            while (next_send < packets || seen.len() < packets as usize) && Instant::now() < deadline
            {
                for _ in 0..2 {
                    if next_send >= packets {
                        break;
                    }
                    let id = (worker_id << 32) | next_send;
                    let mut payload = vec![0u8; 256];
                    payload[..8].copy_from_slice(&id.to_be_bytes());
                    for (i, b) in payload[8..].iter_mut().enumerate() {
                        *b = ((i as u64 + next_send + worker_id) % 251) as u8;
                    }
                    socket.send_to(&payload, "127.0.0.1:33842").unwrap();
                    next_send += 1;
                }

                let mut drained = 0usize;
                loop {
                    match socket.recv_from(&mut recv_buf) {
                        Ok((n, _)) => {
                            assert_eq!(n, 256);
                            let id = u64::from_be_bytes(recv_buf[..8].try_into().unwrap());
                            let seq = (id & 0xFFFF_FFFF) as usize;
                            for (i, b) in recv_buf[8..n].iter().enumerate() {
                                assert_eq!(
                                    *b,
                                    ((i + seq + worker_id as usize) % 251) as u8,
                                    "payload corruption for worker {worker_id}, seq {seq}"
                                );
                            }
                            seen.insert(id);
                            drained += 1;
                            if drained >= 12 {
                                break;
                            }
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(err) => panic!("udp recv failed: {}", err),
                    }
                }

                std::thread::sleep(Duration::from_millis(1));
            }

            seen.len()
        }));
    }

    let mut total_seen = 0usize;
    for w in workers {
        total_seen += w.join().unwrap();
    }
    let total_sent = (worker_count * packets_per_worker) as usize;
    assert!(
        total_seen >= total_sent * 12 / 100,
        "too many UDP drops under congestion: received {}, sent {}",
        total_seen,
        total_sent
    );

    finished.store(true, Ordering::SeqCst);
    echo_thread.join().unwrap();
    fd.join().unwrap();
}
