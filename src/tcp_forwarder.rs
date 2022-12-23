use std::io::prelude::*;
use std::net::{TcpStream, Shutdown, TcpListener, ToSocketAddrs, SocketAddr};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::io;
use std::io::Error;
use std::thread::JoinHandle;
use std::time;
use log::info;

use crate::utils::ArcRel;
use crate::utils;

struct ConnectionStat {
    peer_addr: SocketAddr,
    join_handler: Option<JoinHandle<Result<(),Error>>>,
    stop: Arc<AtomicBool>,
}

impl ConnectionStat {
    pub fn get_peeraddr(self: &Self) -> &SocketAddr {
        &self.peer_addr
    }
}

#[derive(Clone)]
pub struct TcpForwarder {
    tcpFD: Arc<Mutex<TcpListener>>,
    dstAddr: SocketAddr,
    connections: Arc<Mutex<HashMap<i32,ConnectionStat>>>,
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

fn tcp_forward(e1: &mut TcpStream, dst: SocketAddr, connections: Arc<Mutex<HashMap<i32,ConnectionStat>>>, cid: i32) -> io::Result<()> {
    let mut e2 = TcpStream::connect(dst)?;
    e1.set_read_timeout(Some(time::Duration::from_millis(100)))?;
    e2.set_read_timeout(Some(time::Duration::from_millis(100)))?;

    let mut e1c = e1.try_clone()?;
    let mut e2c = e2.try_clone()?;

    let a1 = {
        let locked_connections = connections.lock().unwrap();
        let stat = &locked_connections[&cid];
        info!("forward connection from {} to {}", stat.get_peeraddr(), dst);
        stat.stop.clone()
    };
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

    let mut locked_connections = connections.lock().unwrap();
    {
        let stat = &locked_connections[&cid];
        info!("close connection from {} to {}", stat.get_peeraddr(), dst);
    }
    locked_connections.remove(&cid);

    Ok(())
}

impl TcpForwarder {
    pub fn from<T: ToSocketAddrs>(bind_addr: T, dst_addr: T) -> io::Result<TcpForwarder> {
        let ts = TcpListener::bind(&bind_addr)?;
        ts.set_nonblocking(true)?;

        Ok(Self {
            tcpFD: Arc::new(Mutex::new(ts)),
            dstAddr: utils::toSockAddr(&dst_addr),
            connections: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub fn listen(self: &Self) -> io::Result<()> {
        let mut count = 0;
        let cons = self.connections.clone();

        let fd = self.tcpFD.lock().unwrap();
        info!("listen at tcp://{}", fd.local_addr().unwrap());
        for inc in fd.incoming() {
            match inc {
                Ok(mut stream) => {
                    let tcp_dst = self.dstAddr.clone();
                    let c2 = cons.clone();
                    let peer_addr = stream.peer_addr().unwrap();
                    let thread_handle = std::thread::spawn(move || tcp_forward(&mut stream, tcp_dst, c2, count));
                    let mut cc = cons.lock().unwrap();
                    cc.insert(count, ConnectionStat{ peer_addr, join_handler: Some(thread_handle), stop: Arc::from(AtomicBool::from(false)) });
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
        }

        Ok(())
    }

    pub fn close(self: &Self) -> () {
        let mut handlers = vec![];
        let cons = self.connections.clone();
        {
            let mut connections = cons.lock().unwrap();
            for conn in connections.values_mut() {
                conn.stop.store(true, Ordering::SeqCst);
                handlers.push(conn.join_handler.take());
            }
        }

        for _handler in handlers {
            let handler = _handler.unwrap();
            match handler.join() {
                Ok(_) => (),
                Err(_) => (),
            }
        }
    }
}
