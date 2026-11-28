use crate::config::Config;
use crate::dns::{DnsResolver, PendingLookup};
use crate::happy_eyeballs::TARGET_TIMEOUT_MESSAGE;
use crate::protocol::{self, Host, Target};
use hickory_resolver::net::NetError;
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{OnceCell, OwnedSemaphorePermit, Semaphore, watch};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

mod http_proxy;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const UDP_BUFFER_SIZE: usize = 65_535;
const MAX_TCP_CLIENTS: usize = 2048;
const MAX_UDP_PACKET_TASKS: usize = 256;
const MAX_UDP_FLOWS: usize = 1024;
const MAX_FLOWS_PER_ASSOCIATION: usize = 128;
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(250);
const UDP_DNS_FALLBACK_GRACE: Duration = Duration::from_millis(50);
const INITIAL_TCP_BUFFER_SIZE: usize = 8192;
const MAX_TCP_BUFFER_SIZE: usize = 65536;
const UDP_CLIENT_ADDRESS_MISMATCH: &str = "UDP client address differs from the control connection";
static NEXT_UDP_FLOW_ID: AtomicU64 = AtomicU64::new(1);

type Associations = Arc<Mutex<AssociationRegistry>>;

#[derive(Default)]
struct AssociationRegistry {
    entries: HashMap<u64, Arc<Association>>,
    endpoints: HashMap<SocketAddr, u64>,
}

impl AssociationRegistry {
    fn insert(&mut self, id: u64, association: Arc<Association>) -> bool {
        let ip = normalize(association.client_ip);
        let endpoint = SocketAddr::new(ip, association.requested_port);
        if association.requested_port == 0 || self.endpoints.contains_key(&endpoint) {
            return false;
        }
        self.endpoints.insert(endpoint, id);
        self.entries.insert(id, association);
        true
    }

    fn find(&self, source: SocketAddr) -> Option<Arc<Association>> {
        let endpoint = endpoint_key(source);
        let id = self.endpoints.get(&endpoint)?;
        let association = self.entries.get(id)?.clone();
        (association.alive.load(Ordering::Relaxed) && association.accepts(source))
            .then_some(association)
    }

    fn remove(&mut self, id: u64) {
        let Some(association) = self.entries.remove(&id) else {
            return;
        };
        let ip = normalize(association.client_ip);
        let endpoint = SocketAddr::new(ip, association.requested_port);
        if self.endpoints.get(&endpoint) == Some(&id) {
            self.endpoints.remove(&endpoint);
        }
    }
}

struct Association {
    client_ip: IpAddr,
    requested_port: u16,
    endpoint: Mutex<Option<SocketAddr>>,
    alive: AtomicBool,
    stopped: CancellationToken,
    flows: Mutex<HashMap<Target, Arc<FlowSlot>>>,
    preferred_addresses: Mutex<HashMap<Target, UdpPreference>>,
}

#[derive(Clone, Copy)]
struct UdpPreference {
    address: SocketAddr,
    flow_id: u64,
}

impl Association {
    fn accepts(&self, source: SocketAddr) -> bool {
        if normalize(source.ip()) != normalize(self.client_ip)
            || self.requested_port == 0
            || self.requested_port != source.port()
        {
            return false;
        }
        let mut endpoint = self.endpoint.lock().unwrap();
        match *endpoint {
            Some(existing) if endpoint_key(existing) == endpoint_key(source) => {
                *endpoint = Some(source);
                true
            }
            Some(_) => false,
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
    active: AtomicBool,
    stopped: CancellationToken,
    address_index: usize,
    addresses: Mutex<Vec<SocketAddr>>,
    pending_lookup: Mutex<Option<PendingLookup>>,
}

struct FlowSlot {
    id: u64,
    flow: OnceCell<Option<Arc<Flow>>>,
    _permit: OwnedSemaphorePermit,
}

struct UdpFlows {
    slots: Arc<Semaphore>,
    tasks: TaskTracker,
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

fn endpoint_key(address: SocketAddr) -> SocketAddr {
    SocketAddr::new(normalize(address.ip()), address.port())
}

fn remember_udp_address(
    association: &Association,
    target: &Target,
    address: SocketAddr,
    flow_id: u64,
) {
    let mut preferred = association.preferred_addresses.lock().unwrap();
    if preferred.len() >= MAX_FLOWS_PER_ASSOCIATION
        && !preferred.contains_key(target)
        && let Some(oldest) = preferred.keys().next().cloned()
    {
        preferred.remove(&oldest);
    }
    preferred.insert(target.clone(), UdpPreference { address, flow_id });
}

fn advance_udp_address(association: &Association, target: &Target, flow: &Flow, slot: &FlowSlot) {
    let addresses = flow.addresses.lock().unwrap();
    if addresses.len() > 1 {
        let mut preferred = association.preferred_addresses.lock().unwrap();
        // 晚到的旧流结果不能覆盖后续新流已经选择的地址。
        if let Some(current) = preferred.get_mut(target)
            && current.flow_id == slot.id
        {
            current.address = addresses[(flow.address_index + 1) % addresses.len()];
        }
    }
}

fn append_udp_addresses(addresses: &mut Vec<SocketAddr>, more: Vec<SocketAddr>) {
    for address in more {
        if !addresses.contains(&address) {
            addresses.push(address);
        }
    }
}

pub struct Server {
    config: Arc<Config>,
    tcp: std::net::TcpListener,
    udp: std::net::UdpSocket,
    associations: Associations,
    next_id: AtomicU64,
    shutdown: CancellationToken,
}

struct TcpServices {
    dns: Arc<DnsResolver>,
    http: Arc<http_proxy::ConnectionPool>,
}

impl Server {
    pub fn bind(config: Config, shutdown: CancellationToken) -> io::Result<Self> {
        config
            .validate()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let address = SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), config.port);
        let tcp_socket = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))?;
        tcp_socket.set_only_v6(false)?;
        tcp_socket.set_nonblocking(true)?;
        // 仅在 Unix 允许快速重启，避免多个独立实例分流同一端口。
        #[cfg(unix)]
        tcp_socket.set_reuse_address(true)?;
        tcp_socket.bind(&address.into())?;
        tcp_socket.listen(1024)?;
        let tcp: std::net::TcpListener = tcp_socket.into();
        let udp_socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
        udp_socket.set_only_v6(false)?;
        udp_socket.set_nonblocking(true)?;
        let udp_address =
            SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), tcp.local_addr()?.port());
        udp_socket.bind(&udp_address.into())?;
        let udp = std::net::UdpSocket::from(udp_socket);
        Ok(Self {
            config: Arc::new(config),
            tcp,
            udp,
            associations: Arc::new(Mutex::new(AssociationRegistry::default())),
            next_id: AtomicU64::new(1),
            shutdown,
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.tcp.local_addr()
    }

    pub fn run(self) -> io::Result<()> {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            if handle.runtime_flavor() != tokio::runtime::RuntimeFlavor::MultiThread {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "Use Server::run_async on a single-threaded async runtime",
                ));
            }
            tokio::task::block_in_place(|| handle.block_on(self.run_async()))
        } else {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            runtime.block_on(self.run_async())
        }
    }

    pub async fn run_async(self) -> io::Result<()> {
        // 外部取消服务 future 时同样通知 UDP 任务释放监听套接字。
        let _shutdown_guard = self.shutdown.clone().drop_guard();
        // 在服务运行时内创建解析器，复用已有工作线程
        let dns = Arc::new(DnsResolver::new(&self.config.dns_servers)?);
        let services = Arc::new(TcpServices {
            dns: Arc::clone(&dns),
            http: http_proxy::ConnectionPool::new(self.shutdown.clone()),
        });
        let tcp = tokio::net::TcpListener::from_std(self.tcp)?;
        // 直接消费 self.udp 转为 tokio 套接字，避免 try_clone 产生冗余句柄
        let udp = Arc::new(tokio::net::UdpSocket::from_std(self.udp)?);

        let relay = Arc::clone(&udp);
        let associations = Arc::clone(&self.associations);
        let shutdown = self.shutdown.clone();
        let config = Arc::clone(&self.config);
        let udp_dns = Arc::clone(&dns);

        let mut udp_task =
            tokio::spawn(
                async move { udp_loop(relay, associations, config, udp_dns, shutdown).await },
            );
        let client_slots = Arc::new(Semaphore::new(MAX_TCP_CLIENTS));
        let mut clients = JoinSet::new();

        let mut udp_result = None;
        loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => break,
                Some(outcome) = clients.join_next(), if !clients.is_empty() => {
                    if let Err(error) = outcome {
                        eprintln!("Client task failed: {error}");
                    }
                }
                outcome = &mut udp_task => {
                    udp_result = Some(match outcome {
                        Ok(Ok(())) if !self.shutdown.is_cancelled() => {
                            Err(io::Error::other("UDP task exited unexpectedly"))
                        }
                        Ok(result) => result,
                        Err(error) => Err(io::Error::other(error)),
                    });
                    break;
                }
                accept_res = tcp.accept() => {
                    match accept_res {
                        Ok((stream, address)) => {
                            if !self.config.whitelist.allows(address.ip()) {
                                continue;
                            }
                            let Ok(permit) = Arc::clone(&client_slots).try_acquire_owned() else {
                                continue;
                            };
                            let config = Arc::clone(&self.config);
                            let services = Arc::clone(&services);
                            let associations = Arc::clone(&self.associations);
                            let udp = Arc::clone(&udp);
                            let shutdown = self.shutdown.clone();
                            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
                            clients.spawn(async move {
                                let _permit = permit;
                                if let Err(error) =
                                    handle_client(stream, config, services, udp, associations, id, shutdown).await
                                    && should_log_client_error(&error)
                                {
                                    eprintln!("Connection from {address} failed: {error}");
                                }
                            });
                        }
                        Err(error) => {
                            // 连接中断或描述符耗尽不应使全部代理连接退出；退避避免持续忙等。
                            eprintln!("Failed to accept connection. Retrying: {error}");
                            tokio::select! {
                                _ = self.shutdown.cancelled() => break,
                                _ = tokio::time::sleep(ACCEPT_RETRY_DELAY) => {}
                            }
                        }
                    }
                }
            }
        }

        // 通知所有子任务停止
        self.shutdown.cancel();
        clients.shutdown().await;
        services.http.shutdown().await;
        stop_associations(&self.associations);
        let udp_result = match udp_result {
            Some(result) => result,
            None => udp_task
                .await
                .map_err(|_| io::Error::other("UDP task failed"))?,
        };
        *self.associations.lock().unwrap() = AssociationRegistry::default();
        udp_result
    }
}

