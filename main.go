package main

import (
	"brume/app"
	"flag"
)

func main() {
	// 1. 初始化默认配置
	cfg := app.DefaultConfig()

	// 2. 绑定命令行参数
	flag.StringVar(&cfg.Username, "user", "", "username")
	flag.StringVar(&cfg.Password, "pwd", "", "password")
	flag.IntVar(&cfg.Port, "p", cfg.Port, "port on listen")
	flag.StringVar(&cfg.Whitelist, "whitelist", "", "comma-separated list of allowed IP addresses or CIDRs")

	// 3. 解析参数
	flag.Parse()

	// 4. 启动应用
	app.New(cfg).Run()
}
