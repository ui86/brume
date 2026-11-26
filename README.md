# Brume

Brume 是用 Rust 编写的轻量级代理服务器。SOCKS5 和 HTTP 代理共用同一个 TCP 端口，支持普通 HTTP 转发、HTTPS CONNECT 隧道和 SOCKS5 UDP ASSOCIATE。服务端支持用户名密码认证、IP/CIDR 白名单、可配置 DNS 和连接超时，并提供 SOCKS5 客户端库。详见[使用说明](docs/usage.md)。

容器镜像发布到 GitHub Packages，支持 Linux amd64 和 arm64。详见 [Docker 部署](docs/docker.md)。

## Linux 一键安装

支持 amd64 和 arm64。下载并运行 [GitHub 安装脚本](https://raw.githubusercontent.com/ui86/brume/main/install.sh)：

```bash
curl -fsSLO https://raw.githubusercontent.com/ui86/brume/main/install.sh && bash install.sh
```

已有二进制时，可将 `brume` 和 [`install-local.sh`](install-local.sh) 放在同一目录，运行 `bash install-local.sh`。本地脚本保留安装、修改配置、更新和卸载菜单，无需联网下载；程序保存在脚本所在目录，配置位于该目录的 `.brume/brume.conf`。支持 Debian 的 systemd、Alpine 的 OpenRC 及 SysVinit；没有运行服务管理器时显示前台启动命令。Alpine 需预先安装 Bash。详见[本地安装说明](docs/usage.md#本地安装)。
