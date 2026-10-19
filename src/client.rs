use crate::client_dns::ClientDns;
use crate::protocol::{self, Host, Target};
use socket2::{Domain, Protocol, Socket, Type};
use std::future::Future;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream, UdpSocket};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::Duration;
use tokio::runtime::{Builder, Runtime};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const UDP_BUFFER_SIZE: usize = 65_535;
static CONNECT_RUNTIME: OnceLock<Result<Runtime, String>> = OnceLock::new();

fn connector_runtime() -> io::Result<&'static Runtime> {
    CONNECT_RUNTIME
        .get_or_init(|| {
            Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(|error| io::Error::other(error.clone()))
}

fn connect_proxy(server: String, dns: Arc<ClientDns>, timeout: Duration) -> io::Result<TcpStream> {
    if let Ok(address) = server.parse::<SocketAddr>() {
        return TcpStream::connect_timeout(&address, timeout);
    }
    let (sender, receiver) = mpsc::sync_channel(1);
    let task = connector_runtime()?.spawn(async move {
        let result = tokio::time::timeout(timeout, async {
            let stream = dns.connect_server(&server, timeout).await?;
            stream.into_std()
        })
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "连接代理服务器超时"))
        .and_then(|result| result);
        let _ = sender.send(result);
    });
    let stream = match receiver.recv_timeout(timeout + Duration::from_secs(1)) {
        Ok(result) => result?,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            task.abort();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "连接代理服务器超时",
            ));
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            return Err(io::Error::other("代理连接任务意外退出"));
        }
    };
    stream.set_nonblocking(false)?;
    Ok(stream)
}

fn resolve_udp_relay_with_timeout<F>(
    lookup: F,
    preferred_ipv4: bool,
    timeout: Duration,
) -> io::Result<SocketAddr>
where
    F: Future<Output = io::Result<Vec<SocketAddr>>> + Send + 'static,
{
    let (sender, receiver) = mpsc::sync_channel(1);
    let task = connector_runtime()?.spawn(async move {
        let result = tokio::time::timeout(timeout, lookup)
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "解析 UDP 中转地址超时"))
            .and_then(|result| result)
            .and_then(|mut addresses| {
                for address in &mut addresses {
                    *address = normalize_udp_relay(*address);
                }
                addresses
                    .iter()
                    .find(|address| address.is_ipv4() == preferred_ipv4)
                    .or_else(|| addresses.first())
                    .copied()
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::NotFound, "UDP 中转地址没有解析结果")
                    })
            });
        let _ = sender.send(result);
    });
    match receiver.recv_timeout(timeout) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            task.abort();
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "解析 UDP 中转地址超时",
            ))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err(io::Error::other("UDP 中转地址解析任务意外退出"))
        }
    }
}

fn normalize_udp_relay(address: SocketAddr) -> SocketAddr {
    match address.ip() {
        IpAddr::V6(ip) => ip
            .to_ipv4_mapped()
            .map(|ipv4| SocketAddr::new(IpAddr::V4(ipv4), address.port()))
            .unwrap_or(address),
        IpAddr::V4(_) => address,
    }
}

fn bind_udp_socket(address: SocketAddr) -> io::Result<UdpSocket> {
    let socket = Socket::new(
        Domain::for_address(address),
        Type::DGRAM,
        Some(Protocol::UDP),
    )?;
    if address.is_ipv6() {
        // 分别保留两种协议族的端口，切换中继地址时无需更改已声明的端口
        socket.set_only_v6(true)?;
    }
    socket.bind(&address.into())?;
    Ok(socket.into())
}

pub struct Client {
    server: String,
    username: String,
    password: String,
    tcp_timeout: Option<Duration>,
    udp_timeout: Option<Duration>,
    dns: Arc<ClientDns>,
}

pub struct UdpAssociation {
    _control: TcpStream,
    socket: UdpSocket,
    target: Target,
    relay: SocketAddr,
    send_buffer: Mutex<Vec<u8>>,
    recv_buffer: Mutex<Vec<u8>>,
}

