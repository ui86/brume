use crate::dns::lookup_family;
use crate::happy_eyeballs::{happy_eyeballs_connect, happy_eyeballs_connect_resolving};
use hickory_resolver::Hosts;
use hickory_resolver::config::{ResolveHosts, ResolverConfig, ResolverOpts};
use hickory_resolver::lookup_ip::LookupIp;
use hickory_resolver::proto::op::Query;
use hickory_resolver::proto::rr::{Name, RecordType};
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod configuration;
mod transport;
use transport::DnsWorkers;

const SYSTEM_CONFIG_REFRESH: Duration = Duration::from_secs(60);
const UDP_FAMILY_PREFERENCE: Duration = Duration::from_millis(50);

enum LookupSource {
    Local(Vec<SocketAddr>),
    Network(Arc<ResolverSnapshot>),
}

#[derive(Clone)]
pub(crate) struct ResolverSnapshot {
    config: ResolverConfig,
    options: ResolverOpts,
    hosts: Arc<Hosts>,
    loaded_at: Instant,
    checked_at: Instant,
    hosts_source: Option<Arc<Vec<u8>>>,
    configuration_error: Option<Arc<io::Error>>,
}

impl ResolverSnapshot {
    fn from_sources(
        configuration: io::Result<(ResolverConfig, ResolverOpts)>,
        hosts: Arc<Hosts>,
    ) -> Self {
        let (config, mut options, configuration_error) = match configuration {
            Ok((config, options)) => (config, options, None),
            Err(error) => (
                ResolverConfig::from_name_servers(Vec::new()),
                ResolverOpts::default(),
                Some(Arc::new(error)),
            ),
        };
        options.use_hosts_file = ResolveHosts::Never;
        Self {
            config,
            options,
            hosts,
            loaded_at: Instant::now(),
            checked_at: Instant::now(),
            hosts_source: None,
            configuration_error,
        }
    }
}

/// 客户端共享解析器，保留缓存，并定期重新读取系统 DNS 与 hosts 配置。
pub(crate) struct ClientDns {
    snapshot: Arc<Mutex<Option<Arc<ResolverSnapshot>>>>,
    workers: DnsWorkers,
    refresh: Arc<tokio::sync::Semaphore>,
}

impl Default for ClientDns {
    fn default() -> Self {
        Self {
            snapshot: Arc::default(),
            workers: DnsWorkers::default(),
            refresh: Arc::new(tokio::sync::Semaphore::new(1)),
        }
    }
}

impl ClientDns {
    fn cached_configuration(&self) -> io::Result<Option<Arc<ResolverSnapshot>>> {
        let snapshot = self
            .snapshot
            .lock()
            .map_err(|_| io::Error::other("客户端 DNS 配置锁已损坏"))?;
        if let Some(current) = snapshot.as_ref()
            && current.checked_at.elapsed() < SYSTEM_CONFIG_REFRESH
        {
            return Ok(Some(Arc::clone(current)));
        }
        Ok(None)
    }

    async fn configuration(&self) -> io::Result<Arc<ResolverSnapshot>> {
        if let Some(current) = self.cached_configuration()? {
            return Ok(current);
        }
        let permit = Arc::clone(&self.refresh)
            .acquire_owned()
            .await
            .map_err(|_| io::Error::other("客户端 DNS 配置刷新已关闭"))?;
        if let Some(current) = self.cached_configuration()? {
            return Ok(current);
        }
        let snapshot = Arc::clone(&self.snapshot);
        tokio::task::spawn_blocking(move || {
            // 查询取消后仍持有刷新许可，避免重复启动不可取消的文件读取任务。
            let _permit = permit;
            let (hosts, source) = configuration::read_hosts();
            let mut current = ResolverSnapshot::from_sources(read_system_configuration(), hosts);
            current.hosts_source = Some(Arc::new(source));
            let mut snapshot = snapshot
                .lock()
                .map_err(|_| io::Error::other("客户端 DNS 配置锁已损坏"))?;
            Ok(Self::replace_configuration(&mut snapshot, current))
        })
        .await
        .map_err(io::Error::other)?
    }

