# Brume

Brume 是用 Rust 实现的轻量级 SOCKS5 代理服务器，支持 TCP CONNECT、UDP ASSOCIATE、用户名密码认证和 IP/CIDR 白名单。

项目同时提供 Rust 客户端库，可在其他 Rust 程序中建立 SOCKS5 TCP 连接或 UDP 关联。

## 功能

- SOCKS5 无认证与用户名密码认证（RFC 1929）
- IPv4、IPv6 和域名目标地址
- TCP 双向转发及半关闭
- UDP 关联转发；仅接受存活的 TCP 关联对应的 UDP 数据报
- 精确 IP 和 CIDR 白名单，对 TCP 与 UDP 同时生效
- 可选 TCP 空闲超时、UDP 流空闲超时
- 收到 Ctrl+C 或终止信号后停止监听

## 构建与安装

需要 Rust 稳定版工具链。克隆仓库后执行：

```bash
cargo build --release --locked
./target/release/brume
```

Linux amd64 和 arm64 可使用仓库中的 `install.sh` 安装、更新、卸载或修改配置。脚本从 GitHub Release 下载与原有命名规则兼容的 `brume-版本-linux-架构.tar.gz`。运行交互式脚本：

```bash
bash install.sh
```

## 命令行参数

| 参数 | 默认值 | 说明 |
| --- | --- | --- |
| `-p`、`--port` | `1080` | TCP 和 UDP 监听端口，范围 1～65535 |
| `-user`、`--user` | 空 | 认证用户名 |
| `-pwd`、`--pwd` | 空 | 认证密码，须与用户名同时设置 |
| `--whitelist` | 空 | 允许的 IP 或 CIDR，多个条目用逗号分隔；空值允许所有来源 |
| `--tcp-timeout` | `0` | TCP 转发读写空闲超时，单位秒；0 表示不限制 |
| `--udp-timeout` | `60` | UDP 目标流空闲超时，单位秒；0 表示不限制 |
| `-h`、`--help` |  | 显示帮助 |
| `--version` |  | 显示版本 |

例如：

```bash
./target/release/brume -p 8080 -user admin -pwd password123 --whitelist 127.0.0.1,192.168.1.0/24
```

## 协议与安全说明

客户端必须先通过 TCP 协商并发送 UDP ASSOCIATE 请求，保持控制连接开启。关联建立后，UDP 中转只接收对应客户端 IP 和端口的数据报；客户端在请求中填入端口 0 时，服务端会在首个有效数据报到达时确定端口。SOCKS5 的 UDP 分片不受支持。

未设置认证和白名单时，服务器会向所有来源开放。用户名密码认证按照 SOCKS5 标准以明文传输，公网使用时应在可信网络或加密隧道中部署。域名使用系统 DNS 解析器。

## Rust 客户端库

`brume::client::Client` 提供 `connect` 和 `associate` 方法。目标地址可使用 IP 或域名，例如：

```rust
use brume::client::Client;
use brume::protocol::{Host, Target};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::new("127.0.0.1:1080", "admin", "password123")?;
    let target = Target { host: Host::Domain("example.com".into()), port: 443 };
    let _stream = client.connect(target)?;
    Ok(())
}
```

`associate` 返回的关联对象会保持 TCP 控制连接存活；其 `send` 和 `recv` 方法负责封装及解析 SOCKS5 UDP 数据报。

## 验证

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets
cargo bench --features performance-bench --bench benchmarks --locked
```

基准测试使用 Release 优化配置，对协议、白名单和本机 TCP/UDP 代理进行测量；每项输出五轮结果的中位数。本机结果和适用范围见[重构分析与性能基准](docs/benchmark-comparison.md)。

## 许可

MIT
