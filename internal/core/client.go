package core

import (
	"errors"
	"net"
	"time"
)

// Client 是 socks5 客户端包装器
type Client struct {
	Server   string
	UserName string
	Password string
	// 对于 UDP 命令，让服务器控制 TCP 和 UDP 连接的关系
	TCPConn       net.Conn
	UDPConn       net.Conn
	RemoteAddress net.Addr
	TCPTimeout    int
	UDPTimeout    int
	Dst           string

	// 缓存目标地址解析结果，避免每次 Write 都重复解析
	dstAtyp byte
	dstAddr []byte
	dstPort []byte
}

// 这只是创建一个客户端，你需要使用 Dial 来创建连接
func NewClient(addr, username, password string, tcpTimeout, udpTimeout int) (*Client, error) {
	c := &Client{
		Server:     addr,
		UserName:   username,
		Password:   password,
		TCPTimeout: tcpTimeout,
		UDPTimeout: udpTimeout,
	}
	return c, nil
}

// Dial 拨号连接到目标地址，自动处理 TCP/UDP 协议协商
func (c *Client) Dial(network, addr string) (net.Conn, error) {
	return c.DialWithLocalAddr(network, "", addr, nil)
}

// 如果你想发送期望用于发送 UDP 的地址，只需将其分配给 src，否则它将发送零地址。
// 建议在非 NAT 环境中指定 src 地址，在其他情况下保持为空。
// DialWithLocalAddr 允许指定本地地址进行拨号，适用于多网卡环境
func (c *Client) DialWithLocalAddr(network, src, dst string, remoteAddr net.Addr) (net.Conn, error) {
	c = &Client{
		Server:        c.Server,
		UserName:      c.UserName,
		Password:      c.Password,
		TCPTimeout:    c.TCPTimeout,
		UDPTimeout:    c.UDPTimeout,
		Dst:           dst,
		RemoteAddress: remoteAddr,
	}
	var err error
	if network == "tcp" {
		var laddr net.Addr
		if src != "" {
			laddr, err = net.ResolveTCPAddr("tcp", src)
			if err != nil {
				return nil, err
			}
		}
		if err := c.Negotiate(laddr); err != nil {
			return nil, err
		}
		a, h, p, err := ParseAddress(dst)
		if err != nil {
			return nil, err
		}
		if a == ATYPDomain {
			h = h[1:]
		}
		if _, err := c.Request(NewRequest(CmdConnect, a, h, p)); err != nil {
			return nil, err
		}
		return c, nil
	}
	if network == "udp" {
		var laddr net.Addr
		if src != "" {
			laddr, err = net.ResolveTCPAddr("tcp", src)
			if err != nil {
				return nil, err
			}
		}
		if err := c.Negotiate(laddr); err != nil {
			return nil, err
		}

		a, h, p := ATYPIPv4, net.IPv4zero, []byte{0x00, 0x00}
		if src != "" {
			a, h, p, err = ParseAddress(src)
			if err != nil {
				return nil, err
			}
			if a == ATYPDomain {
				h = h[1:]
			}
		}
		rp, err := c.Request(NewRequest(CmdUDP, a, h, p))
		if err != nil {
			return nil, err
		}
		c.UDPConn, err = DialUDP("udp", src, rp.Address())
		if err != nil {
			return nil, err
		}
		if c.UDPTimeout != 0 {
			if err := c.UDPConn.SetDeadline(time.Now().Add(time.Duration(c.UDPTimeout) * time.Second)); err != nil {
				return nil, err
			}
		}
		// 缓存目标地址解析结果
		if dst != "" {
			a, h, p, err := ParseAddress(dst)
			if err != nil {
				return nil, err
			}
			if a == ATYPDomain {
				h = h[1:]
			}
			c.dstAtyp = a
			c.dstAddr = h
			c.dstPort = p
		}
		return c, nil
	}
	return nil, errors.New("unsupport network")
}

