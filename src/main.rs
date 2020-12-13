#![allow(non_snake_case)]

mod tcp_forwarder;
mod udp_forwarder;
mod tcp_udp_forwarder;
mod utils;
use tcp_udp_forwarder::*;
use regex::Regex;

static USAGE: &'static str = 
"[-h] <bind_addr> <forward_address>

    -h    show help";

fn usage() {
    let args: Vec<String> = std::env::args().collect();
    println!("usage: \n    {} {}", args[0], USAGE);
}

fn main() {
    let mut bind_addr = "";
    let mut forward_addr = "";
    let mut args: Vec<String> = std::env::args().collect();
    args.remove(0);
    let valid_ipv4_port = Regex::new(r"^([0-9]{1,3}.){3}[0-9]{1,3}:[0-9]{1,5}$").unwrap();
    for i in 0..args.len() {
        let s = args[i].as_str();
        match s {
            "-h" => {
                usage();
                std::process::exit(0);
            }
            _ => {
                if valid_ipv4_port.is_match(s) {
                    if bind_addr.len() == 0 {
                        bind_addr = s;
                    } else if forward_addr.len() == 0 {
                        forward_addr = s;
                    } else {
                        usage();
                        std::process::exit(-1);
                    }
                } else {
                    usage();
                    std::process::exit(-1);
                }
            }
        }
    }

    if bind_addr.len() == 0 || forward_addr.len() == 0 {
        usage();
        std::process::exit(-1);
    }

    let forwarder = TcpUdpForwarder::from(&bind_addr,&forward_addr,true,true).unwrap();
    forwarder.listen();
}

