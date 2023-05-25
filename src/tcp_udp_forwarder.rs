use crate::udp_forwarder::UdpForwarder;
use crate::forward_config::ForwardSessionConfig;
use crate::tcp_forwarder::TcpForwarder;
use std::net::ToSocketAddrs;
use std::error::Error;
use std::result::Result;
use std::sync::Arc;

#[derive(Clone)]
pub struct TcpUdpForwarder {
    udp: Arc<Option<UdpForwarder>>,
    tcpe: Arc<Option<TcpForwarder>>,
}

impl TcpUdpForwarder {
    pub fn from<T>(config: &ForwardSessionConfig<T>)
        -> Result<TcpUdpForwarder, Box<dyn Error>> where T: ToSocketAddrs
    {
        assert!(config.enable_tcp || config.enable_udp);

        let mut udpi = None;
        let mut tcpei = None;
        if config.enable_tcp {
            tcpei = Some(TcpForwarder::from(&config)?);
        }
        if config.enable_udp {
            udpi = Some(UdpForwarder::from(&config)?);
        }

        Ok(TcpUdpForwarder {
            udp: Arc::from(udpi),
            tcpe: Arc::from(tcpei),
        })
    }

    pub fn listen(&self) {
        let mut tte = None;
        let mut tu = None;
        if self.tcpe.is_some() {
            let m = self.tcpe.clone();
            tte = Some(std::thread::spawn(move || {
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

        if tte.is_some() {
            tte.unwrap().join().unwrap_or_default();
        }
        if tu.is_some() {
            tu.unwrap().join().unwrap_or_default();
        }
    }

#[allow(dead_code)]
    pub fn close(&self) {
        match self.tcpe.as_ref() {
            Some(tcp) => tcp.close(),
            None => {}
        }

        match self.udp.as_ref() {
            Some(udp) => udp.close(),
            None => {}
        }
    }
}