impl Client {
    pub fn new(
        server: impl Into<String>,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> io::Result<Self> {
        let username = username.into();
        let password = password.into();
        if username.is_empty() != password.is_empty()
            || username.len() > 255
            || password.len() > 255
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "用户名和密码必须同时设置，且长度不能超过 255 字节",
            ));
        }
        Ok(Self {
            server: server.into(),
            username,
            password,
            tcp_timeout: None,
            udp_timeout: Some(Duration::from_secs(60)),
            dns: Arc::new(ClientDns::default()),
        })
    }

    pub fn set_tcp_timeout(&mut self, timeout: Option<Duration>) {
        self.tcp_timeout = timeout;
    }

    pub fn set_udp_timeout(&mut self, timeout: Option<Duration>) {
        self.udp_timeout = timeout;
    }

    fn negotiate(&self) -> io::Result<TcpStream> {
        let mut stream =
            connect_proxy(self.server.clone(), Arc::clone(&self.dns), CONNECT_TIMEOUT)?;
        // SOCKS5 协商使用多个小报文，关闭 Nagle 以减少交互等待
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(CONNECT_TIMEOUT))?;
        stream.set_write_timeout(Some(CONNECT_TIMEOUT))?;
        let method = if self.username.is_empty() { 0 } else { 2 };
        stream.write_all(&[5, 1, method])?;
        let mut reply = [0; 2];
        stream.read_exact(&mut reply)?;
        if reply != [5, method] {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "代理服务器未接受认证方法",
            ));
        }
        if method == 2 {
            let mut request = Vec::with_capacity(3 + self.username.len() + self.password.len());
            request.extend_from_slice(&[1, self.username.len() as u8]);
            request.extend_from_slice(self.username.as_bytes());
            request.push(self.password.len() as u8);
            request.extend_from_slice(self.password.as_bytes());
            stream.write_all(&request)?;
            stream.read_exact(&mut reply)?;
            if reply != [1, 0] {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "代理服务器拒绝用户名或密码",
                ));
            }
        }
        Ok(stream)
    }

    pub fn connect(&self, target: Target) -> io::Result<TcpStream> {
        let mut stream = self.negotiate()?;
        protocol::write_request(&mut stream, protocol::CONNECT, &target)?;
        let (status, _) = protocol::read_reply(&mut stream)?;
        check_status(status)?;
        stream.set_read_timeout(self.tcp_timeout)?;
        stream.set_write_timeout(self.tcp_timeout)?;
        Ok(stream)
    }

    pub fn associate(&self, target: Target) -> io::Result<UdpAssociation> {
        let mut control = self.negotiate()?;
        let peer = control.peer_addr()?;
        let bind = unspecified(control.local_addr()?.ip());
        let mut socket = bind_udp_socket(bind)?;
        let request = Target {
            host: Host::Ip(bind.ip()),
            port: socket.local_addr()?.port(),
        };
        protocol::write_request(&mut control, protocol::UDP_ASSOCIATE, &request)?;
        let (status, reply) = protocol::read_reply(&mut control)?;
        check_status(status)?;
        let relay = normalize_udp_relay(match reply.host {
            Host::Ip(ip) if !ip.is_unspecified() => SocketAddr::new(ip, reply.port),
            Host::Ip(_) => SocketAddr::new(peer.ip(), reply.port),
            Host::Domain(name) => {
                let dns = Arc::clone(&self.dns);
                resolve_udp_relay_with_timeout(
                    async move { dns.lookup(&name, reply.port, bind.is_ipv4()).await },
                    bind.is_ipv4(),
                    CONNECT_TIMEOUT,
                )?
            }
        });
        if relay.is_ipv4() != bind.is_ipv4() {
            let mut relay_bind = unspecified(relay.ip());
            relay_bind.set_port(request.port);
            // 新协议族绑定成功前保留原套接字，端口冲突时明确失败
            socket = bind_udp_socket(relay_bind).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("无法为 UDP 中继保留已声明端口 {}：{error}", request.port),
                )
            })?;
        }
        socket.connect(relay)?;
        socket.set_read_timeout(self.udp_timeout)?;
        socket.set_write_timeout(self.udp_timeout)?;
        control.set_read_timeout(None)?;
        control.set_write_timeout(None)?;
        Ok(UdpAssociation {
            _control: control,
            socket,
            target,
            relay,
            send_buffer: Mutex::new(Vec::new()),
            recv_buffer: Mutex::new(vec![0; UDP_BUFFER_SIZE]),
        })
    }
}

