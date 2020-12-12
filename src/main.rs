#![allow(non_snake_case)]

mod tcp_forwarder;
mod udp_forwarder;
mod tcp_udp_forwarder;
mod utils;
use tcp_udp_forwarder::*;

fn main() {
    let mut forwarder = TcpUdpForwarder::from(&"0.0.0.0:7722", &"127.0.0.1:80", true, true).unwrap();
    forwarder.listen();
}

