use crate::forward_config::TrafficRecordingConfig;
use log::{error, warn};
use rusqlite::{Connection, ErrorCode, params};
use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, channel, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS traffic_sessions (
    id                         TEXT PRIMARY KEY,
    transport                  TEXT NOT NULL,
    application_protocol       TEXT,
    client_addr                TEXT NOT NULL,
    listener_addr              TEXT NOT NULL,
    target_addr                TEXT NOT NULL,
    target_name                TEXT,
    started_at_ms              INTEGER NOT NULL,
    ended_at_ms                INTEGER,
    client_to_target_bytes     INTEGER NOT NULL DEFAULT 0,
    target_to_client_bytes     INTEGER NOT NULL DEFAULT 0,
    recorded_events            INTEGER NOT NULL DEFAULT 0,
    dropped_events             INTEGER NOT NULL DEFAULT 0,
    dropped_bytes              INTEGER NOT NULL DEFAULT 0,
    close_reason               TEXT
);

CREATE TABLE IF NOT EXISTS traffic_events (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id         TEXT NOT NULL REFERENCES traffic_sessions(id) ON DELETE CASCADE,
    sequence           INTEGER NOT NULL,
    timestamp_ms       INTEGER NOT NULL,
    event_type         TEXT NOT NULL,
    direction          TEXT NOT NULL,
    from_addr          TEXT NOT NULL,
    to_addr            TEXT NOT NULL,
    original_size      INTEGER NOT NULL,
    stored_size        INTEGER NOT NULL,
    payload             BLOB,
    UNIQUE(session_id, sequence)
);

CREATE INDEX IF NOT EXISTS idx_traffic_sessions_started_at
    ON traffic_sessions(started_at_ms);
CREATE INDEX IF NOT EXISTS idx_traffic_events_timestamp
    ON traffic_events(timestamp_ms);
CREATE INDEX IF NOT EXISTS idx_traffic_events_session
    ON traffic_events(session_id, sequence);
"#;

#[derive(Clone, Copy, Debug)]
pub enum Transport {
    Tcp,
    Udp,
}

impl Transport {
    fn as_str(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        }
    }

    fn event_type(self) -> &'static str {
        match self {
            Self::Tcp => "tcp_chunk",
            Self::Udp => "udp_datagram",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Direction {
    ClientToTarget,
    TargetToClient,
}

impl Direction {
    fn as_str(self) -> &'static str {
        match self {
            Self::ClientToTarget => "client_to_target",
            Self::TargetToClient => "target_to_client",
        }
    }
}

struct SessionCounters {
    dropped_events: AtomicU64,
    dropped_bytes: AtomicU64,
}

#[derive(Clone)]
pub struct TrafficSession {
    id: String,
    transport: Transport,
    client_addr: String,
    target_addr: String,
    counters: Arc<SessionCounters>,
}

impl TrafficSession {
    pub fn id(&self) -> &str {
        &self.id
    }
}

enum RecorderMessage {
    Start {
        id: String,
        transport: &'static str,
        application_protocol: Option<String>,
        client_addr: String,
        listener_addr: String,
        target_addr: String,
        target_name: Option<String>,
        timestamp_ms: i64,
    },
    Event {
        session_id: String,
        timestamp_ms: i64,
        event_type: &'static str,
        direction: &'static str,
        from_addr: String,
        to_addr: String,
        original_size: u64,
        payload: Option<Vec<u8>>,
    },
    End {
        session_id: String,
        timestamp_ms: i64,
        client_to_target_bytes: u64,
        target_to_client_bytes: u64,
        dropped_events: u64,
        dropped_bytes: u64,
        close_reason: String,
    },
    Flush(SyncSender<()>),
    Shutdown,
}

