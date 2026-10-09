use brume::client::Client;
use brume::config::{Config, Whitelist};
use brume::protocol::{self, Target};
use brume::server::Server;
use std::hint::black_box;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

const SAMPLES: usize = 5;

fn measure(name: &str, iterations: usize, operations: usize, mut action: impl FnMut()) {
    for _ in 0..iterations.min(10_000) {
        action();
    }
    let mut durations = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let started = Instant::now();
        for _ in 0..iterations {
            action();
        }
        durations.push(
            started.elapsed().as_secs_f64() * 1_000_000_000.0 / (iterations * operations) as f64,
        );
    }
    durations.sort_by(f64::total_cmp);
    println!(
        "{name}: {:.2} ns/op (5 次中位数，{} 次/轮)",
        durations[SAMPLES / 2],
        iterations * operations
    );
}

fn protocol_benchmarks() {
    let destination: SocketAddr = "127.0.0.1:443".parse().unwrap();
    let payload = vec![42; 1024];
    let ipv4_packet = protocol::encode_datagram(destination, &payload);
    let domain_packet = protocol::encode_datagram_to(
        &Target {
            host: protocol::Host::Domain("example.com".into()),
            port: 443,
        },
        &payload,
    )
    .unwrap();
    let whitelist = Whitelist::parse("198.51.100.10,192.0.2.0/24,2001:db8::/32").unwrap();
    let large_entries = (0..256)
        .map(|index| format!("10.{index}.0.0/16"))
        .collect::<Vec<_>>()
        .join(",");
    let large_whitelist = Whitelist::parse(&large_entries).unwrap();
    let matching: IpAddr = "192.0.2.20".parse().unwrap();
    let missing: IpAddr = "203.0.113.20".parse().unwrap();

    measure("UDP IPv4 解析 1024 字节", 500_000, 1, || {
        black_box(protocol::parse_datagram(black_box(&ipv4_packet)).unwrap());
    });
    measure("UDP 域名解析 1024 字节", 500_000, 1, || {
        black_box(protocol::parse_datagram(black_box(&domain_packet)).unwrap());
    });
    measure("UDP IPv4 编码 1024 字节", 500_000, 1, || {
        black_box(protocol::encode_datagram(
            black_box(destination),
            black_box(&payload),
        ));
    });
    measure("白名单命中", 1_000_000, 1, || {
        black_box(whitelist.allows(black_box(matching)));
    });
    measure("白名单未命中", 1_000_000, 1, || {
        black_box(whitelist.allows(black_box(missing)));
    });
    measure("256 个网段白名单未命中", 200_000, 1, || {
        black_box(large_whitelist.allows(black_box(missing)));
    });
}

fn udp_roundtrip_benchmark() {
    let remote = UdpSocket::bind("127.0.0.1:0").unwrap();
    remote
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let destination = remote.local_addr().unwrap();
    let remote_shutdown = Arc::new(AtomicBool::new(false));
    let remote_flag = Arc::clone(&remote_shutdown);
    let echo = thread::spawn(move || {
        let mut packet = [0; 2048];
        while !remote_flag.load(Ordering::Relaxed) {
            match remote.recv_from(&mut packet) {
                Ok((length, source)) => {
                    remote.send_to(&packet[..length], source).unwrap();
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) => {}
                Err(error) => panic!("UDP 回显失败：{error}"),
            }
        }
    });

    let shutdown = CancellationToken::new();
    let config = Config {
        port: 0,
        username: String::new(),
        password: String::new(),
        whitelist: Whitelist::parse("127.0.0.1").unwrap(),
        tcp_timeout: None,
        udp_timeout: Duration::from_secs(60),
        dns_servers: vec!["127.0.0.1:53".parse().unwrap()],
    };
    let server = Server::bind(config, shutdown.clone()).unwrap();
    let server_addr = SocketAddr::new(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        server.local_addr().unwrap().port(),
    );
    let server_thread = thread::spawn(move || server.run().unwrap());
    let mut client = Client::new(server_addr.to_string(), "", "").unwrap();
    client.set_udp_timeout(Some(Duration::from_secs(10)));
    let association = client.associate(Target::from(destination)).unwrap();
    let payload = [42; 1024];
    let mut response = [0; 1024];
    measure("UDP 本机代理往返 1024 字节", 1_000, 1, || {
        association.send(black_box(&payload)).unwrap();
        let (length, _) = association.recv(&mut response).unwrap();
        assert_eq!(length, payload.len());
        black_box(&response);
    });
    measure("UDP 本机代理流水线 1024 字节", 250, 16, || {
        for _ in 0..16 {
            association.send(black_box(&payload)).unwrap();
        }
        for _ in 0..16 {
            let (length, _) = association.recv(&mut response).unwrap();
            assert_eq!(length, payload.len());
            black_box(&response);
        }
    });
    drop(association);
    shutdown.cancel();
    remote_shutdown.store(true, Ordering::Relaxed);
    server_thread.join().unwrap();
    echo.join().unwrap();
}

