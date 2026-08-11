package core

import (
	"bytes"
	"net"
	"testing"
	"time"
)

type memoryConn struct {
	bytes.Buffer
}

func (c *memoryConn) Close() error                     { return nil }
func (c *memoryConn) LocalAddr() net.Addr              { return &net.UDPAddr{} }
func (c *memoryConn) RemoteAddr() net.Addr             { return &net.UDPAddr{} }
func (c *memoryConn) SetDeadline(time.Time) error      { return nil }
func (c *memoryConn) SetReadDeadline(time.Time) error  { return nil }
func (c *memoryConn) SetWriteDeadline(time.Time) error { return nil }

func TestServerIsAllowed(t *testing.T) {
	server, err := NewClassicServer(":1080", "0.0.0.0", "", "", 0, 0, []string{
		"198.51.100.10",
		"192.0.2.0/24",
		"2001:db8::/32",
	})
	if err != nil {
		t.Fatal(err)
	}

	cases := []struct {
		name string
		ip   string
		want bool
	}{
		{name: "精确匹配", ip: "198.51.100.10", want: true},
		{name: "IPv4网段匹配", ip: "192.0.2.20", want: true},
		{name: "IPv6网段匹配", ip: "2001:db8::20", want: true},
		{name: "未匹配", ip: "203.0.113.10", want: false},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			if got := server.IsAllowed(net.ParseIP(tc.ip)); got != tc.want {
				t.Fatalf("白名单判断错误: got %v, want %v", got, tc.want)
			}
		})
	}
}

func TestDatagramRoundTrip(t *testing.T) {
	payload := []byte("benchmark-payload")
	cases := []struct {
		name string
		atyp byte
		addr []byte
	}{
		{name: "IPv4", atyp: ATYPIPv4, addr: []byte{192, 0, 2, 10}},
		{name: "IPv6", atyp: ATYPIPv6, addr: net.ParseIP("2001:db8::10").To16()},
		{name: "域名", atyp: ATYPDomain, addr: []byte("example.com")},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			dgram := NewDatagram(tc.atyp, tc.addr, []byte{0x01, 0xbb}, payload)
			packet := dgram.AppendTo(make([]byte, 0, dgram.HeaderLen()+len(payload)))
			parsed, err := ParseDatagram(packet)
			if err != nil {
				t.Fatal(err)
			}
			if parsed.Atyp != dgram.Atyp || !bytes.Equal(parsed.DstAddr, dgram.DstAddr) {
				t.Fatalf("地址解析错误: got %v, want %v", parsed.DstAddr, dgram.DstAddr)
			}
			if !bytes.Equal(parsed.DstPort, dgram.DstPort) || !bytes.Equal(parsed.Data, payload) {
				t.Fatal("端口或负载解析错误")
			}

			compatible, err := NewDatagramFromBytes(packet)
			if err != nil {
				t.Fatal(err)
			}
			if !bytes.Equal(compatible.Data, payload) {
				t.Fatal("兼容解析接口返回错误")
			}
		})
	}
}

func TestDatagramHeadroomEncoding(t *testing.T) {
	payload := []byte("headroom-payload")
	storage := make([]byte, maxSOCKS5UDPHeaderSize+len(payload))
	copy(storage[maxSOCKS5UDPHeaderSize:], payload)

	dgram := NewDatagram(
		ATYPIPv4,
		[]byte{192, 0, 2, 10},
		[]byte{0x01, 0xbb},
		storage[maxSOCKS5UDPHeaderSize:],
	)
	packetStart := maxSOCKS5UDPHeaderSize - dgram.HeaderLen()
	packet := dgram.AppendHeaderTo(storage[packetStart:packetStart])
	packet = append(packet, dgram.Data...)

	if want := dgram.Bytes(); !bytes.Equal(packet, want) {
		t.Fatalf("预留空间编码结果错误: got %v, want %v", packet, want)
	}
}

func TestClientUDPReadWrite(t *testing.T) {
	payload := []byte("client-udp-payload")
	writeConn := &memoryConn{}
	client := &Client{
		UDPConn: writeConn,
		dstAtyp: ATYPDomain,
		dstAddr: []byte("example.com"),
		dstPort: []byte{0x01, 0xbb},
	}

	n, err := client.Write(payload)
	if err != nil {
		t.Fatal(err)
	}
	if n != len(payload) {
		t.Fatalf("客户端写入长度错误: got %d, want %d", n, len(payload))
	}
	written, err := ParseDatagram(writeConn.Bytes())
	if err != nil {
		t.Fatal(err)
	}
	if written.Address() != "example.com:443" || !bytes.Equal(written.Data, payload) {
		t.Fatal("客户端 UDP 封装结果错误")
	}

	packet := NewDatagram(ATYPIPv4, []byte{192, 0, 2, 10}, []byte{0x01, 0xbb}, payload).Bytes()
	readConn := &memoryConn{}
	_, _ = readConn.Write(packet)
	client.UDPConn = readConn
	buffer := make([]byte, len(packet))
	n, err = client.Read(buffer)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(buffer[:n], payload) {
		t.Fatalf("客户端 UDP 解封装结果错误: got %q, want %q", buffer[:n], payload)
	}
}

func TestParseDatagramRejectsInvalidPacket(t *testing.T) {
	cases := [][]byte{
		nil,
		{0, 0, 0, 0xff},
		{0, 0, 0, ATYPIPv4, 192, 0, 2},
		{0, 0, 0, ATYPDomain, 0},
		{0, 0, 0, ATYPDomain, 3, 'a', 'b'},
		{0, 0, 0, ATYPIPv4, 192, 0, 2, 10, 0x01, 0xbb},
	}

	for _, packet := range cases {
		if _, err := ParseDatagram(packet); err == nil {
			t.Fatalf("无效数据报未返回错误: %v", packet)
		}
	}
}

func TestNewRequestFrom(t *testing.T) {
	cases := []struct {
		name string
		atyp byte
		addr []byte
	}{
		{name: "IPv4", atyp: ATYPIPv4, addr: []byte{192, 0, 2, 10}},
		{name: "IPv6", atyp: ATYPIPv6, addr: net.ParseIP("2001:db8::10").To16()},
		{name: "域名", atyp: ATYPDomain, addr: []byte("example.com")},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			packet := makeBenchmarkRequestPacket(tc.atyp, tc.addr)
			request, err := NewRequestFrom(bytes.NewReader(packet))
			if err != nil {
				t.Fatal(err)
			}
			if request.Atyp != tc.atyp || !bytes.Equal(request.DstPort, []byte{0x01, 0xbb}) {
				t.Fatal("请求地址类型或端口解析错误")
			}
			wantAddr := tc.addr
			if tc.atyp == ATYPDomain {
				wantAddr = append([]byte{byte(len(tc.addr))}, tc.addr...)
			}
			if !bytes.Equal(request.DstAddr, wantAddr) {
				t.Fatalf("请求地址解析错误: got %v, want %v", request.DstAddr, wantAddr)
			}
		})
	}
}
