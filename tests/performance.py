#!/usr/bin/env python3
"""在本机回环网络上交替测量两个构建，输出每轮数据及中位数。"""

import argparse
import asyncio
import contextlib
import json
import os
import socket
import statistics
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


async def main(args):
    results = {binary: [] for binary in args.binaries}
    for round_index in range(args.rounds):
        # 交替执行次序，降低机器状态随时间变化带来的偏差。
        order = args.binaries if round_index % 2 == 0 else list(reversed(args.binaries))
        for binary in order:
            sample = await asyncio.wait_for(http_sample(binary, args), 60)
            results[binary].append(sample)
    medians = {
        binary: {key: statistics.median(sample[key] for sample in samples) for key in samples[0]}
        for binary, samples in results.items()
    }
    print(json.dumps({"parameters": vars(args), "medians": medians, "samples": results}, indent=2))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", nargs="+", help="待比较的代理程序绝对路径")
    parser.add_argument("--rounds", type=int, default=5)
    parser.add_argument("--requests", type=int, default=1000, help="每条连接的请求次数")
    parser.add_argument("--concurrency", type=int, default=1)
    parser.add_argument("--size", type=int, default=1024, help="响应体字节数")
    parser.add_argument("--timeout", type=int, default=0, help="TCP 空闲超时秒数")
    asyncio.run(main(parser.parse_args()))
