package core

import (
	"errors"
	"io"
	"log"
	"net"
)

var (
	ErrVersion         = errors.New("Invalid Version")
	ErrUserPassVersion = errors.New("Invalid Version of Username Password Auth")
	ErrBadRequest      = errors.New("Bad Request")
)

// NewNegotiationRequestFrom 从 io.Reader 中读取并解析 SOCKS5 协商请求
func NewNegotiationRequestFrom(r io.Reader) (*NegotiationRequest, error) {
	var bb [2]byte // 优化：栈分配
	if _, err := io.ReadFull(r, bb[:]); err != nil {
		return nil, err
	}
	if bb[0] != Ver {
		return nil, ErrVersion
	}
	if bb[1] == 0 {
		return nil, ErrBadRequest
	}
	ms := make([]byte, int(bb[1]))
	if _, err := io.ReadFull(r, ms); err != nil {
		return nil, err
	}
	if Debug {
		log.Printf("Got NegotiationRequest: %#v %#v %#v\n", bb[0], bb[1], ms)
	}
	return &NegotiationRequest{
		Ver:      bb[0],
		NMethods: bb[1],
		Methods:  ms,
	}, nil
}

// NewNegotiationReply 创建一个新的协商响应包
func NewNegotiationReply(method byte) *NegotiationReply {
	return &NegotiationReply{
		Ver:    Ver,
		Method: method,
	}
}

// WriteTo 将协商响应包写入 io.Writer
func (r *NegotiationReply) WriteTo(w io.Writer) (int64, error) {
	i, err := w.Write([]byte{r.Ver, r.Method})
	if err != nil {
		return 0, err
	}
	if Debug {
		log.Printf("Sent NegotiationReply: %#v %#v\n", r.Ver, r.Method)
	}
	return int64(i), nil
}

// NewUserPassNegotiationRequestFrom 从 Reader 解析用户名/密码认证请求
func NewUserPassNegotiationRequestFrom(r io.Reader) (*UserPassNegotiationRequest, error) {
	var bb [2]byte // 优化
	if _, err := io.ReadFull(r, bb[:]); err != nil {
		return nil, err
	}
	if bb[0] != UserPassVer {
		return nil, ErrUserPassVersion
	}
	if bb[1] == 0 {
		return nil, ErrBadRequest
	}
	ub := make([]byte, int(bb[1])+1)
	if _, err := io.ReadFull(r, ub); err != nil {
		return nil, err
	}
	if ub[int(bb[1])] == 0 {
		return nil, ErrBadRequest
	}
	p := make([]byte, int(ub[int(bb[1])]))
	if _, err := io.ReadFull(r, p); err != nil {
		return nil, err
	}
	if Debug {
		log.Printf("Got UserPassNegotiationRequest: %#v %#v %#v %#v %#v\n", bb[0], bb[1], ub[:int(bb[1])], ub[int(bb[1])], p)
	}
	return &UserPassNegotiationRequest{
		Ver:    bb[0],
		Ulen:   bb[1],
		Uname:  ub[:int(bb[1])],
		Plen:   ub[int(bb[1])],
		Passwd: p,
	}, nil
}

// NewUserPassNegotiationReply 创建用户名/密码认证结果的响应包
func NewUserPassNegotiationReply(status byte) *UserPassNegotiationReply {
	return &UserPassNegotiationReply{
		Ver:    UserPassVer,
		Status: status,
	}
}

// WriteTo 将认证结果响应包写入 io.Writer
func (r *UserPassNegotiationReply) WriteTo(w io.Writer) (int64, error) {
	i, err := w.Write([]byte{r.Ver, r.Status})
	if err != nil {
		return 0, err
	}
	if Debug {
		log.Printf("Sent UserPassNegotiationReply: %#v %#v \n", r.Ver, r.Status)
	}
	return int64(i), nil
}

// NewRequestFrom 从 Reader 解析 SOCKS5 请求包（包含目标地址和端口）
func NewRequestFrom(r io.Reader) (*Request, error) {
	var bb [4]byte // 优化
	if _, err := io.ReadFull(r, bb[:]); err != nil {
		return nil, err
	}
	if bb[0] != Ver {
		return nil, ErrVersion
	}
	var addrLen int
	var domainLen byte
	switch bb[3] {
	case ATYPIPv4:
		addrLen = net.IPv4len
	case ATYPIPv6:
		addrLen = net.IPv6len
	case ATYPDomain:
		var dal [1]byte
		if _, err := io.ReadFull(r, dal[:]); err != nil {
			return nil, err
		}
		if dal[0] == 0 {
			return nil, ErrBadRequest
		}
		domainLen = dal[0]
		addrLen = int(domainLen) + 1
	default:
		return nil, ErrBadRequest
	}

	wire := make([]byte, addrLen+2)
	readOffset := 0
	if bb[3] == ATYPDomain {
		wire[0] = domainLen
		readOffset = 1
	}
	if _, err := io.ReadFull(r, wire[readOffset:]); err != nil {
		return nil, err
	}
	addr := wire[:addrLen]
	port := wire[addrLen:]
	if Debug {
		log.Printf("Got Request: %#v %#v %#v %#v %#v %#v\n", bb[0], bb[1], bb[2], bb[3], addr, port)
	}
	return &Request{
		Ver:     bb[0],
		Cmd:     bb[1],
		Rsv:     bb[2],
		Atyp:    bb[3],
		DstAddr: addr,
		DstPort: port,
	}, nil
}

