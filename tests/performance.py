#!/usr/bin/env python3
"""在本机回环网络上交替测量两个构建，输出每轮数据及中位数。"""

import argparse
import asyncio
import contextlib
import json
import os
import socket
import statistics
import struct
import time


def process_cpu(pid):
    with open(f"/proc/{pid}/stat", encoding="utf-8") as source:
        fields = source.read().split()
    return (int(fields[13]) + int(fields[14])) / os.sysconf("SC_CLK_TCK")


@contextlib.asynccontextmanager
async def proxy(binary, timeout):
    with socket.socket() as reservation:
        reservation.bind(("127.0.0.1", 0))
        port = reservation.getsockname()[1]
    process = await asyncio.create_subprocess_exec(
        binary, "--port", str(port), "--whitelist", "127.0.0.1",
        "--dns-servers", "127.0.0.1:53", "--tcp-timeout", str(timeout),
        stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.PIPE,
    )
    try:
        for _ in range(100):
            if process.returncode is not None:
                raise RuntimeError((await process.stderr.read()).decode())
            try:
                _, writer = await asyncio.open_connection("127.0.0.1", port)
                writer.close()
                await writer.wait_closed()
                break
            except ConnectionRefusedError:
                await asyncio.sleep(0.01)
        else:
            raise TimeoutError("代理服务启动超时")
        yield port, process
    finally:
        if process.returncode is None:
            process.terminate()
        await asyncio.wait_for(process.communicate(), 5)


async def http_sample(binary, args):
    accepted = 0
    connections = set()
    payload = b"X" * args.size
    response = f"HTTP/1.1 200 OK\r\nContent-Length: {len(payload)}\r\n\r\n".encode()

    async def origin(reader, writer):
        nonlocal accepted
        accepted += 1
        connections.add(writer)
        try:
            while True:
                head = await reader.readuntil(b"\r\n\r\n")
                writer.write(response)
                writer.write(payload)
                await writer.drain()
                if b"connection: close" in head.lower():
                    break
        except (asyncio.IncompleteReadError, ConnectionError):
            pass
        finally:
            connections.discard(writer)
            writer.close()

    async with await asyncio.start_server(origin, "127.0.0.1", 0) as upstream:
        destination = upstream.sockets[0].getsockname()[1]
        async with proxy(binary, args.timeout) as (port, process):
            request = f"GET http://127.0.0.1:{destination}/ HTTP/1.1\r\nHost: local\r\n\r\n".encode()
            async def lane(count):
                reader, writer = await asyncio.open_connection("127.0.0.1", port)
                samples = []
                try:
                    for _ in range(count):
                        start = time.perf_counter()
                        writer.write(request)
                        await writer.drain()
                        head = await reader.readuntil(b"\r\n\r\n")
                        assert head.startswith(b"HTTP/1.1 200"), head
                        body = await reader.readexactly(len(payload))
                        assert body == payload
                        samples.append((time.perf_counter() - start) * 1e6)
                finally:
                    writer.close()
                    await writer.wait_closed()
                return samples
            await lane(20)
            before_connections = accepted
            before_cpu = process_cpu(process.pid)
            started = time.perf_counter()
            lanes = await asyncio.gather(*(lane(args.requests) for _ in range(args.concurrency)))
            elapsed = time.perf_counter() - started
            cpu = process_cpu(process.pid) - before_cpu
            samples = sorted(sample for lane_samples in lanes for sample in lane_samples)
            result = {
                "requests_per_second": len(samples) / elapsed,
                "mean_us": statistics.mean(samples),
                "p99_us": samples[min(len(samples) - 1, int(len(samples) * 0.99))],
                "proxy_cpu_seconds": cpu,
                "upstream_connections": accepted - before_connections,
            }
    for writer in connections:
        writer.close()
    return result


async def socks(port, command, target_port):
    reader, writer = await asyncio.open_connection('127.0.0.1', port)
    writer.write(b'\x05\x01\x00')
    await writer.drain()
    assert await reader.readexactly(2) == b'\x05\x00'
    writer.write(bytes([5, command, 0, 1, 127, 0, 0, 1]) + struct.pack('!H', target_port))
    await writer.drain()
    reply = await reader.readexactly(4)
    assert reply[:3] == b'\x05\x00\x00', reply
    assert reply[3] in (1, 4), reply
    await reader.readexactly((4 if reply[3] == 1 else 16) + 2)
    return reader, writer

async def udp_sample(binary, args):
    class Echo(asyncio.DatagramProtocol):
        def connection_made(self, transport):
            self.transport = transport
        def datagram_received(self, data, addr):
            self.transport.sendto(data, addr)
    loop = asyncio.get_running_loop()
    transport, _ = await loop.create_datagram_endpoint(Echo, local_addr=('127.0.0.1', 0))
    target_port = transport.get_extra_info('sockname')[1]
    try:
        async with proxy(binary, args.timeout) as (port, process):
            sockets = []
            controls = []
            try:
                for _ in range(args.concurrency):
                    udp = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
                    udp.setblocking(False)
                    udp.bind(('127.0.0.1', 0))
                    udp.connect(('127.0.0.1', port))
                    sockets.append(udp)
                    controls.append(await socks(port, 3, udp.getsockname()[1]))
                header = bytes([0, 0, 0, 1, 127, 0, 0, 1]) + struct.pack('!H', target_port)
                packet = header + b'X' * args.size
                async def lane(udp, count):
                    samples = []
                    for _ in range(count):
                        started = time.perf_counter()
                        for _ in range(args.batch):
                            await loop.sock_sendall(udp, packet)
                        for _ in range(args.batch):
                            reply = await asyncio.wait_for(loop.sock_recv(udp, 65535), 2)
                            assert reply == packet
                        samples.append((time.perf_counter() - started) * 1e6 / args.batch)
                    return samples
                await asyncio.gather(*(lane(udp, 10) for udp in sockets))
                before_cpu = process_cpu(process.pid)
                start = time.perf_counter()
                lanes = await asyncio.gather(*(lane(udp, args.requests) for udp in sockets))
                elapsed = time.perf_counter() - start
                samples = sorted(sample for values in lanes for sample in values)
                return {
                    'packets_per_second': len(samples) * args.batch / elapsed,
                    'mean_us_per_packet': statistics.mean(samples),
                    'proxy_cpu_seconds': process_cpu(process.pid) - before_cpu,
                }
            finally:
                for udp in sockets:
                    udp.close()
                for _, writer in controls:
                    writer.close()
                await asyncio.gather(*(writer.wait_closed() for _, writer in controls))
    finally:
        transport.close()


