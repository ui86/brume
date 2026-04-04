package core

import (
	"bytes"
	"log"
	"net"
)

func (r *Request) UDP(c net.Conn, serverAddr net.Addr) (net.Addr, error) {
	var clientAddr net.Addr
	var err error

	// 优化：使用类型断言避免字符串解析
	if bytes.Equal(r.DstPort, []byte{0x00, 0x00}) {
		if tcpAddr, ok := c.RemoteAddr().(*net.TCPAddr); ok {
			clientAddr = &net.UDPAddr{
				IP:   tcpAddr.IP,
				Port: tcpAddr.Port,
				Zone: tcpAddr.Zone,
			}
		} else {
			clientAddr, err = net.ResolveUDPAddr("udp", c.RemoteAddr().String())
		}
	} else {
		clientAddr, err = net.ResolveUDPAddr("udp", r.Address())
	}

	if err != nil {
		var p *Reply
		if r.Atyp == ATYPIPv4 || r.Atyp == ATYPDomain {
			p = NewReply(RepHostUnreachable, ATYPIPv4, []byte{0x00, 0x00, 0x00, 0x00}, []byte{0x00, 0x00})
		} else {
			p = NewReply(RepHostUnreachable, ATYPIPv6, []byte(net.IPv6zero), []byte{0x00, 0x00})
		}
		if _, err = p.WriteTo(c); err != nil {
			return nil, err
		}
		return nil, err
	}
	if Debug {
		log.Println("Client wants to start UDP talk use", clientAddr.String())
	}
	a, addr, port, err := ParseAddress(serverAddr.String())
	if err != nil {
		var p *Reply
		if r.Atyp == ATYPIPv4 || r.Atyp == ATYPDomain {
			p = NewReply(RepHostUnreachable, ATYPIPv4, []byte{0x00, 0x00, 0x00, 0x00}, []byte{0x00, 0x00})
		} else {
			p = NewReply(RepHostUnreachable, ATYPIPv6, []byte(net.IPv6zero), []byte{0x00, 0x00})
		}
		if _, err = p.WriteTo(c); err != nil {
			return nil, err
		}
		return nil, err
	}
	if a == ATYPDomain {
		addr = addr[1:]
	}
	p := NewReply(RepSuccess, a, addr, port)
	if _, err := p.WriteTo(c); err != nil {
		return nil, err
	}

	return clientAddr, nil
}