// Read 从连接读取数据，如果是 UDP 则会自动剥离 SOCKS5 数据报头
func (c *Client) Read(b []byte) (int, error) {
	if c.UDPConn == nil {
		return c.TCPConn.Read(b)
	}
	n, err := c.UDPConn.Read(b)
	if err != nil {
		return 0, err
	}
	d, err := NewDatagramFromBytes(b[0:n])
	if err != nil {
		return 0, err
	}
	n = copy(b, d.Data)
	return n, nil
}

// Write 向连接写入数据，如果是 UDP 则会自动封装 SOCKS5 数据报头
func (c *Client) Write(b []byte) (int, error) {
	if c.UDPConn == nil {
		return c.TCPConn.Write(b)
	}
	d := NewDatagram(c.dstAtyp, c.dstAddr, c.dstPort, b)
	b1 := d.Bytes()
	n, err := c.UDPConn.Write(b1)
	if err != nil {
		return 0, err
	}
	if len(b1) != n {
		return 0, errors.New("not write full")
	}
	return len(b), nil
}

// Close 关闭 TCP 和可能的 UDP 连接
func (c *Client) Close() error {
	if c.UDPConn == nil {
		if c.TCPConn == nil {
			return nil
		}
		return c.TCPConn.Close()
	}
	if c.TCPConn != nil {
		c.TCPConn.Close()
	}
	return c.UDPConn.Close()
}

func (c *Client) LocalAddr() net.Addr {
	if c.UDPConn == nil {
		return c.TCPConn.LocalAddr()
	}
	return c.UDPConn.LocalAddr()
}

func (c *Client) RemoteAddr() net.Addr {
	return c.RemoteAddress
}

func (c *Client) SetDeadline(t time.Time) error {
	if c.UDPConn == nil {
		return c.TCPConn.SetDeadline(t)
	}
	return c.UDPConn.SetDeadline(t)
}

func (c *Client) SetReadDeadline(t time.Time) error {
	if c.UDPConn == nil {
		return c.TCPConn.SetReadDeadline(t)
	}
	return c.UDPConn.SetReadDeadline(t)
}

func (c *Client) SetWriteDeadline(t time.Time) error {
	if c.UDPConn == nil {
		return c.TCPConn.SetWriteDeadline(t)
	}
	return c.UDPConn.SetWriteDeadline(t)
}

// Negotiate 执行 SOCKS5 握手阶段，包括版本协商和用户名密码认证
func (c *Client) Negotiate(laddr net.Addr) error {
	src := ""
	if laddr != nil {
		src = laddr.String()
	}
	var err error
	c.TCPConn, err = DialTCP("tcp", src, c.Server)
	if err != nil {
		return err
	}
	if c.TCPTimeout != 0 {
		if err := c.TCPConn.SetDeadline(time.Now().Add(time.Duration(c.TCPTimeout) * time.Second)); err != nil {
			return err
		}
	}
	m := MethodNone
	if c.UserName != "" && c.Password != "" {
		m = MethodUsernamePassword
	}
	rq := NewNegotiationRequest([]byte{m})
	if _, err := rq.WriteTo(c.TCPConn); err != nil {
		return err
	}
	rp, err := NewNegotiationReplyFrom(c.TCPConn)
	if err != nil {
		return err
	}
	if rp.Method != m {
		return errors.New("Unsupport method")
	}
	if m == MethodUsernamePassword {
		urq := NewUserPassNegotiationRequest([]byte(c.UserName), []byte(c.Password))
		if _, err := urq.WriteTo(c.TCPConn); err != nil {
			return err
		}
		urp, err := NewUserPassNegotiationReplyFrom(c.TCPConn)
		if err != nil {
			return err
		}
		if urp.Status != UserPassStatusSuccess {
			return ErrUserPassAuth
		}
	}
	return nil
}

// Request 发送 SOCKS5 请求包（如 CONNECT/UDP）并接收服务器回复
func (c *Client) Request(r *Request) (*Reply, error) {
	if _, err := r.WriteTo(c.TCPConn); err != nil {
		return nil, err
	}
	rp, err := NewReplyFrom(c.TCPConn)
	if err != nil {
		return nil, err
	}
	if rp.Rep != RepSuccess {
		return nil, errors.New("Host unreachable")
	}
	return rp, nil
}
