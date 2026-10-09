use crate::config::Config;
use crate::dns::DnsResolver;
use crate::happy_eyeballs::happy_eyeballs_connect;
use crate::protocol::{self, Host, Target};
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const UDP_BUFFER_SIZE: usize = 65_535;

type Associations = Arc<Mutex<HashMap<u64, Arc<Association>>>>;

struct Association {
    client_ip: IpAddr,
    requested_port: u16,
    endpoint: Mutex<Option<SocketAddr>>,
    alive: AtomicBool,
    flows: Mutex<HashMap<Target, Arc<Flow>>>,
}

impl Association {
    fn is_bound_to(&self, source: SocketAddr) -> bool {
        self.endpoint.lock().unwrap().as_ref() == Some(&source)
    }

    fn accepts(&self, source: SocketAddr) -> bool {
        if normalize(source.ip()) != normalize(self.client_ip)
            || (self.requested_port != 0 && self.requested_port != source.port())
        {
            return false;
        }
        let mut endpoint = self.endpoint.lock().unwrap();
        match *endpoint {
            Some(existing) => existing == source,
            None => {
                *endpoint = Some(source);
                true
            }
        }
    }
}

struct Flow {
    socket: Arc<tokio::net::UdpSocket>,
    last_activity: Mutex<Instant>,
}

fn normalize(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(ip) => ip
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(ip)),
        ip => ip,
    }
}

pub struct Server {
    config: Arc<Config>,
    dns: Arc<DnsResolver>,
    tcp: std::net::TcpListener,
    udp: std::net::UdpSocket,
    associations: Associations,
    next_id: AtomicU64,
    shutdown: CancellationToken,
}

impl Server {
    pub fn bind(config: Config, shutdown: CancellationToken) -> io::Result<Self> {
        let dns = Arc::new(DnsResolver::new(&config.dns_servers)?);
        let address = SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), config.port);
        let tcp_socket = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))?;
        tcp_socket.set_only_v6(false)?;
        tcp_socket.set_nonblocking(true)?;
        tcp_socket.set_reuse_address(true)?;
        #[cfg(unix)]
        let _ = tcp_socket.set_reuse_port(true);
        tcp_socket.bind(&address.into())?;
        tcp_socket.listen(1024)?;
        let tcp: std::net::TcpListener = tcp_socket.into();
        let udp_socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
        udp_socket.set_only_v6(false)?;
        udp_socket.set_nonblocking(true)?;
        udp_socket.set_reuse_address(true)?;
        #[cfg(unix)]
        let _ = udp_socket.set_reuse_port(true);
        let udp_address =
            SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), tcp.local_addr()?.port());
        udp_socket.bind(&udp_address.into())?;
        let udp = std::net::UdpSocket::from(udp_socket);
        Ok(Self {
            config: Arc::new(config),
            dns,
            tcp,
            udp,
            associations: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
            shutdown,
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.tcp.local_addr()
    }

    pub fn run(self) -> io::Result<()> {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            tokio::task::block_in_place(|| handle.block_on(self.run_async()))
        } else {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            runtime.block_on(self.run_async())
        }
    }

    pub async fn run_async(self) -> io::Result<()> {
        let tcp = tokio::net::TcpListener::from_std(self.tcp)?;
        // 直接消费 self.udp 转为 tokio 套接字，避免 try_clone 产生冗余句柄
        let udp = Arc::new(tokio::net::UdpSocket::from_std(self.udp)?);

        let relay = Arc::clone(&udp);
        let associations = Arc::clone(&self.associations);
        let shutdown = self.shutdown.clone();
        let config = Arc::clone(&self.config);
        let dns = Arc::clone(&self.dns);

        let udp_task =
            tokio::spawn(async move { udp_loop(relay, associations, config, dns, shutdown).await });

        let mut result = Ok(());
        loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => break,
                accept_res = tcp.accept() => {
                    match accept_res {
                        Ok((stream, address)) => {
                            if !self.config.whitelist.allows(address.ip()) {
                                continue;
                            }
                            let config = Arc::clone(&self.config);
                            let dns = Arc::clone(&self.dns);
                            let associations = Arc::clone(&self.associations);
                            let udp = Arc::clone(&udp);
                            let shutdown = self.shutdown.clone();
                            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
                            tokio::spawn(async move {
                                if let Err(error) =
                                    handle_client(stream, config, dns, udp, associations, id, shutdown).await
                                    && !matches!(
                                        error.kind(),
                                        io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
                                    )
                                {
                                    eprintln!("连接 {address} 处理失败：{error}");
                                }
                            });
                        }
                        Err(error) => {
                            result = Err(error);
                            break;
                        }
                    }
                }
            }
        }

        // 通知所有子任务停止
        self.shutdown.cancel();
        for association in self.associations.lock().unwrap().values() {
            association.alive.store(false, Ordering::Relaxed);
        }
        let udp_result = udp_task
            .await
            .map_err(|_| io::Error::other("UDP 任务异常退出"))?;
        result.and(udp_result)
    }
}

