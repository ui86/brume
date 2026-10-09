# 实现说明与性能基准

## 代码结构

| 模块 | 职责 |
| --- | --- |
| `src/config.rs` | 解析命令行参数，校验认证配置及 IP/CIDR 白名单 |
| `src/dns.rs` | 使用配置的 DNS 服务器解析目标域名并缓存结果 |
| `src/protocol.rs` | 解析和编码 SOCKS5 请求、回复及 UDP 数据报 |
| `src/server.rs` | 监听 TCP/UDP、处理认证、转发流量并维护 UDP 关联 |
| `src/client.rs` | 提供 TCP CONNECT 和 UDP ASSOCIATE 客户端接口 |
| `src/main.rs`、`src/lib.rs` | 启动服务及导出公共库模块 |

服务端在同一端口监听 IPv4 和 IPv6 的 TCP、UDP 流量。每个 TCP 客户端由独立线程处理，建立 CONNECT 后使用两个方向转发数据，并支持半关闭。UDP 接收线程将数据报放入有界队列，由 4～16 个工作线程处理；每个目标流使用一个回复线程。接收缓冲区在池中复用。TCP 和 UDP 域名目标通过配置的 DNS 服务器解析，解析器按记录 TTL 缓存最多 8192 条响应；IP 目标直接连接。

UDP 数据报必须属于存活的 TCP 关联，来源 IP 与控制连接一致；若请求端口为 `0`，首个有效数据报确定来源端口。白名单同时检查 TCP 和 UDP 来源地址，未配置时允许所有来源。白名单地址和网段采用顺序查找。

## 验证

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets
```

测试覆盖 IPv4、IPv6 和域名报文、UDP 数据报校验、白名单、命令行参数、认证、TCP 半关闭、UDP 关联、自定义 DNS 与缓存，以及客户端库的端到端连接。GitHub Actions 的 CI 执行上述检查；发布工作流构建并用 UPX 压缩 Linux amd64 和 arm64 的静态链接程序。

## 运行基准

```bash
cargo bench --features performance-bench --bench benchmarks --locked
```

以下数据于 2026-10-09 在 Windows、Intel Core i5-12600KF、4 个可用逻辑处理器、Rust 1.98.1 环境测得。基准程序每项先预热，再运行 5 轮，表格列出每次操作耗时的中位数。TCP 和 UDP 代理场景使用本机接收端或回显端，结果包含本机网络栈、线程调度和客户端处理开销。

| 场景 | 本机测量值 |
| --- | ---: |
| UDP IPv4 数据报解析，1024 字节 | 6.66 ns/包 |
| UDP 域名数据报解析，1024 字节 | 44.44 ns/包 |
| UDP IPv4 数据报编码，1024 字节 | 45.43 ns/包 |
| 小白名单命中／未命中 | 6.68／6.60 ns/次 |
| 256 个网段白名单未命中 | 225.13 ns/次 |
| UDP 顺序代理往返，1024 字节 | 130.94 μs/包 |
| UDP 16 包流水线收发，1024 字节 | 16.34 μs/包 |
| TCP 连接目标并完成 SOCKS5 协商 | 618.08 μs/连接 |
| TCP 代理往返，1 KiB | 84.18 μs/次 |
| TCP 代理上传并等待确认，1 MiB | 1.33 ms/次 |

这些数字是本机环境的参考值，不能代表公网延迟或 Linux 部署性能。当前实现为 TCP 连接和 UDP 目标流创建线程，白名单与 UDP 关联没有索引；并发连接数、目标数和数据包速率变化时，耗时可能不同。评估实际部署性能时，应在目标机器上按预期负载重新运行基准。
