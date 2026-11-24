# Docker 部署

镜像仓库为 `ghcr.io/ui86/brume`，Docker 会按宿主机自动选择 `linux/amd64` 或 `linux/arm64`。镜像标签与 GitHub Release 的标签一致，`latest` 指向已发布镜像的最新稳定版。从 `1.0.9` 起，镜像支持 HTTP 和 SOCKS5 共用同一端口。

如果镜像仓库要求认证，先使用具有 `read:packages` 权限的 GitHub 令牌登录 `ghcr.io`。首次发布的包由仓库管理员在 GitHub Packages 设置中管理可见性。[GitHub 容器仓库说明](https://docs.github.com/en/packages/working-with-a-github-packages-registry/working-with-the-container-registry)

## 使用 Docker 运行

创建 `brume.env`，将示例密码替换为自己的密码。用户名和密码需要同时设置。

```dotenv
BRUME_PORT=1080
BRUME_USERNAME=proxy
BRUME_PASSWORD=replace-with-your-password
```

```bash
chmod 600 brume.env
docker run -d --name brume --restart unless-stopped \
  --env-file brume.env \
  -p 127.0.0.1:1080:1080/tcp \
  -p 127.0.0.1:1080:1080/udp \
  ghcr.io/ui86/brume:latest
docker logs brume
```

上例的发布端口仅绑定宿主机回环地址。要允许其他机器连接，在设置认证或来源白名单后，把 `127.0.0.1` 改为需要监听的宿主机地址。需要固定版本时，将 `latest` 替换为具体 Release 标签。

SOCKS5 TCP 和 HTTP 代理使用 TCP 端口。SOCKS5 UDP ASSOCIATE 还需要 UDP 端口，客户端必须能够访问代理协商返回的中继地址和端口。来自容器网络外的客户端若无法访问桥接网络地址，可在 Linux 使用 `--network host` 代替 `-p`，此时监听端口由 `BRUME_PORT` 直接决定。白名单匹配 Brume 收到的源 IP，经过容器网关或地址转换时，以实际到达的源 IP 为准。

## 使用 Docker Compose

将以下内容保存为 `compose.yaml`，与 `brume.env` 放在同一目录。

```yaml
services:
  brume:
    image: ghcr.io/ui86/brume:latest
    restart: unless-stopped
    env_file: brume.env
    ports:
      - "127.0.0.1:1080:1080/tcp"
      - "127.0.0.1:1080:1080/udp"
```

```bash
docker compose up -d
docker compose logs brume
```

修改环境变量后执行 `docker compose up -d --force-recreate`。更新使用 `docker compose pull && docker compose up -d`。

## 环境变量

| 变量 | 默认值 | 用途 |
| --- | --- | --- |
| `BRUME_PORT` | `1080` | 容器内部 TCP 和 UDP 端口。修改时要同步更新端口映射的容器端口。 |
| `BRUME_USERNAME` | 空 | 代理认证用户名。HTTP Basic 认证要求用户名中不含冒号。 |
| `BRUME_PASSWORD` | 空 | 代理认证密码。 |
| `BRUME_WHITELIST` | 空 | 逗号分隔的来源 IP 或 CIDR，留空允许所有来源。 |
| `BRUME_TCP_TIMEOUT` | `0` | TCP 空闲超时秒数，`0` 表示关闭空闲超时。 |
| `BRUME_UDP_TIMEOUT` | `60` | UDP 空闲超时秒数，`0` 表示关闭空闲超时。 |
| `BRUME_DNS_SERVERS` | 由 Release 程序决定 | 逗号分隔的 DNS 服务器，例如 `9.9.9.9,[2606:4700:4700::1111]:53`。 |

容器以非 root 用户启动，将设置写入权限为 `0600` 的容器内配置文件。环境变量不能含换行。Docker 管理员可以读取容器的初始环境变量，含凭据的 `brume.env` 应保存在访问受限的目录。

## 镜像发布

发布新 Release 后，二进制发布任务会上传两个架构的归档，随后 **Publish Docker Image** 工作流校验并打包它们，发布同一版本标签。要补发已有版本，在 Actions 中打开 **Publish Docker Image** 并选择 **Run workflow**，输入已发布的 Release 标签，或留空使用最新稳定版。