async fn negotiate(stream: &mut tokio::net::TcpStream, config: &Config) -> io::Result<()> {
    let mut header = [0u8; 2];
    stream.read_exact(&mut header).await?;
    if header[0] != 5 || header[1] == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "SOCKS5 协商请求无效",
        ));
    }
    let mut methods = vec![0u8; usize::from(header[1])];
    stream.read_exact(&mut methods).await?;
    let selected = if config.username.is_empty() { 0 } else { 2 };
    if !methods.contains(&selected) {
        stream.write_all(&[5, 255]).await?;
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "客户端没有提供可用的认证方法",
        ));
    }
    stream.write_all(&[5, selected]).await?;
    if selected == 2 {
        let mut auth_header = [0u8; 2];
        stream.read_exact(&mut auth_header).await?;
        if auth_header[0] != 1 || auth_header[1] == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "用户名密码认证请求无效",
            ));
        }
        let mut username = vec![0u8; usize::from(auth_header[1])];
        stream.read_exact(&mut username).await?;
        let mut password_length = [0u8; 1];
        stream.read_exact(&mut password_length).await?;
        if password_length[0] == 0 {
            stream.write_all(&[1, 1]).await?;
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "密码不能为空",
            ));
        }
        let mut password = vec![0u8; usize::from(password_length[0])];
        stream.read_exact(&mut password).await?;
        let allowed = constant_time_eq(&username, config.username.as_bytes())
            & constant_time_eq(&password, config.password.as_bytes());
        stream.write_all(&[1, if allowed { 0 } else { 1 }]).await?;
        if !allowed {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "用户名或密码错误",
            ));
        }
    }
    Ok(())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut difference = a.len() ^ b.len();
    for (index, value) in a.iter().enumerate() {
        difference |= usize::from(*value ^ b.get(index).copied().unwrap_or(0));
    }
    difference == 0
}

fn unspecified(ip: IpAddr) -> SocketAddr {
    match ip {
        IpAddr::V4(_) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
        IpAddr::V6(_) => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
    }
}