fn stop_associations(associations: &Associations) {
    for association in associations.lock().unwrap().entries.values() {
        association.alive.store(false, Ordering::Relaxed);
        association.stopped.cancel();
    }
}

async fn negotiate(stream: &mut tokio::net::TcpStream, config: &Config) -> io::Result<()> {
    let mut header = [0u8; 2];
    stream.read_exact(&mut header).await?;
    if header[0] != 5 || header[1] == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Invalid SOCKS5 greeting",
        ));
    }
    let mut methods = vec![0u8; usize::from(header[1])];
    stream.read_exact(&mut methods).await?;
    let selected = if config.username.is_empty() { 0 } else { 2 };
    if !methods.contains(&selected) {
        stream.write_all(&[5, 255]).await?;
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Client offered no supported authentication method",
        ));
    }
    stream.write_all(&[5, selected]).await?;
    if selected == 2 {
        let mut auth_header = [0u8; 2];
        stream.read_exact(&mut auth_header).await?;
        if auth_header[0] != 1 || auth_header[1] == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Invalid username/password authentication request",
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
                "Password must not be empty",
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
                "Invalid username or password",
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

fn should_log_client_error(error: &io::Error) -> bool {
    if matches!(
        error.kind(),
        io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::NetworkUnreachable
    ) {
        return false;
    }
    if error.kind() == io::ErrorKind::PermissionDenied
        && error.to_string() == UDP_CLIENT_ADDRESS_MISMATCH
    {
        return false;
    }
    if error.kind() == io::ErrorKind::TimedOut && error.to_string() == TARGET_TIMEOUT_MESSAGE {
        return false;
    }
    !error
        .get_ref()
        .and_then(|source| source.downcast_ref::<NetError>())
        .is_some_and(NetError::is_no_records_found)
}

async fn handle_client(
    mut stream: tokio::net::TcpStream,
    config: Arc<Config>,
    services: Arc<TcpServices>,
    udp: Arc<tokio::net::UdpSocket>,
    associations: Associations,
    id: u64,
    shutdown: CancellationToken,
) -> io::Result<()> {
    // 协商及转发小报文时避免 Nagle 引入额外等待
    stream.set_nodelay(true)?;
    let deadline = tokio::time::Instant::now() + HANDSHAKE_TIMEOUT;
    let mut first = [0];
    let received = tokio::time::timeout_at(deadline, stream.peek(&mut first))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "Proxy protocol read timed out"))??;
    if received == 0 {
        return Ok(());
    }
    if first[0] != 5 {
        return http_proxy::serve(stream, &config, &services.dns, &services.http).await;
    }
    tokio::time::timeout_at(deadline, negotiate(&mut stream, &config))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "SOCKS5 negotiation timed out"))??;

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
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Request read timed out",
                ));
            }
        };

    match command {
        protocol::CONNECT => {
            connect(stream, target, config.tcp_timeout, &services.dns, shutdown).await
        }
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
        // 可用地址族解析完成后立即连接，解析和建连共用总时限。
        let mut remote = match dns.connect_tcp(&target, HANDSHAKE_TIMEOUT).await {
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
        remote.set_nodelay(true)?;

        protocol::write_reply_async(&mut client, protocol::SUCCESS, remote.local_addr()?).await?;
        forward_bidirectional(client.split(), &mut remote, timeout).await
    };

    tokio::select! {
        _ = shutdown.cancelled() => Ok(()),
        res = connect_task => res,
    }
}

