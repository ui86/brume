use futures_util::StreamExt;
use hickory_resolver::config::{ConnectionConfig, ProtocolConfig};
use hickory_resolver::net::NetError;
use hickory_resolver::net::runtime::RuntimeProvider;
use hickory_resolver::net::tcp::TcpClientStream;
use hickory_resolver::net::xfer::{DnsExchange, DnsHandle, DnsResponseStream};
use hickory_resolver::proto::op::DnsRequest;
use hickory_resolver::{ConnectionProvider, PoolContext};
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::OnceCell;

/// 在单个上游内部完成截断回退，避免并行上游的截断响应互相干扰。
#[derive(Clone)]
pub(crate) struct DnsTransport<R>(pub R);

#[derive(Clone)]
pub(crate) struct Connection<R: RuntimeProvider> {
    primary: DnsExchange<R>,
    fallback: Option<Arc<TcpFallback<R>>>,
}

struct TcpFallback<R: RuntimeProvider> {
    runtime: R,
    address: SocketAddr,
    bind_addr: Option<SocketAddr>,
    timeout: Duration,
    max_active_requests: usize,
    connection: OnceCell<DnsExchange<R>>,
}

impl<R: RuntimeProvider> ConnectionProvider for DnsTransport<R> {
    type Conn = Connection<R>;
    type FutureConn = Pin<Box<dyn Future<Output = Result<Self::Conn, NetError>> + Send>>;
    type RuntimeProvider = R;

    fn new_connection(
        &self,
        ip: IpAddr,
        config: &ConnectionConfig,
        context: &PoolContext,
    ) -> Result<Self::FutureConn, NetError> {
        let primary = self.0.new_connection(ip, config, context)?;
        let fallback = matches!(&config.protocol, ProtocolConfig::Udp).then(|| {
            Arc::new(TcpFallback {
                runtime: self.0.clone(),
                address: SocketAddr::new(ip, config.port),
                bind_addr: config.bind_addr,
                timeout: context.options.timeout,
                max_active_requests: context.options.max_active_requests,
                connection: OnceCell::new(),
            })
        });
        Ok(Box::pin(async move {
            Ok(Connection {
                primary: primary.await?,
                fallback,
            })
        }))
    }

    fn runtime_provider(&self) -> &R {
        &self.0
    }
}

impl<R: RuntimeProvider> DnsHandle for Connection<R> {
    type Response = DnsResponseStream;
    type Runtime = R;

