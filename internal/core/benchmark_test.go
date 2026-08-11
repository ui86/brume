package core

import (
	"bytes"
	"io"
	"net"
	"net/netip"
	"runtime"
	"strconv"
	"testing"
)

var (
	benchmarkAllowed  bool
	benchmarkType     byte
	benchmarkAddr     []byte
	benchmarkPort     []byte
	benchmarkErr      error
	benchmarkRequest  *Request
	benchmarkDatagram *Datagram
	benchmarkDgramVal Datagram
	benchmarkBytes    []byte
	benchmarkWritten  int64
)

// makeBenchmarkAddressPacket 构造带地址类型和端口的 SOCKS5 地址字段。
func makeBenchmarkAddressPacket(atyp byte, addr []byte) []byte {
	packet := []byte{atyp}
	if atyp == ATYPDomain {
		packet = append(packet, byte(len(addr)))
	}
	packet = append(packet, addr...)
	return append(packet, 0x01, 0xbb)
}

// makeBenchmarkRequestPacket 构造完整的 SOCKS5 连接请求包。
func makeBenchmarkRequestPacket(atyp byte, addr []byte) []byte {
	packet := []byte{Ver, CmdConnect, 0}
	return append(packet, makeBenchmarkAddressPacket(atyp, addr)...)
}

// BenchmarkServerIsAllowed 测量白名单精确匹配和网段匹配的开销。
func BenchmarkServerIsAllowed(b *testing.B) {
	cidr, err := netip.ParsePrefix("192.0.2.0/24")
	if err != nil {
		b.Fatal(err)
	}

	cases := []struct {
		name   string
		server *Server
		ip     net.IP
		want   bool
	}{
		{
			name:   "无白名单",
			server: &Server{},
			ip:     net.ParseIP("198.51.100.10"),
			want:   true,
		},
		{
			name: "精确匹配",
			server: &Server{AllowedIPs: map[netip.Addr]struct{}{
				netip.MustParseAddr("198.51.100.10"): {},
			}},
			ip:   net.ParseIP("198.51.100.10"),
			want: true,
		},
		{
			name:   "网段匹配",
			server: &Server{AllowedCIDRs: []netip.Prefix{cidr}},
			ip:     net.ParseIP("192.0.2.10"),
			want:   true,
		},
		{
			name:   "未匹配",
			server: &Server{AllowedCIDRs: []netip.Prefix{cidr}},
			ip:     net.ParseIP("198.51.100.10"),
			want:   false,
		},
	}

	for _, tc := range cases {
		b.Run(tc.name, func(b *testing.B) {
			b.ReportAllocs()
			b.ResetTimer()
			for b.Loop() {
				benchmarkAllowed = tc.server.IsAllowed(tc.ip)
			}
			b.StopTimer()
			if benchmarkAllowed != tc.want {
				b.Fatalf("白名单判断结果错误: got %v, want %v", benchmarkAllowed, tc.want)
			}
		})
	}
}

// BenchmarkServerIsAllowedCIDRScale 测量未命中时网段数量增长带来的线性开销。
func BenchmarkServerIsAllowedCIDRScale(b *testing.B) {
	for _, size := range []int{1, 16, 64, 256} {
		prefixes := make([]netip.Prefix, 0, size)
		for i := range size {
			addr := netip.AddrFrom4([4]byte{10, byte(i), 0, 0})
			prefixes = append(prefixes, netip.PrefixFrom(addr, 16))
		}
		server := &Server{AllowedCIDRs: prefixes}
		ip := net.ParseIP("203.0.113.10")

		b.Run(strconv.Itoa(size), func(b *testing.B) {
			b.ReportAllocs()
			b.ResetTimer()
			for b.Loop() {
				benchmarkAllowed = server.IsAllowed(ip)
			}
			b.StopTimer()
			if benchmarkAllowed {
				b.Fatal("未命中地址被错误放行")
			}
		})
	}
}

// BenchmarkServerIsAllowedParallel 测量并发读取白名单的开销。
func BenchmarkServerIsAllowedParallel(b *testing.B) {
	server := &Server{AllowedIPs: map[netip.Addr]struct{}{
		netip.MustParseAddr("198.51.100.10"): {},
	}}
	ip := net.ParseIP("198.51.100.10")
	if !server.IsAllowed(ip) {
		b.Fatal("精确匹配地址未被放行")
	}
	b.ReportAllocs()
	b.ResetTimer()
	b.RunParallel(func(pb *testing.PB) {
		var allowed bool
		for pb.Next() {
			allowed = server.IsAllowed(ip)
		}
		runtime.KeepAlive(allowed)
	})
}

