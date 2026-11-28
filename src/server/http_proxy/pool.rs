use super::{HANDSHAKE_TIMEOUT, MAX_HTTP_BUFFER_SIZE, ProxyBody};
use crate::dns::DnsResolver;
use crate::protocol::Target;
use http_body_util::BodyExt;
use hyper::StatusCode;
use hyper::body::{Body, Bytes, Frame, SizeHint};
use hyper::client::conn::http1::SendRequest;
use hyper_util::rt::TokioIo;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::time::Instant;
use tokio_util::sync::{CancellationToken, DropGuard};
use tokio_util::task::TaskTracker;

const MAX_IDLE_CONNECTIONS: usize = 64;
const MAX_IDLE_PER_TARGET: usize = 2;
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

pub(in crate::server) struct ConnectionPool {
    idle: Mutex<Vec<IdleConnection>>,
    stopped: CancellationToken,
    tasks: TaskTracker,
}

struct IdleConnection {
    target: Target,
    since: Instant,
    connection: Connection,
}

pub(super) struct Connection {
    pub sender: SendRequest<ProxyBody>,
    // 未完整消费响应体或淘汰空闲连接时，立即停止驱动任务并关闭套接字。
    _stop: DropGuard,
}

impl ConnectionPool {
    pub(in crate::server) fn new(stopped: CancellationToken) -> Arc<Self> {
        let pool = Arc::new(Self {
            idle: Mutex::new(Vec::new()),
            stopped,
            tasks: TaskTracker::new(),
        });
        let weak = Arc::downgrade(&pool);
        let stopped = pool.stopped.clone();
        pool.tasks.spawn(async move {
            let mut tick = tokio::time::interval(IDLE_TIMEOUT / 2);
            loop {
                tokio::select! {
                    _ = stopped.cancelled() => break,
                    _ = tick.tick() => {
                        let Some(pool) = weak.upgrade() else { break };
                        pool.idle.lock().unwrap().retain(IdleConnection::usable);
                    }
                }
            }
        });
        pool
    }

    pub(in crate::server) async fn shutdown(&self) {
        self.idle.lock().unwrap().clear();
        self.tasks.close();
        self.tasks.wait().await;
    }

    pub(super) async fn acquire(
        &self,
        target: &Target,
        dns: &DnsResolver,
    ) -> Result<Connection, StatusCode> {
        loop {
            let cached = {
                let mut idle = self.idle.lock().unwrap();
                idle.retain(IdleConnection::usable);
                idle.iter()
                    .rposition(|entry| &entry.target == target)
                    .map(|index| idle.swap_remove(index).connection)
            };
            let Some(mut connection) = cached else { break };
            // 响应体结束后，驱动任务仍可能正在完成请求体或更新就绪状态。
            if matches!(
                tokio::time::timeout(HANDSHAKE_TIMEOUT, connection.sender.ready()).await,
                Ok(Ok(()))
            ) {
                return Ok(connection);
            }
        }
        let remote = connect(dns, target).await?;
        let (sender, driver) = hyper::client::conn::http1::Builder::new()
            .max_buf_size(MAX_HTTP_BUFFER_SIZE)
            .handshake(TokioIo::new(remote))
            .await
            .map_err(|_| StatusCode::BAD_GATEWAY)?;
        let stopped = self.stopped.child_token();
        let guard = stopped.clone().drop_guard();
        self.tasks.spawn(async move {
            tokio::select! {
                _ = stopped.cancelled() => {}
                _ = driver => {}
            }
        });
        Ok(Connection {
            sender,
            _stop: guard,
        })
    }

    fn release(&self, target: Target, connection: Connection) {
        if self.stopped.is_cancelled() || connection.sender.is_closed() {
            return;
        }
        let mut idle = self.idle.lock().unwrap();
        idle.retain(IdleConnection::usable);
        if idle.len() < MAX_IDLE_CONNECTIONS
            && idle.iter().filter(|entry| entry.target == target).count() < MAX_IDLE_PER_TARGET
        {
            idle.push(IdleConnection {
                target,
                since: Instant::now(),
                connection,
            });
        }
    }

