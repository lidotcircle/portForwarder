use std::net::{Shutdown, ToSocketAddrs, SocketAddr};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::time;
use log::info;
use mio::{Interest,Token,Poll,Events};
use mio::net::{TcpListener, TcpStream};
use crate::address_matcher::IpAddrMatcher;
use crate::forward_config::ForwardSessionConfig;
use crate::utils::toSockAddr;

pub struct TcpForwarderEPoll {
    local_addr: SocketAddr,
    remote_addr: SocketAddr,
    allowed_nets: IpAddrMatcher,
    max_connections: Option<u64>,
    cache_size: usize,
}

fn nextToken(token: &mut Token) -> Token {
    token.0 = token.0 + 1;
    *token
}

fn set_readable(poll: &mut Poll, source: &mut TcpStream, token: &Token, stateMap: &mut HashMap<Token, Interest>) {
    match stateMap.get(token) {
        Some(state) => {
            poll.registry().reregister(source, *token, Interest::READABLE | *state).unwrap();
            stateMap.insert(*token, Interest::READABLE | *state);
        },
        None => {
            poll.registry().register(source, *token, Interest::READABLE).unwrap();
            stateMap.insert(*token, Interest::READABLE);
        }
    }
}
fn set_writable(poll: &mut Poll, source: &mut TcpStream, token: &Token, stateMap: &mut HashMap<Token, Interest>) {
    match stateMap.get(token) {
        Some(state) => {
            poll.registry().reregister(source, *token, Interest::WRITABLE | *state).unwrap();
            stateMap.insert(*token, Interest::WRITABLE | *state);
        },
        None => {
            poll.registry().register(source, *token, Interest::WRITABLE).unwrap();
            stateMap.insert(*token, Interest::WRITABLE);
        }
    }
}
fn clear_readable(poll: &mut Poll, source: &mut TcpStream, token: &Token, stateMap: &mut HashMap<Token, Interest>) {
    match stateMap.get(token) {
        Some(state) => {
            if *state == Interest::READABLE {
                poll.registry().deregister(source).unwrap();
                stateMap.remove(token);
            } else {
                let newstate = state.remove(Interest::READABLE).unwrap();
                poll.registry().reregister(source, *token, newstate).unwrap();
                stateMap.insert(*token, newstate);
            }
        },
        None => {}
    }
}
fn clear_writable(poll: &mut Poll, source: &mut TcpStream, token: &Token, stateMap: &mut HashMap<Token, Interest>) {
    match stateMap.get(token) {
        Some(state) => {
            if *state == Interest::WRITABLE {
                poll.registry().deregister(source).unwrap();
                stateMap.remove(token);
            } else {
                let newstate = state.remove(Interest::WRITABLE).unwrap();
                poll.registry().reregister(source, *token, newstate).unwrap();
                stateMap.insert(*token, newstate);
            }
        },
        None => {}
    }
}

impl TcpForwarderEPoll {
    pub fn from<T: ToSocketAddrs>(config: &ForwardSessionConfig<T>) -> std::io::Result<TcpForwarderEPoll> {
        Ok(Self {
            local_addr: toSockAddr(&config.local),
            remote_addr: toSockAddr(&config.remote),
            allowed_nets: IpAddrMatcher::from(&config.allow_nets),
            max_connections: if config.max_connections >= 0 { Some(config.max_connections as u64) } else { None },
            cache_size: config.conn_bufsize,
        })
    }