async fn handle_client(
    mut stream: tokio::net::TcpStream,
    config: Arc<Config>,
    dns: Arc<DnsResolver>,
    udp: Arc<tokio::net::UdpSocket>,
    associations: Associations,
    id: u64,
    shutdown: CancellationToken,
) -> io::Result<()> {
    tokio::time::timeout(HANDSHAKE_TIMEOUT, negotiate(&mut stream, &config))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "SOCKS5 协商超时"))??;

    let (command, target) =
        match tokio::time::timeout(HANDSHAKE_TIMEOUT, protocol::read_request_async(&mut stream))
            .await
        {
            Ok(Ok(request)) => request,
            Ok(Err(error)) => {
                if error.kind() == io::ErrorKind::InvalidData {
                    let address = unspecified(stream.local_addr()?.ip());
                    let _ = protocol::write_reply_async(
                        &mut stream,
                        protocol::ADDRESS_UNSUPPORTED,
                        address,
                    )
                    .await;
                }
                return Err(error);
            }
            Err(_) => return Err(io::Error::new(io::ErrorKind::TimedOut, "读取请求超时")),
        };

    match command {
        protocol::CONNECT => connect(stream, target, config.tcp_timeout, &dns, shutdown).await,
        protocol::UDP_ASSOCIATE => associate(stream, target, udp, associations, id, shutdown).await,
        _ => {
            let address = unspecified(stream.local_addr()?.ip());
            protocol::write_reply_async(&mut stream, protocol::COMMAND_UNSUPPORTED, address).await
        }
    }
}

async fn connect(
    mut client: tokio::net::TcpStream,
    target: Target,
    timeout: Option<Duration>,
    dns: &DnsResolver,
    shutdown: CancellationToken,
) -> io::Result<()> {
    let connect_task = async {
        let address = unspecified(client.local_addr()?.ip());
        let addresses = match dns.lookup(&target).await {
            Ok(addresses) => addresses,
            Err(error) => {
                protocol::write_reply_async(&mut client, protocol::HOST_UNREACHABLE, address)
                    .await?;
                return Err(error);
            }
        };

        // 使用 Happy Eyeballs 双栈并发竞速建立连接
        let mut remote = match happy_eyeballs_connect(&addresses, HANDSHAKE_TIMEOUT).await {
            Ok(stream) => stream,
            Err(error) => {
                let status = if error.kind() == io::ErrorKind::ConnectionRefused {
                    protocol::CONNECTION_REFUSED
                } else {
                    protocol::HOST_UNREACHABLE
                };
                protocol::write_reply_async(&mut client, status, address).await?;
                return Err(error);
            }
        };

        protocol::write_reply_async(&mut client, protocol::SUCCESS, remote.local_addr()?).await?;
        forward_bidirectional(&mut client, &mut remote, timeout).await
    };

    tokio::select! {
        _ = shutdown.cancelled() => Ok(()),
        res = connect_task => res,
    }
}

async fn forward_bidirectional(
    client: &mut tokio::net::TcpStream,
    remote: &mut tokio::net::TcpStream,
    timeout: Option<Duration>,
) -> io::Result<()> {
    match timeout {
        None => tokio::io::copy_bidirectional_with_sizes(client, remote, 65536, 65536)
            .await
            .map(|_| ()),
        Some(idle_timeout) => {
            let (mut client_r, mut client_w) = client.split();
            let (mut remote_r, mut remote_w) = remote.split();

            let c2r = copy_direction_idle(&mut client_r, &mut remote_w, idle_timeout);
            let r2c = copy_direction_idle(&mut remote_r, &mut client_w, idle_timeout);

            tokio::try_join!(c2r, r2c).map(|_| ())
        }
    }
}

async fn copy_direction_idle<R, W>(
    reader: &mut R,
    writer: &mut W,
    timeout: Duration,
) -> io::Result<u64>
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    let mut buffer = vec![0u8; 65536];
    let mut total = 0u64;
    loop {
        let n = match tokio::time::timeout(timeout, reader.read(&mut buffer)).await {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err(io::Error::new(io::ErrorKind::TimedOut, "TCP 连接空闲超时")),
        };
        if n == 0 {
            writer.shutdown().await?;
            break;
        }
        writer.write_all(&buffer[..n]).await?;
        total += n as u64;
    }
    Ok(total)
}

