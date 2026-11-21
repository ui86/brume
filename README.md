# Brume

Brume 是用 Rust 编写的轻量级代理服务器。SOCKS5 和 HTTP 代理共用同一个 TCP 端口，支持普通 HTTP 转发、HTTPS CONNECT 隧道和 SOCKS5 UDP ASSOCIATE。服务端支持用户名密码认证、IP/CIDR 白名单、可配置 DNS 和连接超时，并提供 SOCKS5 客户端库。详见[使用说明](docs/usage.md)。

## Linux 一键安装

支持 amd64 和 arm64。下载并运行 [GitHub 安装脚本](https://raw.githubusercontent.com/ui86/brume/main/install.sh)：

```bash
curl -fsSLO https://raw.githubusercontent.com/ui86/brume/main/install.sh && bash install.sh
```
