use std::fs;
use std::net::IpAddr;
use std::path::Path;
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
        let mut args = args.into_iter();
        let mut config_path = None;
        let mut overrides = Vec::new();
        while let Some(arg) = args.next() {
            if matches!(arg.as_str(), "-h" | "--help") {
                println!(
                    "brume {}\n用法：brume [--config 文件] [-p 端口] [-user 用户名 -pwd 密码] [--whitelist IP或CIDR,...] [--tcp-timeout 秒] [--udp-timeout 秒]",
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
            if !matches!(
                key,
                "--config"
                    | "-p"
                    | "--port"
                    | "-user"
                    | "--user"
                    | "-pwd"
                    | "--pwd"
                    | "--whitelist"
                    | "--tcp-timeout"
                    | "--udp-timeout"
            ) {
                return Err(format!("未知参数：{key}"));
            }
            let value = match inline {
                Some(value) => value.to_owned(),
                None => args.next().ok_or_else(|| format!("参数 {key} 缺少值"))?,
            };
            if key == "--config" {
                if value.is_empty() {
                    return Err("配置文件路径不能为空".into());
                }
                if config_path.replace(value).is_some() {
                    return Err("只能指定一个配置文件".into());
                }
            } else {
                overrides.push((key.to_owned(), value));
            }
        }

        let mut config = Self {
            port: 1080,
            username: String::new(),
            password: String::new(),
            whitelist: Whitelist::default(),
            tcp_timeout: None,
            udp_timeout: Duration::from_secs(60),
        };
        if let Some(path) = config_path {
            config.read_file(Path::new(&path))?;
        }
        for (key, value) in overrides {
            config.apply_value(&key, &value)?;
        }
        config.validate()?;
        Ok(Some(config))
    }

    fn read_file(&mut self, path: &Path) -> Result<(), String> {
        let contents = fs::read_to_string(path)
            .map_err(|error| format!("读取配置文件 {} 失败：{error}", path.display()))?;
        for (index, line) in contents.lines().enumerate() {
            let line = line.strip_suffix('\r').unwrap_or(line);
            if line.trim().is_empty() || line.trim_start().starts_with('#') {
                continue;
            }
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| format!("配置文件第 {} 行缺少等号", index + 1))?;
            let option = match key.trim() {
                "port" => "--port",
                "username" => "--user",
                "password" => "--pwd",
                "whitelist" => "--whitelist",
                "tcp_timeout" => "--tcp-timeout",
                "udp_timeout" => "--udp-timeout",
                _ => return Err(format!("配置文件第 {} 行存在未知配置项", index + 1)),
            };
            self.apply_value(option, value)
                .map_err(|error| format!("配置文件第 {} 行：{error}", index + 1))?;
        }
        Ok(())
    }

    fn apply_value(&mut self, key: &str, value: &str) -> Result<(), String> {
        match key {
            "-p" | "--port" => {
                self.port = value
                    .parse()
                    .map_err(|_| "端口必须在 1 到 65535 之间".to_string())?;
                if self.port == 0 {
                    return Err("端口必须在 1 到 65535 之间".into());
                }
            }
            "-user" | "--user" => self.username = value.to_owned(),
            "-pwd" | "--pwd" => self.password = value.to_owned(),
            "--whitelist" => self.whitelist = Whitelist::parse(value)?,
            "--tcp-timeout" => {
                let seconds: u64 = value
                    .parse()
                    .map_err(|_| "TCP 超时必须是非负整数秒".to_string())?;
                self.tcp_timeout = (seconds > 0).then(|| Duration::from_secs(seconds));
            }
            "--udp-timeout" => {
                let seconds: u64 = value
                    .parse()
                    .map_err(|_| "UDP 超时必须是非负整数秒".to_string())?;
                self.udp_timeout = Duration::from_secs(seconds);
            }
            _ => return Err(format!("未知参数：{key}")),
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), String> {
        if self.username.is_empty() != self.password.is_empty() {
            return Err("用户名和密码必须同时设置".into());
        }
        if self.username.len() > 255 || self.password.len() > 255 {
            return Err("用户名和密码不能超过 255 字节".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temporary_config_path() -> std::path::PathBuf {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "brume-config-{}-{}.conf",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ))
    }

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

    #[test]
    fn loads_config_file_and_applies_command_line_overrides() {
        let path = temporary_config_path();
        fs::write(
            &path,
            "# 服务配置\r\nport=1080\r\nusername=admin\r\npassword=pass=#word\r\nwhitelist=127.0.0.1\r\ntcp_timeout=5\r\nudp_timeout=15\r\n",
        )
        .unwrap();
        let config = Config::parse([
            "--port".to_owned(),
            "26547".to_owned(),
            "--config".to_owned(),
            path.to_string_lossy().into_owned(),
            "--udp-timeout=0".to_owned(),
        ])
        .unwrap()
        .unwrap();
        assert_eq!(config.port, 26547);
        assert_eq!(config.username, "admin");
        assert_eq!(config.password, "pass=#word");
        assert!(config.whitelist.allows("127.0.0.1".parse().unwrap()));
        assert_eq!(config.tcp_timeout, Some(Duration::from_secs(5)));
        assert_eq!(config.udp_timeout, Duration::ZERO);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn rejects_invalid_config_files() {
        let path = temporary_config_path();
        let arg = format!("--config={}", path.display());
        assert!(Config::parse([arg.clone()]).is_err());
        fs::write(&path, "password=secret\n").unwrap();
        assert!(Config::parse([arg.clone()]).is_err());
        fs::write(&path, "unknown=value\n").unwrap();
        assert!(Config::parse([arg.clone()]).is_err());
        fs::write(&path, "port 1080\n").unwrap();
        assert!(Config::parse([arg.clone()]).is_err());
        assert!(Config::parse([arg.clone(), arg]).is_err());
        fs::remove_file(path).unwrap();
    }
}
