extern crate mio;

use log::info;
use std::cmp;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::error::Error;
use std::io;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time;

use crate::connection_plugin::{ConnectionPlugin, RegexMultiplexer};
use crate::forward_config::ForwardSessionConfig;
use crate::utils;

use mio::net::UdpSocket;
use mio::{Events, Interest, Poll, Token};

pub struct UdpForwarder {
    bindAddr: SocketAddr,
    plugin: Box<dyn ConnectionPlugin + Send + Sync>,
    max_connections: Option<u64>,
}

fn next(token: &mut Token) -> Token {
    token.0 += 1;
    let ans = Token(token.0);
    ans
}

fn reset_readable(poll: &mut Poll, source: &mut UdpSocket, token: &Token) {
    poll.registry().deregister(source).unwrap();
    poll.registry()
        .register(source, *token, Interest::READABLE)
        .unwrap();
}
fn reset_readable_writable(poll: &mut Poll, source: &mut UdpSocket, token: &Token) {
    poll.registry().deregister(source).unwrap();
    poll.registry()
        .register(source, *token, Interest::READABLE.add(Interest::WRITABLE))
        .unwrap();
}

impl UdpForwarder {
    pub fn from<T: ToSocketAddrs>(
        config: &ForwardSessionConfig<T>,
    ) -> Result<UdpForwarder, Box<dyn Error>> {
        let baddr = utils::toSockAddr(&config.local);

        Ok(UdpForwarder {
            bindAddr: baddr,
            plugin: Box::new(RegexMultiplexer::from((
                config.remoteMap.clone(),
                config.allow_nets.clone(),
            ))),
            max_connections: if config.max_connections >= 0 {
                Some(config.max_connections as u64)
            } else {
                None
            },
        })
    }