async fn associate(
    mut stream: tokio::net::TcpStream,
    target: Target,
    udp: Arc<tokio::net::UdpSocket>,
    associations: Associations,
    id: u64,
    shutdown: CancellationToken,
) -> io::Result<()> {
    let peer = stream.peer_addr()?;
    match target.host {
        Host::Ip(ip) if !ip.is_unspecified() && normalize(ip) != normalize(peer.ip()) => {
            let address = unspecified(stream.local_addr()?.ip());
            protocol::write_reply_async(&mut stream, protocol::SERVER_FAILURE, address).await?;
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "UDP 客户端地址与控制连接不符",
            ));
        }
        Host::Domain(_) => {
            let address = unspecified(stream.local_addr()?.ip());
            protocol::write_reply_async(&mut stream, protocol::ADDRESS_UNSUPPORTED, address)
                .await?;
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "UDP 客户端地址必须是 IP",
            ));
        }
        _ => {}
    }
    let relay = SocketAddr::new(
        normalize(stream.local_addr()?.ip()),
        udp.local_addr()?.port(),
    );
    let association = Arc::new(Association {
        client_ip: peer.ip(),
        requested_port: target.port,
        endpoint: Mutex::new(None),
        alive: AtomicBool::new(true),
        flows: Mutex::new(HashMap::new()),
    });
    associations
        .lock()
        .unwrap()
        .insert(id, Arc::clone(&association));
    if let Err(error) = protocol::write_reply_async(&mut stream, protocol::SUCCESS, relay).await {
        association.alive.store(false, Ordering::Relaxed);
        associations.lock().unwrap().remove(&id);
        return Err(error);
    }
    let mut buffer = [0u8; 1024];
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            res = stream.read(&mut buffer) => {
                match res {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        }
    }
    association.alive.store(false, Ordering::Relaxed);
    associations.lock().unwrap().remove(&id);
    Ok(())
}

async fn udp_loop(
    udp: Arc<tokio::net::UdpSocket>,
    associations: Associations,
    config: Arc<Config>,
    dns: Arc<DnsResolver>,
    shutdown: CancellationToken,
) -> io::Result<()> {
    let mut buffer = vec![0u8; UDP_BUFFER_SIZE];
    loop {
        let (length, source) = tokio::select! {
            _ = shutdown.cancelled() => break,
            res = udp.recv_from(&mut buffer) => {
                match res {
                    Ok(val) => val,
                    Err(error) => {
                        if matches!(
                            error.kind(),
                            io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionRefused
                        ) {
                            continue;
                        }
                        return Err(error);
                    }
                }
            }
        };

        if !config.whitelist.allows(source.ip()) {
            continue;
        }

        let Ok((target, payload)) = protocol::parse_datagram(&buffer[..length]) else {
            continue;
        };

        let association = {
            let map = associations.lock().unwrap();
            map.values()
                .find(|association| {
                    association.alive.load(Ordering::Relaxed) && association.is_bound_to(source)
                })
                .or_else(|| {
                    map.values().find(|association| {
                        association.alive.load(Ordering::Relaxed) && association.accepts(source)
                    })
                })
                .cloned()
        };

        let Some(association) = association else {
            continue;
        };

        let relay = Arc::clone(&udp);
        let config = Arc::clone(&config);
        let dns = Arc::clone(&dns);
        let payload = payload.to_vec();
        tokio::spawn(async move {
            handle_udp_packet(&relay, &association, &config, &dns, target, payload).await;
        });
    }
    Ok(())
}