    pub(super) fn response_body(
        self: &Arc<Self>,
        target: Target,
        connection: Connection,
        body: ProxyBody,
    ) -> ProxyBody {
        let mut response = PooledBody {
            body,
            lease: Some((target, connection)),
            pool: Arc::clone(self),
        };
        if response.body.is_end_stream() {
            response.release();
        }
        response.boxed_unsync()
    }
}

impl IdleConnection {
    fn usable(&self) -> bool {
        self.since.elapsed() < IDLE_TIMEOUT && !self.connection.sender.is_closed()
    }
}

struct PooledBody {
    body: ProxyBody,
    lease: Option<(Target, Connection)>,
    pool: Arc<ConnectionPool>,
}

impl PooledBody {
    fn release(&mut self) {
        if let Some((target, connection)) = self.lease.take() {
            self.pool.release(target, connection);
        }
    }
}

impl Body for PooledBody {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, hyper::Error>>> {
        let result = Pin::new(&mut self.body).poll_frame(cx);
        match &result {
            Poll::Ready(Some(Err(_))) => {
                self.lease.take();
            }
            Poll::Ready(None) => self.release(),
            Poll::Ready(Some(Ok(_))) if self.body.is_end_stream() => self.release(),
            _ => {}
        }
        result
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}

pub(super) async fn connect(dns: &DnsResolver, target: &Target) -> Result<TcpStream, StatusCode> {
    let remote = dns
        .connect_tcp(target, HANDSHAKE_TIMEOUT)
        .await
        .map_err(|error| match error.kind() {
            io::ErrorKind::TimedOut => StatusCode::GATEWAY_TIMEOUT,
            _ => StatusCode::BAD_GATEWAY,
        })?;
    remote
        .set_nodelay(true)
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    Ok(remote)
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::Empty;
    use hyper::Request;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn incomplete_response_is_closed_and_idle_connections_are_bounded() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = Target::from(listener.local_addr().unwrap());
        let stopped = CancellationToken::new();
        let pool = ConnectionPool::new(stopped.clone());
        let dns = DnsResolver::new(&["127.0.0.1:53".parse().unwrap()]).unwrap();
        let origin = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nx")
                .await
                .unwrap();
            assert_eq!(stream.read(&mut request).await.unwrap(), 0);
        });
        let mut connection = pool.acquire(&target, &dns).await.unwrap();
        let response = connection
            .sender
            .send_request(
                Request::builder()
                    .uri("/")
                    .header("Host", "local")
                    .body(
                        Empty::<Bytes>::new()
                            .map_err(|never| match never {})
                            .boxed_unsync(),
                    )
                    .unwrap(),
            )
            .await
            .unwrap();
        drop(pool.response_body(
            target.clone(),
            connection,
            response.into_body().boxed_unsync(),
        ));
        tokio::time::timeout(Duration::from_secs(1), origin)
            .await
            .unwrap()
            .unwrap();
        assert!(pool.idle.lock().unwrap().is_empty());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = Target::from(listener.local_addr().unwrap());
        let mut connections = Vec::new();
        for _ in 0..MAX_IDLE_PER_TARGET + 2 {
            connections.push(pool.acquire(&target, &dns).await.unwrap());
        }
        for connection in connections {
            pool.release(target.clone(), connection);
        }
        assert_eq!(pool.idle.lock().unwrap().len(), MAX_IDLE_PER_TARGET);
        {
            let mut idle = pool.idle.lock().unwrap();
            idle[0].since = Instant::now() - IDLE_TIMEOUT;
            idle.retain(IdleConnection::usable);
            assert_eq!(idle.len(), MAX_IDLE_PER_TARGET - 1);
        }
        stopped.cancel();
        tokio::time::timeout(Duration::from_secs(1), pool.shutdown())
            .await
            .unwrap();
    }
}