fn tcp_benchmarks() {
    let remote = TcpListener::bind("127.0.0.1:0").unwrap();
    remote.set_nonblocking(true).unwrap();
    let destination = remote.local_addr().unwrap();
    let echo_shutdown = Arc::new(AtomicBool::new(false));
    let echo_flag = Arc::clone(&echo_shutdown);
    let echo = thread::spawn(move || {
        while !echo_flag.load(Ordering::Relaxed) {
            match remote.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    // 顺序处理基准连接，避免为大量短连接创建系统线程
                    let mut buffer = [0; 32 * 1024];
                    loop {
                        match stream.read(&mut buffer) {
                            Ok(0) => break,
                            Ok(length) => {
                                if stream.write_all(&buffer[..length]).is_err() {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("TCP 回显监听失败：{error}"),
            }
        }
    });

    let shutdown = CancellationToken::new();
    let config = Config {
        port: 0,
        username: String::new(),
        password: String::new(),
        whitelist: Whitelist::parse("127.0.0.1").unwrap(),
        tcp_timeout: None,
        udp_timeout: Duration::from_secs(60),
        dns_servers: vec!["127.0.0.1:53".parse().unwrap()],
    };
    let server = Server::bind(config, shutdown.clone()).unwrap();
    let address = SocketAddr::new(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        server.local_addr().unwrap().port(),
    );
    let server_thread = thread::spawn(move || server.run().unwrap());
    let client = Client::new(address.to_string(), "", "").unwrap();
    let target = Target::from(destination);
    measure("TCP 本机连接与协商", 200, 1, || {
        black_box(client.connect(target.clone()).unwrap());
    });
    let mut stream = client.connect(target).unwrap();
    let small_payload = [42; 1024];
    let mut small_response = [0; 1024];
    measure("TCP 本机往返 1 KiB", 1_000, 1, || {
        stream.write_all(black_box(&small_payload)).unwrap();
        stream.read_exact(&mut small_response).unwrap();
        black_box(&small_response);
    });
    drop(stream);

    let sink = TcpListener::bind("127.0.0.1:0").unwrap();
    let sink_target = Target::from(sink.local_addr().unwrap());
    let sink_thread = thread::spawn(move || {
        let (mut stream, _) = sink.accept().unwrap();
        let mut buffer = [0; 32 * 1024];
        loop {
            for _ in 0..32 {
                if stream.read_exact(&mut buffer).is_err() {
                    return;
                }
            }
            if stream.write_all(&[1]).is_err() {
                return;
            }
        }
    });
    let mut upload = client.connect(sink_target).unwrap();
    let payload = vec![42; 1024 * 1024];
    let mut ack = [0; 1];
    measure("TCP 本机上传 1 MiB", 20, 1, || {
        upload.write_all(black_box(&payload)).unwrap();
        upload.read_exact(&mut ack).unwrap();
    });
    drop(upload);
    sink_thread.join().unwrap();
    shutdown.cancel();
    echo_shutdown.store(true, Ordering::Relaxed);
    server_thread.join().unwrap();
    echo.join().unwrap();
}

fn main() {
    protocol_benchmarks();
    udp_roundtrip_benchmark();
    tcp_benchmarks();
}
