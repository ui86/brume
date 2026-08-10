package core

import (
	"bytes"
	"io"
	"net"
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
	_, cidr, err := net.ParseCIDR("192.0.2.0/24")
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
			server: &Server{AllowedIPs: map[string]struct{}{
				"198.51.100.10": {},
			}},
			ip:   net.ParseIP("198.51.100.10"),
			want: true,
		},
		{
			name:   "网段匹配",
			server: &Server{AllowedCIDRs: []*net.IPNet{cidr}},
			ip:     net.ParseIP("192.0.2.10"),
			want:   true,
		},
		{
			name:   "未匹配",
			server: &Server{AllowedCIDRs: []*net.IPNet{cidr}},
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
