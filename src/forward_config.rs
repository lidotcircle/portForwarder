use std::net::ToSocketAddrs;

#[derive(Clone, Debug)]
pub struct TrafficRecordingConfig {
    /// SQLite database path. Recording is disabled when the containing option is `None`.
    pub database: String,
    /// Store payload bytes in addition to metadata.
    pub capture_payload: bool,
    /// Maximum payload bytes stored for each TCP chunk or UDP datagram.
    pub max_payload_bytes: usize,
    /// Maximum number of pending events before payload/event records are dropped.
    pub queue_capacity: usize,
}

impl TrafficRecordingConfig {
    pub fn new(database: impl Into<String>) -> Self {
        Self {
            database: database.into(),
            capture_payload: true,
            max_payload_bytes: 64 * 1024,
            queue_capacity: 1024,
        }
    }
}

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
    pub recording: Option<TrafficRecordingConfig>,
}
