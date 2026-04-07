package core

import (
	"context"
	"net"
	"time"
)

var (
	DNSServersV4  = []string{"8.8.8.8:53", "8.8.4.4:53", "1.1.1.1:53", "1.0.0.1:53"}
	DNSServersV6  = []string{"[2001:4860:4860::8888]:53", "[2001:4860:4860::8844]:53", "[2606:4700:4700::1111]:53", "[2606:4700:4700::1001]:53"}
	DNSServersAll = append(DNSServersV4, DNSServersV6...)
)

var Debug bool

func init() {
	// 使用 Go 内置的 DNS 解析器解析域名
	net.DefaultResolver = &net.Resolver{
		PreferGo: true,
		Dial: func(ctx context.Context, network, address string) (net.Conn, error) {
			d := net.Dialer{
				Timeout: time.Second * 5,
			}
			var conn net.Conn
			var err error
			for _, server := range RangeRnd(DNSServersAll) {
				conn, err = d.DialContext(ctx, "udp", server)
				if err == nil {
					return conn, nil
				}
			}
			return nil, err
		},
	}
}

var Resolve func(network string, addr string) (net.Addr, error) = func(network string, addr string) (net.Addr, error) {
	if network == "tcp" {
		return net.ResolveTCPAddr("tcp", addr)
	}
	return net.ResolveUDPAddr("udp", addr)
}

// 优化：使用 net.Dialer 支持 Happy Eyeballs 和超时控制
var DialTCP func(network string, laddr, raddr string) (net.Conn, error) = func(network string, laddr, raddr string) (net.Conn, error) {
	dialer := &net.Dialer{
		Timeout:   10 * time.Second,
		KeepAlive: 30 * time.Second,
	}
	if laddr != "" {
		local, err := net.ResolveTCPAddr(network, laddr)
		if err == nil {
			dialer.LocalAddr = local
		}
	}
	return dialer.Dial(network, raddr)
}

// 优化：简化 UDP Dial
var DialUDP func(network string, laddr, raddr string) (net.Conn, error) = func(network string, laddr, raddr string) (net.Conn, error) {
	var la, ra *net.UDPAddr
	var err error
	if laddr != "" {
		la, err = net.ResolveUDPAddr(network, laddr)
		if err != nil {
			return nil, err
		}
	}
	ra, err = net.ResolveUDPAddr(network, raddr)
	if err != nil {
		return nil, err
	}
	return net.DialUDP(network, la, ra)
}
