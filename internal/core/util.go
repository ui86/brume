package core

import (
	"bytes"
	"encoding/binary"
	"errors"
	"io"
	"iter"
	"net"
	"strconv"
	"time"
)

// ParseAddress 将地址 x.x.x.x:xx 格式化为原始地址。
// addr 包含域名长度
func ParseAddress(address string) (a byte, addr []byte, port []byte, err error) {
	var h, p string
	h, p, err = net.SplitHostPort(address)
	if err != nil {
		return
	}
	ip := net.ParseIP(h)
	if ip4 := ip.To4(); ip4 != nil {
		a = ATYPIPv4
		addr = []byte(ip4)
	} else if ip6 := ip.To16(); ip6 != nil {
		a = ATYPIPv6
		addr = []byte(ip6)
	} else {
		a = ATYPDomain
		addr = []byte{byte(len(h))}
		addr = append(addr, []byte(h)...)
	}
	i, _ := strconv.Atoi(p)
	port = make([]byte, 2)
	binary.BigEndian.PutUint16(port, uint16(i))
	return
}

// 字节转换为地址
// addr 包含域名长度
func ParseBytesAddress(b []byte) (a byte, addr []byte, port []byte, err error) {
	if len(b) < 1 {
		err = errors.New("Invalid address")
		return
	}
	a = b[0]
	if a == ATYPIPv4 {
		if len(b) < 1+4+2 {
			err = errors.New("Invalid address")
			return
		}
		addr = b[1 : 1+4]
		port = b[1+4 : 1+4+2]
		return
	}
	if a == ATYPIPv6 {
		if len(b) < 1+16+2 {
			err = errors.New("Invalid address")
			return
		}
		addr = b[1 : 1+16]
		port = b[1+16 : 1+16+2]
		return
	}
	if a == ATYPDomain {
		if len(b) < 1+1 {
			err = errors.New("Invalid address")
			return
		}
		l := int(b[1])
		if len(b) < 1+1+l+2 {
			err = errors.New("Invalid address")
			return
		}
		addr = b[1 : 1+1+l]
		port = b[1+1+l : 1+1+l+2]
		return
	}
	err = errors.New("Invalid address")
	return
}

// ToAddress 将原始地址格式化为 x.x.x.x:xx
// addr 包含域名长度
func ToAddress(a byte, addr []byte, port []byte) string {
	var h, p string
	if a == ATYPIPv4 || a == ATYPIPv6 {
		h = net.IP(addr).String()
	}
	if a == ATYPDomain {
		if len(addr) < 1 {
			return ""
		}
		if len(addr) < int(addr[0])+1 {
			return ""
		}
		h = string(addr[1:])
	}
	p = strconv.Itoa(int(binary.BigEndian.Uint16(port)))
	return net.JoinHostPort(h, p)
}

// Address 返回请求地址，如 ip:xx
func (r *Request) Address() string {
	var s string
	if r.Atyp == ATYPDomain {
		s = bytes.NewBuffer(r.DstAddr[1:]).String()
	} else {
		s = net.IP(r.DstAddr).String()
	}
	p := strconv.Itoa(int(binary.BigEndian.Uint16(r.DstPort)))
	return net.JoinHostPort(s, p)
}

// Address 返回请求地址，如 ip:xx
func (r *Reply) Address() string {
	var s string
	if r.Atyp == ATYPDomain {
		s = bytes.NewBuffer(r.BndAddr[1:]).String()
	} else {
		s = net.IP(r.BndAddr).String()
	}
	p := strconv.Itoa(int(binary.BigEndian.Uint16(r.BndPort)))
	return net.JoinHostPort(s, p)
}

// Address 返回数据报地址，如 ip:xx
func (d *Datagram) Address() string {
	var s string
	if d.Atyp == ATYPDomain {
		s = bytes.NewBuffer(d.DstAddr[1:]).String()
	} else {
		s = net.IP(d.DstAddr).String()
	}
	p := strconv.Itoa(int(binary.BigEndian.Uint16(d.DstPort)))
	return net.JoinHostPort(s, p)
}

// writeHostUnreachableReply 发送主机不可达的错误回复（提取公共逻辑）
func writeHostUnreachableReply(w io.Writer, atyp byte) error {
	var p *Reply
	if atyp == ATYPIPv4 || atyp == ATYPDomain {
		p = NewReply(RepHostUnreachable, ATYPIPv4, []byte{0x00, 0x00, 0x00, 0x00}, []byte{0x00, 0x00})
	} else {
		p = NewReply(RepHostUnreachable, ATYPIPv6, []byte(net.IPv6zero), []byte{0x00, 0x00})
	}
	_, err := p.WriteTo(w)
	return err
}

// RotateQueue1 随机打乱切片
func RotateQueue1(start, i, size int) int {
	return (start + i) % size
}

// RangeRnd 随机打乱切片
func RangeRnd[S ~[]E, E any](s S) iter.Seq2[int, E] {
	index := int(time.Now().Unix()) % len(s)
	return func(yield func(int, E) bool) {
		for i := range len(s) {
			r := RotateQueue1(index, i, len(s))
			if !yield(r, s[r]) {
				break
			}
		}
	}
}
