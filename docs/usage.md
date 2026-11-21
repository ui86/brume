# 使用说明

## 功能

- SOCKS5 无认证与用户名密码认证（RFC 1929）
- 同端口 HTTP/1.0 和 HTTP/1.1 正向代理，支持普通 HTTP 请求及 HTTPS CONNECT
- IPv4、IPv6 和域名目标地址
- TCP 双向转发及半关闭
- UDP 关联转发；仅接受存活的 TCP 关联对应的 UDP 数据报
- 精确 IP 和 CIDR 白名单，对 TCP 与 UDP 同时生效
- 可选 TCP 双向转发空闲超时、UDP 目标流空闲超时
- 可配置 DNS 服务器与按 TTL 缓存的域名解析
- 收到 Ctrl+C 或终止信号后停止监听

## 构建与运行

需要 Rust 稳定版工具链。在仓库目录执行：

```bash
cargo build --release --locked
./target/release/brume --whitelist 127.0.0.1
```

上述命令使用默认端口 `1080`，仅允许本机客户端连接。服务端在同一端口监听 TCP 和 UDP，使用 Ctrl+C 停止。

同一个端口可分别配置为 HTTP 代理或 SOCKS5 代理。例如：

```bash
curl --proxy http://127.0.0.1:1080 http://example.com/
curl --proxy http://127.0.0.1:1080 https://example.com/
curl --proxy socks5h://127.0.0.1:1080 https://example.com/
```

HTTP 客户端与 SOCKS5 共用白名单和账号密码。配置了认证时，HTTP 客户端使用 Basic 代理认证，例如向 curl 添加 `--proxy-user 'admin:密码'`。Basic 认证要求用户名中不含冒号。代理连接本身使用明文 TCP，HTTPS 目标通过 CONNECT 保持端到端 TLS。普通 HTTP 转发支持有请求体的请求，协议升级需通过 CONNECT 隧道。设置 `tcp_timeout` 后，HTTP 连接也会在双向无流量时关闭。

Linux amd64 和 arm64 可运行交互式安装脚本。脚本从 GitHub Release 下载 `brume-版本-linux-架构.tar.gz`，提供安装、更新、修改配置和卸载选项，并为同一端口的 TCP、UDP 及 IPv4、IPv6 配置可用的防火墙。运行脚本需要 root 或 sudo 权限：

```bash
bash install.sh
```

安装脚本将配置写入 `/etc/brume/brume.conf`，目录权限为 `0700`、文件权限为 `0600`。systemd、OpenRC 和 SysVinit 服务只以 `--config /etc/brume/brume.conf` 启动，不会把密码放入服务命令行。更新旧安装时，脚本会将旧服务参数迁移到配置文件；如果下载的发行版尚不支持 `--config`，脚本会在替换程序前中止。

一键更新需要 `flock`、`setsid` 和 `nohup`。脚本先下载并检查新程序，在旧进程继续运行时替换二进制，然后由独立进程重启并检查服务状态，完成后释放互斥锁。若 SSH 连接经由 Brume 代理，重启时连接会短暂断开，稍后重新连接即可。更新不更改端口或白名单，也不重建防火墙规则；SSH 端口的现有规则不会因更新而被修改。重启结果写入仅 root 可读的 `/etc/brume/upgrade.log.*`。

升级会读取服务实际指定的配置文件，并保留命令行覆盖项，再迁移到标准配置路径。存在 systemd 附加定义、服务文件链接或无法可靠解析的服务参数时，会中止自动迁移并保留原配置；动态展开或复杂转义需要先手动改为配置文件。

修改配置要求当前程序支持 `--check-config`，并安装 `flock`、`setsid` 和 `nohup`。脚本先在旧服务运行期间校验候选配置，再交给脱离 SSH 会话的独立进程应用、重启并检查服务状态；失败时恢复原配置、服务定义和原防火墙规则。修改与升级互斥执行。结果写入仅 root 可读的 `/etc/brume/modify.log.*`；若恢复也失败，会保留私有事务目录中的备份并在日志中提示位置。

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

服务端独立查询 A、AAAA，任一地址族可用后即可开始 TCP 连接或 UDP 建流。TCP 解析与建连共用 10 秒预算；UDP 查询也受 10 秒总预算限制，并保留晚到地址用于失败后的后续数据报回退。

自定义 DNS 配置需要支持 `--dns-servers` 的 Brume 版本。安装脚本在配置文件未显式指定 DNS 时保持对旧版发行包的兼容；旧版程序仍按其自身的解析方式工作。

```bash
./target/release/brume --config ./brume.conf
```

应用前可以单独校验配置，成功退出码为 `0`，失败为非零；该模式不监听端口，也不输出密码：

```bash
./target/release/brume --config ./brume.conf --check-config
```

## 命令行参数