    pub fn listen(self: &UdpForwarder, closed: Arc<AtomicBool>) -> Result<(), Box<dyn Error>> {
        let mut poll = Poll::new()?;
        let capacity = if let Some(mx) = self.max_connections {
            std::cmp::min(mx as usize, 1024)
        } else {
            1024
        };
        let mut events = Events::with_capacity(capacity);
        let t1 = Token(0);
        let mut tx = Token(t1.0);
        let mut addr2token: HashMap<SocketAddr, Token> = HashMap::new();
        let mut token2addr: HashMap<Token, SocketAddr> = HashMap::new();
        let mut token2socket: HashMap<Token, UdpSocket> = HashMap::new();
        let mut tokenWaitWrite: HashMap<Token, Vec<Vec<u8>>> = HashMap::new();
        let mut life2token: BTreeMap<u128, Token> = BTreeMap::new();
        let mut token2life: HashMap<Token, u128> = HashMap::new();
        let mut token2dst: HashMap<Token, SocketAddr> = HashMap::new();
        let mut writeBackQueue: Vec<(SocketAddr, Vec<u8>)> = Vec::new();

        let mut udpfd = UdpSocket::bind(self.bindAddr)?;
        poll.registry()
            .register(&mut udpfd, t1, Interest::READABLE)?;
        log::info!("listen incomming udp://{}", udpfd.local_addr()?);

        let mut read_buf = vec![0; 1 << 16];
        let mut waiting_to_close: Vec<Token> = vec![];
        let lifespan_us = 3 * 60 * 1000 * 1000;
        loop {
            // now as KEY of BTreeMap should be unique for every connections
            let mut now = time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_micros();
            let lastkv = life2token.iter().next();
            if lastkv.is_some() {
                now = cmp::max(now, *lastkv.unwrap().0);
            }
            for outdate in life2token.range(0..now - lifespan_us) {
                waiting_to_close.push(outdate.1.clone());
            }
            for t in &waiting_to_close {
                let k = token2life.remove(&t).unwrap();
                life2token.remove(&k).unwrap();
                let addr = *token2addr.get(&t).unwrap();
                addr2token.remove(&addr);
                token2addr.remove(&t);
                token2socket.remove(&t).unwrap();
                tokenWaitWrite.remove(&t);
                token2dst.remove(&t);
            }
            waiting_to_close.clear();

            let rs = poll.poll(&mut events, Some(time::Duration::from_millis(1000)));
            if closed.load(Ordering::SeqCst) {
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
                                        if !self.plugin.testipaddr(&end) {
                                            info!("drop UDP package from {}", end.ip());
                                            continue;
                                        }

                                        if addr2token.get(&end).is_none() {
                                            if let Some(mx) = self.max_connections {
                                                if addr2token.len() as u64 >= mx {
                                                    info!(
                                                        "drop UDP package from {} because quota is meeted",
                                                        end.ip()
                                                    );
                                                    continue;
                                                }
                                            }
                                            info!("create session, new message from {}", end);

                                            let new_socket = UdpSocket::bind("0.0.0.0:0".parse()?)?;
                                            let t = next(&mut tx);
                                            addr2token.insert(end, t);
                                            token2addr.insert(t, end);
                                            token2socket.insert(t, new_socket);
                                            token2life.insert(t, now);
                                            life2token.insert(now, t);
                                            now += 1;

                                            poll.registry().register(
                                                token2socket.get_mut(&t).unwrap(),
                                                t,
                                                Interest::READABLE,
                                            )?;
                                        }

                                        let t = addr2token.get(&end).unwrap().clone();
                                        if tokenWaitWrite.get(&t).is_none() {
                                            tokenWaitWrite.insert(t, vec![]);
                                            reset_readable_writable(
                                                &mut poll,
                                                token2socket.get_mut(&t).unwrap(),
                                                &t,
                                            );
                                        }

                                        let write_queue = tokenWaitWrite.get_mut(&t).unwrap();
                                        write_queue.push(Vec::from(&read_buf[0..size]));
                                    }
                                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                                        cont = false;
                                    }
                                    Err(err) => {
                                        return Err(Box::from(err));
                                    }
                                }
                            }
                        }
                        if event.is_writable() {
                            assert!(writeBackQueue.len() > 0);
                            let mut cont = true;
                            while writeBackQueue.len() > 0 && cont {
                                let (addr, buf) = writeBackQueue.remove(0);
                                match udpfd.send_to(&buf, addr) {
                                    Ok(_) => {}
                                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                                        cont = false;
                                    }
                                    Err(_) => {}
                                }
                            }

                            if writeBackQueue.is_empty() {
                                reset_readable(&mut poll, &mut udpfd, &t1);
                            }
                        }
                    }
                    token => {
                        let sock = token2socket.get_mut(&token).unwrap();

                        if event.is_readable() {
                            let mut cont = true;
                            while cont {
                                match sock.recv(&mut read_buf) {
                                    Ok(size) if size > 0 => {
                                        let addr = token2addr.get(&token).unwrap().clone();
                                        if writeBackQueue.is_empty() {
                                            reset_readable_writable(&mut poll, &mut udpfd, &t1);
                                        }
                                        writeBackQueue.push((addr, Vec::from(&read_buf[0..size])));

                                        let oldLife = token2life.get(&token).unwrap();
                                        life2token.remove(oldLife);
                                        token2life.insert(token, now);
                                        life2token.insert(now, token);
                                        now += 1;
                                    }
                                    Ok(_) => {}
                                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                                        cont = false;
                                    }
                                    Err(_) => {
                                        cont = false;
                                        waiting_to_close.push(token)
                                    }
                                }
                            }
                        }
                        if event.is_writable() {
                            let bufs: &mut _ = tokenWaitWrite.get_mut(&token).unwrap();
                            assert!(bufs.len() > 0);

                            let mut cont = true;
                            let mut nwritten = 0;
                            while bufs.len() > 0 && cont {
                                let buf = bufs.remove(0);
                                let dst = if token2dst.contains_key(&token) {
                                    Some(*token2dst.get(&token).unwrap())
                                } else {
                                    self.plugin
                                        .decideTarget(&buf, *token2addr.get(&token).unwrap())
                                };
                                match dst {
                                    Some(remote) => match sock.send_to(&buf, remote) {
                                        Ok(s) => {
                                            nwritten += s;
                                        }
                                        Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                                            cont = false;
                                        }
                                        Err(_) => {
                                            cont = false;
                                            waiting_to_close.push(token);
                                        }
                                    },
                                    None => {
                                        waiting_to_close.push(token);
                                        break;
                                    }
                                }
                            }
                            if nwritten > 0 {
                                let oldLife = token2life.get(&token).unwrap();
                                life2token.remove(oldLife);
                                token2life.insert(token, now);
                                life2token.insert(now, token);
                                now += 1;
                            }
                            if bufs.len() == 0 {
                                reset_readable(&mut poll, sock, &token);
                                tokenWaitWrite.remove(&token).unwrap();
                            }
                        }
                        if event.is_error() {
                            waiting_to_close.push(token);
                        }
                    }
                }
            }
        }
    }
}
