use std::net::ToSocketAddrs;


#[derive(Clone)]
pub struct ForwardSessionConfig<T: ToSocketAddrs> {
    pub local: T,
    pub remoteMap: Vec<(String,String)>,
    pub allow_nets: Vec<String>,
    pub enable_tcp: bool,
    pub enable_udp: bool,
    pub conn_bufsize: usize,
    pub max_connections: i64,
}
