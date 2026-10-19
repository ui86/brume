use crate::dns_transport::DnsTransport;
use crate::happy_eyeballs::{happy_eyeballs_connect, happy_eyeballs_connect_resolving};
use crate::protocol::{Host, Target};
use hickory_resolver::config::{
    ConnectionConfig, NameServerConfig, ResolveHosts, ResolverConfig, ResolverOpts,
};
use hickory_resolver::lookup_ip::LookupIp;
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use hickory_resolver::{ConnectionProvider, Resolver};
use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::time::Duration;
use tokio::runtime::{Builder, Runtime, RuntimeFlavor};

pub struct DnsResolver {
    runtime: Option<Runtime>,
    resolver: Resolver<DnsTransport<TokioRuntimeProvider>>,
}

pub(crate) type PendingLookup = Pin<Box<dyn Future<Output = io::Result<Vec<SocketAddr>>> + Send>>;

pub(crate) struct UdpLookup {
    pub addresses: Vec<SocketAddr>,
    pub pending: Option<PendingLookup>,
}

impl DnsResolver {
    pub fn new(servers: &[SocketAddr]) -> io::Result<Self> {
        let nameservers = servers
            .iter()
            .map(|server| {
                let mut udp = ConnectionConfig::udp();
                udp.port = server.port();
                let mut tcp = ConnectionConfig::tcp();
                tcp.port = server.port();
                NameServerConfig::new(server.ip(), true, vec![udp, tcp])
            })
            .collect();
        let mut builder = Resolver::builder_with_config(
            ResolverConfig::from_name_servers(nameservers),
            DnsTransport(TokioRuntimeProvider::default()),
        );
        let opts: &mut ResolverOpts = builder.options_mut();
        opts.timeout = Duration::from_secs(3);
        opts.attempts = 2;
        opts.use_hosts_file = ResolveHosts::Never;

        let (runtime, resolver) = match tokio::runtime::Handle::try_current() {
            Ok(_) => {
                let resolver = builder.build().map_err(io::Error::other)?;
                (None, resolver)
            }
            Err(_) => {
                let rt = Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()?;
                let resolver = {
                    let _guard = rt.enter();
                    builder.build().map_err(io::Error::other)?
                };
                (Some(rt), resolver)
            }
        };
        Ok(Self { runtime, resolver })
    }

