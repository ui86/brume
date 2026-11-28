# 性能优化记录

本轮基于 `5972eed`，按优先级逐项实施、测量并单独提交。仅保留有可重复收益的方案；没有收益或出现明显退化的方案回退。

## 测量条件

2026-11-28，Ubuntu 24.04.5、ARM64、2 个逻辑处理器、Rust 1.100.1。二进制均使用同一 Cargo.lock 和默认 release 配置。本机回环测试交替执行前后版本，各运行 5 轮，表中取各轮指标的中位数。CPU 时间仅统计代理进程，Linux 计时粒度为 10 ms；RSS 包含分配器和任务开销，不包含内核套接字内存。测量期间不并行构建或执行其他基准。这些数据不能直接推算公网表现。

```bash
cargo build --release --locked
python3 tests/performance.py /绝对路径/优化前 /绝对路径/优化后
python3 tests/performance.py /绝对路径/优化前 /绝对路径/优化后 --concurrency 16 --requests 500
```

## 1. 普通 HTTP 上游连接池：保留

每个服务共享按目标区分的 HTTP/1.1 空闲连接池，总空闲连接上限 64，每个目标最多 2 条。空闲超过 30 秒不再借出，后台每 15 秒清理；响应体完整结束后才归还连接。未完整读取、协议失败、连接淘汰和服务停止都会取消驱动任务。仅重试 Hyper 确认尚未序列化的请求。

| 负载 | 指标 | 优化前 | 优化后 |
| --- | --- | ---: | ---: |
| 单连接，1000 次请求，1 KiB 响应 | 请求数/秒 | 3191 | 7391 |
| 同上 | 平均耗时 | 312.63 μs | 134.57 μs |
| 同上 | p99 | 787.12 μs | 404.76 μs |
| 同上 | 代理 CPU 时间 | 0.11 s | 0.05 s |
| 同上 | 测量期间新建上游连接 | 1000 | 0（复用预热连接） |

验证覆盖连接复用、源站主动关闭后重连、未完成响应体取消、空闲连接数量和到期、认证、请求体与逐跳头处理。目标和响应大小相同，单连接吞吐提高约 132%，确认保留。HTTPS CONNECT 继续使用独立隧道。

16 条并发连接、每条 500 次请求时，吞吐从 6849 提高到 9679 次/秒（约 41%），p99 从 5.35 ms 降至 4.18 ms，CPU 从 0.58 s 降至 0.43 s。新建上游连接由 8000 降至 2808；每目标空闲上限为 2，因此突发并发下仍会新建连接。原始数据见 [单连接](benchmarks/http-comparison.json)和[并发](benchmarks/http-concurrent.json)。

## 2. UDP 已有流直接发送：保留

已有流已经初始化、关联仍存活且套接字可立即发送时，直接借用接收缓冲执行 `try_send`，避免负载复制、许可分配和逐包任务调度。遇到未建流、写入未就绪或发送错误时进入原异步路径；发送错误继续执行原有地址回退与代次校验。

```bash
python3 tests/performance.py /绝对路径/优化前 /绝对路径/优化后 --mode udp --batch 16 --requests 1000
python3 tests/performance.py /绝对路径/优化前 /绝对路径/优化后 --mode udp --batch 4 --requests 1000 --concurrency 8
```

| 负载（1 KiB 数据报） | 指标 | 优化前 | 优化后 |
| --- | --- | ---: | ---: |
| 单关联，16 包/批 | 包数/秒 | 21557 | 24138 |
| 同上 | 代理 CPU 时间 | 0.32 s | 0.22 s |
| 8 个关联，4 包/批 | 包数/秒 | 21577 | 21981 |
| 同上 | 代理 CPU 时间 | 0.67 s | 0.47 s |

单关联吞吐提高约 12%，两个负载下 CPU 均减少约 30%。该基准同时包含 Python 回显端和负载生成器的开销，多关联吞吐只小幅变化，本项主要依据 CPU 收益保留。单个回显套接字下 8 个关联各突发 16 包的探索测试出现接收超时，未产生有效对照，因此不作为性能结果；后续采用上述可完整收发的负载。脚本逐包核对内容，超时会使整个测试失败。

