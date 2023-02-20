use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::net::{SocketAddr, ToSocketAddrs};

pub struct ArcRel<'a> {
    v: &'a Arc<AtomicBool>
}
impl<'a> ArcRel<'a> {
    pub fn from(arc: &Arc<AtomicBool>) -> ArcRel {
        ArcRel {
            v: arc
        }
    }
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

pub fn convert_to_bytes(input: &str) -> Option<usize> {
    let units = vec!["B", "KB", "MB", "GB", "TB"];
    let mut num_str = String::new();
    let mut unit_str = String::new();

    for c in input.chars() {
        if c.is_digit(10) {
            num_str.push(c);
        } else {
            unit_str.push(c);
        }
    }

    let num = match num_str.parse::<usize>() {
        Ok(n) => n,
        Err(_) => return None,
    };

    let unit_index = units.iter().position(|&u| u == unit_str.trim().to_uppercase());

    if let Some(index) = unit_index {
        Some(num * (1024 as usize).pow(index as u32))
    } else {
        None
    }
}
