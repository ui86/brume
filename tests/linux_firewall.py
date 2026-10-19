#!/usr/bin/env python3
"""在测试网络命名空间内提供回显及四类防火墙流量探测。"""

from pathlib import Path
import socket
import socketserver
import sys
import threading


class TcpHandler(socketserver.BaseRequestHandler):
    def handle(self):
        data = self.request.recv(64)
        if data:
            self.request.sendall(data)


class UdpHandler(socketserver.BaseRequestHandler):
    def handle(self):
        data, stream = self.request
        stream.sendto(data, self.client_address)


class TcpServer(socketserver.ThreadingTCPServer):
    address_family = socket.AF_INET6
    allow_reuse_address = True
    daemon_threads = True

    def server_bind(self):
        self.socket.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 0)
        super().server_bind()


class UdpServer(socketserver.ThreadingUDPServer):
    address_family = socket.AF_INET6
    daemon_threads = True

    def server_bind(self):
        self.socket.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 1)
        super().server_bind()


class Udp4Server(socketserver.ThreadingUDPServer):
    daemon_threads = True


def serve(ready):
    # 明确绑定回复源地址，避免多地址回环接口把 IPv6 UDP 回复源选成探测端。
    with (
        TcpServer(("::", 26547), TcpHandler) as tcp,
        UdpServer(("::1", 26547), UdpHandler) as udp6,
        Udp4Server(("127.0.0.1", 26547), UdpHandler) as udp4,
    ):
        threading.Thread(target=tcp.serve_forever, daemon=True).start()
        threading.Thread(target=udp6.serve_forever, daemon=True).start()
        threading.Thread(target=udp4.serve_forever, daemon=True).start()
        Path(ready).touch()
        threading.Event().wait()


def probe(mode):
    source_suffix = "2" if mode == "allowed" else "3"
    for family, target, source in (
        (socket.AF_INET, "127.0.0.1", f"127.0.0.{source_suffix}"),
        (socket.AF_INET6, "::1", f"::{source_suffix}"),
    ):
        for protocol in (socket.SOCK_STREAM, socket.SOCK_DGRAM):
            success = False
            with socket.socket(family, protocol) as stream:
                stream.settimeout(0.5)
                stream.bind((source, 0))
                try:
                    stream.connect((target, 26547))
                    stream.sendall(b"firewall")
                    success = stream.recv(64) == b"firewall"
                except (TimeoutError, ConnectionRefusedError):
                    pass
            assert success == (mode != "denied"), (
                f"{mode} 防火墙探测不符：来源 {source}，协议 {protocol}，成功 {success}"
            )


if __name__ == "__main__":
    if sys.argv[1] == "serve":
        serve(sys.argv[2])
    else:
        probe(sys.argv[1])