    /// 异步解析目标地址
    pub async fn lookup(&self, target: &Target) -> io::Result<Vec<SocketAddr>> {
        let addresses: Vec<_> = match &target.host {
            Host::Ip(ip) => vec![SocketAddr::new(*ip, target.port)],
            Host::Domain(name) => self
                .resolver
                .lookup_ip(name.as_str())
                .await
                .map_err(io::Error::other)?
                .iter()
                .map(|ip| SocketAddr::new(ip, target.port))
                .collect(),
        };
        if addresses.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "目标地址没有解析结果",
            ));
        }
        Ok(addresses)
    }

    /// UDP 首包先使用已返回的地址，未完成的地址族由流的回复任务继续查询。
    pub(crate) async fn lookup_udp(&self, target: &Target) -> io::Result<UdpLookup> {
        let ip = match &target.host {
            Host::Ip(ip) => Some(*ip),
            Host::Domain(name) => name.parse().ok(),
        };
        if let Some(ip) = ip {
            return Ok(UdpLookup {
                addresses: vec![SocketAddr::new(ip, target.port)],
                pending: None,
            });
        }
        let Host::Domain(name) = &target.host else {
            unreachable!()
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let make_lookup = |ipv6| {
            let resolver = self.resolver.clone();
            let name = name.clone();
            let port = target.port;
            Box::pin(async move {
                tokio::time::timeout_at(deadline, lookup_family(&resolver, &name, port, ipv6))
                    .await
                    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "UDP 目标域名解析超时"))?
            }) as PendingLookup
        };
        let mut ipv6 = make_lookup(true);
        let mut ipv4 = make_lookup(false);
        let (first, pending) = tokio::select! {
            result = &mut ipv6 => (result, ipv4),
            result = &mut ipv4 => (result, ipv6),
        };
        match first {
            Ok(addresses) => Ok(UdpLookup {
                addresses,
                pending: Some(pending),
            }),
            Err(_) => Ok(UdpLookup {
                addresses: pending.await?,
                pending: None,
            }),
        }
    }

    /// 分别解析两个地址族，优先使用先返回的可连接地址建立 TCP 连接
    pub async fn connect_tcp(
        &self,
        target: &Target,
        total_timeout: Duration,
    ) -> io::Result<tokio::net::TcpStream> {
        match &target.host {
            Host::Ip(ip) => {
                happy_eyeballs_connect(&[SocketAddr::new(*ip, target.port)], total_timeout).await
            }
            Host::Domain(name) => {
                // 域名类型携带 IP 字面量时保留原有 lookup_ip 的直接连接行为。
                if let Ok(ip) = name.parse::<IpAddr>() {
                    return happy_eyeballs_connect(
                        &[SocketAddr::new(ip, target.port)],
                        total_timeout,
                    )
                    .await;
                }
                happy_eyeballs_connect_resolving(
                    self.lookup_family(name, target.port, true),
                    self.lookup_family(name, target.port, false),
                    total_timeout,
                )
                .await
            }
        }
    }

    async fn lookup_family(
        &self,
        name: &str,
        port: u16,
        ipv6: bool,
    ) -> io::Result<Vec<SocketAddr>> {
        lookup_family(&self.resolver, name, port, ipv6).await
    }

    /// 同步阻塞解析目标地址（用于测试及外部同步兼容）
    pub fn lookup_blocking(&self, target: &Target) -> io::Result<Vec<SocketAddr>> {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            // 单线程运行时不能临时阻塞工作线程，调用方应改用异步解析接口
            if handle.runtime_flavor() != RuntimeFlavor::MultiThread {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "当前运行时不支持阻塞解析，请改用异步 lookup 方法",
                ));
            }
            tokio::task::block_in_place(|| handle.block_on(self.lookup(target)))
        } else if let Some(ref rt) = self.runtime {
            rt.block_on(self.lookup(target))
        } else {
            let rt = Builder::new_current_thread().enable_all().build()?;
            rt.block_on(self.lookup(target))
        }
    }
}

pub(crate) async fn lookup_family<P: ConnectionProvider>(
    resolver: &Resolver<P>,
    name: &str,
    port: u16,
    ipv6: bool,
) -> io::Result<Vec<SocketAddr>> {
    // 两族查询共用解析器原有的缓存，由 Hickory 继续管理正负缓存与 TTL。
    let lookup = if ipv6 {
        resolver.ipv6_lookup(name).await
    } else {
        resolver.ipv4_lookup(name).await
    }
    .map_err(io::Error::other)?;
    let addresses: Vec<_> = LookupIp::from(lookup)
        .iter()
        .map(|ip| SocketAddr::new(ip, port))
        .collect();
    if addresses.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "目标地址没有解析结果",
        ));
    }
    Ok(addresses)
}

