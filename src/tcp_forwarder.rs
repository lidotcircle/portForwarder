use crate::connection_plugin::{ConnectionPlugin, RegexMultiplexer};
use crate::forward_config::ForwardSessionConfig;
use crate::utils::toSockAddr;
use log::info;
use mio::net::{TcpListener, TcpStream};
use mio::{Events, Interest, Poll, Token};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, ToSocketAddrs};
use std::rc::Rc;
use std::sync::{Arc, atomic::AtomicBool};
use std::time;

pub struct TcpForwarder {
    local_addr: SocketAddr,
    plugin: Box<dyn ConnectionPlugin + Send + Sync>,
    max_connections: Option<u64>,
    cache_size: usize,
}

fn SafeAddr(addr: &std::io::Result<SocketAddr>) -> String {
    match addr {
        Ok(addr) => addr.to_string(),
        Err(_) => "unknown".to_string(),
    }
}

fn nextToken(token: &mut Token) -> Token {
    token.0 = token.0 + 1;
    *token
}

fn set_readable(
    poll: &mut Poll,
    source: &mut TcpStream,
    token: &Token,
    stateMap: &mut HashMap<Token, Interest>,
) {
    match stateMap.get(token) {
        Some(state) => {
            poll.registry()
                .reregister(source, *token, Interest::READABLE | *state)
                .unwrap();
            stateMap.insert(*token, Interest::READABLE | *state);
        }
        None => {
            poll.registry()
                .register(source, *token, Interest::READABLE)
                .unwrap();
            stateMap.insert(*token, Interest::READABLE);
        }
    }
}
fn set_writable(
    poll: &mut Poll,
    source: &mut TcpStream,
    token: &Token,
    stateMap: &mut HashMap<Token, Interest>,
) {
    match stateMap.get(token) {
        Some(state) => {
            poll.registry()
                .reregister(source, *token, Interest::WRITABLE | *state)
                .unwrap();
            stateMap.insert(*token, Interest::WRITABLE | *state);
        }
        None => {
            poll.registry()
                .register(source, *token, Interest::WRITABLE)
                .unwrap();
            stateMap.insert(*token, Interest::WRITABLE);
        }
    }
}
fn clear_readable(
    poll: &mut Poll,
    source: &mut TcpStream,
    token: &Token,
    stateMap: &mut HashMap<Token, Interest>,
) {
    match stateMap.get(token) {
        Some(state) => {
            if *state == Interest::READABLE {
                poll.registry().deregister(source).unwrap();
                stateMap.remove(token);
            } else {
                let newstate = state.remove(Interest::READABLE).unwrap();
                poll.registry()
                    .reregister(source, *token, newstate)
                    .unwrap();
                stateMap.insert(*token, newstate);
            }
        }
        None => {}
    }
}
fn clear_writable(
    poll: &mut Poll,
    source: &mut TcpStream,
    token: &Token,
    stateMap: &mut HashMap<Token, Interest>,
) {
    match stateMap.get(token) {
        Some(state) => {
            if *state == Interest::WRITABLE {
                poll.registry().deregister(source).unwrap();
                stateMap.remove(token);
            } else {
                let newstate = state.remove(Interest::WRITABLE).unwrap();
                poll.registry()
                    .reregister(source, *token, newstate)
                    .unwrap();
                stateMap.insert(*token, newstate);
            }
        }
        None => {}
    }
}

impl TcpForwarder {
    pub fn from<T: ToSocketAddrs>(
        config: &ForwardSessionConfig<T>,
    ) -> std::io::Result<TcpForwarder> {
        Ok(Self {
            local_addr: toSockAddr(&config.local),
            plugin: Box::new(RegexMultiplexer::from((
                config.remoteMap.clone(),
                config.allow_nets.clone(),
            ))),
            max_connections: if config.max_connections >= 0 {
                Some(config.max_connections as u64)
            } else {
                None
            },
            cache_size: config.conn_bufsize,
        })
    }