async fn handle_udp_packet(
    udp: &Arc<tokio::net::UdpSocket>,
    association: &Arc<Association>,
    config: &Config,
    dns: &DnsResolver,
    target: Target,
    payload: Vec<u8>,
) {
    if !association.alive.load(Ordering::Relaxed) {
        return;
    }

    let existing = {
        let flows = association.flows.lock().unwrap();
        flows.get(&target).cloned()
    };

    let flow = if let Some(flow) = existing {
        flow
    } else {
        let Ok(addresses) = dns.lookup(&target).await else {
            return;
        };
        let destination = addresses[0];
        let Ok(std_socket) = std::net::UdpSocket::bind(unspecified(destination.ip())) else {
            return;
        };
        if std_socket.connect(destination).is_err() {
            return;
        }
        std_socket.set_nonblocking(true).ok();
        let Ok(socket) = tokio::net::UdpSocket::from_std(std_socket) else {
            return;
        };
        let socket = Arc::new(socket);
        let candidate = Arc::new(Flow {
            socket,
            last_activity: Mutex::new(Instant::now()),
        });

        let mut flows = association.flows.lock().unwrap();
        if let Some(flow) = flows.get(&target) {
            Arc::clone(flow)
        } else {
            flows.insert(target.clone(), Arc::clone(&candidate));
            let association = Arc::clone(association);
            let relay = Arc::clone(udp);
            let worker = Arc::clone(&candidate);
            let timeout = config.udp_timeout;
            tokio::spawn(async move {
                receive_remote(worker, relay, association, target, destination, timeout).await;
            });
            candidate
        }
    };

    *flow.last_activity.lock().unwrap() = Instant::now();
    let _ = flow.socket.send(&payload).await;
}

