use crate::address_matcher::IpAddrMatcher;
use crate::connection_plugin::{ConnectionPlugin, RegexMultiplexer};
use crate::forward_config::{ForwardSessionConfig, TcpMode};
use crate::utils::toSockAddr;
use log::info;
use mio::net::{TcpListener, TcpStream};
use mio::{Events, Interest, Poll, Token};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, ErrorKind, Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, ToSocketAddrs};
use std::rc::Rc;
use std::sync::{Arc, atomic::AtomicBool};
use std::time;

enum TcpForwarderMode {
    Forward(Box<dyn ConnectionPlugin + Send + Sync>),
    Socks5Server(IpAddrMatcher),
}

pub struct TcpForwarder {
    local_addr: SocketAddr,
    mode: TcpForwarderMode,
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

#[cfg(test)]
fn socks5_reply(stream: &mut std::net::TcpStream, rep: u8, bound: SocketAddr) -> io::Result<()> {
    let mut response = Vec::with_capacity(22);
    response.push(0x05);
    response.push(rep);
    response.push(0x00);
    match bound {
        SocketAddr::V4(v4) => {
            response.push(0x01);
            response.extend_from_slice(&v4.ip().octets());
            response.extend_from_slice(&v4.port().to_be_bytes());
        }
        SocketAddr::V6(v6) => {
            response.push(0x04);
            response.extend_from_slice(&v6.ip().octets());
            response.extend_from_slice(&v6.port().to_be_bytes());
        }
    }
    stream.write_all(&response)
}

#[cfg(test)]
fn parse_socks5_target(stream: &mut std::net::TcpStream) -> io::Result<SocketAddr> {
    let mut header = [0u8; 4];
    stream.read_exact(&mut header)?;
    if header[0] != 0x05 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid socks5 version",
        ));
    }
    if header[1] != 0x01 {
        let _ = socks5_reply(stream, 0x07, SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)));
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "unsupported socks5 command",
        ));
    }
    if header[2] != 0x00 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid socks5 reserved byte",
        ));
    }

    let target = match header[3] {
        0x01 => {
            let mut body = [0u8; 6];
            stream.read_exact(&mut body)?;
            let ip = Ipv4Addr::new(body[0], body[1], body[2], body[3]);
            let port = u16::from_be_bytes([body[4], body[5]]);
            SocketAddr::from((ip, port))
        }
        0x03 => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len)?;
            let mut host = vec![0u8; len[0] as usize];
            stream.read_exact(&mut host)?;
            let mut port = [0u8; 2];
            stream.read_exact(&mut port)?;
            let port = u16::from_be_bytes(port);
            let host = String::from_utf8_lossy(&host).to_string();
            match (host.as_str(), port).to_socket_addrs()?.next() {
                Some(addr) => addr,
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "target host resolve failed",
                    ));
                }
            }
        }
        0x04 => {
            let mut body = [0u8; 18];
            stream.read_exact(&mut body)?;
            let mut ip = [0u8; 16];
            ip.copy_from_slice(&body[0..16]);
            let port = u16::from_be_bytes([body[16], body[17]]);
            SocketAddr::from((Ipv6Addr::from(ip), port))
        }
        _ => {
            let _ = socks5_reply(stream, 0x08, SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)));
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "unsupported socks5 address type",
            ));
        }
    };
    Ok(target)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Socks5SessionState {
    DetectProtocol,
    Greeting,
    Request,
    Connecting,
    Relay,
    Closing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProxyProtocol {
    Unknown,
    Socks5,
    HttpTunnel,
    HttpForward,
}

fn proxy_protocol_tag(protocol: ProxyProtocol) -> &'static str {
    match protocol {
        ProxyProtocol::Socks5 => "SOCKS5",
        ProxyProtocol::HttpTunnel | ProxyProtocol::HttpForward => "HTTP",
        ProxyProtocol::Unknown => "PROXY",
    }
}

struct Socks5Session {
    client: TcpStream,
    remote: Option<TcpStream>,
    client_addr: SocketAddr,
    target: Option<SocketAddr>,
    target_label: Option<String>,
    protocol: ProxyProtocol,
    remote_token: Option<Token>,
    state: Socks5SessionState,
    close_reason: Option<String>,
    close_after_flush: bool,
    client_in: Vec<u8>,
    c2r_queue: VecDeque<(Vec<u8>, usize)>,
    r2c_queue: VecDeque<(Vec<u8>, usize)>,
    up_bytes: u64,
    down_bytes: u64,
    client_eof: bool,
    remote_eof: bool,
    client_write_shutdown: bool,
    remote_write_shutdown: bool,
}