    fn replace_configuration(
        snapshot: &mut Option<Arc<ResolverSnapshot>>,
        mut current: ResolverSnapshot,
    ) -> Arc<ResolverSnapshot> {
        if let Some(previous) = snapshot.as_ref()
            && previous.same_sources(&current)
        {
            // 检查时间只控制刷新频率，只有配置内容变化才使工作槽缓存失效。
            current.loaded_at = previous.loaded_at;
        }
        let current = Arc::new(current);
        *snapshot = Some(Arc::clone(&current));
        current
    }

    async fn lookup_source(&self, name: &str, port: u16) -> io::Result<LookupSource> {
        if let Ok(ip) = name.parse::<IpAddr>() {
            return Ok(LookupSource::Local(vec![SocketAddr::new(ip, port)]));
        }
        let snapshot = self.configuration().await?;
        if let Ok(host_name) = name.parse::<Name>() {
            let local_addresses: Vec<_> = [RecordType::AAAA, RecordType::A]
                .into_iter()
                .filter_map(|record_type| {
                    snapshot
                        .hosts
                        .lookup_static_host(&Query::query(host_name.clone(), record_type))
                })
                .flat_map(|lookup| LookupIp::from(lookup).into_iter())
                .map(|ip| SocketAddr::new(ip, port))
                .collect();
            if !local_addresses.is_empty() {
                return Ok(LookupSource::Local(local_addresses));
            }
        }
        if let Some(error) = &snapshot.configuration_error {
            return Err(io::Error::new(
                error.kind(),
                format!("读取系统 DNS 配置失败：{error}"),
            ));
        }
        Ok(LookupSource::Network(snapshot))
    }

    /// 先使用已返回的地址族，控制连接的地址族最多获得短暂的优先窗口。
    pub(crate) async fn lookup(
        &self,
        name: &str,
        port: u16,
        preferred_ipv4: bool,
    ) -> io::Result<Vec<SocketAddr>> {
        let snapshot = match self.lookup_source(name, port).await? {
            LookupSource::Local(addresses) => return Ok(addresses),
            LookupSource::Network(snapshot) => snapshot,
        };
        let worker = self.workers.acquire(&snapshot, None).await?;
        let result = self
            .lookup_udp_with_worker(worker.resolver(), name, port, preferred_ipv4)
            .await;
        worker.finish().await;
        result
    }

    async fn lookup_udp_with_worker(
        &self,
        resolver: &transport::ClientResolver,
        name: &str,
        port: u16,
        preferred_ipv4: bool,
    ) -> io::Result<Vec<SocketAddr>> {
        let preferred = lookup_family(resolver, name, port, !preferred_ipv4);
        let fallback = lookup_family(resolver, name, port, preferred_ipv4);
        tokio::pin!(preferred, fallback);
        tokio::select! {
            biased;
            result = &mut preferred => match result {
                Ok(addresses) => Ok(addresses),
                Err(_) => fallback.await,
            },
            result = &mut fallback => match result {
                Err(_) => preferred.await,
                Ok(fallback_addresses) => {
                    // 优先族无响应时及时使用可用结果，取消剩余 DNS 查询。
                    match tokio::time::timeout(UDP_FAMILY_PREFERENCE, &mut preferred).await {
                        Ok(Ok(addresses)) => Ok(addresses),
                        _ => Ok(fallback_addresses),
                    }
                }
            },
        }
    }

    pub(crate) async fn connect_server(
        &self,
        server: &str,
        timeout: Duration,
    ) -> io::Result<tokio::net::TcpStream> {
        if let Ok(address) = server.parse::<SocketAddr>() {
            return happy_eyeballs_connect(&[address], timeout).await;
        }
        let (name, port) = server_parts(server)?;
        match self.lookup_source(name, port).await? {
            LookupSource::Local(addresses) => happy_eyeballs_connect(&addresses, timeout).await,
            LookupSource::Network(snapshot) => {
                happy_eyeballs_connect_resolving(
                    self.lookup_remote_family(&snapshot, name, port, true),
                    self.lookup_remote_family(&snapshot, name, port, false),
                    timeout,
                )
                .await
            }
        }
    }

    async fn lookup_remote_family(
        &self,
        snapshot: &ResolverSnapshot,
        name: &str,
        port: u16,
        ipv6: bool,
    ) -> io::Result<Vec<SocketAddr>> {
        let worker = self.workers.acquire(snapshot, Some(ipv6)).await?;
        let result = lookup_family(worker.resolver(), name, port, ipv6).await;
        worker.finish().await;
        result
    }