impl Drop for DnsResolver {
    fn drop(&mut self) {
        if let Some(rt) = self.runtime.take() {
            // 将自建的 Tokio 运行时移至独立的系统线程中释放，
            // 避免在当前处于 Tokio 异步工作线程或 block_on 上下文时触发不可阻塞清理 panic
            let _ = std::thread::spawn(move || drop(rt)).join();
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::net::UdpSocket;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::thread;
    use tokio::task::{JoinHandle, JoinSet};

    #[derive(Clone, Copy)]
    pub(crate) enum DnsReply {
        Answer(Duration),
        NoRecords(Duration),
        Drop,
    }

    pub(crate) struct TestDns {
        pub(crate) address: SocketAddr,
        queries: Arc<[AtomicUsize; 2]>,
        worker: JoinHandle<()>,
    }

    impl Drop for TestDns {
        fn drop(&mut self) {
            self.worker.abort();
        }
    }

    pub(crate) async fn scripted_dns(ipv4: DnsReply, ipv6: DnsReply, ttl: u32) -> TestDns {
        let socket = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let address = socket.local_addr().unwrap();
        let queries = Arc::new([AtomicUsize::new(0), AtomicUsize::new(0)]);
        let worker_queries = Arc::clone(&queries);
        let worker = tokio::spawn(async move {
            let mut buffer = [0; 4096];
            let mut replies = JoinSet::new();
            loop {
                tokio::select! {
                    Some(_) = replies.join_next(), if !replies.is_empty() => {}
                    received = socket.recv_from(&mut buffer) => {
                        let (length, source) = received.unwrap();
                        let mut response = answer(&buffer[..length]).unwrap();
                        let mut end = 12;
                        while buffer[end] != 0 {
                            end += usize::from(buffer[end]) + 1;
                        }
                        let question_end = end + 5;
                        let family = usize::from(buffer[end + 1..end + 3] == [0, 28]);
                        worker_queries[family].fetch_add(1, Ordering::Relaxed);
                        let (delay, no_records) = match [ipv4, ipv6][family] {
                            DnsReply::Answer(delay) => (delay, false),
                            DnsReply::NoRecords(delay) => (delay, true),
                            DnsReply::Drop => continue,
                        };
                        if no_records {
                            response[6..8].copy_from_slice(&[0, 0]);
                            response.truncate(question_end);
                        } else {
                            response[question_end + 6..question_end + 10]
                                .copy_from_slice(&ttl.to_be_bytes());
                        }
                        let socket = Arc::clone(&socket);
                        replies.spawn(async move {
                            tokio::time::sleep(delay).await;
                            socket.send_to(&response, source).await.unwrap();
                        });
                    }
                }
            }
        });
        TestDns {
            address,
            queries,
            worker,
        }
    }

    fn tcp_target(port: u16) -> Target {
        Target {
            host: Host::Domain("progressive-dns.example.".into()),
            port,
        }
    }

    fn answer(query: &[u8]) -> Option<Vec<u8>> {
        if query.len() < 17 {
            return None;
        }
        let mut end = 12;
        while end < query.len() && query[end] != 0 {
            end += usize::from(query[end]) + 1;
        }
        if end + 5 > query.len() {
            return None;
        }
        let question_end = end + 5;
        let query_type = &query[end + 1..end + 3];
        let is_a = query_type == [0, 1];
        let is_aaaa = query_type == [0, 28];
        let mut response = Vec::from(&query[..2]);
        response.extend_from_slice(&[0x81, 0x80, 0, 1, 0, u8::from(is_a || is_aaaa), 0, 0, 0, 0]);
        response.extend_from_slice(&query[12..question_end]);
        if is_a {
            response.extend_from_slice(&[0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 127, 0, 0, 42]);
        } else if is_aaaa {
            response.extend_from_slice(&[0xc0, 0x0c, 0, 28, 0, 1, 0, 0, 0, 60, 0, 16]);
            response.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        }
        Some(response)
    }

    #[tokio::test]
    async fn blocking_lookup_rejects_current_thread_runtime_without_panicking() {
        let resolver = DnsResolver::new(&["127.0.0.1:53".parse().unwrap()]).unwrap();
        let target = Target::from("127.0.0.42:443".parse::<SocketAddr>().unwrap());

        let error = resolver.lookup_blocking(&target).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert!(error.to_string().contains("lookup"));
        assert_eq!(
            resolver.lookup(&target).await.unwrap(),
            vec!["127.0.0.42:443".parse::<SocketAddr>().unwrap()]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn blocking_lookup_remains_available_in_multi_thread_runtime() {
        let resolver = DnsResolver::new(&["127.0.0.1:53".parse().unwrap()]).unwrap();
        let target = Target::from("127.0.0.42:443".parse::<SocketAddr>().unwrap());

        assert_eq!(
            resolver.lookup_blocking(&target).unwrap(),
            vec!["127.0.0.42:443".parse::<SocketAddr>().unwrap()]
        );
    }

    #[test]
    fn blocking_lookup_remains_available_without_runtime() {
        let resolver = DnsResolver::new(&["127.0.0.1:53".parse().unwrap()]).unwrap();
        let target = Target::from("127.0.0.42:443".parse::<SocketAddr>().unwrap());

        assert_eq!(
            resolver.lookup_blocking(&target).unwrap(),
            vec!["127.0.0.42:443".parse::<SocketAddr>().unwrap()]
        );
    }

    #[tokio::test]
    async fn custom_server_resolves_domain_without_system_dns() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let address = socket.local_addr().unwrap();
        let done = Arc::new(AtomicBool::new(false));
        let queries = Arc::new(AtomicUsize::new(0));
        let worker_done = Arc::clone(&done);
        let worker_queries = Arc::clone(&queries);
        let worker = thread::spawn(move || {
            let mut query = [0u8; 4096];
            while !worker_done.load(Ordering::Relaxed) {
                if let Ok((length, source)) = socket.recv_from(&mut query)
                    && let Some(response) = answer(&query[..length])
                {
                    worker_queries.fetch_add(1, Ordering::Relaxed);
                    socket.send_to(&response, source).unwrap();
                }
            }
        });
        let resolver = DnsResolver::new(&[address]).unwrap();
        let target = Target {
            host: Host::Domain("brume-test.example.com.".into()),
            port: 443,
        };
        let result = resolver.lookup(&target).await.unwrap();
        assert!(result.contains(&"127.0.0.42:443".parse().unwrap()));
        thread::sleep(Duration::from_millis(50));
        let first_queries = queries.load(Ordering::Relaxed);
        assert!(first_queries >= 1);
        assert!(
            resolver
                .lookup(&target)
                .await
                .unwrap()
                .contains(&"127.0.0.42:443".parse().unwrap())
        );
        assert_eq!(queries.load(Ordering::Relaxed), first_queries);
        done.store(true, Ordering::Relaxed);
        worker.join().unwrap();
    }

    #[tokio::test]
    async fn tcp_connect_uses_ipv4_while_aaaa_queries_are_dropped() {
        let listener = tokio::net::TcpListener::bind("127.0.0.42:0").await.unwrap();
        let dns = scripted_dns(DnsReply::Answer(Duration::ZERO), DnsReply::Drop, 60).await;
        let resolver = DnsResolver::new(&[dns.address]).unwrap();
        let target = tcp_target(listener.local_addr().unwrap().port());

        let stream = tokio::time::timeout(
            Duration::from_millis(700),
            resolver.connect_tcp(&target, Duration::from_secs(2)),
        )
        .await
        .expect("可用 IPv4 地址被丢弃的 AAAA 查询阻塞")
        .unwrap();
        assert_eq!(stream.peer_addr().unwrap(), listener.local_addr().unwrap());
        assert!(dns.queries[0].load(Ordering::Relaxed) >= 1);
        assert!(dns.queries[1].load(Ordering::Relaxed) >= 1);
    }

    #[tokio::test]
    async fn tcp_connect_keeps_ip_literals_in_domain_targets_without_dns() {
        let dns = scripted_dns(DnsReply::Drop, DnsReply::Drop, 60).await;
        let resolver = DnsResolver::new(&[dns.address]).unwrap();
        for address in ["127.0.0.42:0", "[::1]:0"] {
            let listener = tokio::net::TcpListener::bind(address).await.unwrap();
            let address = listener.local_addr().unwrap();
            let target = Target {
                host: Host::Domain(address.ip().to_string()),
                port: address.port(),
            };
            let stream = resolver
                .connect_tcp(&target, Duration::from_secs(1))
                .await
                .unwrap();
            assert_eq!(stream.peer_addr().unwrap(), address);
        }
        assert_eq!(dns.queries[0].load(Ordering::Relaxed), 0);
        assert_eq!(dns.queries[1].load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn tcp_connect_uses_ipv6_while_a_queries_are_dropped() {
        let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
        let dns = scripted_dns(DnsReply::Drop, DnsReply::Answer(Duration::ZERO), 60).await;
        let resolver = DnsResolver::new(&[dns.address]).unwrap();
        let target = tcp_target(listener.local_addr().unwrap().port());

        let stream = tokio::time::timeout(
            Duration::from_millis(700),
            resolver.connect_tcp(&target, Duration::from_secs(2)),
        )
        .await
        .expect("可用 IPv6 地址被丢弃的 A 查询阻塞")
        .unwrap();
        assert_eq!(stream.peer_addr().unwrap(), listener.local_addr().unwrap());
        assert!(dns.queries[0].load(Ordering::Relaxed) >= 1);
        assert!(dns.queries[1].load(Ordering::Relaxed) >= 1);
    }

    #[tokio::test]
    async fn tcp_connect_keeps_delayed_other_family_when_first_address_is_unavailable() {
        for available_ipv6 in [false, true] {
            let address = if available_ipv6 {
                "[::1]:0"
            } else {
                "127.0.0.42:0"
            };
            let listener = tokio::net::TcpListener::bind(address).await.unwrap();
            let delayed = DnsReply::Answer(Duration::from_millis(120));
            let immediate = DnsReply::Answer(Duration::ZERO);
            let (ipv4, ipv6) = if available_ipv6 {
                (immediate, delayed)
            } else {
                (delayed, immediate)
            };
            let dns = scripted_dns(ipv4, ipv6, 60).await;
            let resolver = DnsResolver::new(&[dns.address]).unwrap();
            let target = tcp_target(listener.local_addr().unwrap().port());

            let stream = resolver
                .connect_tcp(&target, Duration::from_secs(2))
                .await
                .unwrap();
            assert_eq!(stream.peer_addr().unwrap(), listener.local_addr().unwrap());
            assert!(dns.queries[0].load(Ordering::Relaxed) >= 1);
            assert!(dns.queries[1].load(Ordering::Relaxed) >= 1);
        }
    }

    #[tokio::test]
    async fn tcp_connect_uses_cached_answers_until_ttl_expires() {
        let listener = tokio::net::TcpListener::bind("127.0.0.42:0").await.unwrap();
        let dns = scripted_dns(DnsReply::Answer(Duration::ZERO), DnsReply::Drop, 1).await;
        let resolver = DnsResolver::new(&[dns.address]).unwrap();
        let target = tcp_target(listener.local_addr().unwrap().port());

        resolver
            .connect_tcp(&target, Duration::from_secs(1))
            .await
            .unwrap();
        let first_queries = dns.queries[0].load(Ordering::Relaxed);
        resolver
            .connect_tcp(&target, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(dns.queries[0].load(Ordering::Relaxed), first_queries);
        tokio::time::sleep(Duration::from_millis(1100)).await;
        resolver
            .connect_tcp(&target, Duration::from_secs(1))
            .await
            .unwrap();
        assert!(dns.queries[0].load(Ordering::Relaxed) > first_queries);
    }

    #[tokio::test]
    async fn tcp_connect_total_timeout_includes_dns() {
        let dns = scripted_dns(DnsReply::Drop, DnsReply::Drop, 60).await;
        let resolver = DnsResolver::new(&[dns.address]).unwrap();
        let target = tcp_target(443);

        let error = tokio::time::timeout(
            Duration::from_millis(700),
            resolver.connect_tcp(&target, Duration::from_millis(120)),
        )
        .await
        .expect("DNS 查询超出了 TCP 连接总预算")
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(error.to_string(), "连接目标超时");
    }

    #[tokio::test]
    async fn tcp_connect_preserves_dns_no_records_error() {
        let no_records = DnsReply::NoRecords(Duration::ZERO);
        let dns = scripted_dns(no_records, no_records, 60).await;
        let resolver = DnsResolver::new(&[dns.address]).unwrap();

        let error = resolver
            .connect_tcp(&tcp_target(443), Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(
            error
                .get_ref()
                .and_then(|source| source.downcast_ref::<hickory_resolver::net::NetError>())
                .is_some_and(hickory_resolver::net::NetError::is_no_records_found)
        );
    }

    #[tokio::test]
    async fn tcp_connect_preserves_connection_refused_over_other_family_dns_error() {
        let listener = tokio::net::TcpListener::bind("127.0.0.42:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let dns = scripted_dns(
            DnsReply::Answer(Duration::ZERO),
            DnsReply::NoRecords(Duration::from_millis(120)),
            60,
        )
        .await;
        let resolver = DnsResolver::new(&[dns.address]).unwrap();

        let error = resolver
            .connect_tcp(&tcp_target(port), Duration::from_secs(5))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionRefused);
    }
}