// NewReply 创建一个新的 SOCKS5 响应包
func NewReply(rep byte, atyp byte, bndaddr []byte, bndport []byte) *Reply {
	if atyp == ATYPDomain {
		bndaddr = append([]byte{byte(len(bndaddr))}, bndaddr...)
	}
	return &Reply{
		Ver:     Ver,
		Rep:     rep,
		Rsv:     0x00,
		Atyp:    atyp,
		BndAddr: bndaddr,
		BndPort: bndport,
	}
}

// WriteTo 将 SOCKS5 响应包写入 io.Writer，采用预分配优化
func (r *Reply) WriteTo(w io.Writer) (int64, error) {
	buf := make([]byte, 0, 4+len(r.BndAddr)+len(r.BndPort))
	buf = append(buf, r.Ver, r.Rep, r.Rsv, r.Atyp)
	buf = append(buf, r.BndAddr...)
	buf = append(buf, r.BndPort...)
	i, err := w.Write(buf)
	if err != nil {
		return 0, err
	}
	if Debug {
		log.Printf("Sent Reply: %#v %#v %#v %#v %#v %#v\n", r.Ver, r.Rep, r.Rsv, r.Atyp, r.BndAddr, r.BndPort)
	}
	return int64(i), nil
}

// ParseDatagram 从字节数组零拷贝解析 UDP 数据报
func ParseDatagram(bb []byte) (Datagram, error) {
	n := len(bb)
	minl := 4
	if n < minl {
		return Datagram{}, ErrBadRequest
	}
	var addr []byte
	switch bb[3] {
	case ATYPIPv4:
		minl += 4
		if n < minl {
			return Datagram{}, ErrBadRequest
		}
		addr = bb[minl-4 : minl]
	case ATYPIPv6:
		minl += 16
		if n < minl {
			return Datagram{}, ErrBadRequest
		}
		addr = bb[minl-16 : minl]
	case ATYPDomain:
		minl += 1
		if n < minl {
			return Datagram{}, ErrBadRequest
		}
		l := bb[4]
		if l == 0 {
			return Datagram{}, ErrBadRequest
		}
		minl += int(l)
		if n < minl {
			return Datagram{}, ErrBadRequest
		}
		addr = bb[4:minl]
	default:
		return Datagram{}, ErrBadRequest
	}
	minl += 2
	if n <= minl {
		return Datagram{}, ErrBadRequest
	}
	port := bb[minl-2 : minl]
	data := bb[minl:]
	return Datagram{
		Rsv:     bb[0:2],
		Frag:    bb[2],
		Atyp:    bb[3],
		DstAddr: addr,
		DstPort: port,
		Data:    data,
	}, nil
}

// NewDatagramFromBytes 从字节数组解析 UDP 数据报
func NewDatagramFromBytes(bb []byte) (*Datagram, error) {
	d, err := ParseDatagram(bb)
	if err != nil {
		return nil, err
	}
	return &d, nil
}

// NewDatagram 创建一个新的 UDP 数据报结构体
func NewDatagram(atyp byte, dstaddr []byte, dstport []byte, data []byte) *Datagram {
	if atyp == ATYPDomain {
		dstaddr = append([]byte{byte(len(dstaddr))}, dstaddr...)
	}
	return &Datagram{
		Rsv:     []byte{0x00, 0x00},
		Frag:    0x00,
		Atyp:    atyp,
		DstAddr: dstaddr,
		DstPort: dstport,
		Data:    data,
	}
}

// HeaderLen 返回 SOCKS5 UDP 数据报头长度
func (d *Datagram) HeaderLen() int {
	return len(d.Rsv) + 2 + len(d.DstAddr) + len(d.DstPort)
}

// AppendHeaderTo 将 SOCKS5 UDP 数据报头追加到目标缓冲区
func (d *Datagram) AppendHeaderTo(dst []byte) []byte {
	dst = append(dst, d.Rsv...)
	dst = append(dst, d.Frag, d.Atyp)
	dst = append(dst, d.DstAddr...)
	return append(dst, d.DstPort...)
}

// AppendTo 将完整 SOCKS5 UDP 数据报追加到目标缓冲区
func (d *Datagram) AppendTo(dst []byte) []byte {
	dst = d.AppendHeaderTo(dst)
	return append(dst, d.Data...)
}

func (d *Datagram) Bytes() []byte {
	return d.AppendTo(make([]byte, 0, d.HeaderLen()+len(d.Data)))
}