    #[cfg(test)]
    pub(crate) async fn lookup_server(&self, server: &str) -> io::Result<Vec<SocketAddr>> {
        if let Ok(address) = server.parse::<SocketAddr>() {
            return Ok(vec![address]);
        }
        let (name, port) = server_parts(server)?;
        self.lookup(name, port, false).await
    }

    #[cfg(test)]
    pub(crate) fn with_resolver(snapshot: ResolverSnapshot) -> Self {
        Self {
            snapshot: Arc::new(Mutex::new(Some(Arc::new(snapshot)))),
            ..Self::default()
        }
    }
}

fn read_system_configuration() -> io::Result<(ResolverConfig, ResolverOpts)> {
    hickory_resolver::system_conf::read_system_conf().map_err(|error| {
        #[cfg(all(unix, not(any(target_os = "android", target_vendor = "apple"))))]
        if let hickory_resolver::net::NetError::Io(inner) = &error {
            return io::Error::new(inner.kind(), error);
        }
        io::Error::other(error)
    })
}

fn server_parts(server: &str) -> io::Result<(&str, u16)> {
    let (name, port) = server
        .rsplit_once(':')
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "代理地址缺少端口"))?;
    let port = port
        .parse::<u16>()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "代理地址端口无效"))?;
    if name.is_empty() || name.starts_with('[') || name.ends_with(']') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "代理服务器地址无效",
        ));
    }
    Ok((name, port))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use hickory_resolver::config::{
        ConnectionConfig, NameServerConfig, ResolverConfig, ResolverOpts,
    };
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    pub(crate) fn test_resolver(address: SocketAddr) -> ResolverSnapshot {
        let mut connection = ConnectionConfig::udp();
        connection.port = address.port();
        let mut tcp = ConnectionConfig::tcp();
        tcp.port = address.port();
        let config = ResolverConfig::from_name_servers(vec![NameServerConfig::new(
            address.ip(),
            true,
            vec![connection, tcp],
        )]);
        let mut options = ResolverOpts::default();
        options.use_hosts_file = ResolveHosts::Never;
        options.timeout = Duration::from_secs(5);
        options.attempts = 3;
        ResolverSnapshot {
            config,
            options,
            hosts: Arc::new(Hosts::default()),
            loaded_at: Instant::now(),
            checked_at: Instant::now(),
            hosts_source: Some(Arc::new(Vec::new())),
            configuration_error: None,
        }
    }

    fn answer(query: &[u8]) -> (Vec<u8>, u16) {
        let mut end = 12;
        while query[end] != 0 {
            end += usize::from(query[end]) + 1;
        }
        let record_type = u16::from_be_bytes([query[end + 1], query[end + 2]]);
        assert!([1, 28].contains(&record_type));
        let mut response = query[..2].to_vec();
        response.extend_from_slice(&[0x81, 0x80, 0, 1, 0, 1, 0, 0, 0, 0]);
        response.extend_from_slice(&query[12..end + 5]);
        response.extend_from_slice(&[0xc0, 0x0c]);
        response.extend_from_slice(&record_type.to_be_bytes());
        response.extend_from_slice(&[0, 1, 0, 0, 0, 60]);
        if record_type == 1 {
            response.extend_from_slice(&[0, 4, 127, 0, 0, 42]);
        } else {
            response.extend_from_slice(&[0, 16]);
            response.extend_from_slice(&std::net::Ipv6Addr::LOCALHOST.octets());
        }
        (response, record_type)
    }

    async fn refresh_workload(preserve: bool, count: usize) -> (Duration, usize) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = socket.local_addr().unwrap();
        let queries = Arc::new(AtomicUsize::new(0));
        let upstream_queries = Arc::clone(&queries);
        let upstream = tokio::spawn(async move {
            let mut packet = [0; 4096];
            loop {
                let (length, source) = socket.recv_from(&mut packet).await.unwrap();
                upstream_queries.fetch_add(1, Ordering::Relaxed);
                socket
                    .send_to(&answer(&packet[..length]).0, source)
                    .await
                    .unwrap();
            }
        });
        let initial = test_resolver(address);
        let dns = ClientDns::with_resolver(initial.clone());
        dns.lookup_remote_family(&initial, "refresh.example.", 1080, false)
            .await
            .unwrap();
        let before = queries.load(Ordering::Relaxed);
        let started = Instant::now();
        for _ in 0..count {
            let current = test_resolver(address);
            let snapshot = if preserve {
                ClientDns::replace_configuration(&mut dns.snapshot.lock().unwrap(), current)
            } else {
                // 对照旧刷新行为：每次加载都创建新代次，即使配置内容完全相同。
                let current = Arc::new(current);
                *dns.snapshot.lock().unwrap() = Some(Arc::clone(&current));
                current
            };
            let addresses = dns
                .lookup_remote_family(&snapshot, "refresh.example.", 1080, false)
                .await
                .unwrap();
            assert_eq!(addresses, vec!["127.0.0.42:1080".parse().unwrap()]);
        }
        let elapsed = started.elapsed();
        let requests = queries.load(Ordering::Relaxed) - before;
        upstream.abort();
        let _ = upstream.await;
        (elapsed, requests)
    }

    #[tokio::test]
    async fn unchanged_configuration_refresh_keeps_cached_dns_answers() {
        assert_eq!(refresh_workload(true, 3).await.1, 0);
        assert_eq!(refresh_workload(false, 3).await.1, 3);
    }

    #[test]
    fn configuration_generation_tracks_content_instead_of_check_time() {
        let original = test_resolver("127.0.0.1:53".parse().unwrap());
        let mut snapshot = Some(Arc::new(original.clone()));
        let identical = test_resolver("127.0.0.1:53".parse().unwrap());
        let checked_at = identical.checked_at;
        let refreshed = ClientDns::replace_configuration(&mut snapshot, identical);
        assert_eq!(refreshed.loaded_at, original.loaded_at);
        assert_eq!(refreshed.checked_at, checked_at);
        let mut changes = Vec::new();
        let mut changed = original.clone();
        changed.config.name_servers[0].connections[0].port += 1;
        changes.push(changed);
        let mut changed = original.clone();
        changed.options.timeout += Duration::from_secs(1);
        changes.push(changed);
        let mut changed = original.clone();
        changed.hosts_source = Some(Arc::new(b"127.0.0.2 changed.example".to_vec()));
        changes.push(changed);
        let mut changed = original.clone();
        changed.configuration_error =
            Some(Arc::new(io::Error::from(io::ErrorKind::PermissionDenied)));
        changes.push(changed);
        for mut changed in changes {
            changed.loaded_at = Instant::now();
            snapshot = Some(Arc::new(original.clone()));
            let refreshed = ClientDns::replace_configuration(&mut snapshot, changed);
            assert_ne!(refreshed.loaded_at, original.loaded_at);
        }
    }

    #[cfg(feature = "performance-bench")]
    #[tokio::test]
    #[ignore = "显式运行配置刷新前后的 DNS 缓存基准"]
    async fn benchmark_dns_configuration_refresh() {
        for round in 0..5 {
            let order = if round % 2 == 0 {
                [false, true]
            } else {
                [true, false]
            };
            for preserve in order {
                let (elapsed, queries) = refresh_workload(preserve, 1000).await;
                assert_eq!(queries, if preserve { 0 } else { 1000 });
                println!(
                    "DNS_REFRESH round={round} preserve={preserve} ns_per_refresh={:.2} queries={queries}",
                    elapsed.as_nanos() as f64 / 1000.0
                );
            }
        }
    }

    #[tokio::test]
    async fn literal_addresses_do_not_initialize_system_resolver() {
        let dns = ClientDns::default();
        for address in ["127.0.0.1:1080", "[::1]:1080", "[fe80::1%3]:1080"] {
            assert_eq!(
                dns.lookup_server(address).await.unwrap(),
                vec![address.parse::<SocketAddr>().unwrap()]
            );
        }
        assert_eq!(
            dns.lookup("::1", 53, true).await.unwrap(),
            vec!["[::1]:53".parse::<SocketAddr>().unwrap()]
        );
        assert!(dns.snapshot.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn rejects_invalid_proxy_address_without_network_lookup() {
        let dns = ClientDns::default();
        for address in [
            "localhost",
            "localhost:http",
            "localhost:65536",
            ":80",
            "[::1:80",
        ] {
            assert_eq!(
                dns.lookup_server(address).await.unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
        assert!(dns.snapshot.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn reads_system_configuration_and_resolves_localhost() {
        let dns = ClientDns::default();
        let addresses =
            tokio::time::timeout(Duration::from_secs(2), dns.lookup_server("localhost:1080"))
                .await
                .unwrap()
                .unwrap();
        assert!(addresses.iter().any(|address| address.ip().is_loopback()));
        assert!(addresses.iter().all(|address| address.port() == 1080));
        assert!(dns.snapshot.lock().unwrap().is_some());
    }

    #[tokio::test]
    async fn hosts_remain_available_when_dns_configuration_is_missing_or_unreadable() {
        for kind in [
            io::ErrorKind::NotFound,
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::InvalidData,
        ] {
            let mut hosts = Hosts::default();
            hosts
                .read_hosts_conf(b"127.0.0.7 hosts-only.example localhost\n".as_slice())
                .unwrap();
            let dns = ClientDns::with_resolver(ResolverSnapshot::from_sources(
                Err(io::Error::new(kind, "系统 DNS 配置不可用")),
                Arc::new(hosts),
            ));
            assert_eq!(
                dns.lookup("hosts-only.example", 1080, true).await.unwrap(),
                vec!["127.0.0.7:1080".parse().unwrap()]
            );
            assert_eq!(
                dns.lookup("external.example", 1080, true)
                    .await
                    .unwrap_err()
                    .kind(),
                kind
            );
            assert_eq!(
                dns.connect_server("external.example:1080", Duration::from_secs(1))
                    .await
                    .unwrap_err()
                    .kind(),
                kind
            );
            assert_eq!(dns.workers.available_slots(), (16, 32));
        }
    }

    #[tokio::test]
    async fn consecutive_proxy_connections_reuse_both_family_dns_caches() {
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let dns = ClientDns::with_resolver(test_resolver(socket.local_addr().unwrap()));
        let listener = tokio::net::TcpListener::bind("127.0.0.42:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let upstream = tokio::spawn(async move {
            let mut packet = [0; 4096];
            for _ in 0..2 {
                let (length, source) = socket.recv_from(&mut packet).await.unwrap();
                socket
                    .send_to(&answer(&packet[..length]).0, source)
                    .await
                    .unwrap();
            }
        });
        let server = format!("cached-proxy.example.:{}", address.port());
        for _ in 0..3 {
            let stream = dns
                .connect_server(&server, Duration::from_millis(500))
                .await
                .unwrap();
            assert_eq!(stream.peer_addr().unwrap(), address);
            listener.accept().await.unwrap();
        }
        upstream.await.unwrap();
    }

    #[tokio::test]
    async fn honors_hosts_aliases_before_querying_dns() {
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut resolver = test_resolver(socket.local_addr().unwrap());
        let mut hosts = Hosts::default();
        hosts
            .read_hosts_conf("127.0.0.7 hosts-only.example alias.example\n::7 hosts-only.example alias.example\n".as_bytes())
            .unwrap();
        let hosts = Arc::new(hosts);
        resolver.hosts = Arc::clone(&hosts);
        let dns = ClientDns::with_resolver(resolver);
        Arc::make_mut(dns.snapshot.lock().unwrap().as_mut().unwrap()).hosts = hosts;
        for name in ["hosts-only.example", "alias.example"] {
            let addresses =
                tokio::time::timeout(Duration::from_secs(1), dns.lookup(name, 1080, true))
                    .await
                    .unwrap()
                    .unwrap();
            assert!(addresses.contains(&"127.0.0.7:1080".parse().unwrap()));
            assert!(addresses.contains(&"[::7]:1080".parse().unwrap()));
        }
        let mut packet = [0; 1024];
        assert!(socket.try_recv_from(&mut packet).is_err());
    }

    #[tokio::test]
    async fn single_family_hosts_entry_does_not_query_missing_family() {
        for ip in ["127.0.0.7", "::7"] {
            let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let dns = ClientDns::with_resolver(test_resolver(socket.local_addr().unwrap()));
            let mut hosts = Hosts::default();
            hosts
                .read_hosts_conf(format!("{ip} hosts-only.example\n").as_bytes())
                .unwrap();
            Arc::make_mut(dns.snapshot.lock().unwrap().as_mut().unwrap()).hosts = Arc::new(hosts);
            assert_eq!(
                tokio::time::timeout(
                    Duration::from_secs(1),
                    dns.lookup("hosts-only.example", 1080, true),
                )
                .await
                .unwrap()
                .unwrap(),
                vec![SocketAddr::new(ip.parse().unwrap(), 1080)]
            );
            let mut packet = [0; 1024];
            assert!(socket.try_recv_from(&mut packet).is_err());
        }
    }

    #[tokio::test]
    async fn resolves_both_families_over_udp_and_reuses_ttl_cache() {
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let dns = ClientDns::with_resolver(test_resolver(socket.local_addr().unwrap()));
        let server = tokio::spawn(async move {
            let mut packet = [0; 4096];
            let mut types = Vec::new();
            for _ in 0..2 {
                let (length, source) = socket.recv_from(&mut packet).await.unwrap();
                let (response, record_type) = answer(&packet[..length]);
                types.push(record_type);
                socket.send_to(&response, source).await.unwrap();
            }
            types.sort_unstable();
            assert_eq!(types, [1, 28]);
        });
        let first = tokio::time::timeout(
            Duration::from_secs(2),
            dns.lookup_server("dual-stack.example.:1080"),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(first.contains(&"[::1]:1080".parse().unwrap()));
        server.await.unwrap();
        // 上游已关闭，第二次解析只能命中同一解析器中的 TTL 缓存。
        let second = tokio::time::timeout(
            Duration::from_secs(1),
            dns.lookup_server("dual-stack.example.:1080"),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(second, first);
    }

    #[tokio::test]
    async fn timeout_closes_real_dns_sockets_and_stops_retransmission() {
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let dns = Arc::new(ClientDns::with_resolver(test_resolver(
            socket.local_addr().unwrap(),
        )));
        let lookup_dns = Arc::clone(&dns);
        let lookup = tokio::spawn(async move {
            tokio::time::timeout(
                Duration::from_millis(150),
                lookup_dns.lookup_server("blackhole.example.:1080"),
            )
            .await
        });
        let mut packet = [0; 4096];
        let mut sources = Vec::new();
        for _ in 0..2 {
            let (_, source) =
                tokio::time::timeout(Duration::from_secs(1), socket.recv_from(&mut packet))
                    .await
                    .unwrap()
                    .unwrap();
            sources.push(source);
        }
        assert!(lookup.await.unwrap().is_err());
        // 收到真实 DNS 请求后，再绑定回两个源端口，确认超时已释放网络查询套接字。
        let mut reclaimed = Vec::new();
        for source in sources {
            reclaimed.push(tokio::net::UdpSocket::bind(source).await.unwrap());
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(450), socket.recv_from(&mut packet))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn cancels_dns_after_truncated_udp_answer_falls_back_to_tcp() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let udp = tokio::net::UdpSocket::bind(address).await.unwrap();
        let dns = Arc::new(ClientDns::with_resolver(test_resolver(address)));
        let udp_worker = tokio::spawn(async move {
            let mut packet = [0; 4096];
            for _ in 0..2 {
                let (length, source) = udp.recv_from(&mut packet).await.unwrap();
                let (mut response, record_type) = answer(&packet[..length]);
                // IPv4 标记截断以转用 TCP，IPv6 返回无记录，避免干扰回退取消验证。
                if record_type == 1 {
                    response[2] |= 0x02;
                }
                response[7] = 0;
                let mut end = 12;
                while packet[end] != 0 {
                    end += usize::from(packet[end]) + 1;
                }
                response.truncate(end + 5);
                udp.send_to(&response, source).await.unwrap();
            }
        });
        let lookup_dns = Arc::clone(&dns);
        let lookup = tokio::spawn(async move {
            tokio::time::timeout(
                Duration::from_millis(250),
                lookup_dns.lookup("truncated.example.", 1080, true),
            )
            .await
        });
        let (mut stream, _) = tokio::time::timeout(Duration::from_secs(1), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let length = tokio::time::timeout(Duration::from_secs(1), stream.read_u16())
            .await
            .unwrap()
            .unwrap();
        let mut packet = vec![0; usize::from(length)];
        stream.read_exact(&mut packet).await.unwrap();
        assert_eq!(answer(&packet).1, 1);
        assert!(lookup.await.unwrap().is_err());
        udp_worker.await.unwrap();
        let mut packet = [0; 4096];
        // 客户端继续存活，超时本身也必须关闭后台 TCP 查询连接。
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), stream.read(&mut packet))
                .await
                .unwrap()
                .unwrap(),
            0
        );

        let udp = tokio::net::UdpSocket::bind(address).await.unwrap();
        let udp_worker = tokio::spawn(async move {
            let mut packet = [0; 4096];
            for _ in 0..2 {
                let (length, source) = udp.recv_from(&mut packet).await.unwrap();
                let (mut response, record_type) = answer(&packet[..length]);
                if record_type == 1 {
                    response[2] |= 0x02;
                }
                response[7] = 0;
                let mut end = 12;
                while packet[end] != 0 {
                    end += usize::from(packet[end]) + 1;
                }
                response.truncate(end + 5);
                udp.send_to(&response, source).await.unwrap();
            }
        });
        let next_dns = Arc::clone(&dns);
        let recovered = tokio::spawn(async move {
            tokio::time::timeout(
                Duration::from_secs(2),
                next_dns.lookup("recovered.example.", 1080, true),
            )
            .await
            .unwrap()
            .unwrap()
        });
        let (mut connection, _) = tokio::time::timeout(Duration::from_secs(1), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let length = connection.read_u16().await.unwrap();
        let mut query = vec![0; usize::from(length)];
        connection.read_exact(&mut query).await.unwrap();
        let response = answer(&query).0;
        connection.write_u16(response.len() as u16).await.unwrap();
        connection.write_all(&response).await.unwrap();
        assert_eq!(
            recovered.await.unwrap(),
            vec!["127.0.0.42:1080".parse().unwrap()]
        );
        udp_worker.await.unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), connection.read(&mut packet))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        // 未缓存域名重新建连成功后，上游关闭，重复查询仍必须命中同一 worker 的缓存。
        assert_eq!(
            tokio::time::timeout(
                Duration::from_millis(100),
                dns.lookup("recovered.example.", 1080, true),
            )
            .await
            .unwrap()
            .unwrap(),
            vec!["127.0.0.42:1080".parse().unwrap()]
        );
    }

    #[tokio::test]
    async fn queued_dns_calls_cancel_without_stopping_active_workers() {
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let dns = Arc::new(ClientDns::with_resolver(test_resolver(
            socket.local_addr().unwrap(),
        )));
        let mut active = Vec::new();
        for index in 0..16 {
            let dns = Arc::clone(&dns);
            active.push(tokio::spawn(async move {
                dns.lookup(&format!("active-{index}.example."), 1080, true)
                    .await
            }));
        }
        let mut requests = Vec::new();
        let mut packet = [0; 4096];
        for _ in 0..32 {
            let (length, source) =
                tokio::time::timeout(Duration::from_secs(1), socket.recv_from(&mut packet))
                    .await
                    .unwrap()
                    .unwrap();
            requests.push((packet[..length].to_vec(), source));
        }
        assert_eq!(dns.workers.available_slots().0, 0);
        let mut queued = Vec::new();
        for index in 0..16 {
            let dns = Arc::clone(&dns);
            queued.push(tokio::spawn(async move {
                dns.lookup(&format!("queued-{index}.example."), 1080, true)
                    .await
            }));
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(50), socket.recv_from(&mut packet))
                .await
                .is_err()
        );
        for task in queued {
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        }
        assert!(active.iter().all(|task| !task.is_finished()));
        for (request, source) in requests {
            socket.send_to(&answer(&request).0, source).await.unwrap();
        }
        for task in active {
            let addresses = tokio::time::timeout(Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert!(addresses.contains(&"127.0.0.42:1080".parse().unwrap()));
        }
        assert_eq!(dns.workers.available_slots(), (16, 32));
    }

    #[tokio::test]
    async fn configuration_refresh_replaces_workers_without_reusing_old_dns_cache() {
        async fn upstream(last_octet: u8) -> (SocketAddr, tokio::task::JoinHandle<()>) {
            let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let address = socket.local_addr().unwrap();
            let task = tokio::spawn(async move {
                let mut packet = [0; 4096];
                for _ in 0..2 {
                    let (length, source) = socket.recv_from(&mut packet).await.unwrap();
                    let (mut response, record_type) = answer(&packet[..length]);
                    if record_type == 1 {
                        *response.last_mut().unwrap() = last_octet;
                    }
                    socket.send_to(&response, source).await.unwrap();
                }
            });
            (address, task)
        }

        let (old_address, old_server) = upstream(42).await;
        let dns = ClientDns::with_resolver(test_resolver(old_address));
        assert_eq!(
            dns.lookup("refreshed.example.", 1080, true).await.unwrap(),
            vec!["127.0.0.42:1080".parse().unwrap()]
        );
        old_server.await.unwrap();
        let (new_address, new_server) = upstream(43).await;
        *dns.snapshot.lock().unwrap() = Some(Arc::new(test_resolver(new_address)));
        assert_eq!(
            dns.lookup("refreshed.example.", 1080, true).await.unwrap(),
            vec!["127.0.0.43:1080".parse().unwrap()]
        );
        new_server.await.unwrap();
        assert_eq!(dns.workers.available_slots(), (16, 32));
    }

    #[tokio::test]
    async fn dns_tcp_sockets_remain_bounded_during_refresh_and_cancellation() {
        let first = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = first.local_addr().unwrap().port();
        let second = tokio::net::TcpListener::bind(("127.0.0.2", port))
            .await
            .unwrap();
        let addresses = [first.local_addr().unwrap(), second.local_addr().unwrap()];
        let mut snapshot = test_resolver(addresses[0]);
        snapshot
            .config
            .name_servers
            .extend(test_resolver(addresses[1]).config.name_servers);
        // 直接配置两台 TCP DNS 上游，隔离 UDP 截断策略并制造超过 32 条连接的需求。
        for server in &mut snapshot.config.name_servers {
            server.connections.retain(|connection| {
                matches!(
                    connection.protocol,
                    hickory_resolver::config::ProtocolConfig::Tcp
                )
            });
        }
        let dns = Arc::new(ClientDns::with_resolver(snapshot));
        let mut active = Vec::new();
        for index in 0..16 {
            let dns = Arc::clone(&dns);
            active.push(tokio::spawn(async move {
                dns.lookup(&format!("tcp-load-{index}.example."), 1080, true)
                    .await
            }));
        }
        let mut connections = Vec::new();
        for _ in 0..32 {
            let accepted = tokio::time::timeout(Duration::from_secs(2), async {
                tokio::select! {
                    accepted = first.accept() => accepted,
                    accepted = second.accept() => accepted,
                }
            })
            .await
            .unwrap_or_else(|_| panic!("实际已接受 {} 条 DNS TCP 连接", connections.len()))
            .unwrap();
            connections.push(accepted.0);
        }
        assert_eq!(dns.workers.available_slots(), (0, 0));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), async {
                tokio::select! {
                    accepted = first.accept() => accepted,
                    accepted = second.accept() => accepted,
                }
            })
            .await
            .is_err()
        );

        // 旧代尚持有全部配额时刷新配置，新代查询只能排队，不能创建另一组连接池。
        *dns.snapshot.lock().unwrap() = Some(Arc::new(test_resolver(addresses[0])));
        let queued_dns = Arc::clone(&dns);
        let queued = tokio::spawn(async move {
            queued_dns
                .lookup("next-generation.example.", 1080, true)
                .await
        });
        tokio::task::yield_now().await;
        assert_eq!(dns.workers.available_slots(), (0, 0));
        queued.abort();
        assert!(queued.await.unwrap_err().is_cancelled());
        for task in active {
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        }
        for mut connection in connections {
            tokio::time::timeout(Duration::from_secs(1), async {
                let mut packet = [0; 4096];
                while connection.read(&mut packet).await.unwrap() != 0 {}
            })
            .await
            .unwrap();
        }
        tokio::time::timeout(Duration::from_secs(1), async {
            while dns.workers.available_slots() != (16, 32) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
}