def rss_kib(pid):
    with open(f"/proc/{pid}/status", encoding="utf-8") as source:
        return int(next(line for line in source if line.startswith("VmRSS:")).split()[1])


async def udp_memory_sample(binary, args):
    class Echo(asyncio.DatagramProtocol):
        def connection_made(self, transport):
            self.transport = transport
        def datagram_received(self, data, addr):
            self.transport.sendto(data, addr)
    loop = asyncio.get_running_loop()
    transport, _ = await loop.create_datagram_endpoint(Echo, local_addr=("127.0.0.1", 0))
    destination = transport.get_extra_info("sockname")[1]
    sockets, controls = [], []
    try:
        async with proxy(binary, args.timeout) as (port, process):
            for _ in range(args.flows):
                udp = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
                udp.setblocking(False)
                udp.bind(("127.0.0.1", 0))
                udp.connect(("127.0.0.1", port))
                sockets.append(udp)
                controls.append(await socks(port, 3, udp.getsockname()[1]))
            await asyncio.sleep(0.1)
            before = rss_kib(process.pid)
            packet = bytes([0, 0, 0, 1, 127, 0, 0, 1]) + struct.pack("!H", destination) + b"X"
            for udp in sockets:
                await loop.sock_sendall(udp, packet)
                assert await asyncio.wait_for(loop.sock_recv(udp, 65535), 2) == packet
            await asyncio.sleep(1)
            after = rss_kib(process.pid)
            return {"rss_before_kib": before, "rss_after_kib": after, "rss_growth_kib": after - before}
    finally:
        for udp in sockets:
            udp.close()
        for _, writer in controls:
            writer.close()
        await asyncio.gather(*(writer.wait_closed() for _, writer in controls), return_exceptions=True)
        transport.close()


async def tcp_memory_sample(binary, args):
    async def echo(reader, writer):
        try:
            while data := await reader.read(65536):
                writer.write(data)
                await writer.drain()
        except ConnectionError:
            pass
        finally:
            writer.close()
    async with await asyncio.start_server(echo, "127.0.0.1", 0) as upstream:
        destination = upstream.sockets[0].getsockname()[1]
        async with proxy(binary, args.timeout) as (port, process):
            connections = [await socks(port, 1, destination) for _ in range(args.flows)]
            try:
                await asyncio.sleep(0.1)
                before = rss_kib(process.pid)
                payload = b"X" * (1024 * 1024)
                gate = asyncio.Semaphore(16)
                async def burst(reader, writer):
                    async with gate:
                        writer.write(payload)
                        await asyncio.gather(writer.drain(), reader.readexactly(len(payload)))
                before_cpu = process_cpu(process.pid)
                started = time.perf_counter()
                await asyncio.gather(*(burst(*connection) for connection in connections))
                transfer = time.perf_counter() - started
                cpu = process_cpu(process.pid) - before_cpu
                await asyncio.sleep(2)
                idle = rss_kib(process.pid)
                for reader, writer in connections:
                    writer.write(b"resume")
                    await writer.drain()
                    assert await reader.readexactly(6) == b"resume"
                return {"rss_before_kib": before, "rss_after_kib": idle,
                        "transfer_seconds": transfer, "proxy_cpu_seconds": cpu}
            finally:
                for _, writer in connections:
                    writer.close()
                await asyncio.gather(*(writer.wait_closed() for _, writer in connections))


async def main(args):
    results = {binary: [] for binary in args.binaries}
    for round_index in range(args.rounds):
        # 交替执行次序，降低机器状态随时间变化带来的偏差。
        order = args.binaries if round_index % 2 == 0 else list(reversed(args.binaries))
        for binary in order:
            action = {"http": http_sample, "udp": udp_sample, "udp-memory": udp_memory_sample, "tcp-memory": tcp_memory_sample}[args.mode]
            sample = await asyncio.wait_for(action(binary, args), 60)
            results[binary].append(sample)
    medians = {
        binary: {key: statistics.median(sample[key] for sample in samples) for key in samples[0]}
        for binary, samples in results.items()
    }
    print(json.dumps({"parameters": vars(args), "medians": medians, "samples": results}, indent=2))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", nargs="+", help="待比较的代理程序绝对路径")
    parser.add_argument("--mode", choices=["http", "udp", "udp-memory", "tcp-memory"], default="http")
    parser.add_argument("--flows", type=int, default=256, help="内存基准的流数")
    parser.add_argument("--batch", type=int, default=1, help="每批 UDP 包数")
    parser.add_argument("--rounds", type=int, default=5)
    parser.add_argument("--requests", type=int, default=1000, help="每条连接的请求次数")
    parser.add_argument("--concurrency", type=int, default=1)
    parser.add_argument("--size", type=int, default=1024, help="响应体字节数")
    parser.add_argument("--timeout", type=int, default=0, help="TCP 空闲超时秒数")
    asyncio.run(main(parser.parse_args()))
