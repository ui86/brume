use crate::happy_eyeballs::happy_eyeballs_connect;
use crate::protocol::{self, Host, Target};
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::{Mutex, OnceLock, mpsc};
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

fn connect_proxy(server: String) -> io::Result<TcpStream> {
    if let Ok(address) = server.parse::<SocketAddr>() {
        return TcpStream::connect_timeout(&address, CONNECT_TIMEOUT);
    }
    let (sender, receiver) = mpsc::sync_channel(1);
    let task = connector_runtime()?.spawn(async move {
        let result = tokio::time::timeout(CONNECT_TIMEOUT, async {
            let addresses: Vec<_> = tokio::net::lookup_host(server.as_str()).await?.collect();
            let stream = happy_eyeballs_connect(&addresses, CONNECT_TIMEOUT).await?;
            stream.into_std()
        })
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "连接代理服务器超时"))
        .and_then(|result| result);
        let _ = sender.send(result);
    });
    let stream = match receiver.recv_timeout(CONNECT_TIMEOUT + Duration::from_secs(1)) {
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

pub struct Client {
    server: String,
    username: String,
    password: String,
    tcp_timeout: Option<Duration>,
    udp_timeout: Option<Duration>,
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
        })
    }

    pub fn set_tcp_timeout(&mut self, timeout: Option<Duration>) {
        self.tcp_timeout = timeout;
    }

    pub fn set_udp_timeout(&mut self, timeout: Option<Duration>) {
        self.udp_timeout = timeout;
    }

    fn negotiate(&self) -> io::Result<TcpStream> {
        let mut stream = connect_proxy(self.server.clone())?;
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
        let socket = UdpSocket::bind(bind)?;
        let request = Target {
            host: Host::Ip(bind.ip()),
            port: socket.local_addr()?.port(),
        };
        protocol::write_request(&mut control, protocol::UDP_ASSOCIATE, &request)?;
        let (status, reply) = protocol::read_reply(&mut control)?;
        check_status(status)?;
        let relay_ip = match reply.host {
            Host::Ip(ip) if !ip.is_unspecified() => ip,
            Host::Ip(_) => peer.ip(),
            Host::Domain(name) => {
                let address = (name.as_str(), reply.port)
                    .to_socket_addrs()?
                    .find(|address| address.is_ipv4() == bind.is_ipv4())
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::NotFound, "UDP 中转地址没有匹配的协议族")
                    })?;
                address.ip()
            }
        };
        let relay = SocketAddr::new(relay_ip, reply.port);
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
            for _ in 0..2 {
                let (length, source) = echo.recv_from(&mut payload).unwrap();
                echo.send_to(&payload[..length], source).unwrap();
            }
        });
        let (address, shutdown, server_thread) = start_server();
        let client = Client::new(&address, "admin", "secret").unwrap();
        let association = client.associate(target.clone()).unwrap();
        for _ in 0..2 {
            assert_eq!(association.send(b"hello").unwrap(), 5);
            let mut payload = [0; 100];
            let (length, source) = association.recv(&mut payload).unwrap();
            assert_eq!(&payload[..length], b"hello");
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
}