async fn forward_bidirectional<R, W>(
    (mut client_r, mut client_w): (R, W),
    remote: &mut tokio::net::TcpStream,
    timeout: Option<Duration>,
) -> io::Result<()>
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    let (mut remote_r, mut remote_w) = remote.split();
    match timeout {
        None => tokio::try_join!(
            copy_direction(&mut client_r, &mut remote_w, None),
            copy_direction(&mut remote_r, &mut client_w, None),
        )
        .map(|_| ()),
        Some(idle_timeout) => {
            let (activity, activity_rx) = watch::channel(tokio::time::Instant::now());
            let c2r = copy_direction(&mut client_r, &mut remote_w, Some(&activity));
            let r2c = copy_direction(&mut remote_r, &mut client_w, Some(&activity));
            let transfer = async { tokio::try_join!(c2r, r2c).map(|_| ()) };
            with_tcp_idle_timeout(transfer, activity_rx, idle_timeout).await
        }
    }
}

async fn with_tcp_idle_timeout(
    transfer: impl Future<Output = io::Result<()>>,
    mut activity: watch::Receiver<tokio::time::Instant>,
    idle_timeout: Duration,
) -> io::Result<()> {
    tokio::pin!(transfer);
    loop {
        let deadline = activity
            .borrow_and_update()
            .checked_add(idle_timeout)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "TCP timeout exceeds clock range",
                )
            })?;
        tokio::select! {
            result = &mut transfer => return result,
            _ = tokio::time::sleep_until(deadline) => {
                if tokio::time::Instant::now().duration_since(*activity.borrow()) >= idle_timeout {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "TCP idle timeout"));
                }
            }
            changed = activity.changed() => {
                if changed.is_err() {
                    // 连接对象已释放，此时转发已完成或将返回错误。
                    return transfer.await;
                }
            }
        }
    }
}

