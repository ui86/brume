use crate::config::Config;
use crate::protocol::{self, Host, Target};
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{
    IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket,
};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(200);
const UDP_BUFFER_SIZE: usize = 65_535;
const UDP_POOL_CAPACITY: usize = 64;

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
    socket: UdpSocket,
    last_activity: Mutex<Instant>,
}

struct UdpTask {
    packet: Vec<u8>,
    length: usize,
    source: SocketAddr,
}

type PacketPool = Arc<Mutex<Vec<Vec<u8>>>>;

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
    tcp: TcpListener,
    udp: Arc<UdpSocket>,
    associations: Associations,
    next_id: AtomicU64,
    shutdown: Arc<AtomicBool>,
}

impl Server {
    pub fn bind(config: Config, shutdown: Arc<AtomicBool>) -> io::Result<Self> {
        let address = SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), config.port);
        let tcp_socket = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))?;
        tcp_socket.set_only_v6(false)?;
        tcp_socket.bind(&address.into())?;
        tcp_socket.listen(1024)?;
        let tcp: TcpListener = tcp_socket.into();
        let udp_socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
        udp_socket.set_only_v6(false)?;
        let udp_address =
            SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), tcp.local_addr()?.port());
        udp_socket.bind(&udp_address.into())?;
        let udp = Arc::new(UdpSocket::from(udp_socket));
        udp.set_read_timeout(Some(POLL_INTERVAL))?;
        Ok(Self {
            config: Arc::new(config),
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
        let relay = Arc::clone(&self.udp);
        let associations = Arc::clone(&self.associations);
        let shutdown = Arc::clone(&self.shutdown);
        let config = Arc::clone(&self.config);
        let udp_thread = thread::spawn(move || udp_loop(relay, associations, config, shutdown));

        let wake_address = SocketAddr::new(
            IpAddr::V6(Ipv6Addr::LOCALHOST),
            self.tcp.local_addr()?.port(),
        );
        let wake_flag = Arc::clone(&self.shutdown);
        let wake_thread = thread::spawn(move || {
            while !wake_flag.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(20));
            }
            let _ = TcpStream::connect_timeout(&wake_address, Duration::from_secs(1));
        });

        let mut result = Ok(());
        while !self.shutdown.load(Ordering::Relaxed) {
            match self.tcp.accept() {
                Ok((stream, address)) => {
                    if self.shutdown.load(Ordering::Relaxed) {
                        break;
                    }
                    if !self.config.whitelist.allows(address.ip()) {
                        continue;
                    }
                    if let Err(error) = stream.set_nonblocking(false) {
                        eprintln!("连接 {address} 设置阻塞模式失败：{error}");
                        continue;
                    }
                    let config = Arc::clone(&self.config);
                    let associations = Arc::clone(&self.associations);
                    let udp = Arc::clone(&self.udp);
                    let shutdown = Arc::clone(&self.shutdown);
                    let id = self.next_id.fetch_add(1, Ordering::Relaxed);
                    thread::spawn(move || {
                        if let Err(error) =
                            handle_client(stream, config, udp, associations, id, shutdown)
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
        self.shutdown.store(true, Ordering::Relaxed);
        let _ = wake_thread.join();
        for association in self.associations.lock().unwrap().values() {
            association.alive.store(false, Ordering::Relaxed);
        }
        let udp_result = udp_thread
            .join()
            .map_err(|_| io::Error::other("UDP 接收线程异常退出"))?;
        result.and(udp_result)
    }
}

fn negotiate(stream: &mut TcpStream, config: &Config) -> io::Result<()> {
    let mut header = [0; 2];
    stream.read_exact(&mut header)?;
    if header[0] != 5 || header[1] == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "SOCKS5 协商请求无效",
        ));
    }
    let mut methods = vec![0; usize::from(header[1])];
    stream.read_exact(&mut methods)?;
    let selected = if config.username.is_empty() { 0 } else { 2 };
    if !methods.contains(&selected) {
        stream.write_all(&[5, 255])?;
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "客户端没有提供可用的认证方法",
        ));
    }
    stream.write_all(&[5, selected])?;
    if selected == 2 {
        let mut auth_header = [0; 2];
        stream.read_exact(&mut auth_header)?;
        if auth_header[0] != 1 || auth_header[1] == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "用户名密码认证请求无效",
            ));
        }
        let mut username = vec![0; usize::from(auth_header[1])];
        stream.read_exact(&mut username)?;
        let mut password_length = [0];
        stream.read_exact(&mut password_length)?;
        if password_length[0] == 0 {
            stream.write_all(&[1, 1])?;
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "密码不能为空",
            ));
        }
        let mut password = vec![0; usize::from(password_length[0])];
        stream.read_exact(&mut password)?;
        let allowed = constant_time_eq(&username, config.username.as_bytes())
            & constant_time_eq(&password, config.password.as_bytes());
        stream.write_all(&[1, if allowed { 0 } else { 1 }])?;
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

