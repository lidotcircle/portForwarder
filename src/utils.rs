use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::net::{SocketAddr, ToSocketAddrs};

pub struct ArcRel<'a> {
    v: &'a Arc<AtomicBool>
}

impl<'a> Drop for ArcRel<'a> {
    fn drop(self: &mut ArcRel<'a>) {
        self.v.store(true, Ordering::SeqCst);
    }
}

pub fn toSockAddr<T>(addr: &T) -> SocketAddr 
where T: ToSocketAddrs {
    addr.to_socket_addrs().unwrap().next().unwrap()
}