| 参数 | 默认值 | 说明 |
| --- | --- | --- |
| `--config` | 无 | 读取指定配置文件 |
| `--check-config` | 关闭 | 校验完整配置后退出，不启动服务 |
| `-p`、`--port` | `1080` | TCP 和 UDP 监听端口，范围 1～65535 |
| `-user`、`--user` | 空 | 认证用户名 |
| `-pwd`、`--pwd` | 空 | 认证密码，须与用户名同时设置 |
| `--whitelist` | 空 | 允许的 IP 或 CIDR，多个条目用逗号分隔；空值允许所有来源 |
| `--tcp-timeout` | `0` | TCP 双向转发空闲超时，单位秒；任一方向成功转发数据都会续期，0 表示不限制 |
| `--udp-timeout` | `60` | UDP 目标流空闲超时，单位秒；请求或回复都会续期，0 表示不限制 |
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

访问需要认证的服务端时，在 `Client::new` 中同时传入用户名和密码。`connect` 返回可读写的 TCP 连接；代理地址为 IP 时直接连接，域名解析和多个地址的连接尝试共享 10 秒时限。`associate` 返回 UDP 关联对象；其 `send` 和 `recv` 方法负责封装及解析 SOCKS5 数据报，并在对象存活期间保持 TCP 控制连接。客户端还可通过 `set_tcp_timeout` 和 `set_udp_timeout` 设置超时。

客户端代理域名与 UDP 中继域名通过可取消的异步 DNS 网络查询解析，读取系统 DNS 服务器、搜索域和 hosts 文件；复用解析工作槽及其 TTL 缓存，配置快照满 60 秒后在下次查询时刷新。每客户端最多同时使用 16 个解析工作槽、32 个 DNS TCP 套接字，排队也计入调用总时限；查询结束后回收传输。系统 DNS 配置缺失或不可读时仍支持 hosts，其他域名返回原配置错误。代理域名的可用地址族返回后即可建连；UDP 中继域名解析上限为 10 秒，优先选择控制连接的地址族，优先族无响应时短暂等待后使用另一族。查询超时会取消网络查询。

该客户端解析器不执行 NSS、mDNS、LLMNR、Windows NRPT、按接口拆分 DNS 或系统 DoH 策略。依赖这些系统名称解析机制时，应使用显式 IP、hosts 条目或可由系统 DNS 服务器直接解析的名称。此行为仅影响客户端连接代理及解析中继地址；代理请求中的目标域名仍按服务端 `dns_servers` 配置解析。跨地址族绑定保留关联请求已声明的客户端端口，端口被占用时返回错误。

UDP 支持空负载。服务端嵌入异步程序时优先使用 `Server::run_async`，解析域名使用 `DnsResolver::lookup`；单线程 Tokio 运行时调用对应同步接口会返回 `Unsupported`。正常关闭会等待客户端与 UDP 回复任务释放资源，外部取消服务任务也会通知 UDP 任务停止。

## 协议与安全说明

客户端必须先绑定 UDP 套接字，通过 TCP 协商在 UDP ASSOCIATE 请求中声明实际的非零 UDP 端口，并保持控制连接开启。服务端仅接受控制连接对应 IP 和已声明端口的数据报；端口为 `0` 的请求返回状态码 `2`（规则不允许）。这会拒绝只发送端口 `0` 的通用客户端，也不兼容 NAT 改写 UDP 来源端口的场景。客户端库已经声明实际端口；所有流量继续使用原监听端口，无需新增防火墙端口。SOCKS5 的 UDP 分片不受支持。

未设置认证和白名单时，服务器会向所有来源开放。用户名密码认证按照 SOCKS5 标准以明文传输；手动使用 `-pwd` 时，密码会出现在进程参数中，建议通过权限受限的配置文件传入。旧服务命令行中已经暴露过的密码应在迁移后更换。公网部署仍应使用可信网络或加密隧道，并避免复用敏感密码。服务端不记录普通断线产生的 `Broken pipe`、`Connection reset by peer`，也不记录 UDP 客户端地址不匹配、连接目标超时和 DNS 查询无记录错误；其他连接错误仍会记录。

## 验证

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets
cargo bench --features performance-bench --bench benchmarks --locked
```

基准测试使用优化构建，对协议、白名单和本机 TCP/UDP 代理进行测量；每项输出五轮结果的中位数。测试环境、现有测量结果及实现细节见[实现说明与性能基准](performance.md)。

需要单独测量时，可在基准命令后添加 `-- whitelist` 等分组；支持 `all`、`protocol`、`whitelist`、`udp`、`tcp`，可同时指定多个分组。

发布工作流使用 UPX 压缩 Linux amd64 和 arm64 二进制文件，并在打包前验证压缩后的程序能够运行。

## 许可

MIT