    pub fn listen(self: &Self) -> std::io::Result<()> {
        let mut pollIns = Poll::new()?;
        let mut listener = TcpListener::bind(self.local_addr)?;
        let listener_token = Token(0);
        pollIns.registry().register(&mut listener, listener_token, Interest::READABLE)?;
        info!("listen at tcp://{}", listener.local_addr().unwrap());

        let capacity = if let Some(mx) = self.max_connections {
            std::cmp::min(mx as usize, 1024)
        } else {
            1024
        };
        let mut events = Events::with_capacity(capacity);

        let mut conn_token = Token(1);
        let mut token2stream: HashMap<Token, (TcpStream, SocketAddr)> = HashMap::new();
        let mut token2connss: HashMap<Token, TcpStream> = HashMap::new();
        let mut token2stat   = HashMap::new();
        let mut token2buffer: HashMap<Token, (Vec<_>, usize)> = HashMap::new();
        let mut shutdownMe: HashSet<Token> = HashSet::new();
        let mut alreadyShutdown: HashSet<Token> = HashSet::new();

        let removeConn = |tk: Token, pollIns: &mut Poll, token2stream: &mut HashMap<Token, (TcpStream, SocketAddr)>, 
                              token2stat: &mut _, token2connss: &mut HashMap<Token, TcpStream>, 
                              token2buffer: &mut HashMap<Token, (Vec<_>, usize)>, 
                              shutdownMe: &mut HashSet<Token>, alreadyShutdown: &mut HashSet<Token>|
        {
            let (t1, t2)  = if tk.0 % 2 == 0 {
                (tk, Token(tk.0 + 1))
            } else {
                (Token(tk.0 - 1), tk)
            };

            info!("close connetion from {}, remaining {}", token2stream.get(&t1).unwrap().1, token2stream.len() -1);

            clear_readable(pollIns, &mut token2stream.get_mut(&t1).unwrap().0, &t1, token2stat);
            clear_writable(pollIns, &mut token2stream.get_mut(&t1).unwrap().0, &t1, token2stat);
            clear_readable(pollIns, token2connss.get_mut(&t2).unwrap(), &t2, token2stat);
            clear_writable(pollIns, token2connss.get_mut(&t2).unwrap(), &t2, token2stat);
            token2stream.remove(&t1).unwrap();
            token2connss.remove(&t2).unwrap();
            token2buffer.remove(&t1);
            token2buffer.remove(&t2);
            shutdownMe.remove(&t1);
            shutdownMe.remove(&t2);
            alreadyShutdown.remove(&t1);
            alreadyShutdown.remove(&t2);
        };

        loop {
            pollIns.poll(&mut events, Some(time::Duration::from_secs(1))).unwrap();
            for event in &events {
                let tk = event.token();
                if tk == listener_token {
                    if !event.is_readable() {
                        continue;
                    }

                    loop {
                        match listener.accept() {
                            Ok((mut stream, addr)) => {
                                if let Some(mx) = self.max_connections {
                                    if token2stream.len() as u64 >= mx {
                                        info!("drop TCP connection from {} for quota", addr);
                                        break;
                                    }
                                }

                                if !self.allowed_nets.testipaddr(&addr.ip()) {
                                    info!("drop TCP connection from {}", addr);
                                    stream.shutdown(Shutdown::Both).unwrap_or(());
                                    continue;
                                }

                                let t = nextToken(&mut conn_token);
                                let nt = nextToken(&mut conn_token);
                                match TcpStream::connect(self.remote_addr) {
                                    Ok(mut conn) => {
                                        pollIns.registry().register(&mut stream, t, Interest::READABLE).unwrap();
                                        token2stream.insert(t, (stream, addr));
                                        pollIns.registry().register(&mut conn, nt, Interest::READABLE).unwrap();
                                        token2connss.insert(nt, conn);
                                        token2stat.insert(t,  Interest::READABLE);
                                        token2stat.insert(nt, Interest::READABLE);
                                        info!("accept connection from {} to {}, current connections: {}", addr, self.remote_addr, token2stream.len());
                                    },
                                    Err(reason) => {
                                        info!("close connection from {} because failed to connect remote address {}: {}", addr, self.remote_addr, reason);
                                    }
                                }
                            },
                            Err(reason) => {
                                if reason.kind() == std::io::ErrorKind::WouldBlock {
                                    break;
                                } else {
                                    return Err(reason);
                                }
                            }
                        }
                    }
                }

                if token2stream.get(&tk).is_none() && token2connss.get(&tk).is_none() {
                    continue;
                }

                let (sss, tk2, conn)  = if tk.0 % 2 == 0 {
                    let stream = &mut token2stream.get_mut(&tk).unwrap().0;
                    let tk2 = Token(tk.0 + 1);
                    (stream, tk2, token2connss.get_mut(&tk2).unwrap())
                } else {
                    let conn = token2connss.get_mut(&tk).unwrap();
                    let tk2 = Token(tk.0 - 1);
                    let stream = &mut token2stream.get_mut(&tk2).unwrap().0;
                    (conn, tk2, stream)
                };

                if event.is_readable() {
                    let mut buf = [0; 1<<16];
                    match sss.read(&mut buf[..]) {
                        Ok(s) => {
                            if s == 0 {
                                if token2buffer.get(&tk2).is_none() {
                                    conn.shutdown(Shutdown::Write).unwrap_or(());
                                    if alreadyShutdown.get(&tk).is_some() {
                                        removeConn(tk,  &mut pollIns, &mut token2stream, &mut token2stat, &mut token2connss, &mut token2buffer, 
                                                   &mut shutdownMe, &mut alreadyShutdown);
                                        continue;
                                    } else {
                                        clear_writable(&mut pollIns, conn, &tk2, &mut token2stat);
                                        alreadyShutdown.insert(tk2);
                                    }
                                } else {
                                    shutdownMe.insert(tk2);
                                }
                            } else {
                                let vbuf = Vec::from(&buf[0..s]);
                                match &mut token2buffer.get_mut(&tk2) {
                                    Some(bb) => {
                                        bb.0.push(vbuf);
                                        bb.1 += s;
                                        if bb.1 >= self.cache_size {
                                            clear_readable(&mut pollIns, sss, &tk, &mut token2stat);
                                        }
                                    }
                                    _ => {
                                        token2buffer.insert(tk2, (vec![vbuf], s));
                                        set_writable(&mut pollIns, conn, &tk2, &mut token2stat);
                                        if s >= self.cache_size {
                                            clear_readable(&mut pollIns, sss, &tk, &mut token2stat);
                                        }
                                    }
                                }
                            }
                        },
                        Err(_e) => {
                            info!("close connection {}", _e);
                            removeConn(tk,  &mut pollIns, &mut token2stream, &mut token2stat, &mut token2connss, &mut token2buffer, 
                                       &mut shutdownMe, &mut alreadyShutdown);
                            continue;
                        }
                    }
                }

                if event.is_writable() {
                    if token2buffer.get(&tk).is_none() {
                        continue;
                    }

                    let mut nwrited = 0;
                    let bufstat = &mut token2buffer.get_mut(&tk).unwrap();
                    while !bufstat.0.is_empty() {
                        let buf = bufstat.0.first().unwrap();
                        match sss.write(buf.as_slice()) {
                            Ok(s) => {
                                let bb = bufstat.0.remove(0);
                                nwrited += s;
                                if s < bb.len() {
                                    bufstat.0.insert(0, bb.as_slice()[s..bb.len()].to_vec());
                                    if bufstat.1 >= self.cache_size && (bufstat.1 - nwrited) < self.cache_size {
                                        set_readable(&mut pollIns, conn, &tk2, &mut token2stat);
                                    }
                                    bufstat.1 -= nwrited;
                                    break;
                                }

                                if bufstat.0.is_empty() {
                                    if bufstat.1 >= self.cache_size && (bufstat.1 - nwrited) < self.cache_size {
                                        set_readable(&mut pollIns, conn, &tk2, &mut token2stat);
                                    }
                                    clear_writable(&mut pollIns, sss, &tk, &mut token2stat);
                                    if shutdownMe.get(&tk).is_some() {
                                        sss.shutdown(Shutdown::Write).unwrap_or(());
                                        if alreadyShutdown.get(&tk2).is_some() {
                                            removeConn(tk,  &mut pollIns, &mut token2stream, &mut token2stat, &mut token2connss, &mut token2buffer, 
                                                       &mut shutdownMe, &mut alreadyShutdown);
                                        } else {
                                            clear_writable(&mut pollIns, sss, &tk, &mut token2stat);
                                            alreadyShutdown.insert(tk);
                                        }
                                    } else {
                                        token2buffer.remove(&tk).unwrap();
                                    }
                                    break;
                                }
                            },
                            Err(r) => {
                                if r.kind() == std::io::ErrorKind::WouldBlock {
                                    if bufstat.1 >= self.cache_size && (bufstat.1 - nwrited) < self.cache_size {
                                        set_readable(&mut pollIns, conn, &tk2, &mut token2stat);
                                    }
                                    bufstat.1 -= nwrited;
                                } else {
                                    removeConn(tk,  &mut pollIns, &mut token2stream, &mut token2stat, &mut token2connss, &mut token2buffer, 
                                               &mut shutdownMe, &mut alreadyShutdown);
                                }
                                break;
                            }
                        }
                    }
                }
            }
        }
    }

    pub fn close(self: &Self) -> () {
    }
}
