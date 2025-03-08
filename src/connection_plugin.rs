use crate::address_matcher::IpAddrMatcher;
use hex;
use regex::Regex;
use std::collections::HashMap;
use std::net::{SocketAddr, ToSocketAddrs};

pub trait ConnectionPlugin {
    fn onlySingleTarget(&self) -> Option<SocketAddr>;
    fn decideTarget(&self, buf: &[u8], addr: SocketAddr) -> Option<SocketAddr>;
    fn testipaddr(&self, addr: &SocketAddr) -> bool;
    fn transform(&mut self, buf: &[u8]) -> Option<Vec<u8>>;
}

pub struct RegexMultiplexer {
    utarget: Option<SocketAddr>,
    rules: Vec<(Box<dyn Fn(&[u8]) -> bool + Send + Sync>, SocketAddr)>,
    ip_matcher: IpAddrMatcher,
}

fn build_kmp_table(pattern: &[u8]) -> Vec<usize> {
    let mut table = vec![0; pattern.len()];
    let mut i = 1;
    let mut j = 0;

    while i < pattern.len() {
        if pattern[i] == pattern[j] {
            j += 1;
            table[i] = j;
            i += 1;
        } else {
            if j != 0 {
                j = table[j - 1];
            } else {
                table[i] = 0;
                i += 1;
            }
        }
    }

    table
}

fn kmp_search(text: &[u8], pattern: &[u8]) -> Option<usize> {
    if pattern.is_empty() {
        return Some(0);
    }

    let table = build_kmp_table(pattern);
    let mut i = 0;
    let mut j = 0;

    while i < text.len() {
        if text[i] == pattern[j] {
            i += 1;
            j += 1;

            if j == pattern.len() {
                return Some(i - j);
            }
        } else {
            if j != 0 {
                j = table[j - 1];
            } else {
                i += 1;
            }
        }
    }

    None
}

impl From<(Vec<(String, String)>, Vec<String>)> for RegexMultiplexer {
    fn from(regexPlusAllowed: (Vec<(String, String)>, Vec<String>)) -> Self {
        let proto2regex: HashMap<&str, &str> = vec![
            ("[ssh]", "^SSH-2\\.0-.+"),
            (
                "[http]",
                "^(GET|POST|PUT|DELETE|OPTIONS|HEAD|CONNECT|TRACE).*HTTP.*",
            ),
        ]
        .into_iter()
        .collect();

        let rules = regexPlusAllowed
            .0
            .iter()
            .map(|pair| {
                let gexp = match proto2regex.get(&pair.0.as_str()) {
                    Some(re) => *re,
                    None => &pair.0,
                };

                let addr = pair.1.to_socket_addrs().unwrap().next().unwrap();
                if gexp == "[socks5]" {
                    let func: Box<dyn Fn(&[u8]) -> bool + Send + Sync> =
                        Box::new(|buf: &[u8]| {
                            if buf.len() < 3
                                || buf[0] != 0x05
                                || buf.len() != usize::from(buf[1]) + 2
                            {
                                false
                            } else {
                                let mut is_valid = true;
                                for octet_ in &buf[2..] {
                                    let octet = *octet_;
                                    if octet != 0
                                        && octet != 1
                                        && octet != 2
                                        && octet != 3
                                        && octet != 0x80
                                        && octet != 0xFF
                                    {
                                        is_valid = false;
                                        break;
                                    }
                                }
                                is_valid
                            }
                        });
                    return (func, addr);
                } else if gexp == "[rdp]" {
                    let func: Box<dyn Fn(&[u8]) -> bool + Send + Sync> =
                        Box::new(|buf: &[u8]| {
                            if buf.len() < 11 || buf[0] != 0x03 {
                                return false;
                            }

                            let length = u16::from_be_bytes([buf[2], buf[3]]) as usize;
                            if length != buf.len() {
                                return false;
                            }

                            if (buf[4] as usize + 5) != buf.len() {
                                return false;
                            }

                            // connection request
                            if buf[5] & 0xE0 != 0xE0 {
                                return false;
                            }

                            // DST-REF
                            if buf[6] != 0 || buf[7] != 0 {
                                return false;
                            }

                            true
                        });
                    return (func, addr);
                } else if gexp.starts_with("[https:") && gexp.ends_with("]") {
                    let domain_name = gexp[7..gexp.len() - 1].to_string();
                    let func: Box<dyn Fn(&[u8]) -> bool + Send + Sync> =
                        Box::new(move |buf: &[u8]| {
                            if buf.len() < 3 + domain_name.len() {
                                return false;
                            }
                            if buf[0] != 0x16 && buf[1] != 0x03 && buf[2] != 0x01 {
                                return false;
                            }
                            return kmp_search(buf, domain_name.as_bytes()).is_some();
                        });
                    (func, addr)
                } else {
                    let exp = if gexp.starts_with("[http:") && gexp.ends_with("]") {
                        let domain_name = &gexp[6..gexp.len() - 1];
                        "^(GET|POST|PUT|DELETE|OPTIONS|HEAD|CONNECT|TRACE).*HTTP.*(.\r\n.*)*"
                            .to_string()
                            + domain_name
                    } else {
                        gexp.to_string()
                    };

                    let regex = Regex::new(&exp).unwrap();
                    let func: Box<dyn Fn(&[u8]) -> bool + Send + Sync> =
                        Box::new(move |buf: &[u8]| {
                            let s1 = hex::encode(buf);
                            if regex.is_match(&s1) {
                                return true;
                            }
                            let s2 = String::from_utf8_lossy(buf);
                            if regex.is_match(&s2) {
                                return true;
                            }

                            return false;
                        });
                    return (func, addr);
                }
            })
            .collect();
        let ip_matcher = IpAddrMatcher::from(&regexPlusAllowed.1);
        let utarget =
            if regexPlusAllowed.0.len() == 1 && regexPlusAllowed.0.get(0).unwrap().0 == ".*" {
                Some(
                    regexPlusAllowed
                        .0
                        .get(0)
                        .unwrap()
                        .1
                        .to_socket_addrs()
                        .unwrap()
                        .next()
                        .unwrap(),
                )
            } else {
                None
            };
        RegexMultiplexer {
            utarget,
            rules,
            ip_matcher,
        }
    }
}

impl ConnectionPlugin for RegexMultiplexer {
    fn onlySingleTarget(&self) -> Option<SocketAddr> {
        return self.utarget;
    }

    fn decideTarget(&self, buf: &[u8], _addr: SocketAddr) -> Option<SocketAddr> {
        for rule in &self.rules {
            if rule.0(&buf) {
                return Some(rule.1);
            }
        }
        None
    }

    fn testipaddr(&self, addr: &SocketAddr) -> bool {
        self.ip_matcher.testipaddr(&addr.ip())
    }

    fn transform(&mut self, _: &[u8]) -> Option<Vec<u8>> {
        None
    }
}
