use std::fs;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct Config {
    pub port: u16,
    pub username: String,
    pub password: String,
    pub whitelist: Whitelist,
    pub tcp_timeout: Option<Duration>,
    pub udp_timeout: Duration,
    pub dns_servers: Vec<SocketAddr>,
}

#[derive(Debug)]
pub enum Command {
    Serve(Config),
    CheckConfig(Config),
}

#[derive(Clone, Debug, Default)]
pub struct Whitelist {
    ipv4: Vec<(u32, u32)>,
    ipv6: Vec<(u128, u128)>,
}

fn merge_ranges<T: Copy + Ord>(
    mut ranges: Vec<(T, T)>,
    successor: impl Fn(T) -> Option<T>,
) -> Vec<(T, T)> {
    ranges.sort_unstable();
    let mut merged = 0;
    for position in 0..ranges.len() {
        let (start, end) = ranges[position];
        if let Some(previous) = ranges[..merged].last_mut()
            && (start <= previous.1 || successor(previous.1) == Some(start))
        {
            previous.1 = previous.1.max(end);
        } else {
            ranges[merged] = (start, end);
            merged += 1;
        }
    }
    ranges.truncate(merged);
    ranges.shrink_to_fit();
    ranges
}

#[inline]
fn range_contains<T: Copy + Ord>(ranges: &[(T, T)], address: T) -> bool {
    // 少量范围直接比较，避免小名单承担二分查找的循环开销。
    if ranges.len() <= 4 {
        return ranges
            .iter()
            .any(|&(start, end)| start <= address && address <= end);
    }
    let position = ranges.partition_point(|range| range.0 <= address);
    position > 0 && address <= ranges[position - 1].1
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
                    .map_err(|_| format!("Invalid allowlist address: {entry}"))?;
                let bits: u8 = bits
                    .parse()
                    .map_err(|_| format!("Invalid allowlist network: {entry}"))?;
                if bits > if ip.is_ipv4() { 32 } else { 128 } {
                    return Err(format!("Invalid allowlist network: {entry}"));
                }
                let (ip, bits) = match ip {
                    IpAddr::V6(ip6) if ip6.to_ipv4_mapped().is_some() && bits >= 96 => {
                        (IpAddr::V4(ip6.to_ipv4_mapped().unwrap()), bits - 96)
                    }
                    _ => (ip, bits),
                };
                result.push(ip, bits);
            } else {
                let ip = entry
                    .parse()
                    .map_err(|_| format!("Invalid allowlist address: {entry}"))?;
                let ip = normalize(ip);
                result.push(ip, if ip.is_ipv4() { 32 } else { 128 });
            }
        }
        if !value.trim().is_empty() && result.is_empty() {
            return Err("The allowlist must contain a valid IP address or network".into());
        }
        // 配置加载时合并重叠和相邻范围，连接检查只检索对应地址族的范围。
        result.ipv4 = merge_ranges(result.ipv4, |value| value.checked_add(1));
        result.ipv6 = merge_ranges(result.ipv6, |value| value.checked_add(1));
        Ok(result)
    }

    fn push(&mut self, address: IpAddr, bits: u8) {
        match address {
            IpAddr::V4(address) => {
                let mask = u32::MAX.checked_shl(32 - u32::from(bits)).unwrap_or(0);
                let start = u32::from(address) & mask;
                self.ipv4.push((start, start | !mask));
            }
            IpAddr::V6(address) => {
                let mask = u128::MAX.checked_shl(128 - u32::from(bits)).unwrap_or(0);
                let start = u128::from(address) & mask;
                self.ipv6.push((start, start | !mask));
            }
        }
    }

    #[inline]
    pub fn allows(&self, address: IpAddr) -> bool {
        if self.is_empty() {
            return true;
        }
        match address {
            IpAddr::V4(address) => range_contains(&self.ipv4, u32::from(address)),
            IpAddr::V6(address) => {
                // 原始 IPv6 仍参与匹配，保留宽网段对映射地址的兼容行为。
                address
                    .to_ipv4_mapped()
                    .is_some_and(|address| range_contains(&self.ipv4, u32::from(address)))
                    || range_contains(&self.ipv6, u128::from(address))
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.ipv4.is_empty() && self.ipv6.is_empty()
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

#[cfg(test)]
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
        Self::parse_command(args).map(|command| {
            command.map(|command| match command {
                Command::Serve(config) | Command::CheckConfig(config) => config,
            })
        })
    }

    pub fn parse_command(
        args: impl IntoIterator<Item = String>,
    ) -> Result<Option<Command>, String> {
        let mut args = args.into_iter();
        let mut config_path = None;
        let mut overrides = Vec::new();
        let mut check_config = false;
        while let Some(arg) = args.next() {
            if matches!(arg.as_str(), "-h" | "--help") {
                println!(
                    "brume {}\n用法：brume [--check-config] [--config 文件] [-p 端口] [-user 用户名 -pwd 密码] [--whitelist IP或CIDR,...] [--tcp-timeout 秒] [--udp-timeout 秒] [--dns-servers IP,IP]\n--check-config：仅校验完整配置后退出，不监听端口",
                    env!("CARGO_PKG_VERSION")
                );
                return Ok(None);
            }
            if arg == "--version" {
                println!("brume {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            if arg == "--check-config" {
                check_config = true;
                continue;
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
                    | "--dns-servers"
            ) {
                return Err(format!("Unknown argument: {key}"));
            }
            let value = match inline {
                Some(value) => value.to_owned(),
                None => args
                    .next()
                    .ok_or_else(|| format!("Missing value for {key}"))?,
            };
            if key == "--config" {
                if value.is_empty() {
                    return Err("The config file path must not be empty".into());
                }
                if config_path.replace(value).is_some() {
                    return Err("Only one config file may be specified".into());
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
            dns_servers: vec!["8.8.8.8:53".parse().unwrap(), "1.1.1.1:53".parse().unwrap()],
        };
        if let Some(path) = config_path {
            config.read_file(Path::new(&path))?;
        }
        for (key, value) in overrides {
            config.apply_value(&key, &value)?;
        }
        config.validate()?;
        Ok(Some(if check_config {
            Command::CheckConfig(config)
        } else {
            Command::Serve(config)
        }))
    }

    fn read_file(&mut self, path: &Path) -> Result<(), String> {
        let contents = fs::read_to_string(path)
            .map_err(|error| format!("Could not read config file {}: {error}", path.display()))?;
        for (index, line) in contents.lines().enumerate() {
            let line = line.strip_suffix('\r').unwrap_or(line);
            if line.trim().is_empty() || line.trim_start().starts_with('#') {
                continue;
            }
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| format!("Missing '=' on config line {}", index + 1))?;
            let option = match key.trim() {
                "port" => "--port",
                "username" => "--user",
                "password" => "--pwd",
                "whitelist" => "--whitelist",
                "tcp_timeout" => "--tcp-timeout",
                "udp_timeout" => "--udp-timeout",
                "dns_servers" => "--dns-servers",
                _ => return Err(format!("Unknown setting on config line {}", index + 1)),
            };
            self.apply_value(option, value)
                .map_err(|error| format!("Config line {}: {error}", index + 1))?;
        }
        Ok(())
    }

    fn apply_value(&mut self, key: &str, value: &str) -> Result<(), String> {
        match key {
            "-p" | "--port" => {
                self.port = value
                    .parse()
                    .map_err(|_| "Port must be between 1 and 65535".to_string())?;
                if self.port == 0 {
                    return Err("Port must be between 1 and 65535".into());
                }
            }
            "-user" | "--user" => self.username = value.to_owned(),
            "-pwd" | "--pwd" => self.password = value.to_owned(),
            "--whitelist" => self.whitelist = Whitelist::parse(value)?,
            "--tcp-timeout" => {
                let seconds: u64 = value
                    .parse()
                    .map_err(|_| "TCP timeout must be whole seconds (0 or more)".to_string())?;
                self.tcp_timeout = (seconds > 0).then(|| Duration::from_secs(seconds));
            }
            "--udp-timeout" => {
                let seconds: u64 = value
                    .parse()
                    .map_err(|_| "UDP timeout must be whole seconds (0 or more)".to_string())?;
                self.udp_timeout = Duration::from_secs(seconds);
            }
            "--dns-servers" => {
                let servers: Result<Vec<SocketAddr>, _> = value
                    .split(',')
                    .map(|entry| {
                        let entry = entry.trim();
                        entry
                            .parse::<SocketAddr>()
                            .ok()
                            .or_else(|| {
                                entry
                                    .parse::<IpAddr>()
                                    .ok()
                                    .map(|ip| SocketAddr::new(ip, 53))
                            })
                            .ok_or(())
                    })
                    .collect();
                let servers = servers
                    .map_err(|_| "Expected comma-separated DNS IPs or IP:port pairs".to_string())?;
                if servers.is_empty()
                    || servers.len() > 8
                    || servers
                        .iter()
                        .any(|server| server.ip().is_unspecified() || server.port() == 0)
                {
                    return Err("Specify 1 to 8 DNS servers with valid addresses and ports".into());
                }
                self.dns_servers = servers;
            }
            _ => return Err(format!("Unknown argument: {key}")),
        }
        Ok(())
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.username.is_empty() != self.password.is_empty() {
            return Err("Username and password must be set together".into());
        }
        if self.username.len() > 255 || self.password.len() > 255 {
            return Err("Username and password must each be at most 255 bytes".into());
        }
        let now = Instant::now();
        for (name, timeout) in [("TCP", self.tcp_timeout), ("UDP", Some(self.udp_timeout))] {
            if timeout.is_some_and(|timeout| now.checked_add(timeout).is_none()) {
                return Err(format!("{name} timeout exceeds the system clock range"));
            }
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

    fn assert_whitelist_matches_linear(value: &str, addresses: impl IntoIterator<Item = IpAddr>) {
        let whitelist = Whitelist::parse(value).unwrap();
        // 保留原逐项匹配算法作为参照，不依赖范围构建和二分查找实现。
        let rules = value
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(|entry| {
                if let Some((address, bits)) = entry.split_once('/') {
                    let address: IpAddr = address.parse().unwrap();
                    let bits: u8 = bits.parse().unwrap();
                    match address {
                        IpAddr::V6(address) if bits >= 96 && address.to_ipv4_mapped().is_some() => {
                            (IpAddr::V4(address.to_ipv4_mapped().unwrap()), bits - 96)
                        }
                        _ => (address, bits),
                    }
                } else {
                    let address = normalize(entry.parse().unwrap());
                    (address, if address.is_ipv4() { 32 } else { 128 })
                }
            })
            .collect::<Vec<_>>();
        for address in addresses {
            let normalized = normalize(address);
            let expected = rules.is_empty()
                || rules.iter().any(|(network, bits)| {
                    prefix_matches(*network, normalized, *bits)
                        || prefix_matches(*network, address, *bits)
                });
            assert_eq!(whitelist.allows(address), expected, "{value}: {address}");
        }
    }

    #[test]
    fn whitelist_merges_ranges_without_allowing_gaps() {
        let whitelist = Whitelist::parse(
            "192.0.2.127/25,192.0.2.128/25,192.0.2.64/26,192.0.3.0/24,192.0.5.0/24,2001:db8::1/127,2001:db8::2/127,2001:db8::8/127",
        )
        .unwrap();
        assert_eq!(whitelist.ipv4.len(), 2);
        assert_eq!(whitelist.ipv6.len(), 2);
        for address in ["192.0.2.0", "192.0.3.255", "2001:db8::", "2001:db8::3"] {
            assert!(whitelist.allows(address.parse().unwrap()), "{address}");
        }
        for address in ["192.0.1.255", "192.0.4.0", "2001:db8::4", "2001:db8::7"] {
            assert!(!whitelist.allows(address.parse().unwrap()), "{address}");
        }
        let maximum = Whitelist::parse(
            "255.255.255.255/32,255.255.255.254,255.255.255.255,ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff/128,ffff:ffff:ffff:ffff:ffff:ffff:ffff:fffe,ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
        )
        .unwrap();
        assert_eq!(maximum.ipv4, vec![(u32::MAX - 1, u32::MAX)]);
        assert_eq!(maximum.ipv6, vec![(u128::MAX - 1, u128::MAX)]);
    }

    #[test]
    fn whitelist_preserves_empty_and_address_family_semantics() {
        let addresses = [
            "0.0.0.0",
            "192.0.2.1",
            "255.255.255.255",
            "::",
            "::ffff:192.0.2.1",
            "2001:db8::1",
            "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
        ]
        .map(|address| address.parse().unwrap());
        for value in [
            "",
            "   ",
            "0.0.0.0/0",
            "::/0",
            "::ffff:192.0.2.1/80",
            "::ffff:192.0.2.1/96",
            "::ffff:192.0.2.1/128",
        ] {
            assert_whitelist_matches_linear(value, addresses);
        }
        assert!(Whitelist::default().allows("203.0.113.1".parse().unwrap()));
        assert!(Whitelist::parse(" \t ").unwrap().is_empty());
        assert!(!Whitelist::parse("0.0.0.0/0").unwrap().is_empty());
        assert!(
            !Whitelist::parse("::/0")
                .unwrap()
                .allows("192.0.2.1".parse().unwrap())
        );
        assert!(
            Whitelist::parse("::/0")
                .unwrap()
                .allows("::ffff:192.0.2.1".parse().unwrap())
        );
    }

    #[test]
    fn whitelist_all_prefix_lengths_match_linear_boundaries() {
        let ipv4 = u32::from("192.0.2.129".parse::<Ipv4Addr>().unwrap());
        for bits in 0..=32 {
            let mask = u32::MAX.checked_shl(32 - bits).unwrap_or(0);
            let start = ipv4 & mask;
            let end = start | !mask;
            let addresses = [start, end, start.wrapping_sub(1), end.wrapping_add(1)]
                .into_iter()
                .flat_map(|address| {
                    let address = Ipv4Addr::from(address);
                    [IpAddr::V4(address), IpAddr::V6(address.to_ipv6_mapped())]
                });
            assert_whitelist_matches_linear(&format!("192.0.2.129/{bits}"), addresses);
        }
        for source in [
            "2001:db8:1234:5678:9abc:def0:1234:5678",
            "::ffff:192.0.2.129",
        ] {
            let ipv6 = u128::from(source.parse::<Ipv6Addr>().unwrap());
            for bits in 0..=128 {
                let mask = u128::MAX.checked_shl(128 - bits).unwrap_or(0);
                let start = ipv6 & mask;
                let end = start | !mask;
                let mut addresses = vec![
                    IpAddr::V6(Ipv6Addr::from(start)),
                    IpAddr::V6(Ipv6Addr::from(end)),
                    IpAddr::V6(Ipv6Addr::from(start.wrapping_sub(1))),
                    IpAddr::V6(Ipv6Addr::from(end.wrapping_add(1))),
                    IpAddr::V4(Ipv4Addr::new(192, 0, 2, 129)),
                ];
                addresses.push(source.parse().unwrap());
                assert_whitelist_matches_linear(&format!("{source}/{bits}"), addresses);
            }
        }
    }

    #[test]
    fn large_mixed_whitelist_matches_deterministic_linear_reference() {
        let mut entries = vec![
            "::ffff:192.0.2.0/120".to_owned(),
            "198.51.100.20".to_owned(),
        ];
        let mut addresses = Vec::new();
        for index in 0..512 {
            entries.push(format!("10.{}.{}.129/25", index / 256, index % 256));
            entries.push(format!("2001:db8:{index:x}::3/126"));
            addresses.push(
                format!("10.{}.{}.255", index / 256, index % 256)
                    .parse()
                    .unwrap(),
            );
            addresses.push(format!("2001:db8:{index:x}::3").parse().unwrap());
        }
        let mut state = 0x6a09_e667_f3bc_c908_u128;
        for _ in 0..1024 {
            state = state
                .wrapping_mul(0x2360_ed05_1fc6_5da4_4385_df64_9fcc_f645)
                .wrapping_add(1);
            let ipv4 = Ipv4Addr::from(state as u32);
            addresses.extend([
                IpAddr::V4(ipv4),
                IpAddr::V6(ipv4.to_ipv6_mapped()),
                IpAddr::V6(Ipv6Addr::from(state)),
            ]);
        }
        assert_whitelist_matches_linear(&entries.join(","), addresses);
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
    fn timeout_values_respect_clock_range() {
        for option in ["--tcp-timeout", "--udp-timeout"] {
            assert!(Config::parse([format!("{option}={}", u64::MAX)]).is_err());
            for seconds in [0, 1, 31_536_000] {
                assert!(Config::parse([format!("{option}={seconds}")]).is_ok());
            }
        }
        let mut config = Config::parse([]).unwrap().unwrap();
        config.tcp_timeout = Some(Duration::from_secs(u64::MAX));
        assert!(config.validate().is_err());
        config.tcp_timeout = None;
        config.udp_timeout = Duration::from_secs(u64::MAX);
        assert!(config.validate().is_err());
    }

    #[test]
    fn check_mode_respects_option_value_boundaries() {
        let command = Config::parse_command(
            ["--user", "admin", "--pwd", "--check-config"].map(str::to_owned),
        )
        .unwrap()
        .unwrap();
        let Command::Serve(config) = command else {
            panic!("密码中的参数字面值不应改变运行模式");
        };
        assert_eq!(config.password, "--check-config");
        assert!(matches!(
            Config::parse_command(["--check-config".to_owned()]).unwrap(),
            Some(Command::CheckConfig(_))
        ));
        assert!(Config::parse_command(["--check-config=true".to_owned()]).is_err());
    }

    #[test]
    fn loads_config_file_and_applies_command_line_overrides() {
        let path = temporary_config_path();
        fs::write(
            &path,
            "# 服务配置\r\nport=1080\r\nusername=admin\r\npassword=pass=#word\r\nwhitelist=127.0.0.1\r\ntcp_timeout=5\r\nudp_timeout=15\r\ndns_servers=9.9.9.9,1.0.0.1\r\n",
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
        assert_eq!(
            config.dns_servers,
            vec![
                "9.9.9.9:53".parse::<SocketAddr>().unwrap(),
                "1.0.0.1:53".parse().unwrap()
            ]
        );
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
        assert!(Config::parse(["--dns-servers=".to_owned()]).is_err());
        assert!(Config::parse(["--dns-servers=not-an-ip".to_owned()]).is_err());
        assert!(Config::parse(["--dns-servers=0.0.0.0".to_owned()]).is_err());
        fs::remove_file(path).unwrap();
    }
}
