package core

import (
	"encoding/binary"
	"errors"
	"fmt"
	"io"
	"log"
	"net"
	"net/netip"
	"slices"
	"strings"
	"sync"
	"time"

	"github.com/txthinking/runnergroup"
)

var (
	// ErrUnsupportCmd 是收到不支持命令时的错误
	ErrUnsupportCmd = errors.New("Unsupport Command")
	// ErrUserPassAuth 是用户名或密码无效时的错误
	ErrUserPassAuth = errors.New("Invalid Username or Password for Auth")
)

const (
	tcpBufferSize          = 32 * 1024
	maxUDPPayloadSize      = 65507
	maxSOCKS5UDPHeaderSize = 262
	udpQueueCapacity       = 1024
)

type tcpBuffer [tcpBufferSize]byte
type udpBuffer [maxUDPPayloadSize + maxSOCKS5UDPHeaderSize]byte

// tcpBufPool 用于 TCP 复制的 32KB 缓冲区池
var tcpBufPool = sync.Pool{
	New: func() interface{} {
		return new(tcpBuffer)
	},
}

// udpBufPool 为最大 UDP 数据包预留 SOCKS5 头部空间
var udpBufPool = sync.Pool{
	New: func() interface{} {
		return new(udpBuffer)
	},
}

// Server 是 socks5 服务器包装器
type Server struct {
	UserName          string
	Password          string
	Method            byte
	SupportedCommands []byte
	Addr              string
	ServerAddr        net.Addr
	UDPConn           *net.UDPConn
	UDPExchanges      *sync.Map
	TCPTimeout        int
	UDPTimeout        int
	Handle            Handler
	AssociatedUDP     *sync.Map
	UDPSrc            *sync.Map
	RunnerGroup       *runnergroup.RunnerGroup
	LimitUDP          bool

	// 白名单优化：支持精确IP和CIDR网段
	AllowedIPs   map[netip.Addr]struct{}
	AllowedCIDRs []netip.Prefix

	// UDP 并发处理通道
	udpWorkCh chan udpTask
}

// udpTask 封装 UDP 处理任务
type udpTask struct {
	addr *net.UDPAddr
	buf  *udpBuffer
	n    int
}

type UDPExchange struct {
	ClientAddr *net.UDPAddr
	RemoteConn net.Conn
}

func NewClassicServer(addr, ip, username, password string, tcpTimeout, udpTimeout int, whiteList []string) (*Server, error) {
	_, p, err := net.SplitHostPort(addr)
	if err != nil {
		return nil, err
	}
	saddr, err := Resolve("udp", net.JoinHostPort(ip, p))
	if err != nil {
		return nil, err
	}
	m := MethodNone
	if username != "" && password != "" {
		m = MethodUsernamePassword
	}

	// 解析白名单：区分普通IP和CIDR网段
	allowedIPs := make(map[netip.Addr]struct{})
	var allowedCIDRs []netip.Prefix

	for _, s := range whiteList {
		s = strings.TrimSpace(s)
		if s == "" {
			continue
		}
		// 尝试解析为 CIDR（例如 192.168.1.0/24）
		prefix, err := netip.ParsePrefix(s)
		if err == nil {
			allowedCIDRs = append(allowedCIDRs, prefix.Masked())
			continue
		}
		// 尝试解析为普通 IP（例如 1.2.3.4）
		ip, err := netip.ParseAddr(s)
		if err == nil {
			allowedIPs[ip.Unmap()] = struct{}{}
			continue
		}
		log.Printf("Warning: Invalid whitelist entry skipped: %s", s)
	}

	s := &Server{
		Method:            m,
		UserName:          username,
		Password:          password,
		SupportedCommands: []byte{CmdConnect, CmdUDP},
		Addr:              addr,
		ServerAddr:        saddr,
		UDPExchanges:      &sync.Map{},
		TCPTimeout:        tcpTimeout,
		UDPTimeout:        udpTimeout,
		AssociatedUDP:     &sync.Map{},
		UDPSrc:            &sync.Map{},
		RunnerGroup:       runnergroup.New(),
		AllowedIPs:        allowedIPs,
		AllowedCIDRs:      allowedCIDRs,
		udpWorkCh:         make(chan udpTask, udpQueueCapacity),
	}
	return s, nil
}

