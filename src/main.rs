#![allow(non_snake_case)]

mod tcp_forwarder;
mod udp_forwarder;
mod tcp_udp_forwarder;
mod utils;
mod address_matcher;
mod forward_config;
use forward_config::ForwardSessionConfig;
use tcp_udp_forwarder::*;
use regex::Regex;
use std::fs;
use yaml_rust::{YamlLoader, Yaml};


fn usage() {
    let args: Vec<String> = std::env::args().collect();
    println!(
"usage:
    {} [-htu] <bind-address> <forward-address>
    {} -c <yaml-config-file>

    -t    disable tcp
    -u    disable udp
    -w    network whitelist, eg. 127.0.0.1/24
    -m    max connections
    -c    config file (a yaml file)
    -e    show an example of config file
    -h    show help", args[0], args[0]);
}

fn print_example_of_config_file() {
    println!(
"forwarders:
  - local: <bind-address/0.0.0.0:1234>
    remote: <remote-address/127.0.0.1:2233>
    enable_tcp: true # default is true
    enable_udp: true # default is true
    max_connections: 10000 # optional
    allow_nets: # optional
      - 127.0.0.0/24");
}

impl ForwardSessionConfig<String> {
    fn run(&self) -> std::thread::JoinHandle<()> {
        let cp = self.clone();
        std::thread::spawn(move || {
            let forwarder = TcpUdpForwarder::from(&cp).unwrap();
            forwarder.listen();
        })
   }

    fn from(yaml: &Yaml) -> Result<Self,&'static str> {
        let local = match yaml["local"].as_str() {
            Some(s) => String::from(s),
            None => return Err("missing local"),
        };
        let remote = match yaml["remote"].as_str() {
            Some(s) => String::from(s),
            None => return Err("missing remote"),
        };
        let enable_tcp = yaml["enable_tcp"].as_bool().unwrap_or(true);
        let enable_udp = yaml["enable_udp"].as_bool().unwrap_or(true);
        let allow_nets_opt = yaml["allow_nets"].as_vec();
        let mut allow_nets = vec!();

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

        Ok(Self {local, remote, enable_tcp, enable_udp, allow_nets, max_connections})
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
                    match ForwardSessionConfig::from(ff) {
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

        forwarder_configs.push(
            ForwardSessionConfig {
                local: bind_addr.unwrap(),
                remote: forward_addr.unwrap(),
                enable_tcp,
                enable_udp,
                allow_nets: whitelist,
                max_connections,
            }
        );
    }

    let mut handlers = vec![];
    for cc in forwarder_configs {
        handlers.push(cc.run());
    }

    for h in handlers {
        h.join().unwrap();
    }
}
