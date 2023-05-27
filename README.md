## Application Protocol Multiplexer

forward tcp or udp traffic to different address based on
regex match the first packet sended by client.

### Usage

you can build this crate or just download the prebuild binary `portfd` in [release page](https://github.com/lidotcircle/portForwarder/releases).

`portfd` support running with simple command line arguments `portfd <local-bind> <remote>`, in this case
listening address and remote address should be specified. And more advanced
`portfd` can start with a config file which support more complicated rules. 
below is an example of config file:
``` yaml
forwarders:
  - local: 0.0.0.0:8808
    # specify either 'remoteMap' or 'remote'
    remoteMap:
      - pattern: "[http:localhost]"
        remote: 192.168.44.43:5445
      - pattern: "[https:baidu.com]"
        remote: "39.156.66.10:443"
      - pattern: "[ssh]"
        remote: "192.168.44.43:22"
      - pattern: .*
        remote: 192.168.100.46:3389
    remote: <remote-address/127.0.0.1:2233>
    enable_tcp: true # default is true
    enable_udp: true # default is true
    conn_bufsize: 2MB
    max_connections: 10000 # optional
    allow_nets: # optional whitelist
      - 127.0.0.0/24
```

`pattern` field support four formats, but all specification will convert to regular expressions.
+ `[http]` or `[http:domain_name]`, in this case only HTTP traffic or host name of HTTP request match domain_name will be forwarded to `remote`
+ `[https:domain_name]`, by matching SNI in client hello
+ `[ssh]`, only SSH traffic will be forwarded
+ *any regex*, only traffic of first recieved packet match with this regex will be forwarded
