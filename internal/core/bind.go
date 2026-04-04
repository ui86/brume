package core

import (
	"errors"
	"net"
)

// 待实现：BIND 命令目前暂不支持
func (r *Request) bind(_ net.Conn) error {
	return errors.New("Unsupport BIND now")
}
