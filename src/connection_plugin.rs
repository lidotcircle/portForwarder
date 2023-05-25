use std::net::{SocketAddr, ToSocketAddrs};
use regex::Regex;
use std::collections::HashMap;
use crate::address_matcher::IpAddrMatcher;
use hex;
    

pub trait ConnectionPlugin {
    fn decideTarget(&self, buf: &[u8], addr: SocketAddr) -> Option<SocketAddr>;
    fn transform(&mut self, buf: &[u8]) -> Option<Vec<u8>>;
}

pub struct RegexMultiplexer {
    rules: Vec<(Regex,SocketAddr)>,
    ip_matcher: IpAddrMatcher
}

impl From<(Vec<(String,String)>, Vec<String>)> for RegexMultiplexer {
    fn from(regexPlusAllowed: (Vec<(String,String)>, Vec<String>)) -> Self {
        let proto2regex: HashMap<&str,&str> = vec![
            ("[ssh]", "^SSH-2\\.0-.+"),
            ("[http]", "^(GET|POST|PUT|DELETE|OPTIONS|HEAD|CONNECT|TRACE).*HTTP.*")
        ].into_iter().collect();

        let rules = regexPlusAllowed.0.iter().map(|pair| {
            let exp = match proto2regex.get(&pair.0.as_str()) {
                Some(re) => *re,
                None => &pair.0
            };
            let regex = Regex::new(exp).unwrap();
            let addr = pair.1.to_socket_addrs().unwrap().next().unwrap();
            (regex, addr)
        }).collect();
        let ip_matcher= IpAddrMatcher::from(&regexPlusAllowed.1);
        RegexMultiplexer { rules, ip_matcher }
    }
}

impl ConnectionPlugin for RegexMultiplexer {
    fn decideTarget(&self, buf: &[u8], addr: SocketAddr) -> Option<SocketAddr> {
        if !self.ip_matcher.testipaddr(&addr.ip()) {
            return None;
        }
        let s1 = hex::encode(buf);
        let s2 = String::from_utf8_lossy(buf);
        for rule in &self.rules {
            if rule.0.is_match(&s1) || rule.0.is_match(&s2) {
                return Some(rule.1);
            }
        }
        None
    }

    fn transform(&mut self, _: &[u8]) -> Option<Vec<u8>> {
        None
    }
}