原有 25 项 UDP 相关测试及新增直接发送的空数据报、停止流和关闭关联回归通过。原始数据见[单关联](benchmarks/udp-fast-comparison.json)和[多关联](benchmarks/udp-fast-concurrent.json)。

## 3. UDP 回复缓冲池：保留

套接字可读后借用共享缓冲，回复发送完成或任务取消时归还。池中最多 32 个约 64 KiB 缓冲，总容量约 2 MiB；额度不足时等待，关联关闭和流到期均可取消等待。每个流不再长期保留单独的大缓冲。

```bash
python3 tests/performance.py /绝对路径/优化前 /绝对路径/优化后 --mode udp-memory --flows 256
python3 tests/performance.py /绝对路径/优化前 /绝对路径/优化后 --mode udp --batch 16 --requests 1000
```

256 个关联各向同一目标回显一次 1 字节后保持连接，空闲 1 秒再读取代理 RSS。五轮中位数：优化前 21884 KiB，优化后 5608 KiB，下降约 74%。相对仅建立控制连接时的 RSS 增量从 17088 KiB 降至 684 KiB。该结果包括任务与分配器变化，不等于精确的缓冲负载差值。

16 包流水线对照吞吐为 22105 → 24103 包/秒，代理 CPU 同为 0.22 秒；没有观察到吞吐退化，本项主要按可重复的内存收益保留。27 项 UDP 相关回归通过，覆盖大报文、额度耗尽、缓冲复用和取消归还。原始数据见[内存](benchmarks/udp-pool-memory.json)和[转发](benchmarks/udp-pool-speed.json)。

## 4. TCP 扩容缓冲空闲回收：保留

每方向缓冲增长后，如果连续 1 秒未读到新数据，将其替换为初始的 8 KiB 缓冲；后续大流量仍可按需增长。只在缓冲已扩容时启用该等待，不改变连接的空闲超时配置和半关闭行为。

```bash
python3 tests/performance.py /绝对路径/优化前 /绝对路径/优化后 --mode tcp-memory --flows 256
```

建立 256 条 SOCKS5 隧道，每条回显 1 MiB，最多 16 条同时传输；保持连接，空闲 2 秒后读取 RSS，并逐条验证恢复传输。五轮中位数：传输后 RSS 从 41820 KiB 降至 25084 KiB，减少约 40%；256 MiB 回显阶段耗时为 0.515 → 0.479 秒。CPU 时间为 0.22 → 0.23 秒，差值等于单个计时刻度，不据此认定 CPU 改善。分配器可能保留已释放的页面，因此 RSS 不一定恢复至刚建连水平。

15 项 TCP 相关回归通过，基准验证全部连接在回收后继续正确收发。按内存收益保留，原始数据见[TCP 缓冲对照](benchmarks/tcp-shrink-comparison.json)。

## 5. 客户端 DNS 配置未变时保留缓存：保留

分离配置检查时间和解析器代次，逐项比较 DNS 服务器、解析选项、hosts 来源内容和配置错误状态。每 60 秒按需检查，内容相同则继续使用已有工作槽缓存。同步系统读取转入阻塞任务，每客户端只允许一个刷新任务；调用取消后许可仍由读取任务持有，直到实际结束。

```bash
cargo test --locked --release --features performance-bench --lib benchmark_dns_configuration_refresh -- --ignored --nocapture
```

基准在同一个优化构建中交替运行旧的无条件换代行为与新的内容比较路径，使用本机 DNS 上游、60 秒 TTL，预热后连续执行 1000 次相同配置刷新和 A 记录查询。为避免每轮等待 60 秒，直接调用实际的快照替换逻辑；不包含系统文件读取，也不代表正常缓存命中的平均网络延迟。

五轮每次刷新及查询的耗时中位数：55.14 μs → 2.33 μs。测量阶段每轮 DNS 上游查询由 1000 次降至 0 次，全部查询结果一致。15 项客户端 DNS 测试通过，覆盖缓存保留、配置变化换代、错误、hosts、超时取消和连接配额。原始数据见[DNS 刷新基准](benchmarks/dns-refresh.json)。

## 6. HTTP 分散写透传：回退

试验为 `ClientIo` 增加 `is_write_vectored` 和 `poll_write_vectored`，并验证多缓冲内容和活动时间更新。五项 HTTP 回归通过，但基准没有确认稳定收益，因此已回退实现及对应实现测试，仅保留实验记录。

