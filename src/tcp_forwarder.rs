use std::io::prelude::*;
use std::net::{TcpStream, Shutdown, TcpListener, ToSocketAddrs, SocketAddr};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::io;
use std::time;

use crate::utils::ArcRel;
use crate::utils;

pub struct TcpForwarder {
    tcpFD: TcpListener,
    dstAddr: SocketAddr,
}

/** assume read size equals 0 means eof */
fn try_tcp_forward_one_direction(src: &mut TcpStream, dst: &mut TcpStream, buf: &mut [u8]) -> io::Result<()> {
    match src.read(buf) {
        Ok(n) => {
            if n == 0 {
                dst.shutdown(Shutdown::Write)?;
            } else {
                if dst.write(&buf[0..n])? != n {
                    let err = io::Error::new(io::ErrorKind::TimedOut, "bad write, fixme");
                    return Err(err);
                }
            }
            Ok(())
        },
        Err(err) => {
            if err.kind() == io::ErrorKind::WouldBlock {
                Ok(())
            } else {
                Err(err)
            }
        }
    }
}

fn tcp_forward(e1: &mut TcpStream, dst: SocketAddr) -> io::Result<()> {
    let mut e2 = TcpStream::connect(dst)?;
    e1.set_read_timeout(Some(time::Duration::from_millis(100)))?;
    e2.set_read_timeout(Some(time::Duration::from_millis(100)))?;

    let mut e1c = e1.try_clone()?;
    let mut e2c = e2.try_clone()?;
    let a1 = Arc::from(AtomicBool::from(false));
    let a2 = a1.clone();
    let t1 = std::thread::spawn(move || {
        let guard = ArcRel::from(&a1);
        let mut buf = vec![0; 1<<16];
        while !a1.load(Ordering::SeqCst) {
            match try_tcp_forward_one_direction(&mut e1c, &mut e2c, &mut buf) {
                Ok(_) => {},
                Err(_) => break
            }
        }
        drop(guard);
    });

    let mut e1b = e1.try_clone()?;
    let t2 = std::thread::spawn(move || {
        let guard = ArcRel::from(&a2);
        let mut buf = vec![0; 1<<16];
        while !a2.load(Ordering::SeqCst) {
            match try_tcp_forward_one_direction(&mut e2, &mut e1b, &mut buf) {
                Ok(_) => {},
                Err(_) => break
            }
        }
        drop(guard);
    });

    t1.join().unwrap_or_default();
    t2.join().unwrap_or_default();
    Ok(())
}

impl TcpForwarder {
    pub fn from<T: ToSocketAddrs>(bind_addr: T, dst_addr: T) -> io::Result<TcpForwarder> {
        let ts = TcpListener::bind(&bind_addr)?;
        ts.set_nonblocking(true)?;

        Ok(TcpForwarder {
            tcpFD: ts,
            dstAddr: utils::toSockAddr(&dst_addr)
        })
    }

    pub fn listen(self: &TcpForwarder) -> io::Result<()> {
        let tcp = self.tcpFD.try_clone()?;
        let tcp_dst = self.dstAddr.clone();

        let t_thread = std::thread::spawn(move || {
            let mut count = 0;
            let mut cons = HashMap::new();

            for inc in tcp.incoming() {
                match inc {
                    Ok(mut stream) => {
                        let arc = Arc::new(AtomicBool::new(false));
                        let carc = arc.clone();
                        let tcp_dstx = tcp_dst.clone();
                        let thr = std::thread::spawn(move || {
                            let guard = ArcRel::from(&carc);
                            let ans = tcp_forward(&mut stream, tcp_dstx);
                            drop(guard);
                            ans
                        });
                        cons.insert(count, (thr, arc));
                        count += 1;
                    },
                    Err(err) => {
                        if err.kind() == io::ErrorKind::WouldBlock {
                            std::thread::sleep(time::Duration::from_millis(100));
                        } else {
                            break;
                        }
                    }
                }

                let mut release = vec![];
                {
                    for con in cons.keys() {
                        match cons.get(con) {
                            Some((_, arc)) => {
                                if arc.load(Ordering::SeqCst) {
                                    release.push(*con);
                                }
                            },
                            None => {}
                        }
                    }
                }
                for con in release {
                    match cons.remove(&con) {
                        Some((thr, _)) => {
                            match thr.join() {
                                Ok(_) => {},
                                Err(_) => {}
                            };
                        }, 
                        None => {}
                    };
                }
            }
        });

        t_thread.join().unwrap_or_default();
        Ok(())
    }
}

impl Clone for TcpForwarder {
    fn clone(self: &TcpForwarder) -> TcpForwarder {
        let t = self.tcpFD.try_clone().unwrap();
        TcpForwarder {
            tcpFD: t,
            dstAddr: self.dstAddr.clone()
        }
    }
}

