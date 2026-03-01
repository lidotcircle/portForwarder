use std::net::ToSocketAddrs;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TcpMode {
    Forward,
    Socks5Server,
}

#[derive(Clone, Debug)]
pub struct ForwardSessionConfig<T: ToSocketAddrs> {
    pub local: T,
    pub remoteMap: Vec<(String, String)>,
    pub allow_nets: Vec<String>,
    pub enable_tcp: bool,
    pub enable_udp: bool,
    pub conn_bufsize: usize,
    pub max_connections: i64,
    pub tcp_mode: TcpMode,
}
