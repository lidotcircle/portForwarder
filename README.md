<div align="center">
  <h1>Application Protocol Multiplexer</h1>
  <div>
    <a href="https://github.com/lidotcircle/portForwarder/actions"><img src="https://github.com/lidotcircle/portForwarder/actions/workflows/release.yml/badge.svg" alt="CI Status" /></a>
  </div>
  <div>
    <a href="https://crates.io/crates/portForwarder">Crate 🦀</a>
  </div>
</div>

The Application Protocol Multiplexer is a tool that allows you to forward TCP or UDP traffic to 
different addresses based on regex pattern matching in the first packet sent by the client.

### Usage

You can build this tool from source with `cargo install portForwarder` or download the prebuilt binary called `portfd` from the [release page](https://github.com/lidotcircle/portForwarder/releases).

To run `portfd` with simple command line arguments, use the following syntax: `portfd <local-bind> <remote>`.
In this case, you need to specify the listening address and the remote address.
To run as a SOCKS5 server (CONNECT only), use: `portfd --socks5 <local-bind>`.
For more advanced usage, `portfd` can be started with a configuration file that supports more complex rules. 
Here's an example of a config file in YAML format:

``` yaml
forwarders:
  - local: 0.0.0.0:8808
    # Specify either 'remoteMap' or 'remote'
    remoteMap:
      - pattern: "[http:localhost]"
        remote: 192.168.44.43:5445
      - pattern: "[https:baidu.com]"
        remote: "39.156.66.10:443"
      - pattern: "[ssh]"
        remote: "192.168.44.43:22"
      - pattern: "[socks5]"
        remote: "192.168.100.46:7890"
      - pattern: "[rdp]"
        remote: 192.168.100.46:3389
      - pattern: .*
        remote: 192.168.100.46:23
    remote: <remote-address/127.0.0.1:2233>
    tcp_mode: forward # Optional, support: forward, socks5
    enable_tcp: true # Default is true
    enable_udp: true # Default is true
    conn_bufsize: 2MB
    max_connections: 10000 # Optional
    allow_nets: # Optional whitelist
      - 127.0.0.0/24
    recording: # Optional SQLite traffic recording
      database: traffic.sqlite3
      capture_payload: true
      max_payload_bytes: 64KB
      queue_capacity: 1024
```

The pattern field supports six formats, all of which will be converted to regular expressions:

+ `[http] or [http:domain_name]`: Only HTTP traffic or host names of HTTP requests matching domain_name will be forwarded to the specified remote address.
+ `[https:domain_name]`: Matches the SNI (Server Name Indication) in the client hello for HTTPS traffic.
+ `[ssh]`: Only SSH traffic will be forwarded.
+ `[socks5]`: Only socks5 traffic will be forwarded.
+ `[rdp]`: Only rdp traffic will be forwarded.
+ **any regex**: Only the traffic of the first received packet that matches this regex will be forwarded.

### SQLite traffic recording

Recording is opt-in. For a simple forwarder, pass a database path on the command line:

```sh
portfd --record traffic.sqlite3 0.0.0.0:8808 192.168.44.43:5445
```

For YAML configurations, the `recording` map supports:

- `database` (required): SQLite database path.
- `capture_payload` (default `true`): set to `false` to retain metadata only.
- `max_payload_bytes` (default `64KB`): maximum BLOB bytes retained per event. `original_size` and `stored_size` show whether an event was truncated.
- `queue_capacity` (default `1024`): number of pending events allowed before event records are dropped. Active relay I/O does not wait for a full event queue; dropped totals are saved in the session row.

The recorder uses WAL mode and a background writer. It creates two tables:

- `traffic_sessions` contains transport/application protocol, client/listener/target addresses, optional original target name, start/end timestamps, directional byte totals, close reason, and recording drop counters.
- `traffic_events` contains ordered `tcp_chunk` or `udp_datagram` rows with Unix-epoch millisecond timestamp, direction, `from_addr`, `to_addr`, original/stored sizes, and optional payload BLOB.

For example:

```sql
SELECT timestamp_ms, event_type, direction, from_addr, to_addr,
       original_size, stored_size
FROM traffic_events
ORDER BY session_id, sequence;
```

UDP event boundaries are real datagram boundaries. TCP is a byte stream, so its event boundaries are the forwarder's successful write chunks, not network packets or application messages. Use `session_id`, `direction`, and `sequence` to reassemble TCP bytes when `stored_size = original_size` and the session has no dropped events. Payloads can contain credentials or other sensitive data and the database can grow quickly, so use metadata-only capture or a smaller payload cap when full content is unnecessary.
