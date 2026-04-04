package core

const (
	// Ver 是 socks 协议版本
	Ver byte = 0x05

	// MethodNone 是无认证方法
	MethodNone byte = 0x00
	// MethodGSSAPI 是 GSSAPI 方法
	MethodGSSAPI byte = 0x01 // 必须支持 // todo
	// MethodUsernamePassword 是用户名/密码认证方法
	MethodUsernamePassword byte = 0x02 // 应该支持
	// MethodUnsupportAll 表示不支持所有给定的方法
	MethodUnsupportAll byte = 0xFF

	// UserPassVer 是用户名/密码认证协议版本
	UserPassVer byte = 0x01
	// UserPassStatusSuccess 是用户名/密码认证的成功状态
	UserPassStatusSuccess byte = 0x00
	// UserPassStatusFailure 是用户名/密码认证的失败状态
	UserPassStatusFailure byte = 0x01 // 只要不是 0x00 即可

	// CmdConnect 是连接命令
	CmdConnect byte = 0x01
	// CmdBind 是绑定命令
	CmdBind byte = 0x02
	// CmdUDP 是 UDP 命令
	CmdUDP byte = 0x03

	// ATYPIPv4 是 IPv4 地址类型
	ATYPIPv4 byte = 0x01 // 4 字节
	// ATYPDomain 是域名地址类型
	ATYPDomain byte = 0x03 // 地址字段的第一个八位字节包含随后的名称八位字节数，没有终止 NUL 八位字节。
	// ATYPIPv6 是 IPv6 地址类型
	ATYPIPv6 byte = 0x04 // 16 字节

	// RepSuccess 表示回复成功
	RepSuccess byte = 0x00
	// RepServerFailure 表示服务器故障
	RepServerFailure byte = 0x01
	// RepNotAllowed 表示请求不允许
	RepNotAllowed byte = 0x02
	// RepNetworkUnreachable 表示网络不可达
	RepNetworkUnreachable byte = 0x03
	// RepHostUnreachable 表示主机不可达
	RepHostUnreachable byte = 0x04
	// RepConnectionRefused 表示连接被拒绝
	RepConnectionRefused byte = 0x05
	// RepTTLExpired 表示 TTL 过期
	RepTTLExpired byte = 0x06
	// RepCommandNotSupported 表示请求命令不支持
	RepCommandNotSupported byte = 0x07
	// RepAddressNotSupported 表示请求地址不支持
	RepAddressNotSupported byte = 0x08
)

// NegotiationRequest 是协商请求包
type NegotiationRequest struct {
	Ver      byte
	NMethods byte
	Methods  []byte // 1-255 字节
}

// NegotiationReply 是协商回复包
type NegotiationReply struct {
	Ver    byte
	Method byte
}

// UserPassNegotiationRequest 是用户名/密码协商请求包
type UserPassNegotiationRequest struct {
	Ver    byte
	Ulen   byte
	Uname  []byte // 1-255 字节
	Plen   byte
	Passwd []byte // 1-255 字节
}

// UserPassNegotiationReply 是用户名/密码协商回复包
type UserPassNegotiationReply struct {
	Ver    byte
	Status byte
}

// Request 是请求包
type Request struct {
	Ver     byte
	Cmd     byte
	Rsv     byte // 0x00
	Atyp    byte
	DstAddr []byte
	DstPort []byte // 2 字节
}

// Reply 是回复包
type Reply struct {
	Ver  byte
	Rep  byte
	Rsv  byte // 0x00
	Atyp byte
	// CONNECT：用于连接到目标地址的 socks 服务器地址
	// BIND ...
	// UDP：用于连接到目标地址的 socks 服务器地址
	BndAddr []byte
	// CONNECT：用于连接到目标地址的 socks 服务器端口
	// BIND ...
	// UDP：用于连接到目标地址的 socks 服务器端口
	BndPort []byte // 2 字节
}

// Datagram 是 UDP 数据报
type Datagram struct {
	Rsv     []byte // 0x00 0x00
	Frag    byte
	Atyp    byte
	DstAddr []byte
	DstPort []byte // 2 字节
	Data    []byte
}