impl Socks5Session {
    fn new(client: TcpStream, client_addr: SocketAddr) -> Self {
        Self {
            client,
            remote: None,
            client_addr,
            target: None,
            target_label: None,
            protocol: ProxyProtocol::Unknown,
            remote_token: None,
            state: Socks5SessionState::DetectProtocol,
            close_reason: None,
            close_after_flush: false,
            client_in: vec![],
            c2r_queue: VecDeque::new(),
            r2c_queue: VecDeque::new(),
            up_bytes: 0,
            down_bytes: 0,
            client_eof: false,
            remote_eof: false,
            client_write_shutdown: false,
            remote_write_shutdown: false,
        }
    }
}

impl TcpForwarder {
    pub fn from<T: ToSocketAddrs>(
        config: &ForwardSessionConfig<T>,
    ) -> std::io::Result<TcpForwarder> {
        let mode = match config.tcp_mode {
            TcpMode::Forward => TcpForwarderMode::Forward(Box::new(RegexMultiplexer::from((
                config.remoteMap.clone(),
                config.allow_nets.clone(),
            )))),
            TcpMode::Socks5Server => {
                TcpForwarderMode::Socks5Server(IpAddrMatcher::from(&config.allow_nets))
            }
        };
        Ok(Self {
            local_addr: toSockAddr(&config.local),
            mode,
            max_connections: if config.max_connections >= 0 {
                Some(config.max_connections as u64)
            } else {
                None
            },
            cache_size: config.conn_bufsize,
        })
    }