```bash
python3 tests/performance.py /绝对路径/优化前 /绝对路径/试验版 --size 1048576 --requests 300
python3 tests/performance.py /绝对路径/优化前 /绝对路径/试验版 --concurrency 16 --requests 500
```

五轮中位数：1 MiB 响应吞吐 707.66 → 716.39 次/秒（约 +1.2%），CPU 0.21 → 0.20 秒，差值仅一个计时刻度，且逐轮吞吐波动大于该变化。16 路 1 KiB 响应吞吐 9624 → 9529 次/秒（约 -1.0%），p99 为 4.10 → 4.24 ms。现有数据不足以证明净收益，不保留额外代码。原始数据见[大响应](benchmarks/vectored-bulk.json)和[小响应](benchmarks/vectored-small.json)。

## 7. TCP 空闲超时通知：按 CPU 收益保留

将每块转发数据的 watch 通知改为记录活动时间，复用单个定时器，在原期限到达时复查最近活动并续期。保持双向共享空闲期限，以及写入阻塞、单向传输和半关闭语义。只有启用 TCP 空闲超时的连接使用该路径。

```bash
python3 tests/performance.py /绝对路径/优化前 /绝对路径/优化后 --mode tcp --size 1048576 --requests 100 --concurrency 8 --timeout 60
python3 tests/performance.py /绝对路径/优化前 /绝对路径/优化后 --mode tcp --requests 3000 --timeout 60
```

补充截止时间溢出保护后重新构建并执行最终五轮对照：

| 负载 | 指标 | 优化前 | 优化后 |
| --- | --- | ---: | ---: |
| 8 条连接，1 MiB/次，共回显 800 MiB | CPU 时间 | 0.61 s | 0.56 s |
| 同上 | 负载吞吐 | 538.33 MiB/s | 527.24 MiB/s |
| 同上 | p99 | 25.15 ms | 25.37 ms |
| 单连接，1 KiB/次，共 3000 次 | CPU 时间 | 0.10 s | 0.06 s |
| 同上 | 平均往返耗时 | 107.04 μs | 105.59 μs |
| 同上 | p99 | 247.32 μs | 266.08 μs |

初次五轮对照的 CPU 时间同样降低：大流量 0.62 → 0.56 秒，小报文 0.10 → 0.06 秒。CPU 收益可重复，因此保留；吞吐变化约在 ±2% 内，没有稳定的吞吐提升，最终小报文 p99 略升，不能宣称延迟全面改善。该取舍适用于开启空闲超时且希望降低 CPU 开销的负载。

TCP 相关回归及新增写入阻塞超时测试通过。原始数据：[初次大流量](benchmarks/timer-bulk.json)、[初次小报文](benchmarks/timer-small.json)、[最终大流量](benchmarks/timer-bulk-final.json)、[最终小报文](benchmarks/timer-small-final.json)。

## 最终验证及版本对应

本轮保留 6 项性能改动，HTTP 分散写因没有稳定收益回退。各项分别提交，提交作者和提交者均为 Guccen，新增代码注释使用中文。

| 数据中的程序名称 | 对应版本 |
| --- | --- |
| baseline | `5972eed` |
| http-pool | `a765d36` |
| udp-fast | `f6c8381` |
| udp-pool | `8f8ca27` |
| tcp-shrink | `94bef24` |
| dns-cache | `c3fd730` |
| http-vectored | 已回退的分散写试验，记录提交为 `4344348` |
| tcp-timer | `2019c68` |

网络对照每项使用前一项已保留版本作为基线，收益不可相加；DNS 基准直接在同一构建中对照旧换代逻辑与新逻辑。原始 JSON 中的程序名按上表对应。

最终检查全部通过：

- `cargo fmt --all -- --check`
- `cargo clippy --locked --all-targets --all-features -- -D warnings`
- `cargo test --locked --all-targets`：91 项单元测试、1 项命令行集成测试。
- `python3 tests/linux_resources.py /绝对路径/最终优化版`：真实描述符耗尽、退避、已有连接、恢复及关闭。
- `cargo check --locked --all-targets --all-features --target x86_64-pc-windows-gnu`：Windows 交叉编译检查；没有在 Windows 上运行性能基准。
- 基准脚本语法、所有原始 JSON 数据及 Git 差异空白校验。
