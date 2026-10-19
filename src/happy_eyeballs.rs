use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::task::JoinSet;

/// RFC 8305 推荐的阶梯连接延迟（250 毫秒）
pub const HAPPY_EYEBALLS_DELAY: Duration = Duration::from_millis(250);

/// 对目标地址列表进行交替排序：优先 IPv6，交替排列 IPv6 与 IPv4
pub fn interleave_addresses(addresses: &[SocketAddr]) -> Vec<SocketAddr> {
    let mut v6_addrs = Vec::new();
    let mut v4_addrs = Vec::new();

    for &addr in addresses {
        if addr.is_ipv6() {
            v6_addrs.push(addr);
        } else {
            v4_addrs.push(addr);
        }
    }

    let mut ordered = Vec::with_capacity(addresses.len());
    let mut v6_iter = v6_addrs.into_iter();
    let mut v4_iter = v4_addrs.into_iter();

    loop {
        let mut added = false;
        if let Some(v6) = v6_iter.next() {
            ordered.push(v6);
            added = true;
        }
        if let Some(v4) = v4_iter.next() {
            ordered.push(v4);
            added = true;
        }
        if !added {
            break;
        }
    }

    ordered
}

/// 执行 Happy Eyeballs 双栈并发竞速连接
pub async fn happy_eyeballs_connect(
    addresses: &[SocketAddr],
    total_timeout: Duration,
) -> io::Result<TcpStream> {
    if addresses.is_empty() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "目标地址列表为空"));
    }

    // 单个地址快速通道
    if addresses.len() == 1 {
        return tokio::time::timeout(total_timeout, TcpStream::connect(addresses[0]))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "连接目标超时"))?;
    }

    let ordered = interleave_addresses(addresses);

    tokio::time::timeout(total_timeout, race_connect(ordered))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "连接目标超时"))?
}

enum FamilyConnectError {
    Resolve(io::Error),
    Connect(io::Error),
}

/// 各地址族解析完成后立即连接，另一族保留到成功或全部失败
pub(crate) async fn happy_eyeballs_connect_resolving<V6, V4>(
    ipv6: V6,
    ipv4: V4,
    total_timeout: Duration,
) -> io::Result<TcpStream>
where
    V6: Future<Output = io::Result<Vec<SocketAddr>>>,
    V4: Future<Output = io::Result<Vec<SocketAddr>>>,
{
    async fn connect_family(
        lookup: impl Future<Output = io::Result<Vec<SocketAddr>>>,
        timeout: Duration,
    ) -> Result<TcpStream, FamilyConnectError> {
        let addresses = lookup.await.map_err(FamilyConnectError::Resolve)?;
        happy_eyeballs_connect(&addresses, timeout)
            .await
            .map_err(FamilyConnectError::Connect)
    }

    let race = async {
        let ipv6 = connect_family(ipv6, total_timeout);
        let ipv4 = connect_family(ipv4, total_timeout);
        tokio::pin!(ipv6, ipv4);
        let (first, second) = tokio::select! {
            result = &mut ipv6 => match result {
                Ok(stream) => return Ok(stream),
                Err(error) => (error, ipv4.await),
            },
            result = &mut ipv4 => match result {
                Ok(stream) => return Ok(stream),
                Err(error) => (error, ipv6.await),
            },
        };
        match second {
            Ok(stream) => Ok(stream),
            Err(second) => {
                // 已尝试连接时优先保留连接错误，避免无记录响应掩盖连接拒绝。
                let error = match (first, second) {
                    (FamilyConnectError::Connect(first), FamilyConnectError::Connect(second)) => {
                        if first.kind() == io::ErrorKind::ConnectionRefused {
                            first
                        } else {
                            second
                        }
                    }
                    (FamilyConnectError::Connect(error), _)
                    | (_, FamilyConnectError::Connect(error))
                    | (_, FamilyConnectError::Resolve(error)) => error,
                };
                Err(error)
            }
        }
    };
    // 总预算同时覆盖解析与连接，避免 DNS 重试额外延长连接超时。
    tokio::time::timeout(total_timeout, race)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "连接目标超时"))?
}

async fn race_connect(addresses: Vec<SocketAddr>) -> io::Result<TcpStream> {
    let mut join_set: JoinSet<io::Result<TcpStream>> = JoinSet::new();
    let mut last_error = None;
    let mut addr_iter = addresses.into_iter();

    // 启动首选连接尝试
    if let Some(first_addr) = addr_iter.next() {
        join_set.spawn(async move { TcpStream::connect(first_addr).await });
    }

    let mut next_candidate = addr_iter.next();

    while next_candidate.is_some() || !join_set.is_empty() {
        if let Some(next_addr) = next_candidate {
            tokio::select! {
                Some(join_res) = join_set.join_next() => {
                    match join_res {
                        Ok(Ok(stream)) => {
                            join_set.abort_all();
                            return Ok(stream);
                        }
                        Ok(Err(e)) => {
                            last_error = Some(e);
                            // 当前连接失败，立即发起下一个地址，避免无谓等待
                            join_set.spawn(async move { TcpStream::connect(next_addr).await });
                            next_candidate = addr_iter.next();
                        }
                        Err(join_err) => {
                            last_error = Some(io::Error::other(join_err));
                            join_set.spawn(async move { TcpStream::connect(next_addr).await });
                            next_candidate = addr_iter.next();
                        }
                    }
                }
                _ = tokio::time::sleep(HAPPY_EYEBALLS_DELAY) => {
                    // 到达阶梯延迟，并发启动下一个候选连接进行竞速
                    join_set.spawn(async move { TcpStream::connect(next_addr).await });
                    next_candidate = addr_iter.next();
                }
            }
        } else {
            // 所有候选地址均已发起，等待剩余的任务完成
            match join_set.join_next().await {
                Some(Ok(Ok(stream))) => {
                    join_set.abort_all();
                    return Ok(stream);
                }
                Some(Ok(Err(e))) => {
                    last_error = Some(e);
                }
                Some(Err(join_err)) => {
                    last_error = Some(io::Error::other(join_err));
                }
                None => break,
            }
        }
    }

    Err(last_error.unwrap_or_else(|| io::Error::other("所有目标连接尝试均失败")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[test]
    fn test_interleave_addresses() {
        let v4_1: SocketAddr = "127.0.0.1:80".parse().unwrap();
        let v4_2: SocketAddr = "127.0.0.2:80".parse().unwrap();
        let v6_1: SocketAddr = "[::1]:80".parse().unwrap();
        let v6_2: SocketAddr = "[::2]:80".parse().unwrap();

        let input = vec![v4_1, v4_2, v6_1, v6_2];
        let ordered = interleave_addresses(&input);
        // 应该交替排列，且 IPv6 优先
        assert_eq!(ordered, vec![v6_1, v4_1, v6_2, v4_2]);
    }

    #[tokio::test]
    async fn test_race_connect_single_success() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let stream = happy_eyeballs_connect(&[addr], Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(stream.peer_addr().unwrap(), addr);
    }

    #[tokio::test]
    async fn test_race_connect_fallback_to_second_on_failure() {
        // 第一个地址使用不可达的端口，第二个地址正常监听
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let good_addr = listener.local_addr().unwrap();
        // 找一个通常不可达/无监听的地址端口
        let bad_addr: SocketAddr = "127.0.0.1:1".parse().unwrap();

        let stream = happy_eyeballs_connect(&[bad_addr, good_addr], Duration::from_secs(3))
            .await
            .unwrap();
        assert_eq!(stream.peer_addr().unwrap(), good_addr);
    }
}
