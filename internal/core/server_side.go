package core

import (
	"errors"
	"io"
	"log"
)

var (
	ErrVersion         = errors.New("Invalid Version")
	ErrUserPassVersion = errors.New("Invalid Version of Username Password Auth")
	ErrBadRequest      = errors.New("Bad Request")
)

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

func NewNegotiationReply(method byte) *NegotiationReply {
	return &NegotiationReply{
		Ver:    Ver,
		Method: method,
	}
}

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

func NewUserPassNegotiationReply(status byte) *UserPassNegotiationReply {
	return &UserPassNegotiationReply{
		Ver:    UserPassVer,
		Status: status,
	}
}

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

func NewRequestFrom(r io.Reader) (*Request, error) {
	var bb [4]byte // 优化
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
		var dal [1]byte
		if _, err := io.ReadFull(r, dal[:]); err != nil {
			return nil, err
		}
		if dal[0] == 0 {
			return nil, ErrBadRequest
		}
		addr = make([]byte, int(dal[0]))
		if _, err := io.ReadFull(r, addr); err != nil {
			return nil, err
		}
		addr = append(dal[:], addr...)
	default:
		return nil, ErrBadRequest
	}
	port := make([]byte, 2)
	if _, err := io.ReadFull(r, port); err != nil {
		return nil, err
	}
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

func (r *Reply) WriteTo(w io.Writer) (int64, error) {
	i, err := w.Write(append(append([]byte{r.Ver, r.Rep, r.Rsv, r.Atyp}, r.BndAddr...), r.BndPort...))
	if err != nil {
		return 0, err
	}
	if Debug {
		log.Printf("Sent Reply: %#v %#v %#v %#v %#v %#v\n", r.Ver, r.Rep, r.Rsv, r.Atyp, r.BndAddr, r.BndPort)
	}
	return int64(i), nil
}

func NewDatagramFromBytes(bb []byte) (*Datagram, error) {
	n := len(bb)
	minl := 4
	if n < minl {
		return nil, ErrBadRequest
	}
	var addr []byte
	switch bb[3] {
	case ATYPIPv4:
		minl += 4
		if n < minl {
			return nil, ErrBadRequest
		}
		addr = bb[minl-4 : minl]
	case ATYPIPv6:
		minl += 16
		if n < minl {
			return nil, ErrBadRequest
		}
		addr = bb[minl-16 : minl]
	case ATYPDomain:
		minl += 1
		if n < minl {
			return nil, ErrBadRequest
		}
		l := bb[4]
		if l == 0 {
			return nil, ErrBadRequest
		}
		minl += int(l)
		if n < minl {
			return nil, ErrBadRequest
		}
		addr = bb[minl-int(l) : minl]
		addr = append([]byte{l}, addr...)
	default:
		return nil, ErrBadRequest
	}
	minl += 2
	if n <= minl {
		return nil, ErrBadRequest
	}
	port := bb[minl-2 : minl]
	data := bb[minl:]
	d := &Datagram{
		Rsv:     bb[0:2],
		Frag:    bb[2],
		Atyp:    bb[3],
		DstAddr: addr,
		DstPort: port,
		Data:    data,
	}
	return d, nil
}

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

func (d *Datagram) Bytes() []byte {
	b := make([]byte, 0, len(d.Rsv)+1+1+len(d.DstAddr)+len(d.DstPort)+len(d.Data))
	b = append(b, d.Rsv...)
	b = append(b, d.Frag)
	b = append(b, d.Atyp)
	b = append(b, d.DstAddr...)
	b = append(b, d.DstPort...)
	b = append(b, d.Data...)
	return b
}