async fn receive_remote(
    flow: Arc<Flow>,
    relay: Arc<tokio::net::UdpSocket>,
    association: Arc<Association>,
    target: Target,
    destination: SocketAddr,
    timeout: Duration,
) {
    let mut buffer = vec![0u8; UDP_BUFFER_SIZE];
    while association.alive.load(Ordering::Relaxed) {
        let recv_future = flow.socket.recv(&mut buffer[22..]);
        let res = if !timeout.is_zero() {
            tokio::time::timeout(timeout, recv_future).await
        } else {
            Ok(recv_future.await)
        };

        match res {
            Ok(Ok(length)) => {
                if !association.alive.load(Ordering::Relaxed) {
                    break;
                }
                *flow.last_activity.lock().unwrap() = Instant::now();
                let source = { *association.endpoint.lock().unwrap() };
                if let Some(source) = source {
                    let header_length = protocol::datagram_header_len(destination);
                    let start = 22 - header_length;
                    protocol::write_datagram_header(&mut buffer[start..22], destination);
                    let _ = relay.send_to(&buffer[start..22 + length], source).await;
                }
            }
            Ok(Err(_)) => break,
            Err(_) => break,
        }
    }

    let mut flows = association.flows.lock().unwrap();
    if flows
        .get(&target)
        .is_some_and(|current| Arc::ptr_eq(current, &flow))
    {
        flows.remove(&target);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Whitelist;
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpListener, TcpStream, UdpSocket};
    use std::sync::mpsc;
    use std::thread;

    fn start_server(
        username: &str,
        password: &str,
        whitelist: &str,
    ) -> (
        SocketAddr,
        CancellationToken,
        thread::JoinHandle<io::Result<()>>,
    ) {
        let config = Config {
            port: 0,
            username: username.into(),
            password: password.into(),
            whitelist: Whitelist::parse(whitelist).unwrap(),
            tcp_timeout: None,
            udp_timeout: Duration::from_secs(3),
            dns_servers: vec!["127.0.0.1:53".parse().unwrap()],
        };
        let shutdown = CancellationToken::new();
        let server = Server::bind(config, shutdown.clone()).unwrap();
        let address = SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            server.local_addr().unwrap().port(),
        );
        let handle = thread::spawn(move || server.run());
        (address, shutdown, handle)
    }

    fn negotiate_client(stream: &mut TcpStream, method: u8) -> [u8; 2] {
        stream.write_all(&[5, 1, method]).unwrap();
        let mut reply = [0; 2];
        stream.read_exact(&mut reply).unwrap();
        reply
    }

    fn request(stream: &mut TcpStream, command: u8, destination: SocketAddr) -> [u8; 10] {
        let mut bytes = vec![5, command, 0];
        match destination.ip() {
            IpAddr::V4(ip) => {
                bytes.push(1);
                bytes.extend_from_slice(&ip.octets());
            }
            IpAddr::V6(ip) => {
                bytes.push(4);
                bytes.extend_from_slice(&ip.octets());
            }
        }
        bytes.extend_from_slice(&destination.port().to_be_bytes());
        stream.write_all(&bytes).unwrap();
        let mut reply = [0; 10];
        stream.read_exact(&mut reply).unwrap();
        reply
    }

    #[test]
    fn connect_relays_tcp_and_preserves_half_close() {
        let echo = TcpListener::bind("127.0.0.1:0").unwrap();
        let destination = echo.local_addr().unwrap();
        let echo_thread = thread::spawn(move || {
            let (mut stream, _) = echo.accept().unwrap();
            let mut payload = Vec::new();
            stream.read_to_end(&mut payload).unwrap();
            stream.write_all(&payload).unwrap();
        });
        let (proxy, shutdown, server_thread) = start_server("", "", "127.0.0.1");
        let mut client = TcpStream::connect(proxy).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        assert_eq!(negotiate_client(&mut client, 0), [5, 0]);
        assert_eq!(
            request(&mut client, protocol::CONNECT, destination)[1],
            protocol::SUCCESS
        );
        client.write_all(b"brume").unwrap();
        client.shutdown(Shutdown::Write).unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).unwrap();
        assert_eq!(response, b"brume");
        echo_thread.join().unwrap();
        shutdown.cancel();
        server_thread.join().unwrap().unwrap();
    }

    #[test]
    fn authentication_and_whitelist_reject_unauthorized_clients() {
        let (proxy, shutdown, server_thread) = start_server("admin", "secret", "192.0.2.1");
        let mut denied = TcpStream::connect(proxy).unwrap();
        denied
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        assert!(denied.write_all(&[5, 1, 2]).is_ok());
        assert!(matches!(denied.read(&mut [0; 2]), Ok(0) | Err(_)));
        shutdown.cancel();
        server_thread.join().unwrap().unwrap();

        let (proxy, shutdown, server_thread) = start_server("admin", "secret", "127.0.0.1");
        let mut client = TcpStream::connect(proxy).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        assert_eq!(negotiate_client(&mut client, 0), [5, 255]);
        let mut client = TcpStream::connect(proxy).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        assert_eq!(negotiate_client(&mut client, 2), [5, 2]);
        client
            .write_all(&[1, 5, b'a', b'd', b'm', b'i', b'n', 3, b'b', b'a', b'd'])
            .unwrap();
        let mut reply = [0; 2];
        client.read_exact(&mut reply).unwrap();
        assert_eq!(reply, [1, 1]);
        let mut client = TcpStream::connect(proxy).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        assert_eq!(negotiate_client(&mut client, 2), [5, 2]);
        client
            .write_all(&[
                1, 5, b'a', b'd', b'm', b'i', b'n', 6, b's', b'e', b'c', b'r', b'e', b't',
            ])
            .unwrap();
        client.read_exact(&mut reply).unwrap();
        assert_eq!(reply, [1, 0]);
        assert_eq!(
            request(&mut client, 2, "0.0.0.0:0".parse().unwrap())[1],
            protocol::COMMAND_UNSUPPORTED
        );
        shutdown.cancel();
        server_thread.join().unwrap().unwrap();
    }

    #[test]
    fn accepts_ipv6_clients() {
        let (proxy, shutdown, server_thread) = start_server("", "", "::1");
        let address = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), proxy.port());
        let mut client = TcpStream::connect(address).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        assert_eq!(negotiate_client(&mut client, 0), [5, 0]);
        shutdown.cancel();
        server_thread.join().unwrap().unwrap();
    }

    #[test]
    fn udp_requires_live_association_and_relays_replies() {
        let remote = UdpSocket::bind("127.0.0.1:0").unwrap();
        remote
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let destination = remote.local_addr().unwrap();
        let (probe_sender, probe_receiver) = mpsc::channel();
        let remote_thread = thread::spawn(move || {
            let mut buffer = [0; 100];
            let (length, source) = remote.recv_from(&mut buffer).unwrap();
            remote.send_to(&buffer[..length], source).unwrap();
            probe_receiver.recv().unwrap();
            remote
                .set_read_timeout(Some(Duration::from_millis(500)))
                .unwrap();
            remote.recv_from(&mut buffer).is_err()
        });
        let (proxy, shutdown, server_thread) = start_server("", "", "127.0.0.1");
        let client_udp = UdpSocket::bind("127.0.0.1:0").unwrap();
        client_udp
            .set_read_timeout(Some(Duration::from_millis(350)))
            .unwrap();
        let packet = protocol::encode_datagram(destination, b"udp-ok");
        client_udp.send_to(&packet, proxy).unwrap();
        assert!(client_udp.recv_from(&mut [0; 100]).is_err());

        let mut control = TcpStream::connect(proxy).unwrap();
        control
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        assert_eq!(negotiate_client(&mut control, 0), [5, 0]);
        let reply = request(
            &mut control,
            protocol::UDP_ASSOCIATE,
            "0.0.0.0:0".parse().unwrap(),
        );
        assert_eq!(reply[1], protocol::SUCCESS);
        let relay = SocketAddr::new(proxy.ip(), u16::from_be_bytes([reply[8], reply[9]]));
        client_udp.send_to(&packet, relay).unwrap();
        client_udp
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut response = [0; 100];
        let (length, _) = client_udp.recv_from(&mut response).unwrap();
        let (target, payload) = protocol::parse_datagram(&response[..length]).unwrap();
        assert_eq!(
            target,
            Target {
                host: Host::Ip(destination.ip()),
                port: destination.port()
            }
        );
        assert_eq!(payload, b"udp-ok");
        drop(control);
        thread::sleep(Duration::from_millis(300));
        probe_sender.send(()).unwrap();
        client_udp.send_to(&packet, relay).unwrap();
        assert!(remote_thread.join().unwrap());
        shutdown.cancel();
        server_thread.join().unwrap().unwrap();
    }

    #[test]
    fn udp_relay_supports_ipv6() {
        let remote = UdpSocket::bind("[::1]:0").unwrap();
        remote
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let destination = remote.local_addr().unwrap();
        let echo = thread::spawn(move || {
            let mut buffer = [0; 100];
            let (length, source) = remote.recv_from(&mut buffer).unwrap();
            remote.send_to(&buffer[..length], source).unwrap();
        });
        let (proxy, shutdown, server_thread) = start_server("", "", "::1");
        let proxy = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), proxy.port());
        let mut control = TcpStream::connect(proxy).unwrap();
        control
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        assert_eq!(negotiate_client(&mut control, 0), [5, 0]);
        let mut request = [
            5,
            protocol::UDP_ASSOCIATE,
            0,
            4,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
        ];
        control.write_all(&request).unwrap();
        control.read_exact(&mut request).unwrap();
        assert_eq!(request[1], protocol::SUCCESS);
        let relay = SocketAddr::new(proxy.ip(), u16::from_be_bytes([request[20], request[21]]));
        let client_udp = UdpSocket::bind("[::1]:0").unwrap();
        client_udp
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        client_udp
            .send_to(&protocol::encode_datagram(destination, b"v6-ok"), relay)
            .unwrap();
        let mut response = [0; 100];
        let (length, _) = client_udp.recv_from(&mut response).unwrap();
        assert_eq!(
            protocol::parse_datagram(&response[..length]).unwrap().1,
            b"v6-ok"
        );
        echo.join().unwrap();
        drop(control);
        shutdown.cancel();
        server_thread.join().unwrap().unwrap();
    }
}