    fn send(&self, request: DnsRequest) -> Self::Response {
        let primary = self.primary.clone();
        let fallback = self.fallback.clone();
        DnsResponseStream::from(Box::pin(async move {
            let response = primary
                .send(request.clone())
                .next()
                .await
                .ok_or(NetError::NoConnections)??;
            if !response.truncation {
                return Ok(response);
            }
            let Some(fallback) = fallback else {
                return Err(NetError::Truncated);
            };
            // 同一上游的并发查询共用回退连接；建立过程随调用取消，传输由原运行时跟踪。
            let connection = fallback
                .connection
                .get_or_try_init(|| {
                    TcpClientStream::exchange(
                        fallback.address,
                        fallback.bind_addr,
                        fallback.timeout,
                        Some(fallback.max_active_requests),
                        fallback.runtime.clone(),
                    )
                })
                .await?;
            let response = connection
                .send(request)
                .next()
                .await
                .ok_or(NetError::NoConnections)??;
            if response.truncation {
                return Err(NetError::Truncated);
            }
            Ok(response)
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dns::DnsResolver;
    use crate::protocol::{Host, Target};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, UdpSocket};
    use tokio::sync::Barrier;
    use tokio::task::{JoinHandle, JoinSet};

    struct Upstream {
        address: SocketAddr,
        tcp_requests: Arc<AtomicUsize>,
        task: JoinHandle<()>,
    }

    impl Drop for Upstream {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    fn answer(query: &[u8], truncated: bool) -> Vec<u8> {
        let mut end = 12;
        while query[end] != 0 {
            end += usize::from(query[end]) + 1;
        }
        let record_type = u16::from_be_bytes([query[end + 1], query[end + 2]]);
        let mut response = query[..2].to_vec();
        response.extend_from_slice(&[
            0x81 | if truncated { 2 } else { 0 },
            0x80,
            0,
            1,
            0,
            0,
            0,
            0,
            0,
            0,
        ]);
        response.extend_from_slice(&query[12..end + 5]);
        if !truncated {
            response[7] = 1;
            response.extend_from_slice(&[0xc0, 0x0c]);
            response.extend_from_slice(&record_type.to_be_bytes());
            response.extend_from_slice(&[0, 1, 0, 0, 0, 60]);
            if record_type == 1 {
                response.extend_from_slice(&[0, 4, 127, 0, 0, 42]);
            } else {
                response.extend_from_slice(&[0, 16]);
                response.extend_from_slice(&std::net::Ipv6Addr::LOCALHOST.octets());
            }
        }
        response
    }

    async fn upstream(ip: &str, barrier: Arc<Barrier>, tcp_truncated: bool) -> Upstream {
        let tcp = TcpListener::bind((ip, 0)).await.unwrap();
        let address = tcp.local_addr().unwrap();
        let udp = Arc::new(UdpSocket::bind(address).await.unwrap());
        let tcp_requests = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&tcp_requests);
        let task = tokio::spawn(async move {
            let mut workers = JoinSet::new();
            let mut packet = [0; 4096];
            loop {
                tokio::select! {
                    result = udp.recv_from(&mut packet) => {
                        let (length, source) = result.unwrap();
                        let response = answer(&packet[..length], true);
                        let udp = Arc::clone(&udp);
                        let barrier = Arc::clone(&barrier);
                        workers.spawn(async move {
                            // 两台服务器同时返回截断，确保覆盖并行批次内的回退竞争。
                            barrier.wait().await;
                            udp.send_to(&response, source).await.unwrap();
                        });
                    }
                    result = tcp.accept() => {
                        let (mut stream, _) = result.unwrap();
                        let count = Arc::clone(&count);
                        workers.spawn(async move {
                            while let Ok(length) = stream.read_u16().await {
                                let mut query = vec![0; usize::from(length)];
                                stream.read_exact(&mut query).await.unwrap();
                                count.fetch_add(1, Ordering::Relaxed);
                                let response = answer(&query, tcp_truncated);
                                stream.write_u16(response.len() as u16).await.unwrap();
                                stream.write_all(&response).await.unwrap();
                            }
                        });
                    }
                    _ = workers.join_next(), if !workers.is_empty() => {}
                }
            }
        });
        Upstream {
            address,
            tcp_requests,
            task,
        }
    }

    #[tokio::test]
    async fn concurrent_truncated_servers_fall_back_to_tcp_and_cache_answers() {
        let barrier = Arc::new(Barrier::new(2));
        let first = upstream("127.0.0.1", Arc::clone(&barrier), false).await;
        let second = upstream("127.0.0.2", barrier, false).await;
        let resolver = DnsResolver::new(&[first.address, second.address]).unwrap();
        let target = Target {
            host: Host::Domain("large.example.".into()),
            port: 1080,
        };
        let addresses = tokio::time::timeout(Duration::from_millis(700), resolver.lookup(&target))
            .await
            .unwrap()
            .unwrap();
        assert!(addresses.contains(&"127.0.0.42:1080".parse().unwrap()));
        assert!(addresses.contains(&"[::1]:1080".parse().unwrap()));
        assert!(
            first.tcp_requests.load(Ordering::Relaxed)
                + second.tcp_requests.load(Ordering::Relaxed)
                >= 2
        );
        drop((first, second));
        assert_eq!(resolver.lookup(&target).await.unwrap(), addresses);
    }

    #[tokio::test]
    async fn truncated_tcp_answers_return_an_error_without_retry_loops() {
        let barrier = Arc::new(Barrier::new(2));
        let first = upstream("127.0.0.1", Arc::clone(&barrier), true).await;
        let second = upstream("127.0.0.2", barrier, true).await;
        let resolver = DnsResolver::new(&[first.address, second.address]).unwrap();
        let target = Target {
            host: Host::Domain("truncated.example.".into()),
            port: 1080,
        };
        let error = tokio::time::timeout(Duration::from_millis(700), resolver.lookup(&target))
            .await
            .unwrap()
            .unwrap_err();
        assert!(matches!(
            error
                .get_ref()
                .and_then(|error| error.downcast_ref::<NetError>()),
            Some(NetError::Truncated)
        ));
    }
}