fn handle_client(
    mut stream: TcpStream,
    config: Arc<Config>,
    udp: Arc<UdpSocket>,
    associations: Associations,
    id: u64,
    shutdown: Arc<AtomicBool>,
) -> io::Result<()> {
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
    negotiate(&mut stream, &config)?;
    let (command, target) = match protocol::read_request(&mut stream) {
        Ok(request) => request,
        Err(error) => {
            if error.kind() == io::ErrorKind::InvalidData {
                let address = unspecified(stream.local_addr()?.ip());
                let _ = protocol::write_reply(&mut stream, protocol::ADDRESS_UNSUPPORTED, address);
            }
            return Err(error);
        }
    };
    match command {
        protocol::CONNECT => connect(stream, target, config.tcp_timeout),
        protocol::UDP_ASSOCIATE => associate(&mut stream, target, udp, associations, id, shutdown),
        _ => {
            let address = unspecified(stream.local_addr()?.ip());
            protocol::write_reply(&mut stream, protocol::COMMAND_UNSUPPORTED, address)
        }
    }
}

fn connect(mut client: TcpStream, target: Target, timeout: Option<Duration>) -> io::Result<()> {
    let address = unspecified(client.local_addr()?.ip());
    let addresses = match target.lookup() {
        Ok(addresses) => addresses,
        Err(error) => {
            protocol::write_reply(&mut client, protocol::HOST_UNREACHABLE, address)?;
            return Err(error);
        }
    };
    let mut last_error = None;
    let mut remote = None;
    for destination in addresses {
        match TcpStream::connect_timeout(&destination, HANDSHAKE_TIMEOUT) {
            Ok(stream) => {
                remote = Some(stream);
                break;
            }
            Err(error) => last_error = Some(error),
        }
    }
    let mut remote = match remote {
        Some(stream) => stream,
        None => {
            let error = last_error.unwrap_or_else(|| io::Error::other("目标连接失败"));
            let status = if error.kind() == io::ErrorKind::ConnectionRefused {
                protocol::CONNECTION_REFUSED
            } else {
                protocol::HOST_UNREACHABLE
            };
            protocol::write_reply(&mut client, status, address)?;
            return Err(error);
        }
    };
    protocol::write_reply(&mut client, protocol::SUCCESS, remote.local_addr()?)?;
    client.set_read_timeout(timeout)?;
    remote.set_read_timeout(timeout)?;
    client.set_write_timeout(timeout)?;
    remote.set_write_timeout(timeout)?;

    let mut client_read = client.try_clone()?;
    let mut remote_write = remote.try_clone()?;
    let forward = thread::spawn(move || {
        let result = io::copy(&mut client_read, &mut remote_write);
        let _ = remote_write.shutdown(Shutdown::Write);
        if result.is_err() {
            let _ = client_read.shutdown(Shutdown::Both);
            let _ = remote_write.shutdown(Shutdown::Both);
        }
    });
    let result = io::copy(&mut remote, &mut client);
    let _ = client.shutdown(Shutdown::Write);
    if result.is_err() {
        let _ = client.shutdown(Shutdown::Both);
        let _ = remote.shutdown(Shutdown::Both);
    }
    let _ = forward.join();
    result.map(|_| ())
}

