use crate::protocol::{Host, Target};
use hickory_resolver::TokioResolver;
use hickory_resolver::config::{
    ConnectionConfig, NameServerConfig, ResolveHosts, ResolverConfig, ResolverOpts,
};
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use std::io;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::runtime::{Builder, Runtime};

pub struct DnsResolver {
    runtime: Runtime,
    resolver: TokioResolver,
}

impl DnsResolver {
    pub fn new(servers: &[SocketAddr]) -> io::Result<Self> {
        let runtime = Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
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
        let mut builder = TokioResolver::builder_with_config(
            ResolverConfig::from_name_servers(nameservers),
            TokioRuntimeProvider::default(),
        );
        let opts: &mut ResolverOpts = builder.options_mut();
        opts.timeout = Duration::from_secs(3);
        opts.attempts = 2;
        opts.use_hosts_file = ResolveHosts::Never;
        let resolver = builder.build().map_err(io::Error::other)?;
        Ok(Self { runtime, resolver })
    }

    pub fn lookup(&self, target: &Target) -> io::Result<Vec<SocketAddr>> {
        let addresses: Vec<_> = match &target.host {
            Host::Ip(ip) => vec![SocketAddr::new(*ip, target.port)],
            Host::Domain(name) => self
                .runtime
                .block_on(self.resolver.lookup_ip(name.as_str()))
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::UdpSocket;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::thread;

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

    #[test]
    fn custom_server_resolves_domain_without_system_dns() {
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
        let result = resolver.lookup(&target).unwrap();
        assert!(result.contains(&"127.0.0.42:443".parse().unwrap()));
        thread::sleep(Duration::from_millis(50));
        let first_queries = queries.load(Ordering::Relaxed);
        assert!(first_queries >= 1);
        assert!(
            resolver
                .lookup(&target)
                .unwrap()
                .contains(&"127.0.0.42:443".parse().unwrap())
        );
        assert_eq!(queries.load(Ordering::Relaxed), first_queries);
        done.store(true, Ordering::Relaxed);
        worker.join().unwrap();
    }
}