struct RecorderInner {
    sender: Sender<RecorderMessage>,
    capture_payload: bool,
    max_payload_bytes: usize,
    queue_capacity: usize,
    pending_events: Arc<AtomicUsize>,
    writer: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for RecorderInner {
    fn drop(&mut self) {
        let _ = self.sender.send(RecorderMessage::Shutdown);
        if let Ok(writer) = self.writer.get_mut() {
            if let Some(handle) = writer.take() {
                let _ = handle.join();
            }
        }
    }
}

#[derive(Clone)]
pub struct TrafficRecorder {
    inner: Arc<RecorderInner>,
}

impl TrafficRecorder {
    pub fn open(config: &TrafficRecordingConfig) -> io::Result<Self> {
        if config.queue_capacity == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "recording queue_capacity must be greater than zero",
            ));
        }

        let connection = Connection::open(&config.database).map_err(sqlite_io_error)?;
        connection
            .busy_timeout(Duration::from_millis(250))
            .map_err(sqlite_io_error)?;
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL;\n\
                 PRAGMA synchronous=NORMAL;\n\
                 PRAGMA foreign_keys=ON;",
            )
            .map_err(sqlite_io_error)?;
        connection.execute_batch(SCHEMA).map_err(sqlite_io_error)?;

        let (sender, receiver) = channel();
        let pending_events = Arc::new(AtomicUsize::new(0));
        let writer_pending_events = pending_events.clone();
        let writer = thread::Builder::new()
            .name("traffic-sqlite-writer".to_string())
            .spawn(move || writer_loop(connection, receiver, writer_pending_events))?;

        Ok(Self {
            inner: Arc::new(RecorderInner {
                sender,
                capture_payload: config.capture_payload,
                max_payload_bytes: config.max_payload_bytes,
                queue_capacity: config.queue_capacity,
                pending_events,
                writer: Mutex::new(Some(writer)),
            }),
        })
    }

    pub fn start_session(
        &self,
        transport: Transport,
        application_protocol: Option<&str>,
        client_addr: impl ToString,
        listener_addr: impl ToString,
        target_addr: impl ToString,
        target_name: Option<&str>,
    ) -> TrafficSession {
        let client_addr = client_addr.to_string();
        let target_addr = target_addr.to_string();
        let session = TrafficSession {
            id: new_session_id(),
            transport,
            client_addr: client_addr.clone(),
            target_addr: target_addr.clone(),
            counters: Arc::new(SessionCounters {
                dropped_events: AtomicU64::new(0),
                dropped_bytes: AtomicU64::new(0),
            }),
        };
        let message = RecorderMessage::Start {
            id: session.id.clone(),
            transport: transport.as_str(),
            application_protocol: application_protocol.map(str::to_string),
            client_addr,
            listener_addr: listener_addr.to_string(),
            target_addr,
            target_name: target_name.map(str::to_string),
            timestamp_ms: unix_timestamp_ms(),
        };
        if self.inner.sender.send(message).is_err() {
            warn!("traffic recorder stopped before a session could be recorded");
        }
        session
    }

    /// Queue one forwarded portion without blocking the network loop.
    ///
    /// TCP chunks follow application read/write boundaries and are not IP packets.
    /// UDP events retain datagram boundaries.
    pub fn record(&self, session: &TrafficSession, direction: Direction, data: &[u8]) {
        let (from_addr, to_addr) = match direction {
            Direction::ClientToTarget => (&session.client_addr, &session.target_addr),
            Direction::TargetToClient => (&session.target_addr, &session.client_addr),
        };
        self.record_with_endpoints(session, direction, from_addr, to_addr, data);
    }

    /// Record traffic whose actual endpoints differ from the session's logical endpoints.
    /// This is primarily useful for UDP responses sent from a different address.
    pub fn record_with_endpoints(
        &self,
        session: &TrafficSession,
        direction: Direction,
        from_addr: impl ToString,
        to_addr: impl ToString,
        data: &[u8],
    ) {
        if !self.reserve_event_slot() {
            mark_dropped(session, data.len());
            return;
        }

        let payload = if self.inner.capture_payload {
            let stored = data.len().min(self.inner.max_payload_bytes);
            Some(data[..stored].to_vec())
        } else {
            None
        };
        let message = RecorderMessage::Event {
            session_id: session.id.clone(),
            timestamp_ms: unix_timestamp_ms(),
            event_type: session.transport.event_type(),
            direction: direction.as_str(),
            from_addr: from_addr.to_string(),
            to_addr: to_addr.to_string(),
            original_size: data.len() as u64,
            payload,
        };
        if self.inner.sender.send(message).is_err() {
            self.inner.pending_events.fetch_sub(1, Ordering::AcqRel);
            mark_dropped(session, data.len());
        }
    }

    fn reserve_event_slot(&self) -> bool {
        self.inner
            .pending_events
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |pending| {
                (pending < self.inner.queue_capacity).then_some(pending + 1)
            })
            .is_ok()
    }

    pub fn end_session(
        &self,
        session: TrafficSession,
        client_to_target_bytes: u64,
        target_to_client_bytes: u64,
        close_reason: impl Into<String>,
    ) {
        let message = RecorderMessage::End {
            session_id: session.id,
            timestamp_ms: unix_timestamp_ms(),
            client_to_target_bytes,
            target_to_client_bytes,
            dropped_events: session.counters.dropped_events.load(Ordering::Relaxed),
            dropped_bytes: session.counters.dropped_bytes.load(Ordering::Relaxed),
            close_reason: close_reason.into(),
        };
        if self.inner.sender.send(message).is_err() {
            warn!("traffic recorder stopped before a session summary could be written");
        }
    }

    pub fn shutdown(&self) {
        let mut writer = match self.inner.writer.lock() {
            Ok(writer) => writer,
            Err(_) => return,
        };
        let Some(handle) = writer.take() else {
            return;
        };
        let _ = self.inner.sender.send(RecorderMessage::Shutdown);
        if handle.join().is_err() {
            error!("traffic recorder writer thread panicked");
        }
    }

    /// Wait until all messages queued before this call have been committed.
    pub fn flush(&self) {
        let (sender, receiver) = sync_channel(0);
        if self
            .inner
            .sender
            .send(RecorderMessage::Flush(sender))
            .is_ok()
        {
            let _ = receiver.recv();
        }
    }
}

