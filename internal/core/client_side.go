package core

import (
	"errors"
	"io"
	"log"
)

var (
	// ErrBadReply 是读取回复时的错误
	ErrBadReply = errors.New("Bad Reply")
)

// NewNegotiationRequest 返回可以写入服务器的协商请求包
func NewNegotiationRequest(methods []byte) *NegotiationRequest {
	return &NegotiationRequest{
		Ver:      Ver,
		NMethods: byte(len(methods)),
		Methods:  methods,
	}
}

// WriteTo 将协商请求包写入服务器
func (r *NegotiationRequest) WriteTo(w io.Writer) (int64, error) {
	buf := make([]byte, 0, 2+len(r.Methods))
	buf = append(buf, r.Ver, r.NMethods)
	buf = append(buf, r.Methods...)
	i, err := w.Write(buf)
	if err != nil {
		return 0, err
	}
	if Debug {
		log.Printf("Sent NegotiationRequest: %#v %#v %#v\n", r.Ver, r.NMethods, r.Methods)
	}
	return int64(i), nil
}

// NewNegotiationReplyFrom 从服务器读取协商回复包
func NewNegotiationReplyFrom(r io.Reader) (*NegotiationReply, error) {
	// 优化 3: 使用栈内存
	var bb [2]byte
	if _, err := io.ReadFull(r, bb[:]); err != nil {
		return nil, err
	}
	if bb[0] != Ver {
		return nil, ErrVersion
	}
	if Debug {
		log.Printf("Got NegotiationReply: %#v %#v\n", bb[0], bb[1])
	}
	return &NegotiationReply{
		Ver:    bb[0],
		Method: bb[1],
	}, nil
}

// NewUserPassNegotiationRequest 返回可以写入服务器的用户名密码协商请求包函数
func NewUserPassNegotiationRequest(username []byte, password []byte) *UserPassNegotiationRequest {
	return &UserPassNegotiationRequest{
		Ver:    UserPassVer,
		Ulen:   byte(len(username)),
		Uname:  username,
		Plen:   byte(len(password)),
		Passwd: password,
	}
}

// WriteTo 将用户名密码协商请求包写入服务器
func (r *UserPassNegotiationRequest) WriteTo(w io.Writer) (int64, error) {
	buf := make([]byte, 0, 3+len(r.Uname)+len(r.Passwd))
	buf = append(buf, r.Ver, r.Ulen)
	buf = append(buf, r.Uname...)
	buf = append(buf, r.Plen)
	buf = append(buf, r.Passwd...)
	i, err := w.Write(buf)
	if err != nil {
		return 0, err
	}
	if Debug {
		log.Printf("Sent UserNameNegotiationRequest: %#v %#v %#v %#v %#v\n", r.Ver, r.Ulen, r.Uname, r.Plen, r.Passwd)
	}
	return int64(i), nil
}

// NewUserPassNegotiationReplyFrom 从服务器读取用户名密码协商回复包
func NewUserPassNegotiationReplyFrom(r io.Reader) (*UserPassNegotiationReply, error) {
	// 优化 4: 使用栈内存
	var bb [2]byte
	if _, err := io.ReadFull(r, bb[:]); err != nil {
		return nil, err
	}
	if bb[0] != UserPassVer {
		return nil, ErrUserPassVersion
	}
	if Debug {
		log.Printf("Got UserPassNegotiationReply: %#v %#v \n", bb[0], bb[1])
	}
	return &UserPassNegotiationReply{
		Ver:    bb[0],
		Status: bb[1],
	}, nil
}

// NewRequest 返回可以写入服务器的请求包，dstaddr 不应包含域名长度
func NewRequest(cmd byte, atyp byte, dstaddr []byte, dstport []byte) *Request {
	if atyp == ATYPDomain {
		dstaddr = append([]byte{byte(len(dstaddr))}, dstaddr...)
	}
	return &Request{
		Ver:     Ver,
		Cmd:     cmd,
		Rsv:     0x00,
		Atyp:    atyp,
		DstAddr: dstaddr,
		DstPort: dstport,
	}
}

// WriteTo 将请求包写入服务器
func (r *Request) WriteTo(w io.Writer) (int64, error) {
	buf := make([]byte, 0, 4+len(r.DstAddr)+len(r.DstPort))
	buf = append(buf, r.Ver, r.Cmd, r.Rsv, r.Atyp)
	buf = append(buf, r.DstAddr...)
	buf = append(buf, r.DstPort...)
	i, err := w.Write(buf)
	if err != nil {
		return 0, err
	}
	if Debug {
		log.Printf("Sent Request: %#v %#v %#v %#v %#v %#v\n", r.Ver, r.Cmd, r.Rsv, r.Atyp, r.DstAddr, r.DstPort)
	}
	return int64(i), nil
}

// NewReplyFrom 从服务器读取回复包
func NewReplyFrom(r io.Reader) (*Reply, error) {
	// 优化 5: 使用栈内存
	var bb [4]byte
	if _, err := io.ReadFull(r, bb[:]); err != nil {
		return nil, err
	}
	if bb[0] != Ver {
		return nil, ErrVersion
	}
	var addr []byte
	switch bb[3] {
	case ATYPIPv4:
		addr = make([]byte, 4)
		if _, err := io.ReadFull(r, addr); err != nil {
			return nil, err
		}
	case ATYPIPv6:
		addr = make([]byte, 16)
		if _, err := io.ReadFull(r, addr); err != nil {
			return nil, err
		}
	case ATYPDomain:
		var dal [1]byte // 优化
		if _, err := io.ReadFull(r, dal[:]); err != nil {
			return nil, err
		}
		if dal[0] == 0 {
			return nil, ErrBadReply
		}
		addr = make([]byte, int(dal[0]))
		if _, err := io.ReadFull(r, addr); err != nil {
			return nil, err
		}
		addr = append(dal[:], addr...)
	default:
		return nil, ErrBadReply
	}
	port := make([]byte, 2)
	if _, err := io.ReadFull(r, port); err != nil {
		return nil, err
	}
	if Debug {
		log.Printf("Got Reply: %#v %#v %#v %#v %#v %#v\n", bb[0], bb[1], bb[2], bb[3], addr, port)
	}
	return &Reply{
		Ver:     bb[0],
		Rep:     bb[1],
		Rsv:     bb[2],
		Atyp:    bb[3],
		BndAddr: addr,
		BndPort: port,
	}, nil
}