// IsAllowed 检查 IP 是否在白名单中
func (s *Server) IsAllowed(ip net.IP) bool {
	// 如果没有设置白名单，默认允许所有
	if len(s.AllowedIPs) == 0 && len(s.AllowedCIDRs) == 0 {
		return true
	}

	addr, ok := netip.AddrFromSlice(ip)
	if !ok {
		return false
	}
	addr = addr.Unmap()

	// 1. 精确匹配（O(1)）
	if _, ok := s.AllowedIPs[addr]; ok {
		return true
	}

	// 2. CIDR 网段匹配（O(N)）
	for _, prefix := range s.AllowedCIDRs {
		if prefix.Contains(addr) {
			return true
		}
	}
	return false
}

// Negotiate 执行 SOCKS5 协商流程，支持匿名和用户名密码认证
func (s *Server) Negotiate(rw io.ReadWriter) error {
	rq, err := NewNegotiationRequestFrom(rw)
	if err != nil {
		return err
	}
	var got bool
	var m byte
	for _, m = range rq.Methods {
		if m == s.Method {
			got = true
		}
	}
	if !got {
		rp := NewNegotiationReply(MethodUnsupportAll)
		if _, err := rp.WriteTo(rw); err != nil {
			return err
		}
		return errors.New("no acceptable authentication method")
	}
	rp := NewNegotiationReply(s.Method)
	if _, err := rp.WriteTo(rw); err != nil {
		return err
	}

	if s.Method == MethodUsernamePassword {
		urq, err := NewUserPassNegotiationRequestFrom(rw)
		if err != nil {
			return err
		}
		if string(urq.Uname) != s.UserName || string(urq.Passwd) != s.Password {
			urp := NewUserPassNegotiationReply(UserPassStatusFailure)
			if _, err := urp.WriteTo(rw); err != nil {
				return err
			}
			return ErrUserPassAuth
		}
		urp := NewUserPassNegotiationReply(UserPassStatusSuccess)
		if _, err := urp.WriteTo(rw); err != nil {
			return err
		}
	}
	return nil
}

// GetRequest 获取客户端发送的 SOCKS5 请求包，并验证命令是否受支持
func (s *Server) GetRequest(rw io.ReadWriter) (*Request, error) {
	r, err := NewRequestFrom(rw)
	if err != nil {
		return nil, err
	}
	if !slices.Contains(s.SupportedCommands, r.Cmd) {
		var p *Reply
		if r.Atyp == ATYPIPv4 || r.Atyp == ATYPDomain {
			p = NewReply(RepCommandNotSupported, ATYPIPv4, []byte{0x00, 0x00, 0x00, 0x00}, []byte{0x00, 0x00})
		} else {
			p = NewReply(RepCommandNotSupported, ATYPIPv6, []byte(net.IPv6zero), []byte{0x00, 0x00})
		}
		if _, err := p.WriteTo(rw); err != nil {
			return nil, err
		}
		return nil, ErrUnsupportCmd
	}
	return r, nil
}

