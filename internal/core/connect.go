package core

import (
	"encoding/binary"
	"io"
	"log"
	"net"
)

func (r *Request) Connect(w io.Writer) (net.Conn, error) {
	if Debug {
		log.Println("Call:", r.Address())
	}
	rc, err := DialTCP("tcp", "", r.Address())
	if err != nil {
		if wErr := writeHostUnreachableReply(w, r.Atyp); wErr != nil {
			return nil, wErr
		}
		return nil, err
	}

	// 优化：直接从结构体获取 IP/Port
	var a byte
	var addr, port []byte

	if tcpAddr, ok := rc.LocalAddr().(*net.TCPAddr); ok {
		if ip4 := tcpAddr.IP.To4(); ip4 != nil {
			a = ATYPIPv4
			addr = ip4
		} else {
			a = ATYPIPv6
			addr = tcpAddr.IP
		}
		port = make([]byte, 2)
		binary.BigEndian.PutUint16(port, uint16(tcpAddr.Port))
	} else {
		var err error
		a, addr, port, err = ParseAddress(rc.LocalAddr().String())
		if err != nil {
			rc.Close()
			return nil, err
		}
		if a == ATYPDomain {
			addr = addr[1:]
		}
	}

	p := NewReply(RepSuccess, a, addr, port)
	if _, err := p.WriteTo(w); err != nil {
		rc.Close()
		return nil, err
	}

	return rc, nil
}
