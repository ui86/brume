# Brume

Brume 是用 Rust 编写的 SOCKS5 代理服务器及客户端库，支持 TCP CONNECT、UDP ASSOCIATE、用户名密码认证和 IP/CIDR 白名单。

## 功能

- SOCKS5 无认证与用户名密码认证（RFC 1929）
- IPv4、IPv6 和域名目标地址
- TCP 双向转发及半关闭
- UDP 关联转发；仅接受存活的 TCP 关联对应的 UDP 数据报
- 精确 IP 和 CIDR 白名单，对 TCP 与 UDP 同时生效
- 可选 TCP 读写超时、UDP 目标流空闲超时
- 可配置 DNS 服务器与按 TTL 缓存的域名解析
- 收到 Ctrl+C 或终止信号后停止监听

## 构建与运行

需要 Rust 稳定版工具链。在仓库目录执行：

```bash
cargo build --release --locked
./target/release/brume --whitelist 127.0.0.1
```

上述命令使用默认端口 `1080`，仅允许本机客户端连接。服务端在同一端口监听 TCP 和 UDP，使用 Ctrl+C 停止。

Linux amd64 和 arm64 可运行交互式安装脚本。脚本从 GitHub Release 下载 `brume-版本-linux-架构.tar.gz`，提供安装、更新、修改配置和卸载选项，并配置可用的服务管理器与防火墙。运行脚本需要 root 或 sudo 权限：

```bash
bash install.sh
```

安装脚本将配置写入 `/etc/brume/brume.conf`，目录权限为 `0700`、文件权限为 `0600`。systemd、OpenRC 和 SysVinit 服务只以 `--config /etc/brume/brume.conf` 启动，不会把密码放入服务命令行。更新旧安装时，脚本会将旧服务参数迁移到配置文件；如果下载的发行版尚不支持 `--config`，脚本会在替换程序前中止。

一键更新会先下载并检查新程序，在旧进程继续运行时替换二进制，然后安排服务自动重启。若 SSH 连接经由 Brume 代理，重启时连接会短暂断开，稍后重新连接即可。更新不更改端口或白名单，也不重建防火墙规则；SSH 端口的现有规则不会因更新而被修改。

## 配置文件

使用 `--config` 读取 `.conf` 文件。例如：

```conf
# 每行一个配置项；等号后的内容按字面值读取
port=26547
username=admin
password=请替换为实际密码
whitelist=127.0.0.1,192.168.1.0/24
tcp_timeout=0
udp_timeout=60
dns_servers=8.8.8.8,1.1.1.1
```

空行和以 `#` 开头的行会被忽略；密码中的 `#` 和 `=` 无需转义，行内注释与引号语法不受支持。未填写的配置项使用默认值。手动创建配置文件时应将其权限设为 `0600`，并限制上级目录的访问。命令行参数会覆盖配置文件中的同名配置，与 `--config` 出现的位置无关。

`dns_servers` 用逗号分隔，支持 IPv4、IPv6 地址及自定义端口，例如 `9.9.9.9,[2606:4700:4700::1111]:53,127.0.0.1:5353`。省略端口时使用 53。服务端的 TCP 和 UDP 目标域名都通过这些服务器解析，不回退到系统 DNS；直接使用 IP 目标时不查询 DNS。解析结果按 DNS 记录的 TTL 缓存，最多保存 8192 条响应。修改安装脚本生成的配置文件后，使用对应服务管理器重启 Brume 使配置生效。

自定义 DNS 配置需要支持 `--dns-servers` 的 Brume 版本。安装脚本在配置文件未显式指定 DNS 时保持对旧版发行包的兼容；旧版程序仍按其自身的解析方式工作。

```bash
./target/release/brume --config ./brume.conf
```

## 命令行参数

| 参数 | 默认值 | 说明 |
| --- | --- | --- |
| `--config` | 无 | 读取指定配置文件 |
| `-p`、`--port` | `1080` | TCP 和 UDP 监听端口，范围 1～65535 |
| `-user`、`--user` | 空 | 认证用户名 |
| `-pwd`、`--pwd` | 空 | 认证密码，须与用户名同时设置 |
| `--whitelist` | 空 | 允许的 IP 或 CIDR，多个条目用逗号分隔；空值允许所有来源 |
| `--tcp-timeout` | `0` | TCP 转发读写超时，单位秒；0 表示不限制 |
| `--udp-timeout` | `60` | UDP 目标流空闲超时，单位秒；0 表示不限制 |
| `--dns-servers` | `8.8.8.8,1.1.1.1` | 服务端 DNS 地址，多个条目用逗号分隔；支持 `IP:端口` |
| `-h`、`--help` |  | 显示帮助 |
| `--version` |  | 显示版本 |

## 客户端库

`brume::client::Client` 提供 `connect` 和 `associate` 方法。以下示例连接“构建与运行”中的本机无认证服务：

```rust
use brume::client::Client;
use brume::protocol::{Host, Target};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::new("127.0.0.1:1080", "", "")?;
    let target = Target { host: Host::Domain("example.com".into()), port: 443 };
    let _stream = client.connect(target)?;
    Ok(())
}
```

访问需要认证的服务端时，在 `Client::new` 中同时传入用户名和密码。`connect` 返回可读写的 TCP 连接。`associate` 返回 UDP 关联对象；其 `send` 和 `recv` 方法负责封装及解析 SOCKS5 数据报，并在对象存活期间保持 TCP 控制连接。客户端还可通过 `set_tcp_timeout` 和 `set_udp_timeout` 设置超时。

## 协议与安全说明

客户端必须先通过 TCP 协商并发送 UDP ASSOCIATE 请求，保持控制连接开启。关联建立后，UDP 中转只接收对应客户端 IP 和端口的数据报；客户端在请求中填入端口 0 时，服务端会在首个有效数据报到达时确定端口。SOCKS5 的 UDP 分片不受支持。

未设置认证和白名单时，服务器会向所有来源开放。用户名密码认证按照 SOCKS5 标准以明文传输；手动使用 `-pwd` 时，密码会出现在进程参数中，建议通过权限受限的配置文件传入。旧服务命令行中已经暴露过的密码应在迁移后更换。公网部署仍应使用可信网络或加密隧道，并避免复用敏感密码。普通的客户端断开可能产生 `Broken pipe` 或 `Connection reset by peer`，服务端不会将这两类错误写入日志；其他连接错误仍会记录。

## 验证

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets
cargo bench --features performance-bench --bench benchmarks --locked
```

基准测试使用优化构建，对协议、白名单和本机 TCP/UDP 代理进行测量；每项输出五轮结果的中位数。测试环境、现有测量结果及实现细节见[实现说明与性能基准](docs/performance.md)。

发布工作流使用 UPX 压缩 Linux amd64 和 arm64 二进制文件，并在打包前验证压缩后的程序能够运行。

## 许可

MIT
