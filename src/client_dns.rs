use crate::dns::lookup_family;
use crate::happy_eyeballs::{happy_eyeballs_connect, happy_eyeballs_connect_resolving};
use hickory_resolver::config::{LookupIpStrategy, ResolveHosts};
use hickory_resolver::lookup_ip::LookupIp;
use hickory_resolver::proto::op::Query;
use hickory_resolver::proto::rr::{Name, RecordType};
use hickory_resolver::{Hosts, TokioResolver};
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SYSTEM_CONFIG_REFRESH: Duration = Duration::from_secs(60);
const UDP_FAMILY_PREFERENCE: Duration = Duration::from_millis(50);

enum LookupSource {
    Local(Vec<SocketAddr>),
    Network(Arc<TokioResolver>),
}

struct ResolverSnapshot {
    resolver: Arc<TokioResolver>,
    hosts: Arc<Hosts>,
    loaded_at: Instant,
}

/// 客户端共享解析器，保留缓存，并定期重新读取系统 DNS 与 hosts 配置。
#[derive(Default)]
pub(crate) struct ClientDns {
    snapshot: Mutex<Option<ResolverSnapshot>>,
}

impl ClientDns {
    fn resolver(&self) -> io::Result<(Arc<TokioResolver>, Arc<Hosts>)> {
        let mut snapshot = self
            .snapshot
            .lock()
            .map_err(|_| io::Error::other("客户端 DNS 配置锁已损坏"))?;
        if let Some(current) = snapshot.as_ref()
            && current.loaded_at.elapsed() < SYSTEM_CONFIG_REFRESH
        {
            return Ok((Arc::clone(&current.resolver), Arc::clone(&current.hosts)));
        }

        // 只有本地系统配置与 hosts 文件读取是同步操作；网络查询使用可取消的异步套接字。
        let mut builder = TokioResolver::builder_tokio().map_err(io::Error::other)?;
        let options = builder.options_mut();
        options.ip_strategy = LookupIpStrategy::Ipv6AndIpv4;
        // 显式共享同一份 hosts 快照，单族命中时直接返回，避免等待另一族的上游查询。
        options.use_hosts_file = ResolveHosts::Never;
        let hosts = Arc::new(Hosts::from_system().unwrap_or_default());
        let mut resolver = builder.build().map_err(io::Error::other)?;
        resolver.set_hosts(Arc::clone(&hosts));
        let resolver = Arc::new(resolver);
        *snapshot = Some(ResolverSnapshot {
            resolver: Arc::clone(&resolver),
            hosts: Arc::clone(&hosts),
            loaded_at: Instant::now(),
        });
        Ok((resolver, hosts))
    }

    fn lookup_source(&self, name: &str, port: u16) -> io::Result<LookupSource> {
        if let Ok(ip) = name.parse::<IpAddr>() {
            return Ok(LookupSource::Local(vec![SocketAddr::new(ip, port)]));
        }
        let (resolver, hosts) = self.resolver()?;
        if let Ok(host_name) = name.parse::<Name>() {
            let local_addresses: Vec<_> = [RecordType::AAAA, RecordType::A]
                .into_iter()
                .filter_map(|record_type| {
                    hosts.lookup_static_host(&Query::query(host_name.clone(), record_type))
                })
                .flat_map(|lookup| LookupIp::from(lookup).into_iter())
                .map(|ip| SocketAddr::new(ip, port))
                .collect();
            if !local_addresses.is_empty() {
                return Ok(LookupSource::Local(local_addresses));
            }
        }
        Ok(LookupSource::Network(resolver))
    }

    /// 先使用已返回的地址族，控制连接的地址族最多获得短暂的优先窗口。
    pub(crate) async fn lookup(
        &self,
        name: &str,
        port: u16,
        preferred_ipv4: bool,
    ) -> io::Result<Vec<SocketAddr>> {
        let resolver = match self.lookup_source(name, port)? {
            LookupSource::Local(addresses) => return Ok(addresses),
            LookupSource::Network(resolver) => resolver,
        };
        let preferred = lookup_family(&resolver, name, port, !preferred_ipv4);
        let fallback = lookup_family(&resolver, name, port, preferred_ipv4);
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
        match self.lookup_source(name, port)? {
            LookupSource::Local(addresses) => happy_eyeballs_connect(&addresses, timeout).await,
            LookupSource::Network(resolver) => {
                happy_eyeballs_connect_resolving(
                    lookup_family(&resolver, name, port, true),
                    lookup_family(&resolver, name, port, false),
                    timeout,
                )
                .await
            }
        }
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
    pub(crate) fn with_resolver(resolver: TokioResolver) -> Self {
        Self {
            snapshot: Mutex::new(Some(ResolverSnapshot {
                resolver: Arc::new(resolver),
                hosts: Arc::new(Hosts::default()),
                loaded_at: Instant::now(),
            })),
        }
    }
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
    use hickory_resolver::net::runtime::TokioRuntimeProvider;
    use std::sync::Arc;
    use tokio::io::AsyncReadExt;

    pub(crate) fn test_resolver(address: SocketAddr) -> TokioResolver {
        let mut connection = ConnectionConfig::udp();
        connection.port = address.port();
        let mut tcp = ConnectionConfig::tcp();
        tcp.port = address.port();
        let config = ResolverConfig::from_name_servers(vec![NameServerConfig::new(
            address.ip(),
            true,
            vec![connection, tcp],
        )]);
        let mut builder =
            TokioResolver::builder_with_config(config, TokioRuntimeProvider::default());
        let options: &mut ResolverOpts = builder.options_mut();
        options.use_hosts_file = ResolveHosts::Never;
        options.ip_strategy = LookupIpStrategy::Ipv6AndIpv4;
        options.timeout = Duration::from_secs(5);
        options.attempts = 3;
        builder.build().unwrap()
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
    async fn honors_hosts_aliases_before_querying_dns() {
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut resolver = test_resolver(socket.local_addr().unwrap());
        let mut hosts = Hosts::default();
        hosts
            .read_hosts_conf("127.0.0.7 hosts-only.example alias.example\n::7 hosts-only.example alias.example\n".as_bytes())
            .unwrap();
        let hosts = Arc::new(hosts);
        resolver.set_hosts(Arc::clone(&hosts));
        let dns = ClientDns::with_resolver(resolver);
        dns.snapshot.lock().unwrap().as_mut().unwrap().hosts = hosts;
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
            dns.snapshot.lock().unwrap().as_mut().unwrap().hosts = Arc::new(hosts);
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
        assert!(
            tokio::time::timeout(Duration::from_millis(450), stream.read(&mut packet))
                .await
                .is_err()
        );
        // 客户端存活时可保留闲置连接；最后一个解析器所有者释放后必须关闭连接。
        drop(dns);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), stream.read(&mut packet))
                .await
                .unwrap()
                .unwrap(),
            0
        );
    }
}
