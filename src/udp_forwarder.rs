extern crate mio;

use std::error::Error;
use std::net::{ToSocketAddrs, SocketAddr};
use std::collections::HashMap;
use std::io;
use std::time;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use log::info;

use crate::utils;
use crate::address_matcher::IpAddrMatcher;

use mio::{ Events, Poll, Token, Interest };
use mio::net::UdpSocket;


#[derive(Clone)]
pub struct UdpForwarder {
    bindAddr: SocketAddr,
    dstAddr: SocketAddr,
    close: Arc<AtomicBool>,
    allowed_nets: IpAddrMatcher,
}

fn next(token: &mut Token) -> Token {
    token.0 += 1;
    let ans = Token(token.0);
    ans
}

fn reset_readable(poll: &mut Poll, source: &mut UdpSocket, token: &Token) {
    poll.registry().deregister(source).unwrap();
    poll.registry().register(source, *token, Interest::READABLE).unwrap();
}
fn reset_readable_writable(poll: &mut Poll, source: &mut UdpSocket, token: &Token) {
    poll.registry().deregister(source).unwrap();
    poll.registry().register(source, *token, Interest::READABLE.add(Interest::WRITABLE)).unwrap();
}

impl UdpForwarder {
    pub fn from<T: ToSocketAddrs>(bind_addr: T, dst_addr: T, allowed: &Vec<String>) -> Result<UdpForwarder, Box<dyn Error>> {
        let baddr = utils::toSockAddr(&bind_addr);
        let daddr = utils::toSockAddr(&dst_addr);

        Ok(UdpForwarder {
            bindAddr: baddr,
            dstAddr: daddr,
            close: Arc::from(AtomicBool::from(false)),
            allowed_nets: IpAddrMatcher::from(&allowed),
        })
    }

    pub fn listen(self: &UdpForwarder) -> Result<(), Box<dyn Error>> {
        let mut poll = Poll::new()?;
        let mut events = Events::with_capacity(128);
        let t1 = Token(0);
        let mut tx = Token(t1.0);
        let mut addr2token = HashMap::new();
        let mut token2addr = HashMap::new();
        let mut token2socket = HashMap::new();
        let mut tokenWaitWrite = HashMap::new();
        let mut token2life: HashMap<Token, time::Instant> = HashMap::new();
        let mut writeBackQueue: Vec<(SocketAddr, Vec<u8>)> = Vec::new();

        let mut udpfd = UdpSocket::bind(self.bindAddr)?;
        poll.registry().
            register(&mut udpfd, t1, Interest::READABLE)?;

        let mut outConnections = vec![];
        let mut read_buf = vec![0;1<<16];
        loop {
            for k in token2life.keys() {
                if token2life.get(k).unwrap().elapsed() > time::Duration::from_secs(3 * 60) {
                    outConnections.push(*k);
                }
            }
            while !outConnections.is_empty() {
                let t = outConnections.remove(0);
                let addr = *token2addr.get(&t).unwrap();
                addr2token.remove(&addr);
                token2addr.remove(&t);
                token2socket.remove(&t).unwrap();
                tokenWaitWrite.remove(&t);
                token2life.remove(&t);
            }
            let rs = poll.poll(&mut events, 
                               Some(time::Duration::from_millis(1000)));
            if self.close.load(Ordering::SeqCst) {
                return Ok(());
            }

            if rs.is_err() {
                let err = rs.unwrap_err();
                if err.kind() == io::ErrorKind::WouldBlock {
                    continue;
                } else {
                    return Err(Box::from(err));
                }
            }

            for event in events.iter() {
                match event.token() {
                    token if token == t1 => {
                        if event.is_readable() {
                            let mut cont = true;
                            while cont {
                                match udpfd.recv_from(&mut read_buf) {
                                    Ok((size, end)) => {
                                        if !self.allowed_nets.testipaddr(&end.ip()) {
                                            info!("drop UDP package from {}", end.ip());
                                            continue;
                                        }

                                        if addr2token.get(&end).is_none() {
                                            let new_socket = UdpSocket::bind("0.0.0.0:0".parse()?)?;
                                            let t = next(&mut tx);
                                            addr2token.insert(end, t);
                                            token2addr.insert(t, end);
                                            token2socket.insert(t, new_socket);

                                            poll.registry().
                                                register(token2socket.get_mut(&t).unwrap(), t, Interest::READABLE)?;
                                        }

                                        let t = addr2token.get(&end).unwrap().clone();
                                        if tokenWaitWrite.get(&t).is_none() {
                                            tokenWaitWrite.insert(t, vec![]);
                                            reset_readable_writable(&mut poll, token2socket.get_mut(&t).unwrap(), &t);
                                        }

                                        let write_queue = tokenWaitWrite.get_mut(&t).unwrap();
                                        write_queue.push(Vec::from(&read_buf[0..size]));
                                    },
                                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                                        cont = false;
                                    },
                                    Err(err) => {
                                        return Err(Box::from(err));
                                    }
                                }
                            }
                        } else if event.is_writable() {
                            assert!(writeBackQueue.len() > 0);
                            let mut cont = true;
                            while writeBackQueue.len() > 0 && cont {
                                let (addr, buf) = writeBackQueue.remove(0);
                                match udpfd.send_to(&buf, addr) {
                                    Ok(_) => {},
                                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                                        cont = false;
                                    }
                                    Err(_) => {}
                                }
                            }

                            if writeBackQueue.is_empty() {
                                reset_readable(&mut poll,&mut udpfd,&t1);
                            }
                        }
                    },
                    token => {
                        let sock = token2socket.get_mut(&token).unwrap();

                        if !event.is_error() {
                            token2life.insert(token, time::Instant::now());
                        }

                        if event.is_readable() {
                            let mut cont = true;
                            while cont {
                                match sock.recv(&mut read_buf) {
                                    Ok(size) => {
                                        let addr = token2addr.get(&token).unwrap().clone();
                                        if writeBackQueue.is_empty() {
                                            reset_readable_writable(&mut poll, &mut udpfd, &t1);
                                        }
                                        writeBackQueue.push((addr, Vec::from(&read_buf[0..size])));
                                    },
                                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                                        cont = false;
                                    },
                                    Err(_) => {
                                        cont = false;
                                        outConnections.push(token)
                                    }
                                }
                            }
                        } else if event.is_writable() {
                            let bufs: &mut _ = tokenWaitWrite.get_mut(&token).unwrap();
                            assert!(bufs.len() > 0);

                            let mut cont = true;
                            while bufs.len() > 0 && cont {
                                let buf = bufs.remove(0);
                                match sock.send_to(&buf, self.dstAddr) {
                                    Ok(_) => {},
                                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                                        cont = false;
                                    },
                                    Err(_) => {
                                        cont = false;
                                        outConnections.push(token);
                                    }
                                }
                            }
                            if bufs.len() == 0 {
                                reset_readable(&mut poll,sock,&token);
                                tokenWaitWrite.remove(&token).unwrap();
                            }
                        } else if event.is_error() {
                            outConnections.push(token);
                        }
                    }
                }
            }
        }
    }

    pub fn close(self: &UdpForwarder) {
        self.close.store(true, Ordering::SeqCst);
    }
}

