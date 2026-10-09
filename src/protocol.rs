use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

pub const CONNECT: u8 = 1;
pub const UDP_ASSOCIATE: u8 = 3;
pub const SUCCESS: u8 = 0;
pub const SERVER_FAILURE: u8 = 1;
pub const HOST_UNREACHABLE: u8 = 4;
pub const CONNECTION_REFUSED: u8 = 5;
pub const COMMAND_UNSUPPORTED: u8 = 7;
pub const ADDRESS_UNSUPPORTED: u8 = 8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Host {
    Ip(IpAddr),
    Domain(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub host: Host,
    pub port: u16,
}

impl Target {
    pub fn lookup(&self) -> io::Result<Vec<SocketAddr>> {
        use std::net::ToSocketAddrs;
        let addresses: Vec<_> = match &self.host {
            Host::Ip(ip) => vec![SocketAddr::new(*ip, self.port)],
            Host::Domain(name) => (name.as_str(), self.port).to_socket_addrs()?.collect(),
        };
        if addresses.is_empty() {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                "目标地址没有解析结果",
            ))
        } else {
            Ok(addresses)
        }
    }
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn read_target(reader: &mut impl Read, atyp: u8) -> io::Result<Target> {
    let host = match atyp {
        1 => {
            let mut octets = [0; 4];
            reader.read_exact(&mut octets)?;
            Host::Ip(IpAddr::V4(Ipv4Addr::from(octets)))
        }
        3 => {
            let mut length = [0];
            reader.read_exact(&mut length)?;
            if length[0] == 0 {
                return Err(invalid_data("域名不能为空"));
            }
            let mut name = vec![0; usize::from(length[0])];
            reader.read_exact(&mut name)?;
            let name = String::from_utf8(name).map_err(|_| invalid_data("域名编码无效"))?;
            Host::Domain(name)
        }
        4 => {
            let mut octets = [0; 16];
            reader.read_exact(&mut octets)?;
            Host::Ip(IpAddr::V6(Ipv6Addr::from(octets)))
        }
        _ => return Err(invalid_data("不支持的地址类型")),
    };
    let mut port = [0; 2];
    reader.read_exact(&mut port)?;
    Ok(Target {
        host,
        port: u16::from_be_bytes(port),
    })
}

pub fn read_request(reader: &mut impl Read) -> io::Result<(u8, Target)> {
    let mut header = [0; 4];
    reader.read_exact(&mut header)?;
    if header[0] != 5 || header[2] != 0 {
        return Err(invalid_data("SOCKS5 请求头无效"));
    }
    Ok((header[1], read_target(reader, header[3])?))
}

pub fn write_reply(writer: &mut impl Write, status: u8, address: SocketAddr) -> io::Result<()> {
    let mut bytes = vec![5, status, 0];
    encode_address(&mut bytes, address);
    writer.write_all(&bytes)
}

fn encode_address(bytes: &mut Vec<u8>, address: SocketAddr) {
    match address.ip() {
        IpAddr::V4(ip) => {
            bytes.push(1);
            bytes.extend_from_slice(&ip.octets());
        }
        IpAddr::V6(ip) => {
            bytes.push(4);
            bytes.extend_from_slice(&ip.octets());
        }
    }
    bytes.extend_from_slice(&address.port().to_be_bytes());
}

pub fn parse_datagram(bytes: &[u8]) -> io::Result<(Target, &[u8])> {
    if bytes.len() < 4 || bytes[..3] != [0, 0, 0] {
        return Err(invalid_data("UDP 数据报头无效或不支持分片"));
    }
    let mut reader = &bytes[4..];
    let target = read_target(&mut reader, bytes[3])?;
    if reader.is_empty() {
        return Err(invalid_data("UDP 数据报没有负载"));
    }
    Ok((target, reader))
}

pub fn encode_datagram(address: SocketAddr, payload: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(4 + 18 + payload.len());
    bytes.extend_from_slice(&[0, 0, 0]);
    encode_address(&mut bytes, address);
    bytes.extend_from_slice(payload);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_accept_all_address_types() {
        for address in ["127.0.0.1:443", "[2001:db8::1]:443"] {
            let address: SocketAddr = address.parse().unwrap();
            let mut request = vec![5, CONNECT, 0];
            encode_address(&mut request, address);
            let (command, target) = read_request(&mut request.as_slice()).unwrap();
            assert_eq!(command, CONNECT);
            assert_eq!(
                target,
                Target {
                    host: Host::Ip(address.ip()),
                    port: 443
                }
            );
        }
        let request = [
            5, CONNECT, 0, 3, 11, b'e', b'x', b'a', b'm', b'p', b'l', b'e', b'.', b'c', b'o', b'm',
            1, 187,
        ];
        assert_eq!(
            read_request(&mut request.as_slice()).unwrap().1.host,
            Host::Domain("example.com".into())
        );
    }

    #[test]
    fn datagrams_round_trip_and_reject_invalid_headers() {
        for address in ["127.0.0.1:443", "[2001:db8::1]:443"] {
            let address: SocketAddr = address.parse().unwrap();
            let packet = encode_datagram(address, b"hello");
            let (target, payload) = parse_datagram(&packet).unwrap();
            assert_eq!(
                target,
                Target {
                    host: Host::Ip(address.ip()),
                    port: 443
                }
            );
            assert_eq!(payload, b"hello");
        }
        let packet = [
            0, 0, 0, 3, 11, b'e', b'x', b'a', b'm', b'p', b'l', b'e', b'.', b'c', b'o', b'm', 1,
            187, b'x',
        ];
        assert_eq!(
            parse_datagram(&packet).unwrap().0.host,
            Host::Domain("example.com".into())
        );
        for packet in [
            &[0, 0, 1, 1, 127, 0, 0, 1, 0, 80, 1][..],
            &[1, 0, 0, 1, 127, 0, 0, 1, 0, 80, 1],
            &[0, 0, 0, 3, 0, 0, 80, 1],
        ] {
            assert!(parse_datagram(packet).is_err());
        }
    }
}
