#!/usr/bin/env python3
"""验证 Linux 真实描述符耗尽时的已有连接、恢复和关闭行为。"""

import os
from pathlib import Path
import signal
import socket
import socketserver
import struct
import subprocess
import sys
import tempfile
import threading
import time


class EchoHandler(socketserver.BaseRequestHandler):
    def handle(self):
        while data := self.request.recv(4096):
            self.request.sendall(data)


class EchoServer(socketserver.ThreadingTCPServer):
    daemon_threads = True


def receive_exact(stream, size):
    result = b""
    while len(result) < size:
        part = stream.recv(size - len(result))
        assert part, "代理连接提前关闭"
        result += part
    return result


def open_proxy(port, upstream_port):
    stream = socket.create_connection(("127.0.0.1", port), timeout=2)
    try:
        stream.sendall(b"\x05\x01\x00")
        assert receive_exact(stream, 2) == b"\x05\x00", "协商失败"
        stream.sendall(b"\x05\x01\x00\x01\x7f\x00\x00\x01" + struct.pack("!H", upstream_port))
        header = receive_exact(stream, 4)
        assert header[:3] == b"\x05\x00\x00", "目标连接失败"
        assert header[3] in (1, 4), "代理返回了意外的地址类型"
        receive_exact(stream, (4 if header[3] == 1 else 16) + 2)
        return stream
    except BaseException:
        stream.close()
        raise


def roundtrip(stream):
    stream.sendall(b"descriptor-recovery")
    assert receive_exact(stream, 19) == b"descriptor-recovery", "已有连接未能继续转发"


def wait_for(predicate, description, timeout=4):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.02)
    raise AssertionError(description)


def descriptor_count(process):
    assert process.poll() is None, "描述符耗尽导致服务退出"
    return len(list(Path(f"/proc/{process.pid}/fd").iterdir()))


def cpu_ticks(process):
    # comm 字段可能含有空格，以最后一个右括号定位其后的稳定字段。
    fields = Path(f"/proc/{process.pid}/stat").read_text().rsplit(")", 1)[1].split()
    return int(fields[11]) + int(fields[12])


def main():
    assert sys.platform == "linux", "本测试需要 Linux 的资源限制及 /proc"
    binary = str(Path(sys.argv[1]).resolve(strict=True))
    held = []
    active = None
    process = None
    with EchoServer(("127.0.0.1", 0), EchoHandler) as echo, tempfile.TemporaryDirectory() as directory:
        threading.Thread(target=echo.serve_forever, daemon=True).start()
        with socket.socket() as reservation:
            reservation.bind(("127.0.0.1", 0))
            port = reservation.getsockname()[1]
        log_path = Path(directory) / "server.log"
        try:
            with log_path.open("wb") as log:
                process = subprocess.Popen(
                    ["prlimit", "--nofile=64:64", "--", binary,
                     "--port", str(port), "--whitelist", "127.0.0.1"],
                    stdin=subprocess.DEVNULL,
                    stdout=log,
                    stderr=log,
                    env={**os.environ, "TOKIO_WORKER_THREADS": "2"},
                )
            wait_for(lambda: "Brume" in log_path.read_text(), "服务未能启动")
            active = open_proxy(port, echo.server_address[1])
            roundtrip(active)

            # 客户端描述符属于测试进程；只有服务进程被限制为 64 个。
            for _ in range(96):
                held.append(socket.create_connection(("127.0.0.1", port), timeout=0.5))
            wait_for(lambda: descriptor_count(process) >= 64, "未触发真实文件描述符耗尽")
            wait_for(lambda: "Failed to accept connection" in log_path.read_text(), "未进入 accept 错误恢复分支")
            roundtrip(active)

            before = cpu_ticks(process)
            time.sleep(0.75)
            used = (cpu_ticks(process) - before) / os.sysconf("SC_CLK_TCK")
            assert used < 0.35, f"耗尽期间出现忙等：0.75 秒内消耗 {used:.2f} CPU 秒"

            for stream in held:
                stream.close()
            held.clear()
            wait_for(lambda: descriptor_count(process) < 24, "释放客户端后描述符未恢复")
            with open_proxy(port, echo.server_address[1]) as recovered:
                roundtrip(recovered)
            roundtrip(active)

            # 再次耗尽，在 accept 退避期间仍应响应终止信号。
            for _ in range(96):
                held.append(socket.create_connection(("127.0.0.1", port), timeout=0.5))
            wait_for(lambda: descriptor_count(process) >= 64, "第二次耗尽未发生")
            process.send_signal(signal.SIGTERM)
            assert process.wait(timeout=3) == 0, "服务未正常退出"
            assert active.recv(1) == b"", "关闭服务后仍保留已有连接"
            print("Linux 描述符耗尽、已有连接、退避、恢复及关闭回归通过。")
        except BaseException:
            print(log_path.read_text(), file=sys.stderr)
            raise
        finally:
            for stream in held:
                stream.close()
            if active is not None:
                active.close()
            if process is not None and process.poll() is None:
                process.kill()
                process.wait(timeout=3)
            echo.shutdown()


if __name__ == "__main__":
    main()