// BenchmarkParseAddress 测量文本地址转换为 SOCKS5 地址字段的开销。
func BenchmarkParseAddress(b *testing.B) {
	cases := []struct {
		name    string
		address string
	}{
		{name: "IPv4", address: "192.0.2.10:443"},
		{name: "IPv6", address: "[2001:db8::10]:443"},
		{name: "域名", address: "example.com:443"},
	}

	for _, tc := range cases {
		b.Run(tc.name, func(b *testing.B) {
			b.ReportAllocs()
			b.ResetTimer()
			for b.Loop() {
				benchmarkType, benchmarkAddr, benchmarkPort, benchmarkErr = ParseAddress(tc.address)
			}
			b.StopTimer()
			if benchmarkErr != nil {
				b.Fatal(benchmarkErr)
			}
			if benchmarkType == 0 || len(benchmarkAddr) == 0 || len(benchmarkPort) != 2 {
				b.Fatal("地址转换结果不完整")
			}
		})
	}
}

// BenchmarkParseBytesAddress 测量 SOCKS5 地址字段解析的开销。
func BenchmarkParseBytesAddress(b *testing.B) {
	cases := []struct {
		name string
		data []byte
	}{
		{
			name: "IPv4",
			data: makeBenchmarkAddressPacket(ATYPIPv4, net.ParseIP("192.0.2.10").To4()),
		},
		{
			name: "IPv6",
			data: makeBenchmarkAddressPacket(ATYPIPv6, net.ParseIP("2001:db8::10").To16()),
		},
		{
			name: "域名",
			data: makeBenchmarkAddressPacket(ATYPDomain, []byte("example.com")),
		},
	}

	for _, tc := range cases {
		b.Run(tc.name, func(b *testing.B) {
			b.ReportAllocs()
			b.ResetTimer()
			for b.Loop() {
				benchmarkType, benchmarkAddr, benchmarkPort, benchmarkErr = ParseBytesAddress(tc.data)
			}
			b.StopTimer()
			if benchmarkErr != nil {
				b.Fatal(benchmarkErr)
			}
			if benchmarkType == 0 || len(benchmarkAddr) == 0 || len(benchmarkPort) != 2 {
				b.Fatal("地址字段解析结果不完整")
			}
		})
	}
}

// BenchmarkNewRequestFrom 测量 SOCKS5 请求包解析的开销。
func BenchmarkNewRequestFrom(b *testing.B) {
	cases := []struct {
		name   string
		packet []byte
	}{
		{
			name:   "IPv4",
			packet: makeBenchmarkRequestPacket(ATYPIPv4, net.ParseIP("192.0.2.10").To4()),
		},
		{
			name:   "IPv6",
			packet: makeBenchmarkRequestPacket(ATYPIPv6, net.ParseIP("2001:db8::10").To16()),
		},
		{
			name:   "域名",
			packet: makeBenchmarkRequestPacket(ATYPDomain, []byte("example.com")),
		},
	}

	for _, tc := range cases {
		b.Run(tc.name, func(b *testing.B) {
			var reader bytes.Reader
			b.ReportAllocs()
			b.ResetTimer()
			for b.Loop() {
				reader.Reset(tc.packet)
				benchmarkRequest, benchmarkErr = NewRequestFrom(&reader)
			}
			b.StopTimer()
			if benchmarkErr != nil {
				b.Fatal(benchmarkErr)
			}
			if benchmarkRequest == nil || benchmarkRequest.Atyp == 0 {
				b.Fatal("请求解析结果为空")
			}
		})
	}
}