async fn copy_direction<R, W>(
    reader: &mut R,
    writer: &mut W,
    activity: Option<&watch::Sender<tokio::time::Instant>>,
) -> io::Result<u64>
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    // 小流量连接保持较小缓冲，连续满读时按需增长到批量转发所需的大小。
    let mut buffer = vec![0u8; INITIAL_TCP_BUFFER_SIZE];
    let mut total = 0u64;
    loop {
        let n = reader.read(&mut buffer).await?;
        if n == 0 {
            writer.shutdown().await?;
            break;
        }
        writer.write_all(&buffer[..n]).await?;
        if let Some(activity) = activity {
            activity.send_replace(tokio::time::Instant::now());
        }
        total += n as u64;
        if n == buffer.len() && buffer.len() < MAX_TCP_BUFFER_SIZE {
            buffer.resize((buffer.len() * 2).min(MAX_TCP_BUFFER_SIZE), 0);
        }
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
    if target.port == 0 {
        // 共享中继无法验证同源 IP 的首包归属，必须在认证后的控制连接声明实际端口。
        let address = unspecified(stream.local_addr()?.ip());
        protocol::write_reply_async(&mut stream, protocol::CONNECTION_NOT_ALLOWED, address).await?;
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "UDP association requires a nonzero client port",
        ));
    }
    match target.host {
        Host::Ip(ip) if !ip.is_unspecified() && normalize(ip) != normalize(peer.ip()) => {
            let address = unspecified(stream.local_addr()?.ip());
            protocol::write_reply_async(&mut stream, protocol::SERVER_FAILURE, address).await?;
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                UDP_CLIENT_ADDRESS_MISMATCH,
            ));
        }
        Host::Domain(_) => {
            let address = unspecified(stream.local_addr()?.ip());
            protocol::write_reply_async(&mut stream, protocol::ADDRESS_UNSUPPORTED, address)
                .await?;
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "UDP client address must be an IP address",
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
        stopped: CancellationToken::new(),
        flows: Mutex::new(HashMap::new()),
        preferred_addresses: Mutex::new(HashMap::new()),
    });
    if !associations
        .lock()
        .unwrap()
        .insert(id, Arc::clone(&association))
    {
        let address = unspecified(stream.local_addr()?.ip());
        protocol::write_reply_async(&mut stream, protocol::SERVER_FAILURE, address).await?;
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "UDP client endpoint already has a control connection",
        ));
    }
    if let Err(error) = protocol::write_reply_async(&mut stream, protocol::SUCCESS, relay).await {
        association.alive.store(false, Ordering::Relaxed);
        association.stopped.cancel();
        associations.lock().unwrap().remove(id);
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
    association.stopped.cancel();
    associations.lock().unwrap().remove(id);
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
    let packet_slots = Arc::new(Semaphore::new(MAX_UDP_PACKET_TASKS));
    let flows = Arc::new(UdpFlows {
        slots: Arc::new(Semaphore::new(MAX_UDP_FLOWS)),
        tasks: TaskTracker::new(),
    });
    let mut packets = JoinSet::new();
    let result = loop {
        let (length, source) = tokio::select! {
            _ = shutdown.cancelled() => break Ok(()),
            Some(outcome) = packets.join_next(), if !packets.is_empty() => {
                if let Err(error) = outcome {
                    eprintln!("UDP packet task failed: {error}");
                }
                continue;
            }
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
                        break Err(error);
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

        let association = associations.lock().unwrap().find(source);

        let Some(association) = association else {
            continue;
        };
        let Ok(permit) = Arc::clone(&packet_slots).try_acquire_owned() else {
            continue;
        };

        let relay = Arc::clone(&udp);
        let config = Arc::clone(&config);
        let dns = Arc::clone(&dns);
        let flows = Arc::clone(&flows);
        let payload = payload.to_vec();
        packets.spawn(async move {
            let _permit = permit;
            tokio::select! {
                _ = association.stopped.cancelled() => {}
                _ = handle_udp_packet(
                    &relay,
                    &association,
                    &config,
                    &dns,
                    &flows,
                    target,
                    payload,
                ) => {}
            }
        });
    };
    // 先停止建流任务，再等待回复任务释放套接字和全局流许可。
    shutdown.cancel();
    packets.shutdown().await;
    stop_associations(&associations);
    flows.tasks.close();
    flows.tasks.wait().await;
    result
}

async fn handle_udp_packet(
    udp: &Arc<tokio::net::UdpSocket>,
    association: &Arc<Association>,
    config: &Config,
    dns: &DnsResolver,
    flow_workers: &Arc<UdpFlows>,
    target: Target,
    payload: Vec<u8>,
) {
    if !association.alive.load(Ordering::Relaxed) {
        return;
    }

    let mut attempts = 0;
    let mut stale_retries = 0;
    loop {
        let slot = {
            let mut flows = association.flows.lock().unwrap();
            if let Some(slot) = flows.get(&target) {
                Arc::clone(slot)
            } else {
                if flows.len() >= MAX_FLOWS_PER_ASSOCIATION {
                    return;
                }
                let Ok(permit) = Arc::clone(&flow_workers.slots).try_acquire_owned() else {
                    return;
                };
                let slot = Arc::new(FlowSlot {
                    id: NEXT_UDP_FLOW_ID.fetch_add(1, Ordering::Relaxed),
                    flow: OnceCell::new(),
                    _permit: permit,
                });
                flows.insert(target.clone(), Arc::clone(&slot));
                slot
            }
        };
        let flow = slot
            .flow
            .get_or_init(|| async {
                let mut lookup = dns.lookup_udp(&target).await.ok()?;
                let preferred = association
                    .preferred_addresses
                    .lock()
                    .unwrap()
                    .get(&target)
                    .map(|preference| preference.address);
                // 已知备用地址属于另一族时，给予短暂解析机会，避免反复选中失效地址。
                if preferred.is_some_and(|address| !lookup.addresses.contains(&address))
                    && let Some(pending) = lookup.pending.as_mut()
                    && let Ok(result) = tokio::time::timeout(UDP_DNS_FALLBACK_GRACE, pending).await
                {
                    lookup.pending = None;
                    if let Ok(more) = result {
                        append_udp_addresses(&mut lookup.addresses, more);
                    }
                }
                let mut addresses = lookup.addresses;
                let mut selected = None;
                loop {
                    let start = preferred
                        .and_then(|address| {
                            addresses.iter().position(|candidate| *candidate == address)
                        })
                        .unwrap_or(0);
                    for offset in 0..addresses.len() {
                        let index = (start + offset) % addresses.len();
                        let destination = addresses[index];
                        let Ok(std_socket) =
                            std::net::UdpSocket::bind(unspecified(destination.ip()))
                        else {
                            continue;
                        };
                        if std_socket.connect(destination).is_err()
                            || std_socket.set_nonblocking(true).is_err()
                        {
                            continue;
                        }
                        if let Ok(socket) = tokio::net::UdpSocket::from_std(std_socket) {
                            selected = Some((destination, index, socket));
                            break;
                        }
                    }
                    if selected.is_some() {
                        break;
                    }
                    // 首族套接字无法建立时仍保留晚到的另一族回退。
                    let more = lookup.pending.take()?.await.ok()?;
                    append_udp_addresses(&mut addresses, more);
                }
                let (destination, address_index, socket) = selected?;
                remember_udp_address(association, &target, destination, slot.id);
                let flow = Arc::new(Flow {
                    socket: Arc::new(socket),
                    last_activity: Mutex::new(Instant::now()),
                    active: AtomicBool::new(true),
                    stopped: CancellationToken::new(),
                    address_index,
                    addresses: Mutex::new(addresses),
                    pending_lookup: Mutex::new(lookup.pending),
                });
                let worker = Arc::clone(&flow);
                let association = Arc::clone(association);
                let relay = Arc::clone(udp);
                let slot = Arc::clone(&slot);
                let target = target.clone();
                let timeout = config.udp_timeout;
                flow_workers.tasks.spawn(async move {
                    receive_remote(
                        worker,
                        slot,
                        relay,
                        association,
                        target,
                        destination,
                        timeout,
                    )
                    .await;
                });
                Some(flow)
            })
            .await;
        let Some(flow) = flow else {
            remove_flow_slot(association, &target, &slot);
            return;
        };
        if !flow.active.load(Ordering::Relaxed) {
            remove_flow_slot(association, &target, &slot);
            stale_retries += 1;
            if stale_retries > 2 {
                return;
            }
            continue;
        }
        if !association.alive.load(Ordering::Relaxed) {
            return;
        }
        attempts += 1;
        *flow.last_activity.lock().unwrap() = Instant::now();
        if flow.socket.send(&payload).await.is_ok() {
            *flow.last_activity.lock().unwrap() = Instant::now();
            return;
        }
        if flow.active.swap(false, Ordering::Relaxed) {
            advance_udp_address(association, &target, flow, &slot);
        }
        flow.stopped.cancel();
        remove_flow_slot(association, &target, &slot);
        if attempts >= flow.addresses.lock().unwrap().len() {
            return;
        }
    }
}

fn remove_flow_slot(association: &Association, target: &Target, slot: &Arc<FlowSlot>) {
    let mut flows = association.flows.lock().unwrap();
    if flows
        .get(target)
        .is_some_and(|current| Arc::ptr_eq(current, slot))
    {
        flows.remove(target);
    }
}

async fn receive_remote(
    flow: Arc<Flow>,
    slot: Arc<FlowSlot>,
    relay: Arc<tokio::net::UdpSocket>,
    association: Arc<Association>,
    target: Target,
    destination: SocketAddr,
    timeout: Duration,
) {
    // 无回复的 UDP 流不预分配最大报文缓冲，收到就绪通知后再申请。
    let mut buffer = Vec::new();
    let mut pending_lookup = flow.pending_lookup.lock().unwrap().take();
    let mut received_reply = false;
    let mut failed = false;
    while association.alive.load(Ordering::Relaxed) {
        let deadline = if timeout.is_zero() {
            None
        } else {
            let Some(deadline) = flow.last_activity.lock().unwrap().checked_add(timeout) else {
                failed = true;
                break;
            };
            if Instant::now() >= deadline {
                failed = !received_reply;
                break;
            }
            Some(tokio::time::Instant::from_std(deadline))
        };
        let received = if let Some(deadline) = deadline {
            tokio::select! {
                _ = association.stopped.cancelled() => break,
                _ = flow.stopped.cancelled() => break,
                _ = tokio::time::sleep_until(deadline) => continue,
                result = async { pending_lookup.as_mut().unwrap().await }, if pending_lookup.is_some() => {
                    if let Ok(more) = result {
                        append_udp_addresses(&mut flow.addresses.lock().unwrap(), more);
                    }
                    pending_lookup = None;
                    continue;
                }
                result = receive_udp_payload(&flow.socket, &mut buffer) => result,
            }
        } else {
            tokio::select! {
                _ = association.stopped.cancelled() => break,
                _ = flow.stopped.cancelled() => break,
                result = async { pending_lookup.as_mut().unwrap().await }, if pending_lookup.is_some() => {
                    if let Ok(more) = result {
                        append_udp_addresses(&mut flow.addresses.lock().unwrap(), more);
                    }
                    pending_lookup = None;
                    continue;
                }
                result = receive_udp_payload(&flow.socket, &mut buffer) => result,
            }
        };

        match received {
            Ok(length) => {
                if !association.alive.load(Ordering::Relaxed) {
                    break;
                }
                received_reply = true;
                *flow.last_activity.lock().unwrap() = Instant::now();
                let source = { *association.endpoint.lock().unwrap() };
                if let Some(source) = source {
                    let header_length = protocol::datagram_header_len(destination);
                    let start = 22 - header_length;
                    protocol::write_datagram_header(&mut buffer[start..22], destination);
                    tokio::select! {
                        _ = association.stopped.cancelled() => break,
                        _ = flow.stopped.cancelled() => break,
                        _ = relay.send_to(&buffer[start..22 + length], source) => {}
                    }
                }
            }
            Err(_) => {
                failed = true;
                break;
            }
        }
    }

    let was_active = flow.active.swap(false, Ordering::Relaxed);
    failed |= flow.stopped.is_cancelled() && !received_reply;
    if failed && association.alive.load(Ordering::Relaxed) {
        // 流先停止转发，再在 DNS 总预算内补全备用地址；保留 slot 许可限制等待任务数量。
        if let Some(pending) = pending_lookup.take() {
            tokio::select! {
                _ = association.stopped.cancelled() => {}
                result = pending => {
                    if let Ok(more) = result {
                        append_udp_addresses(&mut flow.addresses.lock().unwrap(), more);
                    }
                }
            }
        }
    }
    if (was_active || flow.stopped.is_cancelled())
        && failed
        && association.alive.load(Ordering::Relaxed)
    {
        advance_udp_address(&association, &target, &flow, &slot);
    }
    remove_flow_slot(&association, &target, &slot);
}

async fn receive_udp_payload(
    socket: &tokio::net::UdpSocket,
    buffer: &mut Vec<u8>,
) -> io::Result<usize> {
    loop {
        socket.readable().await?;
        if buffer.is_empty() {
            buffer.resize(UDP_BUFFER_SIZE, 0);
        }
        match socket.try_recv(&mut buffer[22..]) {
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
            result => return result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Whitelist;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{Shutdown, TcpListener, TcpStream, UdpSocket};
    use std::sync::mpsc;
    use std::thread;

    #[test]
    fn network_unreachable_connection_errors_are_silent() {
        let unreachable = io::Error::from(io::ErrorKind::NetworkUnreachable);
        let refused = io::Error::from(io::ErrorKind::ConnectionRefused);
        assert!(!should_log_client_error(&unreachable));
        assert!(should_log_client_error(&refused));
        #[cfg(target_os = "linux")]
        assert!(!should_log_client_error(&io::Error::from_raw_os_error(101)));
    }

    fn start_server(
        username: &str,
        password: &str,
        whitelist: &str,
    ) -> (
        SocketAddr,
        CancellationToken,
        thread::JoinHandle<io::Result<()>>,
    ) {
        start_server_with_timeout(username, password, whitelist, None)
    }

    fn start_server_with_timeout(
        username: &str,
        password: &str,
        whitelist: &str,
        tcp_timeout: Option<Duration>,
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
            tcp_timeout,
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

    fn test_association(port: u16) -> Arc<Association> {
        Arc::new(Association {
            client_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            requested_port: port,
            endpoint: Mutex::new(None),
            alive: AtomicBool::new(true),
            stopped: CancellationToken::new(),
            flows: Mutex::new(HashMap::new()),
            preferred_addresses: Mutex::new(HashMap::new()),
        })
    }

    fn test_config() -> Config {
        Config {
            port: 0,
            username: String::new(),
            password: String::new(),
            whitelist: Whitelist::parse("127.0.0.1").unwrap(),
            tcp_timeout: None,
            udp_timeout: Duration::ZERO,
            dns_servers: vec!["127.0.0.1:53".parse().unwrap()],
        }
    }

    async fn udp_domain_association(
        config: Config,
    ) -> (
        tokio::net::TcpStream,
        tokio::net::UdpSocket,
        SocketAddr,
        CancellationToken,
        tokio::task::JoinHandle<io::Result<()>>,
        Associations,
    ) {
        let shutdown = CancellationToken::new();
        let server = Server::bind(config, shutdown.clone()).unwrap();
        let proxy = SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            server.local_addr().unwrap().port(),
        );
        let associations = Arc::clone(&server.associations);
        let task = tokio::spawn(server.run_async());
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut control = tokio::net::TcpStream::connect(proxy).await.unwrap();
        control.write_all(&[5, 1, 0]).await.unwrap();
        let mut reply = [0; 2];
        control.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply, [5, 0]);
        protocol::write_request_async(
            &mut control,
            protocol::UDP_ASSOCIATE,
            &Target::from(socket.local_addr().unwrap()),
        )
        .await
        .unwrap();
        let (status, relay) = protocol::read_reply_async(&mut control).await.unwrap();
        assert_eq!(status, protocol::SUCCESS);
        let Host::Ip(ip) = relay.host else {
            panic!("中继必须返回 IP 地址")
        };
        (
            control,
            socket,
            SocketAddr::new(ip, relay.port),
            shutdown,
            task,
            associations,
        )
    }

    #[tokio::test]
    async fn udp_domain_first_packet_uses_available_family_without_waiting() {
        use crate::dns::tests::{DnsReply, scripted_dns};
        for ipv6 in [false, true] {
            let reply = DnsReply::Answer(Duration::ZERO);
            let dns = if ipv6 {
                scripted_dns(DnsReply::Drop, reply, 60).await
            } else {
                scripted_dns(reply, DnsReply::Drop, 60).await
            };
            let remote = tokio::net::UdpSocket::bind(if ipv6 { "[::1]:0" } else { "127.0.0.42:0" })
                .await
                .unwrap();
            let mut config = test_config();
            config.dns_servers = vec![dns.address];
            let (_control, client, relay, shutdown, task, _) = udp_domain_association(config).await;
            let target = Target {
                host: Host::Domain("udp-fast.example.".into()),
                port: remote.local_addr().unwrap().port(),
            };
            let packet = protocol::encode_datagram_to(&target, b"first").unwrap();
            client.send_to(&packet, relay).await.unwrap();
            let mut buffer = [0; 64];
            let (length, source) =
                tokio::time::timeout(Duration::from_millis(700), remote.recv_from(&mut buffer))
                    .await
                    .expect("UDP 首包被无响应的另一地址族阻塞")
                    .unwrap();
            assert_eq!(&buffer[..length], b"first");
            remote.send_to(&buffer[..length], source).await.unwrap();
            let (length, _) =
                tokio::time::timeout(Duration::from_secs(1), client.recv_from(&mut buffer))
                    .await
                    .unwrap()
                    .unwrap();
            assert_eq!(
                protocol::parse_datagram(&buffer[..length]).unwrap().1,
                b"first"
            );
            shutdown.cancel();
            tokio::time::timeout(Duration::from_secs(1), task)
                .await
                .expect("取消服务必须同时停止未完成的 DNS 查询")
                .unwrap()
                .unwrap();
        }
    }

    #[tokio::test]
    async fn udp_domain_keeps_delayed_family_for_next_flow() {
        use crate::dns::tests::{DnsReply, scripted_dns};
        for ipv6 in [false, true] {
            let delayed = DnsReply::Answer(Duration::from_millis(400));
            let first = DnsReply::Answer(Duration::ZERO);
            let dns = if ipv6 {
                scripted_dns(first, delayed, 60).await
            } else {
                scripted_dns(delayed, first, 60).await
            };
            let remote = tokio::net::UdpSocket::bind(if ipv6 { "[::1]:0" } else { "127.0.0.42:0" })
                .await
                .unwrap();
            let destination = remote.local_addr().unwrap();
            let mut config = test_config();
            config.dns_servers = vec![dns.address];
            config.udp_timeout = Duration::from_millis(250);
            let (_control, client, relay, shutdown, task, associations) =
                udp_domain_association(config).await;
            let target = Target {
                host: Host::Domain("udp-fallback.example.".into()),
                port: destination.port(),
            };
            let packet = protocol::encode_datagram_to(&target, b"fallback").unwrap();
            client.send_to(&packet, relay).await.unwrap();
            tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    let ready = {
                        let registry = associations.lock().unwrap();
                        registry.entries.values().any(|association| {
                            association
                                .preferred_addresses
                                .lock()
                                .unwrap()
                                .get(&target)
                                .map(|preference| preference.address)
                                == Some(destination)
                        })
                    };
                    if ready {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("晚到地址未被保留为备用目标");
            client.send_to(&packet, relay).await.unwrap();
            let mut buffer = [0; 64];
            let (length, _) =
                tokio::time::timeout(Duration::from_millis(700), remote.recv_from(&mut buffer))
                    .await
                    .unwrap()
                    .unwrap();
            assert_eq!(&buffer[..length], b"fallback");
            shutdown.cancel();
            task.await.unwrap().unwrap();
        }
    }

    #[tokio::test]
    async fn udp_reply_buffer_is_lazy_and_keeps_large_datagrams() {
        let receiver = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let sender = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut buffer = Vec::new();
        assert!(
            tokio::time::timeout(
                Duration::from_millis(30),
                receive_udp_payload(&receiver, &mut buffer)
            )
            .await
            .is_err()
        );
        assert_eq!(buffer.capacity(), 0);
        let payload = vec![42; 60_000];
        sender
            .send_to(&payload, receiver.local_addr().unwrap())
            .await
            .unwrap();
        let length = tokio::time::timeout(
            Duration::from_secs(1),
            receive_udp_payload(&receiver, &mut buffer),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(length, payload.len());
        assert_eq!(&buffer[22..22 + length], payload);
    }

    #[tokio::test]
    async fn old_udp_worker_cannot_replace_new_flow_preference_or_exceed_limit() {
        let (association, flow, _, target, destination) = test_udp_flow().await;
        let old_slot = Arc::clone(association.flows.lock().unwrap().get(&target).unwrap());
        let new_id = NEXT_UDP_FLOW_ID.fetch_add(1, Ordering::Relaxed);
        remember_udp_address(&association, &target, destination, new_id);
        advance_udp_address(&association, &target, &flow, &old_slot);
        assert_eq!(
            association.preferred_addresses.lock().unwrap()[&target].address,
            destination
        );
        association.preferred_addresses.lock().unwrap().clear();
        for port in 1..=MAX_FLOWS_PER_ASSOCIATION {
            let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port as u16);
            remember_udp_address(&association, &Target::from(address), address, new_id);
        }
        advance_udp_address(&association, &target, &flow, &old_slot);
        let preferred = association.preferred_addresses.lock().unwrap();
        assert_eq!(preferred.len(), MAX_FLOWS_PER_ASSOCIATION);
        assert!(!preferred.contains_key(&target));
    }

    #[tokio::test]
    async fn tcp_buffers_grow_for_bulk_reads_and_stay_small_for_short_messages() {
        struct ProbeReader {
            remaining: usize,
            sizes: Vec<usize>,
        }
        impl tokio::io::AsyncRead for ProbeReader {
            fn poll_read(
                mut self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
                buffer: &mut tokio::io::ReadBuf<'_>,
            ) -> std::task::Poll<io::Result<()>> {
                self.sizes.push(buffer.remaining());
                let length = self.remaining.min(buffer.remaining());
                buffer.put_slice(&vec![42; length]);
                self.remaining -= length;
                std::task::Poll::Ready(Ok(()))
            }
        }
        for length in [1024, 256 * 1024] {
            let mut reader = ProbeReader {
                remaining: length,
                sizes: Vec::new(),
            };
            let mut output = Vec::new();
            assert_eq!(
                copy_direction(&mut reader, &mut output, None)
                    .await
                    .unwrap(),
                length as u64
            );
            assert_eq!(output, vec![42; length]);
            assert_eq!(reader.sizes[0], INITIAL_TCP_BUFFER_SIZE);
            if length < INITIAL_TCP_BUFFER_SIZE {
                assert!(
                    reader
                        .sizes
                        .iter()
                        .all(|size| *size == INITIAL_TCP_BUFFER_SIZE)
                );
            } else {
                assert_eq!(
                    reader.sizes.iter().copied().max(),
                    Some(MAX_TCP_BUFFER_SIZE)
                );
            }
        }
    }

    #[test]
    fn rejects_duplicate_listener() {
        let first = Server::bind(test_config(), CancellationToken::new()).unwrap();
        let mut config = test_config();
        config.port = first.local_addr().unwrap().port();
        assert!(Server::bind(config, CancellationToken::new()).is_err());
    }

    #[test]
    fn udp_association_rejects_unspecified_client_port() {
        let (proxy, shutdown, server_thread) = start_server("", "", "127.0.0.1,::1");
        for host in [
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
        ] {
            let mut client = TcpStream::connect(SocketAddr::new(host, proxy.port())).unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            assert_eq!(negotiate_client(&mut client, 0), [5, 0]);
            let reply = request(
                &mut client,
                protocol::UDP_ASSOCIATE,
                SocketAddr::new(host, 0),
            );
            assert_eq!(
                reply[1],
                protocol::CONNECTION_NOT_ALLOWED,
                "未声明客户端端口的关联必须被拒绝"
            );
        }
        shutdown.cancel();
        server_thread.join().unwrap().unwrap();
    }

    #[tokio::test]
    async fn blocking_server_rejects_current_thread_runtime() {
        let server = Server::bind(test_config(), CancellationToken::new()).unwrap();
        let error = server.run().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    }

    #[test]
    fn binding_validates_programmatic_configuration() {
        let mut config = test_config();
        config.username = "admin".into();
        assert!(matches!(
            Server::bind(config, CancellationToken::new()),
            Err(error) if error.kind() == io::ErrorKind::InvalidInput
        ));
        let mut config = test_config();
        config.udp_timeout = Duration::MAX;
        assert!(matches!(
            Server::bind(config, CancellationToken::new()),
            Err(error) if error.kind() == io::ErrorKind::InvalidInput
        ));
    }

    #[tokio::test]
    async fn aborting_server_releases_udp_listener() {
        let shutdown = CancellationToken::new();
        let server = Server::bind(test_config(), shutdown.clone()).unwrap();
        let proxy = SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            server.local_addr().unwrap().port(),
        );
        let task = tokio::spawn(server.run_async());
        let mut client = tokio::net::TcpStream::connect(proxy).await.unwrap();
        client.write_all(&[5, 1, 0]).await.unwrap();
        client.read_exact(&mut [0; 2]).await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(shutdown.is_cancelled());
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if tokio::net::UdpSocket::bind(proxy).await.is_ok() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("取消服务后 UDP 监听端口仍被占用");
    }

    #[tokio::test]
    async fn shutdown_closes_clients_before_returning_to_external_runtime() {
        let shutdown = CancellationToken::new();
        let server = Server::bind(test_config(), shutdown.clone()).unwrap();
        let proxy = SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            server.local_addr().unwrap().port(),
        );
        let associations = Arc::clone(&server.associations);
        let task = tokio::spawn(server.run_async());
        // 覆盖协商前、等待请求和已建立 UDP 关联的三个阶段。
        let mut waiting_auth = tokio::net::TcpStream::connect(proxy).await.unwrap();
        let mut waiting_request = tokio::net::TcpStream::connect(proxy).await.unwrap();
        waiting_request.write_all(&[5, 1, 0]).await.unwrap();
        let mut reply = [0; 2];
        waiting_request.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply, [5, 0]);
        let mut control = tokio::net::TcpStream::connect(proxy).await.unwrap();
        control.write_all(&[5, 1, 0]).await.unwrap();
        control.read_exact(&mut reply).await.unwrap();
        let client_udp = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = client_udp.local_addr().unwrap().port().to_be_bytes();
        control
            .write_all(&[
                5,
                protocol::UDP_ASSOCIATE,
                0,
                1,
                0,
                0,
                0,
                0,
                port[0],
                port[1],
            ])
            .await
            .unwrap();
        let mut association_reply = [0; 10];
        control.read_exact(&mut association_reply).await.unwrap();
        assert_eq!(association_reply[1], protocol::SUCCESS);
        let remote = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client_udp
            .send_to(
                &protocol::encode_datagram(remote.local_addr().unwrap(), b"active"),
                proxy,
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), remote.recv_from(&mut [0; 32]))
            .await
            .unwrap()
            .unwrap();
        let association = Arc::clone(
            associations
                .lock()
                .unwrap()
                .entries
                .values()
                .next()
                .unwrap(),
        );
        assert_eq!(association.flows.lock().unwrap().len(), 1);
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(associations.lock().unwrap().entries.is_empty());
        assert!(association.flows.lock().unwrap().is_empty());
        for stream in [&mut waiting_auth, &mut waiting_request, &mut control] {
            let closed = tokio::time::timeout(Duration::from_millis(300), stream.read(&mut reply))
                .await
                .expect("服务退出后客户端仍然存活");
            assert!(matches!(closed, Ok(0) | Err(_)));
        }
    }

    #[test]
    fn udp_registry_requires_declared_ports_and_isolates_clients() {
        let mut registry = AssociationRegistry::default();
        let first = test_association(40001);
        let second = test_association(40002);
        assert!(registry.insert(1, Arc::clone(&first)));
        assert!(registry.insert(2, Arc::clone(&second)));
        assert!(!registry.insert(3, test_association(0)));
        assert!(!registry.insert(4, test_association(0)));
        assert!(!registry.insert(5, test_association(40001)));
        assert!(Arc::ptr_eq(
            &registry.find("127.0.0.1:40001".parse().unwrap()).unwrap(),
            &first
        ));
        assert!(Arc::ptr_eq(
            &registry
                .find("[::ffff:127.0.0.1]:40001".parse().unwrap())
                .unwrap(),
            &first
        ));
        assert!(Arc::ptr_eq(
            &registry.find("127.0.0.1:40002".parse().unwrap()).unwrap(),
            &second
        ));
        assert!(registry.find("127.0.0.1:40003".parse().unwrap()).is_none());
        registry.remove(1);
        assert!(registry.find("127.0.0.1:40001".parse().unwrap()).is_none());
        assert!(registry.find("127.0.0.1:40003".parse().unwrap()).is_none());
        assert!(registry.insert(4, test_association(40001)));
    }

    async fn test_udp_flow() -> (
        Arc<Association>,
        Arc<Flow>,
        Arc<tokio::net::UdpSocket>,
        Target,
        SocketAddr,
    ) {
        let remote = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let destination = remote.local_addr().unwrap();
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        socket.connect(destination).await.unwrap();
        let flow = Arc::new(Flow {
            socket: Arc::new(socket),
            last_activity: Mutex::new(Instant::now()),
            active: AtomicBool::new(true),
            stopped: CancellationToken::new(),
            address_index: 0,
            addresses: Mutex::new(vec![destination, "127.0.0.2:9".parse().unwrap()]),
            pending_lookup: Mutex::new(None),
        });
        let target = Target::from(destination);
        let permit = Arc::new(Semaphore::new(1)).try_acquire_owned().unwrap();
        let slot = Arc::new(FlowSlot {
            id: NEXT_UDP_FLOW_ID.fetch_add(1, Ordering::Relaxed),
            flow: OnceCell::new(),
            _permit: permit,
        });
        assert!(slot.flow.set(Some(Arc::clone(&flow))).is_ok());
        let preference = UdpPreference {
            address: destination,
            flow_id: slot.id,
        };
        let association = Arc::new(Association {
            client_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            requested_port: 0,
            endpoint: Mutex::new(None),
            alive: AtomicBool::new(true),
            stopped: CancellationToken::new(),
            flows: Mutex::new(HashMap::from([(target.clone(), slot)])),
            preferred_addresses: Mutex::new(HashMap::from([(target.clone(), preference)])),
        });
        let relay = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
        (association, flow, relay, target, destination)
    }

    #[tokio::test]
    async fn closed_udp_association_stops_unlimited_flow() {
        let (association, flow, relay, target, destination) = test_udp_flow().await;
        let slot = Arc::clone(association.flows.lock().unwrap().get(&target).unwrap());
        let task = tokio::spawn(receive_remote(
            Arc::clone(&flow),
            slot,
            relay,
            Arc::clone(&association),
            target,
            destination,
            Duration::ZERO,
        ));
        tokio::task::yield_now().await;
        association.alive.store(false, Ordering::Relaxed);
        association.stopped.cancel();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        assert!(association.flows.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn outgoing_udp_activity_extends_flow_lifetime() {
        let (association, flow, relay, target, destination) = test_udp_flow().await;
        let slot = Arc::clone(association.flows.lock().unwrap().get(&target).unwrap());
        let task = tokio::spawn(receive_remote(
            Arc::clone(&flow),
            slot,
            relay,
            Arc::clone(&association),
            target.clone(),
            destination,
            Duration::from_millis(300),
        ));
        tokio::time::sleep(Duration::from_millis(200)).await;
        *flow.last_activity.lock().unwrap() = Instant::now();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!task.is_finished());
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        assert!(association.flows.lock().unwrap().is_empty());
        assert_eq!(
            association
                .preferred_addresses
                .lock()
                .unwrap()
                .get(&target)
                .map(|preference| preference.address),
            Some("127.0.0.2:9".parse().unwrap())
        );
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

    fn read_http_head(reader: &mut impl BufRead) -> String {
        let mut head = String::new();
        loop {
            let start = head.len();
            assert_ne!(reader.read_line(&mut head).unwrap(), 0);
            if &head[start..] == "\r\n" {
                return head;
            }
        }
    }

    #[test]
    fn http_connect_and_socks5_share_the_same_tcp_port() {
        let origin = TcpListener::bind("127.0.0.1:0").unwrap();
        let destination = origin.local_addr().unwrap();
        let echo = thread::spawn(move || {
            let (mut stream, _) = origin.accept().unwrap();
            let mut payload = Vec::new();
            stream.read_to_end(&mut payload).unwrap();
            stream.write_all(&payload).unwrap();
        });
        let (proxy, shutdown, server_thread) = start_server("admin", "secret", "127.0.0.1");
        let mut socks = TcpStream::connect(proxy).unwrap();
        socks
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        assert_eq!(negotiate_client(&mut socks, 2), [5, 2]);

        let http = TcpStream::connect(proxy).unwrap();
        http.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut http = BufReader::new(http);
        write!(
            http.get_mut(),
            "CONNECT {destination} HTTP/1.1\r\nHost: {destination}\r\n\r\n"
        )
        .unwrap();
        let head = read_http_head(&mut http).to_ascii_lowercase();
        assert!(head.starts_with("http/1.1 407 "));
        assert!(head.contains("proxy-authenticate: basic"));

        write!(
            http.get_mut(),
            "CONNECT {destination} HTTP/1.1\r\nHost: {destination}\r\nProxy-Authorization: Basic YWRtaW46c2VjcmV0\r\n\r\nprefetched tunnel bytes"
        )
        .unwrap();
        let head = read_http_head(&mut http).to_ascii_lowercase();
        assert!(head.starts_with("http/1.1 200 "));
        assert!(!head.contains("content-length:"));
        assert!(!head.contains("transfer-encoding:"));
        http.get_mut().shutdown(Shutdown::Write).unwrap();
        let mut payload = Vec::new();
        http.read_to_end(&mut payload).unwrap();
        assert_eq!(payload, b"prefetched tunnel bytes");
        echo.join().unwrap();
        shutdown.cancel();
        server_thread.join().unwrap().unwrap();
    }

    #[test]
    fn http_proxy_reuses_upstream_and_reconnects_after_close() {
        let origin = TcpListener::bind("127.0.0.1:0").unwrap();
        let destination = origin.local_addr().unwrap();
        let origin_thread = thread::spawn(move || {
            for _ in 0..2 {
                let (stream, _) = origin.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut stream = BufReader::new(stream);
                for index in 0..3 {
                    let head = read_http_head(&mut stream).to_ascii_lowercase();
                    assert!(head.starts_with("get / http/1.1\r\n"));
                    assert!(!head.contains("connection: close"));
                    let connection = if index == 2 {
                        "Connection: close\r\n"
                    } else {
                        ""
                    };
                    write!(
                        stream.get_mut(),
                        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n{connection}\r\nOK"
                    )
                    .unwrap();
                }
            }
        });
        let (proxy, shutdown, server_thread) = start_server("", "", "127.0.0.1");
        let client = TcpStream::connect(proxy).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut client = BufReader::new(client);
        for _ in 0..6 {
            write!(
                client.get_mut(),
                "GET http://{destination}/ HTTP/1.1\r\nHost: ignored\r\n\r\n"
            )
            .unwrap();
            assert!(read_http_head(&mut client).starts_with("HTTP/1.1 200"));
            let mut body = [0; 2];
            client.read_exact(&mut body).unwrap();
            assert_eq!(&body, b"OK");
        }
        origin_thread.join().unwrap();
        shutdown.cancel();
        server_thread.join().unwrap().unwrap();
    }

    #[test]
    fn http_proxy_forwards_body_and_rewrites_headers() {
        let origin = TcpListener::bind("127.0.0.1:0").unwrap();
        let destination = origin.local_addr().unwrap();
        let origin_thread = thread::spawn(move || {
            let (stream, _) = origin.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut stream = BufReader::new(stream);
            let head = read_http_head(&mut stream).to_ascii_lowercase();
            assert!(head.starts_with("post /submit?key=value http/1.1\r\n"));
            assert!(head.contains(&format!("host: {destination}\r\n")));
            assert!(!head.contains("proxy-authorization"));
            assert!(!head.contains("x-request-hop:"));
            assert!(!head.contains("expect:"));
            let mut payload = [0; 5];
            stream.read_exact(&mut payload).unwrap();
            assert_eq!(&payload, b"brume");
            stream
                .get_mut()
                .write_all(
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close, X-Response-Hop\r\nX-Response-Hop: remove\r\n\r\n5\r\nbrume\r\n0\r\n\r\n",
                )
                .unwrap();
        });
        let (proxy, shutdown, server_thread) = start_server("admin", "secret", "127.0.0.1");
        let client = TcpStream::connect(proxy).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut client = BufReader::new(client);
        write!(
            client.get_mut(),
            "POST http://{destination}/submit?key=value HTTP/1.1\r\nHost: ignored.example\r\nProxy-Authorization: Basic YWRtaW46c2VjcmV0\r\nConnection: close, X-Request-Hop\r\nX-Request-Hop: remove\r\nExpect: 100-continue\r\nContent-Length: 5\r\n\r\n"
        )
        .unwrap();
        assert!(read_http_head(&mut client).starts_with("HTTP/1.1 100 "));
        client.get_mut().write_all(b"brume").unwrap();
        let head = read_http_head(&mut client).to_ascii_lowercase();
        assert!(head.starts_with("http/1.1 200 "));
        assert!(!head.contains("x-response-hop:"));
        assert!(head.contains("transfer-encoding: chunked"));
        let mut payload = Vec::new();
        client.read_to_end(&mut payload).unwrap();
        assert_eq!(payload, b"5\r\nbrume\r\n0\r\n\r\n");
        origin_thread.join().unwrap();
        shutdown.cancel();
        server_thread.join().unwrap().unwrap();
    }

    #[test]
    fn one_way_tcp_transfer_keeps_idle_connection_alive() {
        let remote = TcpListener::bind("127.0.0.1:0").unwrap();
        let destination = remote.local_addr().unwrap();
        let sender = thread::spawn(move || {
            let (mut stream, _) = remote.accept().unwrap();
            for _ in 0..12 {
                stream.write_all(b"x").unwrap();
                thread::sleep(Duration::from_millis(50));
            }
        });
        let (proxy, shutdown, server_thread) =
            start_server_with_timeout("", "", "127.0.0.1", Some(Duration::from_millis(250)));
        let mut client = TcpStream::connect(proxy).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        assert_eq!(negotiate_client(&mut client, 0), [5, 0]);
        assert_eq!(
            request(&mut client, protocol::CONNECT, destination)[1],
            protocol::SUCCESS
        );
        let mut payload = [0; 12];
        client.read_exact(&mut payload).unwrap();
        assert_eq!(&payload, b"xxxxxxxxxxxx");
        sender.join().unwrap();
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
            client_udp.local_addr().unwrap(),
        );
        assert_eq!(reply[1], protocol::SUCCESS);
        let relay = SocketAddr::new(proxy.ip(), u16::from_be_bytes([reply[8], reply[9]]));
        // 同一 IP 的其他端口不能抢占已认证控制连接声明的端点。
        let other_udp = UdpSocket::bind("127.0.0.1:0").unwrap();
        other_udp
            .set_read_timeout(Some(Duration::from_millis(150)))
            .unwrap();
        other_udp.send_to(&packet, relay).unwrap();
        assert!(other_udp.recv_from(&mut [0; 100]).is_err());
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
        let client_udp = UdpSocket::bind("[::1]:0").unwrap();
        let client_port = client_udp.local_addr().unwrap().port().to_be_bytes();
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
        request[20..22].copy_from_slice(&client_port);
        control.write_all(&request).unwrap();
        control.read_exact(&mut request).unwrap();
        assert_eq!(request[1], protocol::SUCCESS);
        let relay = SocketAddr::new(proxy.ip(), u16::from_be_bytes([request[20], request[21]]));
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