    pub fn listen(self: &Self, closed: Arc<AtomicBool>) -> std::io::Result<()> {
        if let TcpForwarderMode::Socks5Server(ip_matcher) = &self.mode {
            return self.listen_socks5(closed, ip_matcher.clone());
        }

        let plugin = match &self.mode {
            TcpForwarderMode::Forward(plugin) => plugin,
            TcpForwarderMode::Socks5Server(_) => unreachable!(),
        };

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

        let mut inComingPeerRecieveBytes: u64 = 0;
        let mut inComingPeerSendBytes: u64 = 0;
        let mut outGoingPeerRecieveBytes: u64 = 0;
        let mut outGoingPeerSendBytes: u64 = 0;

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
                log::debug!(
                    "tcp forwarder closed: inComingPeerRecieveBytes = {inComingPeerRecieveBytes}, inComingPeerSendBytes = {inComingPeerSendBytes}, outGoingPeerRecieveBytes = {outGoingPeerRecieveBytes}, outGoingPeerSendBytes = {outGoingPeerSendBytes}"
                );
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

                                if !plugin.testipaddr(&addr) {
                                    info!("drop TCP connection from {}", addr);
                                    stream.shutdown(Shutdown::Both).unwrap_or(());
                                    continue;
                                }

                                let t = nextToken(&mut conn_token);
                                let nt = nextToken(&mut conn_token);
                                let singleRemote = plugin.onlySingleTarget();
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
                                    if tk.0 % 2 == 0 {
                                        inComingPeerRecieveBytes += s as u64;
                                    } else {
                                        outGoingPeerRecieveBytes += s as u64;
                                    }
                                    let vbuf = Vec::from(&buf[0..s]);
                                    let trueconn = if peerConnOpt.is_none() {
                                        match plugin
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
                                if tk.0 % 2 == 0 {
                                    inComingPeerSendBytes += s as u64;
                                } else {
                                    outGoingPeerSendBytes += s as u64;
                                }
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

    fn listen_socks5(
        &self,
        closed: Arc<AtomicBool>,
        ip_matcher: IpAddrMatcher,
    ) -> std::io::Result<()> {
        fn encode_socks5_reply(rep: u8, bound: SocketAddr) -> Vec<u8> {
            let mut response = Vec::with_capacity(22);
            response.push(0x05);
            response.push(rep);
            response.push(0x00);
            match bound {
                SocketAddr::V4(v4) => {
                    response.push(0x01);
                    response.extend_from_slice(&v4.ip().octets());
                    response.extend_from_slice(&v4.port().to_be_bytes());
                }
                SocketAddr::V6(v6) => {
                    response.push(0x04);
                    response.extend_from_slice(&v6.ip().octets());
                    response.extend_from_slice(&v6.port().to_be_bytes());
                }
            }
            response
        }

        fn flush_queue(
            stream: &mut TcpStream,
            queue: &mut VecDeque<(Vec<u8>, usize)>,
        ) -> io::Result<()> {
            while let Some((buf, off)) = queue.front_mut() {
                match stream.write(&buf[*off..]) {
                    Ok(0) => {
                        return Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "zero write while flushing",
                        ));
                    }
                    Ok(n) => {
                        *off += n;
                        if *off >= buf.len() {
                            queue.pop_front();
                        }
                    }
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                    Err(err) => return Err(err),
                }
            }
            Ok(())
        }

        fn set_client_interest(
            poll: &mut Poll,
            tk: Token,
            sess: &mut Socks5Session,
        ) -> io::Result<()> {
            let has_write = !sess.r2c_queue.is_empty();
            let has_read = !sess.client_eof;
            let interest = if has_read && has_write {
                Interest::READABLE | Interest::WRITABLE
            } else if has_write {
                Interest::WRITABLE
            } else {
                Interest::READABLE
            };
            if !has_read && !has_write {
                // Keep READABLE as a fallback to satisfy mio interest requirements.
                return poll
                    .registry()
                    .reregister(&mut sess.client, tk, Interest::READABLE);
            }
            poll.registry().reregister(&mut sess.client, tk, interest)
        }

        fn set_remote_interest(
            poll: &mut Poll,
            tk: Token,
            sess: &mut Socks5Session,
        ) -> io::Result<()> {
            let remote = sess.remote.as_mut().unwrap();
            let has_read = !sess.remote_eof;
            let has_write =
                sess.state == Socks5SessionState::Connecting || !sess.c2r_queue.is_empty();
            let interest = if has_read && has_write {
                Interest::READABLE | Interest::WRITABLE
            } else if has_write {
                Interest::WRITABLE
            } else {
                Interest::READABLE
            };
            poll.registry().reregister(remote, tk, interest)
        }

        fn parse_target_from_buf(
            buf: &[u8],
        ) -> Result<Option<(usize, SocketAddr, String)>, &'static str> {
            if buf.len() < 4 {
                return Ok(None);
            }
            if buf[0] != 0x05 {
                return Err("invalid socks5 request version");
            }
            if buf[2] != 0x00 {
                return Err("invalid socks5 request reserved field");
            }
            if buf[1] != 0x01 {
                return Err("unsupported socks5 command");
            }
            match buf[3] {
                0x01 => {
                    if buf.len() < 10 {
                        return Ok(None);
                    }
                    let ip = Ipv4Addr::new(buf[4], buf[5], buf[6], buf[7]);
                    let port = u16::from_be_bytes([buf[8], buf[9]]);
                    let addr = SocketAddr::from((ip, port));
                    Ok(Some((10, addr, addr.to_string())))
                }
                0x03 => {
                    if buf.len() < 5 {
                        return Ok(None);
                    }
                    let host_len = buf[4] as usize;
                    if buf.len() < 5 + host_len + 2 {
                        return Ok(None);
                    }
                    let host = String::from_utf8_lossy(&buf[5..5 + host_len]).to_string();
                    let port = u16::from_be_bytes([buf[5 + host_len], buf[5 + host_len + 1]]);
                    let addr = match (host.as_str(), port).to_socket_addrs() {
                        Ok(mut addrs) => addrs.next(),
                        Err(_) => None,
                    };
                    match addr {
                        Some(a) => Ok(Some((5 + host_len + 2, a, format!("{host}:{port}")))),
                        None => Err("target host resolve failed"),
                    }
                }
                0x04 => {
                    if buf.len() < 22 {
                        return Ok(None);
                    }
                    let mut ip = [0u8; 16];
                    ip.copy_from_slice(&buf[4..20]);
                    let port = u16::from_be_bytes([buf[20], buf[21]]);
                    let addr = SocketAddr::from((Ipv6Addr::from(ip), port));
                    Ok(Some((22, addr, addr.to_string())))
                }
                _ => Err("unsupported socks5 address type"),
            }
        }