fn mark_dropped(session: &TrafficSession, bytes: usize) {
    session
        .counters
        .dropped_events
        .fetch_add(1, Ordering::Relaxed);
    session
        .counters
        .dropped_bytes
        .fetch_add(bytes as u64, Ordering::Relaxed);
}

fn sqlite_io_error(error: rusqlite::Error) -> io::Error {
    io::Error::new(io::ErrorKind::Other, error)
}

fn unix_timestamp_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn new_session_id() -> String {
    static NEXT_ID: AtomicU64 = AtomicU64::new(0);
    let sequence = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:x}-{:x}-{:x}", std::process::id(), now, sequence)
}

fn writer_loop(
    mut connection: Connection,
    receiver: Receiver<RecorderMessage>,
    pending_events: Arc<AtomicUsize>,
) {
    let mut sequences: HashMap<String, u64> = HashMap::new();
    let mut stop = false;

    while !stop {
        let first = match receiver.recv() {
            Ok(message) => message,
            Err(_) => break,
        };
        let mut batch = Vec::with_capacity(256);
        if matches!(first, RecorderMessage::Shutdown) {
            stop = true;
        } else {
            batch.push(first);
        }
        while batch.len() < 256 && !stop {
            match receiver.try_recv() {
                Ok(RecorderMessage::Shutdown) => stop = true,
                Ok(message) => batch.push(message),
                Err(_) => break,
            }
        }

        if batch.is_empty() {
            continue;
        }
        let mut retry_count = 0_u64;
        loop {
            let mut next_sequences = sequences.clone();
            match write_batch(&mut connection, &batch, &mut next_sequences) {
                Ok(()) => {
                    sequences = next_sequences;
                    finish_batch(&batch, &pending_events);
                    break;
                }
                Err(error) if is_transient_lock(&error) => {
                    retry_count += 1;
                    if retry_count == 1 || retry_count % 100 == 0 {
                        warn!(
                            "traffic recording database is locked; retaining batch and retrying: {error}"
                        );
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                Err(error) => {
                    error!("failed to write traffic recording batch; stopping recorder: {error}");
                    finish_batch(&batch, &pending_events);
                    return;
                }
            }
        }
    }
}

fn write_batch(
    connection: &mut Connection,
    batch: &[RecorderMessage],
    sequences: &mut HashMap<String, u64>,
) -> rusqlite::Result<()> {
    let transaction = connection.transaction()?;
    for message in batch {
        match message {
            RecorderMessage::Start {
                id,
                transport,
                application_protocol,
                client_addr,
                listener_addr,
                target_addr,
                target_name,
                timestamp_ms,
            } => {
                transaction.execute(
                    "INSERT INTO traffic_sessions (\
                                id, transport, application_protocol, client_addr, listener_addr, \
                                target_addr, target_name, started_at_ms\
                             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        id,
                        transport,
                        application_protocol,
                        client_addr,
                        listener_addr,
                        target_addr,
                        target_name,
                        timestamp_ms,
                    ],
                )?;
            }
            RecorderMessage::Event {
                session_id,
                timestamp_ms,
                event_type,
                direction,
                from_addr,
                to_addr,
                original_size,
                payload,
            } => {
                let sequence = sequences.entry(session_id.clone()).or_insert(0);
                let stored_size = payload.as_ref().map_or(0, Vec::len) as u64;
                transaction.execute(
                    "INSERT INTO traffic_events (\
                                session_id, sequence, timestamp_ms, event_type, direction, \
                                from_addr, to_addr, original_size, stored_size, payload\
                             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    params![
                        session_id,
                        *sequence,
                        timestamp_ms,
                        event_type,
                        direction,
                        from_addr,
                        to_addr,
                        original_size,
                        stored_size,
                        payload,
                    ],
                )?;
                *sequence += 1;
            }
            RecorderMessage::End {
                session_id,
                timestamp_ms,
                client_to_target_bytes,
                target_to_client_bytes,
                dropped_events,
                dropped_bytes,
                close_reason,
            } => {
                transaction.execute(
                    "UPDATE traffic_sessions SET \
                        ended_at_ms = ?2, client_to_target_bytes = ?3, \
                        target_to_client_bytes = ?4, \
                        recorded_events = (SELECT COUNT(*) FROM traffic_events WHERE session_id = ?1), \
                        dropped_events = ?5, dropped_bytes = ?6, close_reason = ?7 \
                     WHERE id = ?1",
                    params![
                        session_id,
                        timestamp_ms,
                        client_to_target_bytes,
                        target_to_client_bytes,
                        dropped_events,
                        dropped_bytes,
                        close_reason,
                    ],
                )?;
                sequences.remove(session_id);
            }
            RecorderMessage::Flush(_) => {}
            RecorderMessage::Shutdown => unreachable!(),
        }
    }
    transaction.commit()
}