// ListenAndServe 同时启动 TCP 和 UDP 数据包接收循环
func (s *Server) ListenAndServe(h Handler) error {
	if h == nil {
		s.Handle = &DefaultHandle{}
	} else {
		s.Handle = h
	}
	addr, err := net.ResolveTCPAddr("tcp", s.Addr)
	if err != nil {
		return err
	}
	l, err := net.ListenTCP("tcp", addr)
	if err != nil {
		return err
	}
	s.RunnerGroup.Add(&runnergroup.Runner{
		Start: func() error {
			for {
				c, err := l.AcceptTCP()
				if err != nil {
					return err
				}
				go func(c *net.TCPConn) {
					defer c.Close()
					// 优化：TCP 连接入口检查白名单
					clientIP := c.RemoteAddr().(*net.TCPAddr).IP
					if !s.IsAllowed(clientIP) {
						log.Printf("TCP Connection rejected from %s (not in whitelist)", clientIP)
						return
					}

					if err := s.Negotiate(c); err != nil {
						return
					}
					r, err := s.GetRequest(c)
					if err != nil {
						log.Println(err)
						return
					}
					if err := s.Handle.TCPHandle(s, c, r); err != nil {
						log.Println(err)
					}
				}(c)
			}
		},
		Stop: func() error {
			return l.Close()
		},
	})

	addr1, err := net.ResolveUDPAddr("udp", s.Addr)
	if err != nil {
		l.Close()
		return err
	}
	s.UDPConn, err = net.ListenUDP("udp", addr1)
	if err != nil {
		l.Close()
		return err
	}

	// 优化：启动 UDP Worker Pool (128个并发)
	numWorkers := 128
	for i := 0; i < numWorkers; i++ {
		go func() {
			for task := range s.udpWorkCh {
				handleUDPTask(s, task)
			}
		}()
	}

	s.RunnerGroup.Add(&runnergroup.Runner{
		Start: func() error {
			for {
				buffer := udpBufPool.Get().(*udpBuffer)
				b := buffer[:maxUDPPayloadSize]

				n, addr, err := s.UDPConn.ReadFromUDP(b)
				if err != nil {
					udpBufPool.Put(buffer)
					return err
				}

				select {
				case s.udpWorkCh <- udpTask{addr: addr, buf: buffer, n: n}:
				default:
					udpBufPool.Put(buffer)
					if Debug {
						log.Println("UDP worker queue full, dropping packet")
					}
				}
			}
		},
		Stop: func() error {
			close(s.udpWorkCh)
			return s.UDPConn.Close()
		},
	})
	return s.RunnerGroup.Wait()
}

// handleUDPTask 处理单个 UDP 任务
func handleUDPTask(s *Server, t udpTask) {
	defer udpBufPool.Put(t.buf)

	// 优化：UDP 包入口检查白名单
	if !s.IsAllowed(t.addr.IP) {
		if Debug {
			log.Printf("UDP Packet rejected from %s", t.addr.IP)
		}
		return
	}

	if h, ok := s.Handle.(*DefaultHandle); ok {
		d, err := ParseDatagram(t.buf[0:t.n])
		if err != nil || d.Frag != 0x00 {
			return
		}
		if err := h.handleUDP(s, t.addr, d); err != nil {
			log.Println(err)
		}
		return
	}

	d, err := NewDatagramFromBytes(t.buf[0:t.n])
	if err != nil || d.Frag != 0x00 {
		return
	}
	if err := s.Handle.UDPHandle(s, t.addr, d); err != nil {
		log.Println(err)
	}
}

// Shutdown 优雅关闭服务器，等待所有活跃连接和 Runner 停止
func (s *Server) Shutdown() error {
	return s.RunnerGroup.Done()
}

type Handler interface {
	TCPHandle(*Server, *net.TCPConn, *Request) error
	UDPHandle(*Server, *net.UDPAddr, *Datagram) error
}

type DefaultHandle struct {
}

// idleTimeoutConn 包装连接以支持 io.CopyBuffer
type idleTimeoutConn struct {
	net.Conn
	timeout time.Duration
}

func (c *idleTimeoutConn) Read(b []byte) (int, error) {
	if c.timeout > 0 {
		if err := c.Conn.SetReadDeadline(time.Now().Add(c.timeout)); err != nil {
			return 0, err
		}
	}
	return c.Conn.Read(b)
}

// TCPHandle 处理 TCP 协议相关的 CONNECT 和 UDP ASSOCIATE 控制连接
func (h *DefaultHandle) TCPHandle(s *Server, c *net.TCPConn, r *Request) error {
	switch r.Cmd {
	case CmdConnect:
		rc, err := r.Connect(c)
		if err != nil {
			return err
		}
		defer rc.Close()

		// 优化：使用 io.CopyBuffer 实现零拷贝转发
		directTransfer := func(dst net.Conn, src net.Conn, timeout int) {
			buf := tcpBufPool.Get().(*tcpBuffer)
			defer tcpBufPool.Put(buf)
			srcWrapped := &idleTimeoutConn{Conn: src, timeout: time.Duration(timeout) * time.Second}
			_, _ = io.CopyBuffer(dst, srcWrapped, buf[:])
		}

		go directTransfer(c, rc, s.TCPTimeout)
		directTransfer(rc, c, s.TCPTimeout)
		return nil
	case CmdUDP:
		caddr, err := r.UDP(c, s.ServerAddr)
		if err != nil {
			return err
		}
		ch := make(chan byte)
		defer close(ch)
		s.AssociatedUDP.Store(caddr.String(), ch)
		defer s.AssociatedUDP.Delete(caddr.String())
		io.Copy(io.Discard, c) // 保持 TCP 连接活跃
		return nil
	default:
		return ErrUnsupportCmd
	}
}

