package core

import (
	"net"
	"time"
)

var Debug bool

func init() {
	// log.SetFlags(log.LstdFlags | log.Lshortfile)
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
