#![allow(non_snake_case)]
extern crate portforwarder;

use portforwarder::tcp_udp_forwarder::TcpUdpForwarder;
use portforwarder::forward_config::ForwardSessionConfig;
use regex::Regex;
use std::fs;
use std::sync::{Arc, Condvar, Mutex};
use yaml_rust::{YamlLoader, Yaml};


fn usage() {
    let args: Vec<String> = std::env::args().collect();
    println!(
"usage:
    {} [-htu] <bind-address> <forward-address>
    {} -c <yaml-config-file>

    -t    disable tcp
    -u    disable udp
    -s    connection buffer size, default value: 2MB.
          support UNITs: KB MB
    -w    network whitelist, eg. 127.0.0.1/24
    -m    max connections
    -c    config file (a yaml file)
    -e    show an example of config file
    -h    show help", args[0], args[0]);
}

fn print_example_of_config_file() {
    println!(
"forwarders:
  - local: 0.0.0.0:8808
    # specify either 'remoteMap' or 'remote'
    remoteMap:
      - pattern: \"[http:localhost]\"
        remote: 192.168.44.43:5445
      - pattern: \"[https:baidu.com]\"
        remote: \"39.156.66.10:443\"
      - pattern: \"[ssh]\"
        remote: \"192.168.44.43:22\"
      - pattern: \"[socks5]\"
        remote: \"192.168.100.46:7890\"
      - pattern: \"[rdp]\"
        remote: 192.168.100.46:3389
      - pattern: .*
        remote: 192.168.100.46:23
    remote: <remote-address/127.0.0.1:2233>
    enable_tcp: true # default is true
    enable_udp: true # default is true
    conn_bufsize: 2MB
    max_connections: 10000 # optional
    allow_nets: # optional
      - 127.0.0.0/24");
}

pub fn convert_to_bytes(input: &str) -> Option<usize> {
    let units = vec!["B", "KB", "MB", "GB", "TB"];
    let mut num_str = String::new();
    let mut unit_str = String::new();

    for c in input.chars() {
        if c.is_digit(10) {
            num_str.push(c);
        } else {
            unit_str.push(c);
        }
    }

    let num = match num_str.parse::<usize>() {
        Ok(n) => n,
        Err(_) => return None,
    };

    let unit_index = units.iter().position(|&u| u == unit_str.trim().to_uppercase());

    if let Some(index) = unit_index {
        Some(num * (1024 as usize).pow(index as u32))
    } else {
        None
    }
}

pub trait FromYaml: Sized {
    fn run(&self, sync_pair: Arc<(Mutex<bool>, Condvar)>) -> std::thread::JoinHandle<()>;
    fn fromYaml(yaml: &Yaml) -> Result<Self,&'static str>;
}

impl FromYaml for ForwardSessionConfig<String> {
    fn run(&self, sync_pair: Arc<(Mutex<bool>, Condvar)>) -> std::thread::JoinHandle<()> {
        let cp = self.clone();
        std::thread::spawn(move || {
            let forwarder = TcpUdpForwarder::from(&cp).unwrap();
            let close_handler = forwarder.listen();

            let (lock, cvar) = &*sync_pair;
            let mut closed = lock.lock().unwrap();
            while !*closed {
                closed = cvar.wait(closed).unwrap();
            }
            close_handler();
        })
   }

    fn fromYaml(yaml: &Yaml) -> Result<Self,&'static str> {
        let local = match yaml["local"].as_str() {
            Some(s) => String::from(s),
            None => return Err("missing local"),
        };
        let enable_tcp = yaml["enable_tcp"].as_bool().unwrap_or(true);
        let enable_udp = yaml["enable_udp"].as_bool().unwrap_or(true);
        let allow_nets_opt = yaml["allow_nets"].as_vec();
        let mut allow_nets = vec!();
        let conn_bufsize = convert_to_bytes(yaml["conn_bufsize"].as_str().unwrap_or("2MB")).unwrap();

        if let Some(nets) = allow_nets_opt {
            for _net in nets {
                if let Some(_str) = _net.as_str() {
                    allow_nets.push(String::from(_str));
                }
            }
        }

        let max_connections =
            if let Some(mc) = yaml["max_connections"].as_i64() {
                mc
            } else {
                -1
            };

        let mut remoteMap: Vec<(String,String)> = vec![];
        if let Some(pairs) = yaml["remoteMap"].as_vec() {
            for pair in pairs {
                let pattern = pair["pattern"].as_str();
                let remote  = pair["remote"].as_str();
                if pattern.is_some() && remote.is_some() {
                    remoteMap.push((pattern.unwrap().to_string(), remote.unwrap().to_string()));
                }
            }
        }
        match yaml["remote"].as_str() {
            Some(s) => {
                remoteMap.push((".*".to_string(), s.to_string()));
            },
            None => {},
        };

        Ok(Self {local, remoteMap, enable_tcp, enable_udp, allow_nets, max_connections, conn_bufsize})
    }
}

fn main() {
    env_logger::init_from_env(
        env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"));
    let mut bind_addr = None;
    let mut forward_addr = None;
    let mut enable_tcp = true;
    let mut enable_udp = true;
    let mut max_connections = -1;
    let mut args: Vec<String> = std::env::args().collect();
    let mut config_file: Option<String> = None;
    let mut whitelist: Vec<String> = vec![];
    let mut conn_bufsize = 2 * 1024 * 1024;
    args.remove(0);
    let valid_ipv4_port = Regex::new(
        r"^(([0-9]{1,3}.){3}[0-9]{1,3}|0-9]{1}|(([0-9]{1}[a-zA-Z]{1})|([a-zA-Z0-9][a-zA-Z0-9-_]{1,61}[a-zA-Z0-9]))\.([a-zA-Z]{2,6}|[a-zA-Z0-9-]{2,30}\.[a-zA-Z]{2,3})|([a-f0-9:]+:+)+[a-f0-9]+|::|localhost):[0-9]{1,5}$").unwrap();
    let mut skipnext = false;
    for i in 0..args.len() {
        if skipnext {
            skipnext = false;
            continue;
        }

        let s = args[i].as_str();
        match s {
            "-h" => {
                usage();
                std::process::exit(0);
            }
            "-u" => {
                enable_udp = false;
            }
            "-t" => {
                enable_tcp= false;
            }
            "-e" => {
                print_example_of_config_file();
                std::process::exit(0);
            }
            "-s" => {
                if i + 1 < args.len() {
                    conn_bufsize = convert_to_bytes(args[i+1].as_str()).unwrap();
                    skipnext = true;
                } else {
                    usage();
                    std::process::exit(1);
                }
            }
            "-w" => {
                if i + 1 < args.len() {
                    whitelist = args[i+1].split(",").map(|s| String::from(s)).collect();
                    skipnext = true;
                } else {
                    usage();
                    std::process::exit(1);
                }
            },
            "-m" => {
                if i + 1 < args.len() {
                    max_connections = args[i+1].parse().unwrap();
                    skipnext = true;
                } else {
                    usage();
                    std::process::exit(1);
                }
            },
            "-c" => {
                if i + 1 < args.len() {
                    config_file = Some(args[i+1].clone());
                    break;
                } else {
                    usage();
                    std::process::exit(1);
                }
            }
            _ => {
                if valid_ipv4_port.is_match(s) {
                    if bind_addr.is_none() {
                        bind_addr = Some(String::from(s));
                    } else if forward_addr.is_none() {
                        forward_addr = Some(String::from(s));
                    } else {
                        usage();
                        std::process::exit(1);
                    }
                } else {
                    usage();
                    std::process::exit(1);
                }
            }
        }
    }

    let mut forwarder_configs = vec![];
    if config_file.is_some() {
        let file = &config_file.unwrap();
        if let Ok(file_content) = fs::read_to_string(file) {
            if let Ok(config) = YamlLoader::load_from_str(file_content.as_str()) {
                if config.is_empty() {
                    println!("do nothing");
                    std::process::exit(0);
                }
                let config = &config[0];

                let forwarders = &config["forwarders"];
                if !forwarders.is_array() {
                    println!("invalid config file, expect an array but get {:?}", forwarders);
                    std::process::exit(1);
                }

                let fflist = forwarders.as_vec().unwrap();
                for ff in fflist {
                    match ForwardSessionConfig::fromYaml(ff) {
                        Ok(c) => forwarder_configs.push(c),
                        Err(e) => {
                            println!("invalid config file: {e}\n{:?}", ff);
                            std::process::exit(1);
                        }
                    }
                }
            } else {
                println!("invalid config file, should be a valid yaml file");
                std::process::exit(1);
            }
        } else {
            println!("open file {} failed", file);
            std::process::exit(1);
        }
    } else {
        if bind_addr.is_none() || forward_addr.is_none() {
            usage();
            std::process::exit(1);
        }

        let mut remoteMap: Vec<(String,String)> = vec![];
        remoteMap.push((".*".to_string(), forward_addr.unwrap()));
        forwarder_configs.push(
            ForwardSessionConfig {
                local: bind_addr.unwrap(),
                remoteMap,
                enable_tcp,
                enable_udp,
                allow_nets: whitelist,
                max_connections,
                conn_bufsize,
            }
        );
    }

    let mut handlers = vec![];
    let sync_pair = Arc::new((Mutex::new(false), Condvar::new()));
    let pair2 = sync_pair.clone();
    ctrlc::set_handler(move || {
        println!("ctrl-c pressed, exiting ...");
        let (lock, cvar) = &*pair2;
        let mut close = lock.lock().unwrap();
        *close = true;
        cvar.notify_all();
    }).unwrap();

    for cc in forwarder_configs {
        handlers.push(cc.run(sync_pair.clone()));
    }

    for h in handlers {
        h.join().unwrap();
    }
}