        fn find_header_end(buf: &[u8]) -> Option<usize> {
            if buf.len() < 4 {
                return None;
            }
            for i in 0..=(buf.len() - 4) {
                if &buf[i..i + 4] == b"\r\n\r\n" {
                    return Some(i + 4);
                }
            }
            None
        }

        fn split_host_port(authority: &str, default_port: u16) -> (String, u16, String) {
            if authority.starts_with('[') {
                if let Some(idx) = authority.find(']') {
                    let host = authority[1..idx].to_string();
                    let rest = &authority[idx + 1..];
                    if let Some(port_s) = rest.strip_prefix(':') {
                        if let Ok(port) = port_s.parse::<u16>() {
                            return (host.clone(), port, format!("[{host}]:{port}"));
                        }
                    }
                    return (
                        host.clone(),
                        default_port,
                        format!("[{host}]:{default_port}"),
                    );
                }
            }
            if let Some((h, p)) = authority.rsplit_once(':') {
                if !h.contains(':') {
                    if let Ok(port) = p.parse::<u16>() {
                        return (h.to_string(), port, format!("{h}:{port}"));
                    }
                }
            }
            (
                authority.to_string(),
                default_port,
                format!("{}:{}", authority, default_port),
            )
        }

        fn resolve_host_port(host: &str, port: u16) -> Result<SocketAddr, &'static str> {
            match (host, port).to_socket_addrs() {
                Ok(mut addrs) => addrs.next().ok_or("target host resolve failed"),
                Err(_) => Err("target host resolve failed"),
            }
        }

        fn parse_http_proxy_request(
            buf: &[u8],
        ) -> Result<Option<(usize, SocketAddr, String, ProxyProtocol, Vec<u8>)>, &'static str>
        {
            let Some(header_end) = find_header_end(buf) else {
                return Ok(None);
            };
            let header = &buf[..header_end];
            let header_str = std::str::from_utf8(header).map_err(|_| "invalid http request")?;
            let mut lines = header_str.split("\r\n");
            let reqline = lines.next().ok_or("empty http request")?;
            let parts: Vec<&str> = reqline.split_whitespace().collect();
            if parts.len() < 3 {
                return Err("invalid http request line");
            }
            let method = parts[0];
            let target = parts[1];
            let version = parts[2];

            if method.eq_ignore_ascii_case("CONNECT") {
                let (host, port, label) = split_host_port(target, 443);
                let addr = resolve_host_port(&host, port)?;
                return Ok(Some((
                    header_end,
                    addr,
                    label,
                    ProxyProtocol::HttpTunnel,
                    Vec::new(),
                )));
            }

            let mut host_header: Option<String> = None;
            let mut headers_raw = String::new();
            for line in lines {
                if line.is_empty() {
                    continue;
                }
                let lower = line.to_ascii_lowercase();
                if lower.starts_with("host:") {
                    host_header = Some(line[5..].trim().to_string());
                }
                if lower.starts_with("proxy-connection:") {
                    continue;
                }
                if lower.starts_with("connection:") {
                    continue;
                }
                headers_raw.push_str(line);
                headers_raw.push_str("\r\n");
            }
            headers_raw.push_str("Connection: close\r\n");

            let (resolved, label, rewritten_uri) =
                if target.len() >= 7 && target[..7].eq_ignore_ascii_case("http://") {
                    let rest = &target[7..];
                    let authority_end = rest
                        .find(|c| c == '/' || c == '?' || c == '#')
                        .unwrap_or(rest.len());
                    let authority = &rest[..authority_end];
                    let path_and_more = &rest[authority_end..];
                    let rewritten_uri = if path_and_more.is_empty() {
                        "/".to_string()
                    } else if path_and_more.starts_with('/') {
                        path_and_more.to_string()
                    } else if path_and_more.starts_with('?') {
                        format!("/{}", path_and_more)
                    } else {
                        "/".to_string()
                    };
                    let (host, port, label) = split_host_port(authority, 80);
                    (resolve_host_port(&host, port)?, label, rewritten_uri)
                } else {
                    let host = host_header.ok_or("missing host header")?;
                    let (host_only, port, label) = split_host_port(host.trim(), 80);
                    (
                        resolve_host_port(&host_only, port)?,
                        label,
                        target.to_string(),
                    )
                };

            let mut rewritten = Vec::new();
            rewritten.extend_from_slice(
                format!("{method} {rewritten_uri} {version}\r\n{headers_raw}\r\n").as_bytes(),
            );
            rewritten.extend_from_slice(&buf[header_end..]);

            Ok(Some((
                header_end,
                resolved,
                label,
                ProxyProtocol::HttpForward,
                rewritten,
            )))
        }

        fn close_session(
            poll: &mut Poll,
            sessions: &mut HashMap<Token, Socks5Session>,
            remote_to_client: &mut HashMap<Token, Token>,
            client_token: Token,
        ) {
            if let Some(mut sess) = sessions.remove(&client_token) {
                let _ = poll.registry().deregister(&mut sess.client);
                if let Some(remote_token) = sess.remote_token {
                    remote_to_client.remove(&remote_token);
                    if let Some(remote) = sess.remote.as_mut() {
                        let _ = poll.registry().deregister(remote);
                    }
                }
                let target = sess
                    .target_label
                    .clone()
                    .or_else(|| sess.target.map(|a| a.to_string()))
                    .unwrap_or_else(|| "unknown".to_string());
                let reason = sess
                    .close_reason
                    .unwrap_or_else(|| "closed by peer".to_string());
                let protocol = proxy_protocol_tag(sess.protocol);
                if sess.state == Socks5SessionState::Relay {
                    info!(
                        "{} {} relay finished to {}: up {} bytes, down {} bytes",
                        protocol, sess.client_addr, target, sess.up_bytes, sess.down_bytes
                    );
                }
                info!(
                    "{} session closed {}: {}",
                    protocol, sess.client_addr, reason
                );
                info!("PROXY opened connections: {}", sessions.len());
            }
        }

        let mut poll = Poll::new()?;
        let mut listener = TcpListener::bind(self.local_addr)?;
        let listener_token = Token(0);
        poll.registry()
            .register(&mut listener, listener_token, Interest::READABLE)?;
        info!("listen at socks5://{}", listener.local_addr().unwrap());

        let mut events = Events::with_capacity(1024);
        let mut next_token = Token(1);
        let mut sessions: HashMap<Token, Socks5Session> = HashMap::new();
        let mut remote_to_client: HashMap<Token, Token> = HashMap::new();
        let mut to_close: Vec<Token> = vec![];

        loop {
            if closed.load(std::sync::atomic::Ordering::SeqCst) {
                return Ok(());
            }

            poll.poll(&mut events, Some(time::Duration::from_secs(1)))?;
            for event in &events {
                let tk = event.token();
                if tk == listener_token {
                    if !event.is_readable() {
                        continue;
                    }
                    loop {
                        match listener.accept() {
                            Ok((mut stream, addr)) => {
                                if !ip_matcher.testipaddr(&addr.ip()) {
                                    info!("drop PROXY connection from {}", addr);
                                    continue;
                                }
                                if let Some(mx) = self.max_connections {
                                    if sessions.len() as u64 >= mx {
                                        info!("drop PROXY connection from {} for quota", addr);
                                        continue;
                                    }
                                }
                                let ctk = nextToken(&mut next_token);
                                poll.registry()
                                    .register(&mut stream, ctk, Interest::READABLE)?;
                                sessions.insert(ctk, Socks5Session::new(stream, addr));
                                info!("accept PROXY connection from {}", addr);
                                info!("PROXY opened connections: {}", sessions.len());
                            }
                            Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                            Err(err) => return Err(err),
                        }
                    }
                    continue;
                }

                let client_token = remote_to_client.get(&tk).copied().unwrap_or(tk);
                if !sessions.contains_key(&client_token) {
                    continue;
                }

                let is_remote_event = remote_to_client.contains_key(&tk);
                if is_remote_event {
                    let sess = sessions.get_mut(&client_token).unwrap();
                    if event.is_readable() {
                        let mut buf = [0u8; 1 << 16];
                        loop {
                            let remote = sess.remote.as_mut().unwrap();
                            match remote.read(&mut buf) {
                                Ok(0) => {
                                    sess.remote_eof = true;
                                    if sess.r2c_queue.is_empty() {
                                        sess.close_reason = Some("remote closed".to_string());
                                        to_close.push(client_token);
                                    }
                                    break;
                                }
                                Ok(n) => {
                                    sess.down_bytes += n as u64;
                                    sess.r2c_queue.push_back((buf[0..n].to_vec(), 0));
                                }
                                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                                Err(err) => {
                                    sess.close_reason = Some(format!("remote read error: {}", err));
                                    to_close.push(client_token);
                                    break;
                                }
                            }
                        }
                    }
                    if event.is_writable() {
                        if sess.state == Socks5SessionState::Connecting {
                            let connect_err = sess.remote.as_mut().unwrap().take_error()?;
                            if let Some(err) = connect_err {
                                match sess.protocol {
                                    ProxyProtocol::Socks5 => {
                                        sess.r2c_queue.push_back((
                                            encode_socks5_reply(
                                                0x05,
                                                SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
                                            ),
                                            0,
                                        ));
                                    }
                                    ProxyProtocol::HttpTunnel | ProxyProtocol::HttpForward => {
                                        sess.r2c_queue.push_back((
                                            b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n"
                                                .to_vec(),
                                            0,
                                        ));
                                    }
                                    ProxyProtocol::Unknown => {}
                                }
                                sess.close_after_flush = true;
                                sess.close_reason =
                                    Some(format!("failed to connect target: {}", err));
                                sess.state = Socks5SessionState::Closing;
                            } else {
                                let bound = sess
                                    .remote
                                    .as_ref()
                                    .and_then(|s| s.local_addr().ok())
                                    .unwrap_or(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)));
                                match sess.protocol {
                                    ProxyProtocol::Socks5 => {
                                        sess.r2c_queue
                                            .push_back((encode_socks5_reply(0x00, bound), 0));
                                    }
                                    ProxyProtocol::HttpTunnel => {
                                        sess.r2c_queue.push_back((
                                            b"HTTP/1.1 200 Connection Established\r\n\r\n".to_vec(),
                                            0,
                                        ));
                                    }
                                    ProxyProtocol::HttpForward | ProxyProtocol::Unknown => {}
                                }
                                sess.state = Socks5SessionState::Relay;
                                let protocol = proxy_protocol_tag(sess.protocol);
                                info!(
                                    "{} {} relay established: client -> {} (bound {})",
                                    protocol,
                                    sess.client_addr,
                                    sess.target_label
                                        .clone()
                                        .or_else(|| sess.target.map(|a| a.to_string()))
                                        .unwrap_or_else(|| "unknown".to_string()),
                                    bound
                                );
                                if !sess.client_in.is_empty() {
                                    let extra = std::mem::take(&mut sess.client_in);
                                    sess.up_bytes += extra.len() as u64;
                                    sess.c2r_queue.push_back((extra, 0));
                                }
                            }
                        }

                        if !sess.c2r_queue.is_empty() && sess.state == Socks5SessionState::Relay {
                            let remote = sess.remote.as_mut().unwrap();
                            if let Err(err) = flush_queue(remote, &mut sess.c2r_queue) {
                                sess.close_reason = Some(format!("remote write error: {}", err));
                                to_close.push(client_token);
                            }
                        }
                        if sess.client_eof
                            && sess.c2r_queue.is_empty()
                            && !sess.remote_write_shutdown
                            && sess.remote.is_some()
                        {
                            if let Some(remote) = sess.remote.as_mut() {
                                let _ = remote.shutdown(Shutdown::Write);
                            }
                            sess.remote_write_shutdown = true;
                        }
                    }
                    if let Some(rtk) = sess.remote_token {
                        let _ = set_remote_interest(&mut poll, rtk, sess);
                    }
                    let _ = set_client_interest(&mut poll, client_token, sess);
                } else {
                    let sess = sessions.get_mut(&client_token).unwrap();
                    if event.is_readable() {
                        let mut buf = [0u8; 1 << 16];
                        loop {
                            match sess.client.read(&mut buf) {
                                Ok(0) => {
                                    sess.client_eof = true;
                                    if sess.remote_eof && sess.r2c_queue.is_empty() {
                                        sess.close_reason = Some("client closed".to_string());
                                        to_close.push(client_token);
                                    }
                                    break;
                                }
                                Ok(n) => {
                                    if sess.state == Socks5SessionState::Relay {
                                        sess.up_bytes += n as u64;
                                        sess.c2r_queue.push_back((buf[0..n].to_vec(), 0));
                                    } else {
                                        sess.client_in.extend_from_slice(&buf[0..n]);
                                    }
                                }
                                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                                Err(err) => {
                                    sess.close_reason = Some(format!("client read error: {}", err));
                                    to_close.push(client_token);
                                    break;
                                }
                            }
                        }
                    }

                    if event.is_writable() {
                        if let Err(err) = flush_queue(&mut sess.client, &mut sess.r2c_queue) {
                            sess.close_reason = Some(format!("client write error: {}", err));
                            to_close.push(client_token);
                        }
                        if sess.remote_eof
                            && sess.r2c_queue.is_empty()
                            && !sess.client_write_shutdown
                        {
                            let _ = sess.client.shutdown(Shutdown::Write);
                            sess.client_write_shutdown = true;
                        }
                        if sess.close_after_flush && sess.r2c_queue.is_empty() {
                            to_close.push(client_token);
                        }
                        if sess.remote_eof && sess.r2c_queue.is_empty() {
                            sess.close_reason = Some("remote closed".to_string());
                            to_close.push(client_token);
                        }
                    }

                    loop {
                        match sess.state {
                            Socks5SessionState::DetectProtocol => {
                                if sess.client_in.is_empty() {
                                    break;
                                }
                                if sess.client_in[0] == 0x05 {
                                    sess.protocol = ProxyProtocol::Socks5;
                                    sess.state = Socks5SessionState::Greeting;
                                    continue;
                                }
                                match parse_http_proxy_request(&sess.client_in) {
                                    Ok(Some((
                                        consumed,
                                        target,
                                        target_label,
                                        protocol,
                                        upstream,
                                    ))) => {
                                        sess.client_in.drain(0..consumed);
                                        sess.target = Some(target);
                                        sess.target_label = Some(target_label.clone());
                                        sess.protocol = protocol;
                                        let protocol = proxy_protocol_tag(sess.protocol);
                                        info!(
                                            "{} {} request target {}",
                                            protocol, sess.client_addr, target_label
                                        );
                                        if !upstream.is_empty() {
                                            sess.up_bytes += upstream.len() as u64;
                                            sess.c2r_queue.push_back((upstream, 0));
                                            if sess.protocol == ProxyProtocol::HttpForward {
                                                sess.client_in.clear();
                                            }
                                        }
                                        match TcpStream::connect(target) {
                                            Ok(mut remote) => {
                                                let rtk = nextToken(&mut next_token);
                                                poll.registry().register(
                                                    &mut remote,
                                                    rtk,
                                                    Interest::WRITABLE | Interest::READABLE,
                                                )?;
                                                sess.remote_token = Some(rtk);
                                                sess.remote = Some(remote);
                                                remote_to_client.insert(rtk, client_token);
                                                sess.state = Socks5SessionState::Connecting;
                                            }
                                            Err(err) => {
                                                sess.r2c_queue.push_back((
                                                    b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n"
                                                        .to_vec(),
                                                    0,
                                                ));
                                                sess.close_after_flush = true;
                                                sess.state = Socks5SessionState::Closing;
                                                sess.close_reason = Some(format!(
                                                    "failed to connect target {}: {}",
                                                    target_label, err
                                                ));
                                                break;
                                            }
                                        }
                                    }
                                    Ok(None) => {
                                        // Wait for more bytes if it may be HTTP.
                                        break;
                                    }
                                    Err(_) => {
                                        sess.close_reason =
                                            Some("unsupported proxy protocol".to_string());
                                        to_close.push(client_token);
                                        break;
                                    }
                                }
                            }
                            Socks5SessionState::Greeting => {
                                if sess.client_in.len() < 2 {
                                    break;
                                }
                                let nmethods = sess.client_in[1] as usize;
                                if sess.client_in.len() < 2 + nmethods {
                                    break;
                                }
                                let methods = sess.client_in[2..2 + nmethods].to_vec();
                                sess.client_in.drain(0..2 + nmethods);
                                if methods.contains(&0x00) {
                                    sess.r2c_queue.push_back((vec![0x05, 0x00], 0));
                                    sess.state = Socks5SessionState::Request;
                                    let protocol = proxy_protocol_tag(sess.protocol);
                                    log::debug!(
                                        "{} {} auth negotiated: no-auth",
                                        protocol,
                                        sess.client_addr
                                    );
                                } else {
                                    sess.r2c_queue.push_back((vec![0x05, 0xFF], 0));
                                    sess.close_after_flush = true;
                                    sess.state = Socks5SessionState::Closing;
                                    sess.close_reason =
                                        Some("rejected: no supported auth method".to_string());
                                    break;
                                }
                            }
                            Socks5SessionState::Request => {
                                match parse_target_from_buf(&sess.client_in) {
                                    Ok(Some((consumed, target, target_label))) => {
                                        sess.client_in.drain(0..consumed);
                                        sess.target = Some(target);
                                        sess.target_label = Some(target_label.clone());
                                        sess.protocol = ProxyProtocol::Socks5;
                                        let protocol = proxy_protocol_tag(sess.protocol);
                                        info!(
                                            "{} {} CONNECT request to {}",
                                            protocol, sess.client_addr, target_label
                                        );
                                        match TcpStream::connect(target) {
                                            Ok(mut remote) => {
                                                let rtk = nextToken(&mut next_token);
                                                poll.registry().register(
                                                    &mut remote,
                                                    rtk,
                                                    Interest::WRITABLE | Interest::READABLE,
                                                )?;
                                                sess.remote_token = Some(rtk);
                                                sess.remote = Some(remote);
                                                remote_to_client.insert(rtk, client_token);
                                                sess.state = Socks5SessionState::Connecting;
                                            }
                                            Err(err) => {
                                                sess.r2c_queue.push_back((
                                                    encode_socks5_reply(
                                                        0x05,
                                                        SocketAddr::from((
                                                            Ipv4Addr::UNSPECIFIED,
                                                            0,
                                                        )),
                                                    ),
                                                    0,
                                                ));
                                                sess.close_after_flush = true;
                                                sess.state = Socks5SessionState::Closing;
                                                sess.close_reason = Some(format!(
                                                    "failed to connect target {}: {}",
                                                    target, err
                                                ));
                                                break;
                                            }
                                        }
                                    }
                                    Ok(None) => break,
                                    Err(reason) => {
                                        let rep = if reason.contains("command") {
                                            0x07
                                        } else if reason.contains("address type") {
                                            0x08
                                        } else {
                                            0x01
                                        };
                                        sess.r2c_queue.push_back((
                                            encode_socks5_reply(
                                                rep,
                                                SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
                                            ),
                                            0,
                                        ));
                                        sess.close_after_flush = true;
                                        sess.state = Socks5SessionState::Closing;
                                        sess.close_reason = Some(reason.to_string());
                                        break;
                                    }
                                }
                            }
                            Socks5SessionState::Connecting
                            | Socks5SessionState::Relay
                            | Socks5SessionState::Closing => break,
                        }
                    }

                    if let Some(rtk) = sess.remote_token {
                        let _ = set_remote_interest(&mut poll, rtk, sess);
                    }
                    if sess.client_eof
                        && sess.remote_eof
                        && sess.r2c_queue.is_empty()
                        && sess.c2r_queue.is_empty()
                    {
                        sess.close_reason = Some("both sides closed".to_string());
                        to_close.push(client_token);
                    }
                    let _ = set_client_interest(&mut poll, client_token, sess);
                }
            }

            for client_token in to_close.drain(..) {
                close_session(
                    &mut poll,
                    &mut sessions,
                    &mut remote_to_client,
                    client_token,
                );
            }
        }
    }

    pub fn close(self: &Self) -> () {}
}

#[cfg(test)]
mod tests {
    use super::parse_socks5_target;
    use std::io::Write;
    use std::net::{SocketAddr, TcpListener, TcpStream};

    #[test]
    fn test_parse_socks5_target_ipv4_connect_request() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let sender = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(addr).unwrap();
            // VER=5 CMD=CONNECT RSV=0 ATYP=IPv4 DST=127.0.0.1:8080
            let req = [0x05, 0x01, 0x00, 0x01, 127, 0, 0, 1, 0x1f, 0x90];
            stream.write_all(&req).unwrap();
        });

        let (mut server_stream, _) = listener.accept().unwrap();
        let target = parse_socks5_target(&mut server_stream).unwrap();
        assert_eq!(target, SocketAddr::from(([127, 0, 0, 1], 8080)));
        sender.join().unwrap();
    }
}
