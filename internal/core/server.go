package core

import (
	"encoding/binary"
	"errors"
	"fmt"
	"io"
	"log"
	"net"
	"slices"
	"strings"
	"sync"
	"time"

	"github.com/txthinking/runnergroup"
)

var (
	// ErrUnsupportCmd is the error when got unsupport command
	ErrUnsupportCmd = errors.New("Unsupport Command")
	// ErrUserPassAuth is the error when got invalid username or password
	ErrUserPassAuth = errors.New("Invalid Username or Password for Auth")
)

// tcpBufPool 32KB buffer for TCP copy
var tcpBufPool = sync.Pool{
	New: func() interface{} {
		return make([]byte, 32*1024)
	},
}

// udpBufPool 64KB buffer for UDP packets
var udpBufPool = sync.Pool{
	New: func() interface{} {
		return make([]byte, 65507)
	},
}

// Server is socks5 server wrapper
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
	AllowedIPs   map[string]struct{}
	AllowedCIDRs []*net.IPNet

	// UDP 并发处理通道
	udpWorkCh chan *udpTask
}

// udpTask 封装 UDP 处理任务
type udpTask struct {
	addr *net.UDPAddr
	buf  []byte
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
	allowedIPs := make(map[string]struct{})
	var allowedCIDRs []*net.IPNet

	for _, s := range whiteList {
		s = strings.TrimSpace(s)
		if s == "" {
			continue
		}
		// 尝试解析为 CIDR (e.g. 192.168.1.0/24)
		_, ipNet, err := net.ParseCIDR(s)
		if err == nil {
			allowedCIDRs = append(allowedCIDRs, ipNet)
			continue
		}
		// 尝试解析为普通 IP (e.g. 1.2.3.4)
		ip := net.ParseIP(s)
		if ip != nil {
			allowedIPs[ip.String()] = struct{}{}
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
		udpWorkCh:         make(chan *udpTask, 5000), // 缓冲区大小可调整
	}
	return s, nil
}

// IsAllowed 检查 IP 是否在白名单中
func (s *Server) IsAllowed(ip net.IP) bool {
	// 如果没有设置白名单，默认允许所有
	if len(s.AllowedIPs) == 0 && len(s.AllowedCIDRs) == 0 {
		return true
	}

	// 1. 精确匹配 (O(1))
	if _, ok := s.AllowedIPs[ip.String()]; ok {
		return true
	}

	// 2. CIDR 网段匹配 (O(N))
	for _, ipNet := range s.AllowedCIDRs {
		if ipNet.Contains(ip) {
			return true
		}
	}
	return false
}

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

func (s *Server) GetRequest(rw io.ReadWriter) (*Request, error) {
	r, err := NewRequestFrom(rw)
	if err != nil {
		return nil, err
	}
	var supported bool
	if slices.Contains(s.SupportedCommands, r.Cmd) {
		supported = true
	}
	if !supported {
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
				b := udpBufPool.Get().([]byte)
				b = b[:cap(b)] // Reset length

				n, addr, err := s.UDPConn.ReadFromUDP(b)
				if err != nil {
					udpBufPool.Put(b)
					return err
				}

				select {
				case s.udpWorkCh <- &udpTask{addr: addr, buf: b, n: n}:
				default:
					udpBufPool.Put(b)
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
func handleUDPTask(s *Server, t *udpTask) {
	defer udpBufPool.Put(t.buf)

	// 优化：UDP 包入口检查白名单
	if !s.IsAllowed(t.addr.IP) {
		if Debug {
			log.Printf("UDP Packet rejected from %s", t.addr.IP)
		}
		return
	}

	d, err := NewDatagramFromBytes(t.buf[0:t.n])
	if err != nil {
		return
	}
	if d.Frag != 0x00 {
		return
	}
	if err := s.Handle.UDPHandle(s, t.addr, d); err != nil {
		log.Println(err)
	}
}

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

func (h *DefaultHandle) TCPHandle(s *Server, c *net.TCPConn, r *Request) error {
	if r.Cmd == CmdConnect {
		rc, err := r.Connect(c)
		if err != nil {
			return err
		}
		defer rc.Close()

		// 优化：使用 io.CopyBuffer 实现零拷贝转发
		directTransfer := func(dst net.Conn, src net.Conn, timeout int) {
			buf := tcpBufPool.Get().([]byte)
			defer tcpBufPool.Put(buf)
			srcWrapped := &idleTimeoutConn{Conn: src, timeout: time.Duration(timeout) * time.Second}
			_, _ = io.CopyBuffer(dst, srcWrapped, buf)
		}

		go directTransfer(c, rc, s.TCPTimeout)
		directTransfer(rc, c, s.TCPTimeout)
		return nil
	}
	if r.Cmd == CmdUDP {
		caddr, err := r.UDP(c, s.ServerAddr)
		if err != nil {
			return err
		}
		ch := make(chan byte)
		defer close(ch)
		s.AssociatedUDP.Store(caddr.String(), ch)
		defer s.AssociatedUDP.Delete(caddr.String())
		io.Copy(io.Discard, c) // Keep TCP connection alive
		return nil
	}
	return ErrUnsupportCmd
}

func (h *DefaultHandle) UDPHandle(s *Server, addr *net.UDPAddr, d *Datagram) error {
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
		select {
		case <-ch:
			return fmt.Errorf("Association closed")
		default:
			_, err := ue.RemoteConn.Write(data)
			return err
		}
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
		b := udpBufPool.Get().([]byte)
		defer udpBufPool.Put(b)

		for {
			select {
			case <-ch:
				return
			default:
				if s.UDPTimeout != 0 {
					ue.RemoteConn.SetDeadline(time.Now().Add(time.Duration(s.UDPTimeout) * time.Second))
				}
				buf := b[:cap(b)]
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
				if _, err := s.UDPConn.WriteToUDP(d1.Bytes(), ue.ClientAddr); err != nil {
					return
				}
			}
		}
	}(ue, dst)
	return nil
}
