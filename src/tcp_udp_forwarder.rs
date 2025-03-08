use crate::forward_config::ForwardSessionConfig;
use crate::tcp_forwarder::TcpForwarder;
use crate::udp_forwarder::UdpForwarder;
use std::error::Error;
use std::net::ToSocketAddrs;
use std::result::Result;
use std::sync::{Arc, atomic::AtomicBool};

#[derive(Clone)]
pub struct TcpUdpForwarder {
    udp: Arc<Option<UdpForwarder>>,
    tcpe: Arc<Option<TcpForwarder>>,
}

impl TcpUdpForwarder {
    pub fn from<T>(config: &ForwardSessionConfig<T>) -> Result<TcpUdpForwarder, Box<dyn Error>>
    where
        T: ToSocketAddrs,
    {
        assert!(config.enable_tcp || config.enable_udp);

        let mut udpi = None;
        let mut tcpei = None;
        if config.enable_tcp {
            tcpei = Some(TcpForwarder::from(&config).unwrap());
        }
        if config.enable_udp {
            udpi = Some(UdpForwarder::from(&config).unwrap());
        }

        Ok(TcpUdpForwarder {
            udp: Arc::from(udpi),
            tcpe: Arc::from(tcpei),
        })
    }

    pub fn listen(&self) -> Box<dyn FnOnce() -> ()> {
        let mut tte = None;
        let mut tu = None;
        let closed = Arc::new(AtomicBool::from(false));
        if self.tcpe.is_some() {
            let m = self.tcpe.clone();
            let tcp_closed = closed.clone();
            tte = Some(std::thread::spawn(move || match m.as_ref() {
                Some(l) => {
                    l.listen(tcp_closed).unwrap_or_default();
                }
                None => {}
            }));
        }
        if self.udp.is_some() {
            let m = self.udp.clone();
            let udp_closed = closed.clone();
            tu = Some(std::thread::spawn(move || match m.as_ref() {
                Some(l) => l.listen(udp_closed).unwrap_or_default(),
                None => {}
            }));
        }

        let close_handler = move || {
            closed.store(true, std::sync::atomic::Ordering::SeqCst);
            if tte.is_some() {
                tte.unwrap().join().unwrap_or_default();
            }
            if tu.is_some() {
                tu.unwrap().join().unwrap_or_default();
            }
        };
        return Box::new(close_handler);
    }
}