fn associate(
    stream: &mut TcpStream,
    target: Target,
    udp: Arc<UdpSocket>,
    associations: Associations,
    id: u64,
    shutdown: Arc<AtomicBool>,
) -> io::Result<()> {
    let peer = stream.peer_addr()?;
    match target.host {
        Host::Ip(ip) if !ip.is_unspecified() && normalize(ip) != normalize(peer.ip()) => {
            let address = unspecified(stream.local_addr()?.ip());
            protocol::write_reply(stream, protocol::SERVER_FAILURE, address)?;
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "UDP 客户端地址与控制连接不符",
            ));
        }
        Host::Domain(_) => {
            let address = unspecified(stream.local_addr()?.ip());
            protocol::write_reply(stream, protocol::ADDRESS_UNSUPPORTED, address)?;
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
    if let Err(error) = protocol::write_reply(stream, protocol::SUCCESS, relay) {
        association.alive.store(false, Ordering::Relaxed);
        associations.lock().unwrap().remove(&id);
        return Err(error);
    }
    stream.set_read_timeout(Some(POLL_INTERVAL))?;
    let mut buffer = [0; 1024];
    while !shutdown.load(Ordering::Relaxed) {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(_) => break,
        }
    }
    association.alive.store(false, Ordering::Relaxed);
    associations.lock().unwrap().remove(&id);
    Ok(())
}

fn udp_loop(
    udp: Arc<UdpSocket>,
    associations: Associations,
    config: Arc<Config>,
    shutdown: Arc<AtomicBool>,
) -> io::Result<()> {
    let (sender, receiver): (SyncSender<UdpTask>, Receiver<UdpTask>) = mpsc::sync_channel(1024);
    let receiver = Arc::new(Mutex::new(receiver));
    let pool: PacketPool = Arc::new(Mutex::new(Vec::new()));
    let worker_count = thread::available_parallelism().map_or(4, |count| count.get().clamp(4, 16));
    let workers: Vec<_> = (0..worker_count)
        .map(|_| {
            let receiver = Arc::clone(&receiver);
            let udp = Arc::clone(&udp);
            let associations = Arc::clone(&associations);
            let config = Arc::clone(&config);
            let pool = Arc::clone(&pool);
            thread::spawn(move || {
                loop {
                    let task = receiver.lock().unwrap().recv();
                    let Ok(task) = task else { break };
                    handle_udp_packet(&udp, &associations, &config, &task);
                    return_packet(&pool, task.packet);
                }
            })
        })
        .collect();
    let mut result = Ok(());
    while !shutdown.load(Ordering::Relaxed) {
        let mut packet = pool
            .lock()
            .unwrap()
            .pop()
            .unwrap_or_else(|| vec![0; UDP_BUFFER_SIZE]);
        let (length, source) = match udp.recv_from(&mut packet) {
            Ok(packet) => packet,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                return_packet(&pool, packet);
                continue;
            }
            Err(error) => {
                return_packet(&pool, packet);
                shutdown.store(true, Ordering::Relaxed);
                result = Err(error);
                break;
            }
        };
        if let Err(TrySendError::Full(task) | TrySendError::Disconnected(task)) =
            sender.try_send(UdpTask {
                packet,
                length,
                source,
            })
        {
            return_packet(&pool, task.packet);
        }
    }
    drop(sender);
    for worker in workers {
        let _ = worker.join();
    }
    result
}

fn return_packet(pool: &PacketPool, packet: Vec<u8>) {
    let mut buffers = pool.lock().unwrap();
    if buffers.len() < UDP_POOL_CAPACITY {
        buffers.push(packet);
    }
}

