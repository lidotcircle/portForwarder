#[allow(non_snake_case)]
use std::net::UdpSocket;

pub fn uu() {
    let sockaddr = "127.0.0.1:7744";
    let usock = UdpSocket::bind(sockaddr).unwrap();
    let mut buf: [u8; 16*16] = [0; 16*16];
    let mut exit = false;
    while !exit {
        match usock.recv(&mut buf[..]) {
            Ok(v) => {println!("{:?}", v);},
            Err(_) => {exit = true;}
        }
    }
}

