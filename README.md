## Application Protocol Multiplexer

The Application Protocol Multiplexer is a tool that allows you to forward TCP or UDP traffic to 
different addresses based on regex pattern matching in the first packet sent by the client.

### Usage

You can build this tool from source with `cargo install portForwarder` or download the prebuilt binary called `portfd` from the [release page](https://github.com/lidotcircle/portForwarder/releases).

To run `portfd` with simple command line arguments, use the following syntax: `portfd <local-bind> <remote>`.
In this case, you need to specify the listening address and the remote address.
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
    enable_tcp: true # Default is true
    enable_udp: true # Default is true
    conn_bufsize: 2MB
    max_connections: 10000 # Optional
    allow_nets: # Optional whitelist
      - 127.0.0.0/24
```

The pattern field supports six formats, all of which will be converted to regular expressions:

+ `[http] or [http:domain_name]`: Only HTTP traffic or host names of HTTP requests matching domain_name will be forwarded to the specified remote address.
+ `[https:domain_name]`: Matches the SNI (Server Name Indication) in the client hello for HTTPS traffic.
+ `[ssh]`: Only SSH traffic will be forwarded.
+ `[socks5]`: Only socks5 traffic will be forwarded.
+ `[rdp]`: Only rdp traffic will be forwarded.
+ **any regex**: Only the traffic of the first received packet that matches this regex will be forwarded.
