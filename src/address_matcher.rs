extern crate ipnet;
use ipnet::{IpNet, Ipv4Net, Ipv6Net};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::str::FromStr;

#[derive(Clone)]
pub struct IpAddrMatcher {
    nets: Vec<IpNet>,
}

impl IpAddrMatcher {
    pub fn from(addr_vec: &Vec<String>) -> IpAddrMatcher {
        let mut nets = vec![];
        for addr in addr_vec {
            if let Ok(ipv4) = Ipv4Net::from_str(addr.as_str()) {
                nets.push(IpNet::from(ipv4));
            }
            if let Ok(ipv6) = Ipv6Net::from_str(addr.as_str()) {
                nets.push(IpNet::from(ipv6));
            }
        }
        IpAddrMatcher { nets }
    }

    pub fn testipaddr(self: &Self, ipaddr: &IpAddr) -> bool {
        if self.nets.is_empty() {
            return true;
        }

        for net in &self.nets {
            match net {
                IpNet::V4(n4) => {
                    if let IpAddr::V4(v4) = ipaddr {
                        if n4.contains(v4) {
                            return true;
                        }
                    }
                }
                IpNet::V6(n6) => {
                    if let IpAddr::V6(v6) = ipaddr {
                        if n6.contains(v6) {
                            return true;
                        }
                    }
                }
            }
        }

        false
    }

    #[allow(dead_code)]
    pub fn testaddr(self: &Self, addr: &str) -> bool {
        let addr4 = Ipv4Addr::from_str(addr);
        let addr6 = Ipv6Addr::from_str(addr);
        let ipaddr_opt = {
            if let Ok(ip4) = addr4 {
                Some(IpAddr::from(ip4))
            } else {
                if let Ok(ip6) = addr6 {
                    Some(IpAddr::from(ip6))
                } else {
                    None
                }
            }
        };
        if ipaddr_opt.is_none() {
            return false;
        }

        self.testipaddr(&ipaddr_opt.unwrap())
    }
}
