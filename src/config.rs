use std::net::IpAddr;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Config {
    pub port: u16,
    pub username: String,
    pub password: String,
    pub whitelist: Whitelist,
    pub tcp_timeout: Option<Duration>,
    pub udp_timeout: Duration,
}

#[derive(Clone, Debug, Default)]
pub struct Whitelist {
    addresses: Vec<IpAddr>,
    networks: Vec<(IpAddr, u8)>,
}

impl Whitelist {
    pub fn parse(value: &str) -> Result<Self, String> {
        let mut result = Self::default();
        for entry in value
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
        {
            if let Some((address, bits)) = entry.split_once('/') {
                let ip: IpAddr = address
                    .parse()
                    .map_err(|_| format!("无效的白名单地址：{entry}"))?;
                let bits: u8 = bits
                    .parse()
                    .map_err(|_| format!("无效的白名单网段：{entry}"))?;
                if bits > if ip.is_ipv4() { 32 } else { 128 } {
                    return Err(format!("无效的白名单网段：{entry}"));
                }
                let (ip, bits) = match ip {
                    IpAddr::V6(ip6) if ip6.to_ipv4_mapped().is_some() && bits >= 96 => {
                        (IpAddr::V4(ip6.to_ipv4_mapped().unwrap()), bits - 96)
                    }
                    _ => (ip, bits),
                };
                result.networks.push((ip, bits));
            } else {
                let ip = entry
                    .parse()
                    .map_err(|_| format!("无效的白名单地址：{entry}"))?;
                result.addresses.push(normalize(ip));
            }
        }
        if !value.trim().is_empty() && result.is_empty() {
            return Err("白名单至少需要一个有效的 IP 或网段".into());
        }
        Ok(result)
    }

    pub fn allows(&self, address: IpAddr) -> bool {
        if self.addresses.is_empty() && self.networks.is_empty() {
            return true;
        }
        let normalized = normalize(address);
        self.addresses.contains(&normalized)
            || self.networks.iter().any(|(network, bits)| {
                prefix_matches(*network, normalized, *bits)
                    || prefix_matches(*network, address, *bits)
            })
    }

    pub fn is_empty(&self) -> bool {
        self.addresses.is_empty() && self.networks.is_empty()
    }
}

fn normalize(address: IpAddr) -> IpAddr {
    match address {
        IpAddr::V6(ip) => ip
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(ip)),
        other => other,
    }
}

fn prefix_matches(network: IpAddr, address: IpAddr, bits: u8) -> bool {
    match (network, address) {
        (IpAddr::V4(a), IpAddr::V4(b)) => {
            let mask = u32::MAX.checked_shl(32 - u32::from(bits)).unwrap_or(0);
            u32::from(a) & mask == u32::from(b) & mask
        }
        (IpAddr::V6(a), IpAddr::V6(b)) => {
            let mask = u128::MAX.checked_shl(128 - u32::from(bits)).unwrap_or(0);
            u128::from(a) & mask == u128::from(b) & mask
        }
        _ => false,
    }
}

impl Config {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Option<Self>, String> {
        let mut config = Self {
            port: 1080,
            username: String::new(),
            password: String::new(),
            whitelist: Whitelist::default(),
            tcp_timeout: None,
            udp_timeout: Duration::from_secs(60),
        };
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            if matches!(arg.as_str(), "-h" | "--help") {
                println!(
                    "brume {}\n用法：brume [-p 端口] [-user 用户名 -pwd 密码] [--whitelist IP或CIDR,...] [--tcp-timeout 秒] [--udp-timeout 秒]",
                    env!("CARGO_PKG_VERSION")
                );
                return Ok(None);
            }
            if arg == "--version" {
                println!("brume {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            let (key, inline) = arg
                .split_once('=')
                .map_or((arg.as_str(), None), |(key, value)| (key, Some(value)));
            let value = match inline {
                Some(value) => value.to_owned(),
                None => args.next().ok_or_else(|| format!("参数 {key} 缺少值"))?,
            };
            match key {
                "-p" | "--port" => {
                    config.port = value
                        .parse()
                        .map_err(|_| "端口必须在 1 到 65535 之间".to_string())?;
                    if config.port == 0 {
                        return Err("端口必须在 1 到 65535 之间".into());
                    }
                }
                "-user" | "--user" => config.username = value,
                "-pwd" | "--pwd" => config.password = value,
                "--whitelist" => config.whitelist = Whitelist::parse(&value)?,
                "--tcp-timeout" => {
                    let seconds: u64 = value
                        .parse()
                        .map_err(|_| "TCP 超时必须是非负整数秒".to_string())?;
                    config.tcp_timeout = (seconds > 0).then(|| Duration::from_secs(seconds));
                }
                "--udp-timeout" => {
                    let seconds: u64 = value
                        .parse()
                        .map_err(|_| "UDP 超时必须是非负整数秒".to_string())?;
                    config.udp_timeout = Duration::from_secs(seconds);
                }
                _ => return Err(format!("未知参数：{key}")),
            }
        }
        if config.username.is_empty() != config.password.is_empty() {
            return Err("用户名和密码必须同时设置".into());
        }
        if config.username.len() > 255 || config.password.len() > 255 {
            return Err("用户名和密码不能超过 255 字节".into());
        }
        Ok(Some(config))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn whitelist_matches_addresses_and_networks() {
        let list = Whitelist::parse("198.51.100.10,192.0.2.0/24,2001:db8::/32").unwrap();
        for address in [
            "198.51.100.10",
            "192.0.2.20",
            "2001:db8::20",
            "::ffff:192.0.2.20",
        ] {
            assert!(list.allows(address.parse().unwrap()), "{address}");
        }
        assert!(!list.allows("203.0.113.10".parse().unwrap()));
        assert!(Whitelist::parse("bad-ip").is_err());
        assert!(Whitelist::parse("192.0.2.0/33").is_err());
        assert!(Whitelist::parse(",,,").is_err());
        let mapped = Whitelist::parse("::ffff:192.0.2.0/120").unwrap();
        assert!(mapped.allows("192.0.2.20".parse().unwrap()));
    }

    #[test]
    fn zero_bit_prefix_matches_entire_family() {
        assert!(prefix_matches(
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            "203.0.113.1".parse().unwrap(),
            0
        ));
        assert!(prefix_matches(
            IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            "2001:db8::1".parse().unwrap(),
            0
        ));
    }

    #[test]
    fn parses_legacy_arguments() {
        let config = Config::parse(
            [
                "-p",
                "8080",
                "-user",
                "admin",
                "-pwd",
                "secret",
                "--whitelist",
                "127.0.0.1",
            ]
            .map(str::to_owned),
        )
        .unwrap()
        .unwrap();
        assert_eq!(config.port, 8080);
        assert!(!config.whitelist.is_empty());
        assert!(Config::parse(["-user", "admin"].map(str::to_owned)).is_err());
    }
}
