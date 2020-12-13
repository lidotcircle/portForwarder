use crate::tcp_forwarder::TcpForwarder;
use crate::udp_forwarder::UdpForwarder;
use std::net::ToSocketAddrs;
use std::error::Error;
use std::result::Result;
use std::sync::Arc;

#[derive(Clone)]
pub struct TcpUdpForwarder {
    tcp: Arc<Option<TcpForwarder>>,
    udp: Arc<Option<UdpForwarder>>
}

impl TcpUdpForwarder {
    pub fn from<T: ToSocketAddrs>(bind_addr: &T, connect_addr: &T, enable_udp: bool, enable_tcp: bool)
        -> Result<TcpUdpForwarder, Box<dyn Error>> {
            assert!(enable_tcp || enable_udp);

            let mut tcpi = None;
            let mut udpi = None;
            if enable_tcp {
                tcpi = Some(TcpForwarder::from(bind_addr,connect_addr)?);
            }
            if enable_udp {
                udpi = Some(UdpForwarder::from(bind_addr, connect_addr)?);
            }

            Ok(TcpUdpForwarder {
                tcp: Arc::from(tcpi),
                udp: Arc::from(udpi)
            })
        }

    pub fn listen(&self) {
        let mut tt = None;
        let mut tu = None;
        if self.tcp.is_some() {
            let m = self.tcp.clone();
            tt = Some(std::thread::spawn(move || {
                match m.as_ref() {
                    Some(l) => {
                        l.listen().unwrap_or_default();
                    },
                    None => {}
                }
            }));
        }
        if self.udp.is_some() {
            let m = self.udp.clone();
            tu = Some(std::thread::spawn(move || {
                match m.as_ref() {
                    Some(l) => l.listen().unwrap_or_default(),
                    None => {}
                }
            }));
        }

        if tt.is_some() {
            tt.unwrap().join().unwrap_or_default();
        }
        if tu.is_some() {
            tu.unwrap().join().unwrap_or_default();
        }
    }

#[allow(dead_code)]
    pub fn close(&self) {
        match self.udp.as_ref() {
            Some(udp) => udp.close(),
            None => {}
        }
        // TODO tcp close
    }
}