// BenchmarkDatagramBytes 测量 UDP 数据报编码的开销。
func BenchmarkDatagramBytes(b *testing.B) {
	cases := []struct {
		name  string
		dgram *Datagram
	}{
		{
			name:  "IPv4",
			dgram: NewDatagram(ATYPIPv4, []byte{192, 0, 2, 10}, []byte{0x01, 0xbb}, bytes.Repeat([]byte{0xab}, 1024)),
		},
		{
			name:  "域名",
			dgram: NewDatagram(ATYPDomain, []byte("example.com"), []byte{0x01, 0xbb}, bytes.Repeat([]byte{0xab}, 1024)),
		},
	}

	for _, tc := range cases {
		b.Run(tc.name, func(b *testing.B) {
			b.ReportAllocs()
			b.ResetTimer()
			for b.Loop() {
				benchmarkBytes = tc.dgram.Bytes()
			}
			b.StopTimer()
			if len(benchmarkBytes) == 0 {
				b.Fatal("数据报编码结果为空")
			}
		})
	}
}

// BenchmarkDatagramAppendTo 测量复用缓冲区时的 UDP 数据报编码开销。
func BenchmarkDatagramAppendTo(b *testing.B) {
	dgram := NewDatagram(ATYPIPv4, []byte{192, 0, 2, 10}, []byte{0x01, 0xbb}, bytes.Repeat([]byte{0xab}, 1024))
	buffer := make([]byte, 0, dgram.HeaderLen()+len(dgram.Data))
	b.ReportAllocs()
	b.ResetTimer()
	for b.Loop() {
		benchmarkBytes = dgram.AppendTo(buffer[:0])
	}
	b.StopTimer()
	if len(benchmarkBytes) == 0 {
		b.Fatal("数据报编码结果为空")
	}
}

// BenchmarkDatagramAppendToPool 测量生产路径中池化缓冲区的编码开销。
func BenchmarkDatagramAppendToPool(b *testing.B) {
	dgram := NewDatagram(ATYPIPv4, []byte{192, 0, 2, 10}, []byte{0x01, 0xbb}, bytes.Repeat([]byte{0xab}, 1024))
	b.ReportAllocs()
	b.ResetTimer()
	for b.Loop() {
		buffer := udpBufPool.Get().(*udpBuffer)
		benchmarkBytes = dgram.AppendTo(buffer[:0])
		udpBufPool.Put(buffer)
	}
	b.StopTimer()
	if len(benchmarkBytes) == 0 {
		b.Fatal("数据报编码结果为空")
	}
}

// BenchmarkParseDatagram 测量返回值形式的 UDP 数据报解析开销。
func BenchmarkParseDatagram(b *testing.B) {
	packet := NewDatagram(ATYPIPv4, []byte{192, 0, 2, 10}, []byte{0x01, 0xbb}, bytes.Repeat([]byte{0xab}, 1024)).Bytes()
	b.ReportAllocs()
	b.ResetTimer()
	for b.Loop() {
		benchmarkDgramVal, benchmarkErr = ParseDatagram(packet)
	}
	b.StopTimer()
	if benchmarkErr != nil {
		b.Fatal(benchmarkErr)
	}
	if len(benchmarkDgramVal.Data) == 0 {
		b.Fatal("数据报解析结果为空")
	}
}

// BenchmarkNewDatagramFromBytes 测量 UDP 数据报解析的开销。
func BenchmarkNewDatagramFromBytes(b *testing.B) {
	packet := NewDatagram(ATYPIPv4, []byte{192, 0, 2, 10}, []byte{0x01, 0xbb}, bytes.Repeat([]byte{0xab}, 1024)).Bytes()
	b.ReportAllocs()
	b.ResetTimer()
	for b.Loop() {
		benchmarkDatagram, benchmarkErr = NewDatagramFromBytes(packet)
	}
	b.StopTimer()
	if benchmarkErr != nil {
		b.Fatal(benchmarkErr)
	}
	if benchmarkDatagram == nil || len(benchmarkDatagram.Data) == 0 {
		b.Fatal("数据报解析结果为空")
	}
}

// BenchmarkRequestWriteTo 测量 SOCKS5 请求包序列化的开销。
func BenchmarkRequestWriteTo(b *testing.B) {
	request := NewRequest(CmdConnect, ATYPDomain, []byte("example.com"), []byte{0x01, 0xbb})
	b.ReportAllocs()
	b.ResetTimer()
	for b.Loop() {
		benchmarkWritten, benchmarkErr = request.WriteTo(io.Discard)
	}
	b.StopTimer()
	if benchmarkErr != nil {
		b.Fatal(benchmarkErr)
	}
	if benchmarkWritten == 0 {
		b.Fatal("请求序列化结果为空")
	}
}