impl UdpAssociation {
    pub fn send(&self, payload: &[u8]) -> io::Result<usize> {
        let mut packet = self.send_buffer.lock().unwrap();
        protocol::append_datagram_to(&mut packet, &self.target, payload)?;
        let length = self.socket.send(&packet)?;
        if length != packet.len() {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "UDP 数据报未完整发送",
            ));
        }
        Ok(payload.len())
    }

    pub fn recv(&self, payload: &mut [u8]) -> io::Result<(usize, Target)> {
        let mut buffer = self.recv_buffer.lock().unwrap();
        let length = self.socket.recv(&mut buffer)?;
        let (target, data) = protocol::parse_datagram(&buffer[..length])?;
        if data.len() > payload.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "接收缓冲区不足",
            ));
        }
        payload[..data.len()].copy_from_slice(data);
        Ok((data.len(), target))
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    pub fn relay_addr(&self) -> SocketAddr {
        self.relay
    }
}

fn check_status(status: u8) -> io::Result<()> {
    if status == protocol::SUCCESS {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "SOCKS5 请求失败，状态码 {status}"
        )))
    }
}

fn unspecified(ip: IpAddr) -> SocketAddr {
    match ip {
        IpAddr::V4(_) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
        IpAddr::V6(_) => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Whitelist};
    use crate::server::Server;
    use std::net::{TcpListener, UdpSocket};
    use std::thread;
    use tokio_util::sync::CancellationToken;

    fn start_server() -> (
        String,
        CancellationToken,
        thread::JoinHandle<io::Result<()>>,
    ) {
        let config = Config {
            port: 0,
            username: "admin".into(),
            password: "secret".into(),
            whitelist: Whitelist::parse("127.0.0.1").unwrap(),
            tcp_timeout: None,
            udp_timeout: Duration::from_secs(3),
            dns_servers: vec!["127.0.0.1:53".parse().unwrap()],
        };
        let shutdown = CancellationToken::new();
        let server = Server::bind(config, shutdown.clone()).unwrap();
        let address = format!("127.0.0.1:{}", server.local_addr().unwrap().port());
        let handle = thread::spawn(move || server.run());
        (address, shutdown, handle)
    }

    #[test]
    fn client_connects_and_authenticates() {
        let echo = TcpListener::bind("127.0.0.1:0").unwrap();
        let target = Target::from(echo.local_addr().unwrap());
        let echo_thread = thread::spawn(move || {
            let (mut stream, _) = echo.accept().unwrap();
            let mut payload = [0; 5];
            stream.read_exact(&mut payload).unwrap();
            stream.write_all(&payload).unwrap();
        });
        let (address, shutdown, server_thread) = start_server();
        let client = Client::new(&address, "admin", "secret").unwrap();
        let mut stream = client.connect(target).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream.write_all(b"hello").unwrap();
        let mut response = [0; 5];
        stream.read_exact(&mut response).unwrap();
        assert_eq!(&response, b"hello");
        echo_thread.join().unwrap();
        shutdown.cancel();
        server_thread.join().unwrap().unwrap();
    }

    #[test]
    fn client_associates_and_exchanges_udp() {
        let echo = UdpSocket::bind("127.0.0.1:0").unwrap();
        echo.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let destination = echo.local_addr().unwrap();
        let target = Target {
            host: Host::Domain("127.0.0.1".into()),
            port: destination.port(),
        };
        let echo_thread = thread::spawn(move || {
            let mut payload = [0; 100];
            for _ in 0..3 {
                let (length, source) = echo.recv_from(&mut payload).unwrap();
                echo.send_to(&payload[..length], source).unwrap();
            }
        });
        let (address, shutdown, server_thread) = start_server();
        let mut client = Client::new(&address, "admin", "secret").unwrap();
        client.set_udp_timeout(Some(Duration::from_secs(3)));
        let association = client.associate(target.clone()).unwrap();
        for message in [b"hello".as_slice(), [].as_slice(), b"again".as_slice()] {
            assert_eq!(association.send(message).unwrap(), message.len());
            let mut payload = [0; 100];
            let (length, source) = association.recv(&mut payload).unwrap();
            assert_eq!(&payload[..length], message);
            assert_eq!(source, Target::from(destination));
        }
        echo_thread.join().unwrap();
        drop(association);
        shutdown.cancel();
        server_thread.join().unwrap().unwrap();
    }

    #[test]
    fn rejects_incomplete_credentials() {
        assert!(Client::new("127.0.0.1:1080", "admin", "").is_err());
    }

    #[test]
    fn udp_associate_declares_local_port() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut greeting = [0; 3];
            stream.read_exact(&mut greeting).unwrap();
            assert_eq!(greeting, [5, 1, 0]);
            stream.write_all(&[5, 0]).unwrap();
            let (command, request) = protocol::read_request(&mut stream).unwrap();
            assert_eq!(command, protocol::UDP_ASSOCIATE);
            protocol::write_reply(&mut stream, protocol::SUCCESS, proxy).unwrap();
            request.port
        });
        let client = Client::new(proxy.to_string(), "", "").unwrap();
        let association = client
            .associate(Target::from("127.0.0.1:53".parse::<SocketAddr>().unwrap()))
            .unwrap();
        assert_eq!(
            server.join().unwrap(),
            association.local_addr().unwrap().port()
        );
    }

    fn assert_udp_relay_works(control_bind: &str, relay_bind: &str, domain_reply: bool) {
        let listener = TcpListener::bind(control_bind).unwrap();
        let proxy = listener.local_addr().unwrap();
        let relay = UdpSocket::bind(relay_bind).unwrap();
        relay
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let relay_address = relay.local_addr().unwrap();
        let destination: SocketAddr = "127.0.0.1:53".parse().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut greeting = [0; 3];
            stream.read_exact(&mut greeting).unwrap();
            assert_eq!(greeting, [5, 1, 0]);
            stream.write_all(&[5, 0]).unwrap();
            let (command, request) = protocol::read_request(&mut stream).unwrap();
            assert_eq!(command, protocol::UDP_ASSOCIATE);
            assert_ne!(request.port, 0);
            if domain_reply {
                let name = b"localhost";
                let mut reply = vec![5, protocol::SUCCESS, 0, 3, name.len() as u8];
                reply.extend_from_slice(name);
                reply.extend_from_slice(&relay_address.port().to_be_bytes());
                stream.write_all(&reply).unwrap();
            } else {
                protocol::write_reply(&mut stream, protocol::SUCCESS, relay_address).unwrap();
            }
            let mut packet = [0; 100];
            let (length, source) = relay.recv_from(&mut packet).unwrap();
            assert_eq!(source.port(), request.port);
            assert_eq!(
                protocol::parse_datagram(&packet[..length]).unwrap(),
                (Target::from(destination), b"hello".as_slice())
            );
            relay
                .send_to(&protocol::encode_datagram(destination, b"world"), source)
                .unwrap();
            request.port
        });
        let mut client = Client::new(proxy.to_string(), "", "").unwrap();
        client.set_udp_timeout(Some(Duration::from_secs(3)));
        let association = client.associate(Target::from(destination)).unwrap();
        assert_eq!(association.relay_addr(), relay_address);
        assert_eq!(
            association.local_addr().unwrap().is_ipv4(),
            relay_address.is_ipv4()
        );
        association.send(b"hello").unwrap();
        let mut payload = [0; 100];
        let (length, source) = association.recv(&mut payload).unwrap();
        assert_eq!(&payload[..length], b"world");
        assert_eq!(source, Target::from(destination));
        assert_eq!(
            association.local_addr().unwrap().port(),
            server.join().unwrap()
        );
    }

    #[test]
    fn udp_associate_supports_ipv4_control_and_ipv6_relay() {
        assert_udp_relay_works("127.0.0.1:0", "[::1]:0", false);
    }

    #[test]
    fn udp_associate_supports_ipv6_control_and_ipv4_relay() {
        assert_udp_relay_works("[::1]:0", "127.0.0.1:0", false);
    }

    #[test]
    fn udp_associate_resolves_relay_domain() {
        assert_udp_relay_works("127.0.0.1:0", "127.0.0.1:0", true);
    }

    #[test]
    fn udp_relay_resolution_prefers_control_family_and_allows_fallback() {
        let ipv4: SocketAddr = "127.0.0.1:1080".parse().unwrap();
        let ipv6: SocketAddr = "[::1]:1080".parse().unwrap();
        for preferred_ipv4 in [true, false] {
            let selected = resolve_udp_relay_with_timeout(
                async move { Ok(vec![ipv6, ipv4]) },
                preferred_ipv4,
                Duration::from_secs(1),
            )
            .unwrap();
            assert_eq!(selected, if preferred_ipv4 { ipv4 } else { ipv6 });
            let fallback = if preferred_ipv4 { ipv6 } else { ipv4 };
            assert_eq!(
                resolve_udp_relay_with_timeout(
                    async move { Ok(vec![fallback]) },
                    preferred_ipv4,
                    Duration::from_secs(1),
                )
                .unwrap(),
                fallback
            );
        }
    }

    #[test]
    fn udp_relay_normalizes_ipv4_mapped_addresses() {
        let mapped: SocketAddr = "[::ffff:127.0.0.1]:1080".parse().unwrap();
        let ipv4: SocketAddr = "127.0.0.1:1080".parse().unwrap();
        assert_eq!(normalize_udp_relay(mapped), ipv4);
        let selected = resolve_udp_relay_with_timeout(
            async move { Ok(vec![mapped]) },
            true,
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(selected, ipv4);
    }

    #[test]
    fn udp_relay_resolution_times_out_and_cancels_lookup() {
        struct NotifyDrop(mpsc::SyncSender<()>);
        impl Drop for NotifyDrop {
            fn drop(&mut self) {
                let _ = self.0.send(());
            }
        }

        let (cancelled, cancellation) = mpsc::sync_channel(1);
        let guard = NotifyDrop(cancelled);
        let started = std::time::Instant::now();
        let error = resolve_udp_relay_with_timeout(
            async move {
                let _guard = guard;
                std::future::pending().await
            },
            true,
            Duration::from_millis(30),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(1));
        cancellation.recv_timeout(Duration::from_secs(1)).unwrap();
    }

    #[test]
    fn proxy_and_udp_timeouts_cancel_real_dns_queries() {
        for proxy_lookup in [true, false] {
            let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let dns = Arc::new(ClientDns::with_resolver(
                crate::client_dns::tests::test_resolver(socket.local_addr().unwrap()),
            ));
            let server = thread::spawn(move || {
                let mut sources = Vec::new();
                let mut packet = [0; 4096];
                for _ in 0..2 {
                    let (_, source) = socket.recv_from(&mut packet).unwrap();
                    sources.push(source);
                }
                (socket, sources)
            });
            let lookup_dns = Arc::clone(&dns);
            let timeout = Duration::from_millis(150);
            let result = if proxy_lookup {
                connect_proxy("blackhole.example.:1080".into(), lookup_dns, timeout).map(|_| ())
            } else {
                resolve_udp_relay_with_timeout(
                    async move { lookup_dns.lookup("blackhole.example.", 1080, true).await },
                    true,
                    timeout,
                )
                .map(|_| ())
            };
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
            let (socket, sources) = server.join().unwrap();
            // 解析器仍保留在客户端中，超时查询本身必须关闭两个 DNS 请求的源端口。
            let reclaimed: Vec<_> = sources
                .into_iter()
                .map(|source| {
                    let deadline = std::time::Instant::now() + Duration::from_secs(1);
                    loop {
                        match UdpSocket::bind(source) {
                            Ok(socket) => break socket,
                            Err(error)
                                if error.kind() == io::ErrorKind::AddrInUse
                                    && std::time::Instant::now() < deadline =>
                            {
                                thread::sleep(Duration::from_millis(1));
                            }
                            Err(error) => panic!("超时后未释放 DNS 源端口：{error}"),
                        }
                    }
                })
                .collect();
            assert_eq!(reclaimed.len(), 2);
            socket
                .set_read_timeout(Some(Duration::from_millis(450)))
                .unwrap();
            let mut packet = [0; 4096];
            let error = socket.recv_from(&mut packet).unwrap_err();
            assert!([io::ErrorKind::TimedOut, io::ErrorKind::WouldBlock].contains(&error.kind()));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proxy_connects_while_other_dns_family_is_dropped() {
        use crate::dns::tests::{DnsReply, scripted_dns};

        for ipv4 in [true, false] {
            let listener =
                tokio::net::TcpListener::bind(if ipv4 { "127.0.0.42:0" } else { "[::1]:0" })
                    .await
                    .unwrap();
            let replies = [DnsReply::Answer(Duration::ZERO), DnsReply::Drop];
            let upstream = if ipv4 {
                scripted_dns(replies[0], replies[1], 60).await
            } else {
                scripted_dns(replies[1], replies[0], 60).await
            };
            let dns = Arc::new(ClientDns::with_resolver(
                crate::client_dns::tests::test_resolver(upstream.address),
            ));
            let address = listener.local_addr().unwrap();
            let server = format!("available.example.:{}", address.port());
            let stream = tokio::task::spawn_blocking(move || {
                connect_proxy(server, dns, Duration::from_millis(700))
            })
            .await
            .unwrap()
            .unwrap();
            assert_eq!(stream.peer_addr().unwrap(), address);
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .unwrap()
                .unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn udp_relay_uses_other_family_when_preferred_dns_family_is_dropped() {
        use crate::dns::tests::{DnsReply, scripted_dns};

        for ipv4 in [true, false] {
            let upstream = if ipv4 {
                scripted_dns(DnsReply::Answer(Duration::ZERO), DnsReply::Drop, 60).await
            } else {
                scripted_dns(DnsReply::Drop, DnsReply::Answer(Duration::ZERO), 60).await
            };
            let dns = Arc::new(ClientDns::with_resolver(
                crate::client_dns::tests::test_resolver(upstream.address),
            ));
            let relay = tokio::task::spawn_blocking(move || {
                resolve_udp_relay_with_timeout(
                    async move { dns.lookup("available.example.", 1080, !ipv4).await },
                    !ipv4,
                    Duration::from_millis(700),
                )
            })
            .await
            .unwrap()
            .unwrap();
            assert_eq!(
                relay,
                if ipv4 {
                    "127.0.0.42:1080".parse::<SocketAddr>().unwrap()
                } else {
                    "[::1]:1080".parse::<SocketAddr>().unwrap()
                }
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proxy_waits_for_late_dns_family_after_first_connection_fails() {
        use crate::dns::tests::{DnsReply, scripted_dns};

        let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let upstream = scripted_dns(
            DnsReply::Answer(Duration::ZERO),
            DnsReply::Answer(Duration::from_millis(100)),
            60,
        )
        .await;
        let dns = Arc::new(ClientDns::with_resolver(
            crate::client_dns::tests::test_resolver(upstream.address),
        ));
        let stream = tokio::task::spawn_blocking(move || {
            connect_proxy(
                format!("fallback.example.:{}", address.port()),
                dns,
                Duration::from_secs(1),
            )
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(stream.peer_addr().unwrap(), address);
    }

    #[test]
    fn udp_relay_resolution_rejects_empty_answers_and_preserves_lookup_errors() {
        let empty =
            resolve_udp_relay_with_timeout(async { Ok(Vec::new()) }, true, Duration::from_secs(1))
                .unwrap_err();
        assert_eq!(empty.kind(), io::ErrorKind::NotFound);
        let refused = resolve_udp_relay_with_timeout(
            async {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "DNS 查询被拒绝",
                ))
            },
            true,
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn udp_relay_family_switch_rejects_conflicting_declared_ports() {
        for (control_bind, relay_ip) in [
            ("127.0.0.1:0", IpAddr::V6(Ipv6Addr::LOCALHOST)),
            ("[::1]:0", IpAddr::V4(Ipv4Addr::LOCALHOST)),
        ] {
            let listener = TcpListener::bind(control_bind).unwrap();
            let proxy = listener.local_addr().unwrap();
            let server = thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut greeting = [0; 3];
                stream.read_exact(&mut greeting).unwrap();
                stream.write_all(&[5, 0]).unwrap();
                let (_, request) = protocol::read_request(&mut stream).unwrap();
                let mut blocked_address = unspecified(relay_ip);
                blocked_address.set_port(request.port);
                let _blocked = bind_udp_socket(blocked_address).unwrap();
                protocol::write_reply(
                    &mut stream,
                    protocol::SUCCESS,
                    SocketAddr::new(relay_ip, 53),
                )
                .unwrap();
                let mut closed = [0];
                assert_eq!(stream.read(&mut closed).unwrap(), 0);
                request.port
            });
            let client = Client::new(proxy.to_string(), "", "").unwrap();
            let error = client
                .associate(Target::from("127.0.0.1:53".parse::<SocketAddr>().unwrap()))
                .err()
                .expect("目标协议族端口被占用时应拒绝关联");
            let declared_port = server.join().unwrap();
            assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
            assert!(
                error
                    .to_string()
                    .contains(&format!("已声明端口 {declared_port}"))
            );
        }
    }
}