    pub fn listen(self: &Self, closed: Arc<AtomicBool>) -> std::io::Result<()> {
        let mut pollIns = Poll::new().unwrap();
        let mut listener = match TcpListener::bind(self.local_addr) {
            Ok(l) => l,
            Err(e) => {
                panic!(
                    "fail to bind tcp://{}: make sure the address is not in use and you have permission to bind\n  {}",
                    self.local_addr, e
                );
            }
        };
        let listener_token = Token(0);
        pollIns
            .registry()
            .register(&mut listener, listener_token, Interest::READABLE)
            .unwrap();
        info!("listen at tcp://{}", listener.local_addr().unwrap());

        let capacity = if let Some(mx) = self.max_connections {
            std::cmp::min(mx as usize, 1024)
        } else {
            1024
        };
        let mut events = Events::with_capacity(capacity);

        let mut conn_token = Token(1);
        let mut token2stream: HashMap<Token, (Rc<RefCell<TcpStream>>, SocketAddr)> = HashMap::new();
        let mut token2connss: HashMap<Token, Rc<RefCell<TcpStream>>> = HashMap::new();
        let mut token2stat = HashMap::new();
        let mut token2buffer: HashMap<Token, (Vec<_>, usize)> = HashMap::new();
        let mut shutdownMe: HashSet<Token> = HashSet::new();
        let mut alreadyShutdown: HashSet<Token> = HashSet::new();

        let removeConn =
            |tk: Token,
             pollIns: &mut Poll,
             token2stream: &mut HashMap<Token, (Rc<RefCell<TcpStream>>, SocketAddr)>,
             token2stat: &mut _,
             token2connss: &mut HashMap<Token, Rc<RefCell<TcpStream>>>,
             token2buffer: &mut HashMap<Token, (Vec<_>, usize)>,
             shutdownMe: &mut HashSet<Token>,
             alreadyShutdown: &mut HashSet<Token>| {
                let (t1, t2) = if tk.0 % 2 == 0 {
                    (tk, Token(tk.0 + 1))
                } else {
                    (Token(tk.0 - 1), tk)
                };

                // prevent double clear
                if !token2stream.contains_key(&t1) {
                    return;
                }

                info!(
                    "close connection from {}, remaining {}",
                    token2stream.get(&t1).unwrap().1,
                    token2stream.len() - 1
                );

                clear_readable(
                    pollIns,
                    &mut token2stream.get(&t1).unwrap().0.borrow_mut(),
                    &t1,
                    token2stat,
                );
                clear_writable(
                    pollIns,
                    &mut token2stream.get(&t1).unwrap().0.borrow_mut(),
                    &t1,
                    token2stat,
                );
                if token2connss.contains_key(&t2) {
                    clear_readable(
                        pollIns,
                        &mut token2connss.get(&t2).unwrap().borrow_mut(),
                        &t2,
                        token2stat,
                    );
                    clear_writable(
                        pollIns,
                        &mut token2connss.get(&t2).unwrap().borrow_mut(),
                        &t2,
                        token2stat,
                    );
                }
                token2stream.remove(&t1).unwrap();
                token2connss.remove(&t2);
                token2buffer.remove(&t1);
                token2buffer.remove(&t2);
                shutdownMe.remove(&t1);
                shutdownMe.remove(&t2);
                alreadyShutdown.remove(&t1);
                alreadyShutdown.remove(&t2);
            };

        loop {
            if closed.load(std::sync::atomic::Ordering::SeqCst) {
                return Ok(());
            }

            pollIns
                .poll(&mut events, Some(time::Duration::from_secs(1)))
                .unwrap();
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

                                if !self.plugin.testipaddr(&addr) {
                                    info!("drop TCP connection from {}", addr);
                                    stream.shutdown(Shutdown::Both).unwrap_or(());
                                    continue;
                                }

                                let t = nextToken(&mut conn_token);
                                let nt = nextToken(&mut conn_token);
                                let singleRemote = self.plugin.onlySingleTarget();
                                if singleRemote.is_some() {
                                    let remote = singleRemote.unwrap();
                                    match TcpStream::connect(remote) {
                                        Ok(mut conn) => {
                                            pollIns
                                                .registry()
                                                .register(&mut stream, t, Interest::READABLE)
                                                .unwrap();
                                            token2stream
                                                .insert(t, (Rc::new(RefCell::new(stream)), addr));
                                            pollIns
                                                .registry()
                                                .register(&mut conn, nt, Interest::READABLE)
                                                .unwrap();
                                            token2connss.insert(nt, Rc::new(RefCell::new(conn)));
                                            token2stat.insert(t, Interest::READABLE);
                                            token2stat.insert(nt, Interest::READABLE);
                                            info!(
                                                "accept connection from {} to {}, current connections: {}",
                                                addr,
                                                remote,
                                                token2stream.len()
                                            );
                                        }
                                        Err(reason) => {
                                            info!(
                                                "close connection from {} because failed to connect remote address {}: {}",
                                                addr, remote, reason
                                            );
                                        }
                                    }
                                } else {
                                    pollIns
                                        .registry()
                                        .register(&mut stream, t, Interest::READABLE)
                                        .unwrap();
                                    token2stream.insert(t, (Rc::new(RefCell::new(stream)), addr));
                                    token2stat.insert(t, Interest::READABLE);
                                }
                            }
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

                let (sss, tk2, conn) = if tk.0 % 2 == 0 {
                    let stream = token2stream.get_mut(&tk).unwrap().0.clone();
                    let tk2 = Token(tk.0 + 1);
                    let peersock = match token2connss.get(&tk2) {
                        Some(ss) => Some(ss.clone()),
                        None => None,
                    };
                    (stream, tk2, peersock)
                } else {
                    let conn = token2connss.get(&tk).unwrap().clone();
                    let tk2 = Token(tk.0 - 1);
                    let stream = token2stream.get(&tk2).unwrap().0.clone();
                    (conn, tk2, Some(stream))
                };

                let mut peerConnOpt = conn.clone();

                if event.is_readable() {
                    let mut buf = [0; 1 << 16];
                    let mut n = 0;
                    loop {
                        let mut sss_mut = sss.borrow_mut();
                        match sss_mut.read(&mut buf[..]) {
                            Ok(s) => {
                                if n > 0 && s == 0 {
                                    break;
                                }
                                if s == 0 {
                                    if token2buffer.get(&tk2).is_none() {
                                        if peerConnOpt.is_some() {
                                            let conn_c = peerConnOpt.as_ref().unwrap().clone();
                                            let mut conn_mut = conn_c.borrow_mut();
                                            conn_mut.shutdown(Shutdown::Write).unwrap_or(());
                                            if alreadyShutdown.get(&tk).is_some() {
                                                drop(sss_mut);
                                                drop(conn_mut);
                                                removeConn(
                                                    tk,
                                                    &mut pollIns,
                                                    &mut token2stream,
                                                    &mut token2stat,
                                                    &mut token2connss,
                                                    &mut token2buffer,
                                                    &mut shutdownMe,
                                                    &mut alreadyShutdown,
                                                );
                                                break;
                                            } else {
                                                clear_writable(
                                                    &mut pollIns,
                                                    &mut conn_mut,
                                                    &tk2,
                                                    &mut token2stat,
                                                );
                                                alreadyShutdown.insert(tk2);
                                            }
                                        } else {
                                            drop(sss_mut);
                                            removeConn(
                                                tk,
                                                &mut pollIns,
                                                &mut token2stream,
                                                &mut token2stat,
                                                &mut token2connss,
                                                &mut token2buffer,
                                                &mut shutdownMe,
                                                &mut alreadyShutdown,
                                            );
                                            break;
                                        }
                                    } else {
                                        shutdownMe.insert(tk2);
                                    }
                                } else {
                                    log::debug!(
                                        "read buffer[{}] from {}",
                                        s,
                                        SafeAddr(&sss_mut.peer_addr())
                                    );
                                    let vbuf = Vec::from(&buf[0..s]);
                                    let trueconn = if peerConnOpt.is_none() {
                                        match self
                                            .plugin
                                            .decideTarget(&vbuf, sss_mut.peer_addr().unwrap())
                                        {
                                            Some(addr) => match TcpStream::connect(addr) {
                                                Ok(mut ccc) => {
                                                    pollIns
                                                        .registry()
                                                        .register(&mut ccc, tk2, Interest::READABLE)
                                                        .unwrap();
                                                    token2connss
                                                        .insert(tk2, Rc::new(RefCell::new(ccc)));
                                                    token2stat.insert(tk2, Interest::READABLE);
                                                    peerConnOpt = Some(
                                                        token2connss.get(&tk2).unwrap().clone(),
                                                    );
                                                    info!("create connection to {}", addr);
                                                    Some(token2connss.get(&tk2).unwrap().clone())
                                                }
                                                Err(reason) => {
                                                    info!(
                                                        "fail to create connection to {} '{}', so release resources",
                                                        addr, reason
                                                    );
                                                    None
                                                }
                                            },
                                            None => None,
                                        }
                                    } else {
                                        Some(peerConnOpt.as_ref().unwrap().clone())
                                    };
                                    if trueconn.is_none() {
                                        drop(sss_mut);
                                        removeConn(
                                            tk,
                                            &mut pollIns,
                                            &mut token2stream,
                                            &mut token2stat,
                                            &mut token2connss,
                                            &mut token2buffer,
                                            &mut shutdownMe,
                                            &mut alreadyShutdown,
                                        );
                                    } else {
                                        match &mut token2buffer.get_mut(&tk2) {
                                            Some(bb) => {
                                                bb.0.push(vbuf);
                                                bb.1 += s;
                                                if bb.1 >= self.cache_size {
                                                    clear_readable(
                                                        &mut pollIns,
                                                        &mut sss_mut,
                                                        &tk,
                                                        &mut token2stat,
                                                    );
                                                }
                                            }
                                            _ => {
                                                token2buffer.insert(tk2, (vec![vbuf], s));
                                                set_writable(
                                                    &mut pollIns,
                                                    &mut trueconn.as_ref().unwrap().borrow_mut(),
                                                    &tk2,
                                                    &mut token2stat,
                                                );
                                                if s >= self.cache_size {
                                                    clear_readable(
                                                        &mut pollIns,
                                                        &mut sss_mut,
                                                        &tk,
                                                        &mut token2stat,
                                                    );
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            Err(_e) => {
                                if _e.kind() == ErrorKind::WouldBlock {
                                    break;
                                }
                                info!("close connection {}", _e);
                                drop(sss_mut);
                                removeConn(
                                    tk,
                                    &mut pollIns,
                                    &mut token2stream,
                                    &mut token2stat,
                                    &mut token2connss,
                                    &mut token2buffer,
                                    &mut shutdownMe,
                                    &mut alreadyShutdown,
                                );
                                break;
                            }
                        }
                        n += 1;
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
                        let mut sss_mut = sss.borrow_mut();
                        let connx = peerConnOpt.as_ref().unwrap();
                        let mut conn_mut = connx.borrow_mut();
                        log::debug!(
                            "TRY write {} bytes from {} to {}",
                            buf.len(),
                            SafeAddr(&conn_mut.peer_addr()),
                            SafeAddr(&sss_mut.peer_addr())
                        );
                        match sss_mut.write(buf.as_slice()) {
                            Ok(s) => {
                                log::debug!(
                                    "write {} bytes from {} to {}",
                                    s,
                                    SafeAddr(&conn_mut.peer_addr()),
                                    SafeAddr(&sss_mut.peer_addr())
                                );
                                let bb = bufstat.0.remove(0);
                                nwrited += s;
                                if s < bb.len() {
                                    bufstat.0.insert(0, bb.as_slice()[s..bb.len()].to_vec());
                                    if bufstat.1 >= self.cache_size
                                        && (bufstat.1 - nwrited) < self.cache_size
                                    {
                                        set_readable(
                                            &mut pollIns,
                                            &mut conn_mut,
                                            &tk2,
                                            &mut token2stat,
                                        );
                                    }
                                    bufstat.1 -= nwrited;
                                    break;
                                }

                                if bufstat.0.is_empty() {
                                    if bufstat.1 >= self.cache_size
                                        && (bufstat.1 - nwrited) < self.cache_size
                                    {
                                        set_readable(
                                            &mut pollIns,
                                            &mut conn_mut,
                                            &tk2,
                                            &mut token2stat,
                                        );
                                    }
                                    clear_writable(
                                        &mut pollIns,
                                        &mut sss_mut,
                                        &tk,
                                        &mut token2stat,
                                    );
                                    if shutdownMe.get(&tk).is_some() {
                                        sss_mut.shutdown(Shutdown::Write).unwrap_or(());
                                        if alreadyShutdown.get(&tk2).is_some() {
                                            drop(sss_mut);
                                            drop(conn_mut);
                                            removeConn(
                                                tk,
                                                &mut pollIns,
                                                &mut token2stream,
                                                &mut token2stat,
                                                &mut token2connss,
                                                &mut token2buffer,
                                                &mut shutdownMe,
                                                &mut alreadyShutdown,
                                            );
                                        } else {
                                            clear_writable(
                                                &mut pollIns,
                                                &mut sss_mut,
                                                &tk,
                                                &mut token2stat,
                                            );
                                            alreadyShutdown.insert(tk);
                                        }
                                    } else {
                                        token2buffer.remove(&tk).unwrap();
                                    }
                                    break;
                                }
                            }
                            Err(r) => {
                                if r.kind() == std::io::ErrorKind::WouldBlock {
                                    if bufstat.1 >= self.cache_size
                                        && (bufstat.1 - nwrited) < self.cache_size
                                    {
                                        set_readable(
                                            &mut pollIns,
                                            &mut conn_mut,
                                            &tk2,
                                            &mut token2stat,
                                        );
                                    }
                                    bufstat.1 -= nwrited;
                                } else {
                                    drop(sss_mut);
                                    drop(conn_mut);
                                    removeConn(
                                        tk,
                                        &mut pollIns,
                                        &mut token2stream,
                                        &mut token2stat,
                                        &mut token2connss,
                                        &mut token2buffer,
                                        &mut shutdownMe,
                                        &mut alreadyShutdown,
                                    );
                                }
                                break;
                            }
                        }
                    }
                }
            }
        }
    }

    pub fn close(self: &Self) -> () {}
}