fn is_transient_lock(error: &rusqlite::Error) -> bool {
    matches!(
        error.sqlite_error_code(),
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    )
}

fn finish_batch(batch: &[RecorderMessage], pending_events: &AtomicUsize) {
    let event_count = batch
        .iter()
        .filter(|message| matches!(message, RecorderMessage::Event { .. }))
        .count();
    if event_count != 0 {
        pending_events.fetch_sub(event_count, Ordering::AcqRel);
    }
    for message in batch {
        if let RecorderMessage::Flush(sender) = message {
            let _ = sender.send(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Direction, TrafficRecorder, Transport};
    use crate::forward_config::TrafficRecordingConfig;
    use rusqlite::Connection;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    #[test]
    fn writes_session_and_directional_events() {
        let path = std::env::temp_dir().join(format!(
            "portforwarder-recording-{}-{}.sqlite3",
            std::process::id(),
            super::unix_timestamp_ms()
        ));
        let mut config = TrafficRecordingConfig::new(path.to_string_lossy());
        config.max_payload_bytes = 3;
        let recorder = TrafficRecorder::open(&config).unwrap();
        let session = recorder.start_session(
            Transport::Tcp,
            Some("test"),
            "127.0.0.1:1000",
            "127.0.0.1:2000",
            "127.0.0.1:3000",
            None,
        );
        recorder.record(&session, Direction::ClientToTarget, b"hello");
        recorder.record(&session, Direction::TargetToClient, b"world");
        recorder.end_session(session, 5, 5, "test complete");
        recorder.shutdown();

        let db = Connection::open(&path).unwrap();
        let session_row: (String, i64, i64, i64) = db
            .query_row(
                "SELECT transport, client_to_target_bytes, target_to_client_bytes, recorded_events \
                 FROM traffic_sessions",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(session_row, ("tcp".to_string(), 5, 5, 2));

        let event_row: (String, String, i64, Vec<u8>) = db
            .query_row(
                "SELECT from_addr, to_addr, original_size, payload \
                 FROM traffic_events WHERE sequence = 0",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(event_row.0, "127.0.0.1:1000");
        assert_eq!(event_row.1, "127.0.0.1:3000");
        assert_eq!(event_row.2, 5);
        assert_eq!(event_row.3, b"hel");

        drop(db);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{}-wal", path.to_string_lossy()));
        let _ = std::fs::remove_file(format!("{}-shm", path.to_string_lossy()));
    }

    #[test]
    fn retries_locked_batches_without_blocking_or_losing_events() {
        let path = std::env::temp_dir().join(format!(
            "portforwarder-recording-lock-{}-{}.sqlite3",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut config = TrafficRecordingConfig::new(path.to_string_lossy());
        config.queue_capacity = 1;
        let recorder = TrafficRecorder::open(&config).unwrap();
        let session = recorder.start_session(
            Transport::Tcp,
            Some("test"),
            "127.0.0.1:1000",
            "127.0.0.1:2000",
            "127.0.0.1:3000",
            None,
        );
        let session_id = session.id().to_string();
        recorder.flush();

        let lock = Connection::open(&path).unwrap();
        lock.execute_batch("BEGIN IMMEDIATE").unwrap();
        recorder.record(&session, Direction::ClientToTarget, b"kept");
        recorder.record(&session, Direction::ClientToTarget, b"dropped");

        let started = Instant::now();
        recorder.end_session(session, 11, 0, "complete");
        let second = recorder.start_session(
            Transport::Udp,
            None,
            "127.0.0.1:4000",
            "127.0.0.1:5000",
            "127.0.0.1:6000",
            None,
        );
        recorder.end_session(second, 0, 0, "complete");
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "session lifecycle calls blocked behind SQLite"
        );

        // Exceed the writer's busy timeout so success requires retaining and retrying
        // the same batch rather than discarding it.
        std::thread::sleep(Duration::from_millis(600));
        lock.execute_batch("ROLLBACK").unwrap();
        recorder.flush();
        recorder.shutdown();

        let db = Connection::open(&path).unwrap();
        let summary: (i64, i64, i64) = db
            .query_row(
                "SELECT recorded_events, dropped_events, dropped_bytes \
                 FROM traffic_sessions WHERE id = ?1",
                [&session_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(summary, (1, 1, 7));
        let payload: Vec<u8> = db
            .query_row(
                "SELECT payload FROM traffic_events WHERE session_id = ?1",
                [&session_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(payload, b"kept");
        let sessions: i64 = db
            .query_row("SELECT COUNT(*) FROM traffic_sessions", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(sessions, 2);

        drop(db);
        drop(lock);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{}-wal", path.to_string_lossy()));
        let _ = std::fs::remove_file(format!("{}-shm", path.to_string_lossy()));
    }
}