// UDPHandle 处理 UDP 数据报转发逻辑
func (h *DefaultHandle) UDPHandle(s *Server, addr *net.UDPAddr, d *Datagram) error {
	return h.handleUDP(s, addr, *d)
}

// handleUDP 使用值类型数据报执行默认转发路径
func (h *DefaultHandle) handleUDP(s *Server, addr *net.UDPAddr, d Datagram) error {
	src := addr.String()
	var ch chan byte
	if s.LimitUDP {
		any, ok := s.AssociatedUDP.Load(src)
		if !ok {
			return fmt.Errorf("Address %s not associated", src)
		}
		ch = any.(chan byte)
	}

	send := func(ue *UDPExchange, data []byte) error {
		if ch != nil {
			select {
			case <-ch:
				return fmt.Errorf("Association closed")
			default:
			}
		}
		_, err := ue.RemoteConn.Write(data)
		return err
	}

	dst := d.Address()
	if any, ok := s.UDPExchanges.Load(src + dst); ok {
		ue := any.(*UDPExchange)
		return send(ue, d.Data)
	}

	var laddr string
	if any, ok := s.UDPSrc.Load(src + dst); ok {
		laddr = any.(string)
	}
	rc, err := DialUDP("udp", laddr, dst)
	if err != nil {
		rc, err = DialUDP("udp", "", dst)
		if err != nil {
			return err
		}
		laddr = ""
	}
	if laddr == "" {
		s.UDPSrc.Store(src+dst, rc.LocalAddr().String())
	}

	ue := &UDPExchange{
		ClientAddr: addr,
		RemoteConn: rc,
	}

	if err := send(ue, d.Data); err != nil {
		ue.RemoteConn.Close()
		return err
	}
	s.UDPExchanges.Store(src+dst, ue)

	go func(ue *UDPExchange, dst string) {
		defer func() {
			ue.RemoteConn.Close()
			s.UDPExchanges.Delete(ue.ClientAddr.String() + dst)
		}()
		buffer := udpBufPool.Get().(*udpBuffer)
		defer udpBufPool.Put(buffer)
		b := buffer[:]

		for {
			if ch != nil {
				select {
				case <-ch:
					return
				default:
				}
			}
			if s.UDPTimeout != 0 {
				ue.RemoteConn.SetDeadline(time.Now().Add(time.Duration(s.UDPTimeout) * time.Second))
			}
			buf := b[maxSOCKS5UDPHeaderSize : maxSOCKS5UDPHeaderSize+maxUDPPayloadSize]
			n, err := ue.RemoteConn.Read(buf)
			if err != nil {
				return
			}

			// 优化：从 RemoteAddr 直接获取 IP/Port，避免 ParseAddress
			var a byte
			var addr, port []byte

			if udpAddr, ok := ue.RemoteConn.RemoteAddr().(*net.UDPAddr); ok {
				if ip4 := udpAddr.IP.To4(); ip4 != nil {
					a = ATYPIPv4
					addr = ip4
				} else {
					a = ATYPIPv6
					addr = udpAddr.IP
				}
				port = make([]byte, 2)
				binary.BigEndian.PutUint16(port, uint16(udpAddr.Port))
			} else {
				var err error
				a, addr, port, err = ParseAddress(dst)
				if err != nil {
					log.Println(err)
					return
				}
				if a == ATYPDomain {
					addr = addr[1:]
				}
			}

			d1 := NewDatagram(a, addr, port, buf[0:n])
			packetStart := maxSOCKS5UDPHeaderSize - d1.HeaderLen()
			packet := d1.AppendHeaderTo(b[packetStart:packetStart])
			packet = append(packet, buf[0:n]...)
			if _, err := s.UDPConn.WriteToUDP(packet, ue.ClientAddr); err != nil {
				return
			}
		}
	}(ue, dst)
	return nil
}