fn handle_udp_packet(
    udp: &Arc<UdpSocket>,
    associations: &Associations,
    config: &Config,
    task: &UdpTask,
) {
    if !config.whitelist.allows(task.source.ip()) {
        return;
    }
    let Ok((target, payload)) = protocol::parse_datagram(&task.packet[..task.length]) else {
        return;
    };
    let association = {
        let map = associations.lock().unwrap();
        map.values()
            .find(|association| {
                association.alive.load(Ordering::Relaxed) && association.is_bound_to(task.source)
            })
            .or_else(|| {
                map.values().find(|association| {
                    association.alive.load(Ordering::Relaxed) && association.accepts(task.source)
                })
            })
            .cloned()
    };
    let Some(association) = association else {
        return;
    };
    let existing = association.flows.lock().unwrap().get(&target).cloned();
    let flow = if let Some(flow) = existing {
        flow
    } else {
        let Ok(addresses) = target.lookup() else {
            return;
        };
        let destination = addresses[0];
        let Ok(socket) = UdpSocket::bind(unspecified(destination.ip())) else {
            return;
        };
        if socket.connect(destination).is_err()
            || socket
                .set_read_timeout(Some(Duration::from_secs(1)))
                .is_err()
        {
            return;
        }
        let candidate = Arc::new(Flow {
            socket,
            last_activity: Mutex::new(Instant::now()),
        });
        let mut flows = association.flows.lock().unwrap();
        if let Some(flow) = flows.get(&target) {
            Arc::clone(flow)
        } else {
            flows.insert(target.clone(), Arc::clone(&candidate));
            let association = Arc::clone(&association);
            let relay = Arc::clone(udp);
            let worker = Arc::clone(&candidate);
            let timeout = config.udp_timeout;
            thread::spawn(move || {
                receive_remote(worker, relay, association, target, destination, timeout)
            });
            candidate
        }
    };
    *flow.last_activity.lock().unwrap() = Instant::now();
    let _ = flow.socket.send(payload);
}

fn receive_remote(
    flow: Arc<Flow>,
    relay: Arc<UdpSocket>,
    association: Arc<Association>,
    target: Target,
    destination: SocketAddr,
    timeout: Duration,
) {
    let mut buffer = vec![0; UDP_BUFFER_SIZE];
    while association.alive.load(Ordering::Relaxed) {
        match flow.socket.recv(&mut buffer[22..]) {
            Ok(length) => {
                if !association.alive.load(Ordering::Relaxed) {
                    break;
                }
                *flow.last_activity.lock().unwrap() = Instant::now();
                if let Some(source) = *association.endpoint.lock().unwrap() {
                    let header_length = protocol::datagram_header_len(destination);
                    let start = 22 - header_length;
                    protocol::write_datagram_header(&mut buffer[start..22], destination);
                    let _ = relay.send_to(&buffer[start..22 + length], source);
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(_) => break,
        }
        if !timeout.is_zero() && flow.last_activity.lock().unwrap().elapsed() >= timeout {
            break;
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

    fn start_server(
        username: &str,
        password: &str,
        whitelist: &str,
    ) -> (
        SocketAddr,
        Arc<AtomicBool>,
        thread::JoinHandle<io::Result<()>>,
    ) {
        let config = Config {
            port: 0,
            username: username.into(),
            password: password.into(),
            whitelist: Whitelist::parse(whitelist).unwrap(),
            tcp_timeout: None,
            udp_timeout: Duration::from_secs(3),
        };
        let shutdown = Arc::new(AtomicBool::new(false));
        let server = Server::bind(config, Arc::clone(&shutdown)).unwrap();
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
            .set_read_timeout(Some(Duration::from_secs(3)))
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
        shutdown.store(true, Ordering::Relaxed);
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
        shutdown.store(true, Ordering::Relaxed);
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
        shutdown.store(true, Ordering::Relaxed);
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
        shutdown.store(true, Ordering::Relaxed);
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
        shutdown.store(true, Ordering::Relaxed);
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
        shutdown.store(true, Ordering::Relaxed);
        server_thread.join().unwrap().unwrap();
    }
}
