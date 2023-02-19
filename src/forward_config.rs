use std::net::ToSocketAddrs;


#[derive(Clone)]
pub struct ForwardSessionConfig<T: ToSocketAddrs> {
    pub local: T,
    pub remote: T,
    pub enable_tcp: bool,
    pub enable_udp: bool,
    pub allow_nets: Vec<String>,
    pub max_connections: i64,
    pub epoll_tcp: bool,
}
