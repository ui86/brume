package app

import (
	"brume/internal/core"
	"log"
	"net"
	"os"
	"os/signal"
	"strconv"
	"strings"
	"syscall"
)

// Config 聚合所有配置项
type Config struct {
	Port       int
	Username   string
	Password   string
	Whitelist  string
	TCPTimeout int
	UDPTimeout int
}

// DefaultConfig 返回默认配置
func DefaultConfig() *Config {
	return &Config{
		Port:       1080,
		UDPTimeout: 60,
		TCPTimeout: 0, // 0 表示无超时
	}
}

// App 封装应用实例
type App struct {
	Config *Config
	Server *core.Server
}

// New 创建应用实例
func New(cfg *Config) *App {
	return &App{
		Config: cfg,
	}
}

// Run 启动应用
func (a *App) Run() {
	log.Println("Welcome use socks5 server")

	// 2. 参数校验
	if err := a.validate(); err != nil {
		log.Fatalf("Config error: %v", err)
	}

	// 3. 解析监听地址
	serverAddr := a.resolveAddr()

	// 4. 解析白名单
	whitelist := a.parseWhitelist()
	if len(whitelist) == 0 {
		log.Println("Warning: whitelist is empty, all IPs are allowed")
	} else {
		log.Printf("Whitelist: %v\n", whitelist)
	}

	// 5. 初始化 Server 实例
	var err error
	a.Server, err = core.NewClassicServer(
		serverAddr.String(),
		"0.0.0.0",
		a.Config.Username,
		a.Config.Password,
		a.Config.TCPTimeout,
		a.Config.UDPTimeout,
		whitelist,
	)
	if err != nil {
		log.Fatalf("Failed to create server: %v", err)
	}

	log.Printf("Server is listening on %s\n", serverAddr.String())

	// 6. 监听系统信号实现优雅关闭
	go a.handleSignals()

	// 7. 启动服务 (阻塞直到出错)
	if err := a.Server.ListenAndServe(nil); err != nil {
		log.Fatalf("Server error: %v", err)
	}
}

// validate 验证配置合法性
func (a *App) validate() error {
	if a.Config.Port <= 0 || a.Config.Port > 65535 {
		return net.InvalidAddrError("Port must be between 1 and 65535")
	}
	return nil
}

// resolveAddr 解析 TCP 地址
func (a *App) resolveAddr() *net.TCPAddr {
	addr, err := net.ResolveTCPAddr("tcp", ":"+strconv.Itoa(a.Config.Port))
	if err != nil {
		log.Fatalf("Invalid address: %v", err)
	}
	return addr
}

// parseWhitelist 处理白名单字符串
func (a *App) parseWhitelist() []string {
	var ips []string
	if a.Config.Whitelist != "" {
		parts := strings.SplitSeq(a.Config.Whitelist, ",")
		for p := range parts {
			if s := strings.TrimSpace(p); s != "" {
				ips = append(ips, s)
			}
		}
	}
	return ips
}

// handleSignals 捕获 Ctrl+C 或 Kill 信号
func (a *App) handleSignals() {
	c := make(chan os.Signal, 1)
	signal.Notify(c, os.Interrupt, syscall.SIGTERM)

	// 阻塞直到收到信号
	sig := <-c
	log.Printf("Received signal: %v. Shutting down...", sig)

	if err := a.Server.Shutdown(); err != nil {
		log.Printf("Shutdown error: %v", err)
	} else {
		log.Println("Server stopped gracefully.")
	}
	os.Exit(0)
}
