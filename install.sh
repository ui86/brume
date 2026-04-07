#!/bin/bash

# Brume 服务器安装脚本
# 支持 linux/amd64 和 linux/arm64 架构
# 功能：安装、卸载、修改配置
# 自动适配服务管理器：systemd / OpenRC / SysVinit
# 自动适配防火墙：firewalld / ufw / nftables / iptables

# 默认参数
DEFAULT_PORT=1080
DEFAULT_USER=""
DEFAULT_PASSWORD=""
DEFAULT_WHITELIST=""
INSTALL_DIR="/usr/local/bin"
SERVICE_NAME="brume"
GITHUB_REPO="ui86/brume"

# iptables chain 名称（用于标识 Brume 添加的规则）
IPTABLES_CHAIN="BRUME_WHITELIST"

# 检测是否有sudo命令并设置执行特权命令的方式
has_sudo=false
if command -v sudo &> /dev/null; then
    has_sudo=true
fi

# 执行特权命令的辅助函数
execute_privileged() {
    if [ "$has_sudo" = true ]; then
        sudo "$@"
    else
        "$@"
    fi
}

# 颜色定义
RED="\033[31m"
GREEN="\033[32m"
YELLOW="\033[33m"
BLUE="\033[34m"
CYAN="\033[36m"
RESET="\033[0m"

# 显示帮助信息
show_help() {
    echo -e "${BLUE}Brume 服务器安装脚本${RESET}"
    echo "使用方法: 运行脚本后根据提示进行交互操作"
    echo ""
    echo "此脚本支持以下操作："
    echo "  1. 安装Brume服务器"
    echo "  2. 卸载Brume服务器"
    echo "  3. 修改Brume服务器配置"
    echo ""
    echo "支持的服务管理器: systemd / OpenRC / SysVinit"
    echo "支持的防火墙: firewalld / ufw / nftables / iptables"
    echo ""
    echo "注意：此脚本需要以root用户或使用sudo运行"
    exit 0
}

# ============================================================
# 服务管理器检测与适配
# ============================================================

# 检测可用的服务管理器类型
# 返回: systemd / openrc / sysvinit
detect_init_system() {
    if command -v systemctl &> /dev/null && systemctl --version &> /dev/null; then
        echo "systemd"
    elif command -v rc-service &> /dev/null; then
        echo "openrc"
    elif command -v service &> /dev/null || [ -d /etc/init.d ]; then
        echo "sysvinit"
    else
        echo "unknown"
    fi
}

# 创建 systemd 服务文件
create_systemd_service() {
    local port=$1
    local user=$2
    local password=$3
    local whitelist=$4
    local service_file="/etc/systemd/system/${SERVICE_NAME}.service"

    echo "正在创建 systemd 服务文件..."

    # 构建命令参数
    local cmd_args="-p ${port}"
    if [ -n "${user}" ] && [ -n "${password}" ]; then
        cmd_args="${cmd_args} -user ${user} -pwd ${password}"
    fi
    if [ -n "${whitelist}" ]; then
        cmd_args="${cmd_args} --whitelist ${whitelist}"
    fi

    # 创建服务文件
    if ! execute_privileged tee "${service_file}" > /dev/null <<EOF
[Unit]
Description=Brume Proxy Server
After=network.target

[Service]
Type=simple
ExecStart=${INSTALL_DIR}/brume ${cmd_args}
Restart=on-failure
RestartSec=5s

[Install]
WantedBy=multi-user.target
EOF
    then
        echo -e "${RED}创建服务文件失败，请检查权限${RESET}"
        exit 1
    fi

    execute_privileged systemctl daemon-reload
    execute_privileged systemctl enable "${SERVICE_NAME}"

    echo -e "${GREEN}systemd 服务创建成功${RESET}"
}

# 创建 OpenRC 服务脚本
create_openrc_service() {
    local port=$1
    local user=$2
    local password=$3
    local whitelist=$4
    local service_file="/etc/init.d/${SERVICE_NAME}"

    echo "正在创建 OpenRC 服务脚本..."

    # 构建命令参数
    local cmd_args="-p ${port}"
    if [ -n "${user}" ] && [ -n "${password}" ]; then
        cmd_args="${cmd_args} -user ${user} -pwd ${password}"
    fi
    if [ -n "${whitelist}" ]; then
        cmd_args="${cmd_args} --whitelist ${whitelist}"
    fi

    if ! execute_privileged tee "${service_file}" > /dev/null <<EOF
#!/sbin/openrc-run

name="brume"
description="Brume Proxy Server"
command="${INSTALL_DIR}/brume"
command_args="${cmd_args}"
command_background=true
pidfile="/run/\${RC_SVCNAME}.pid"

depend() {
    need net
    after firewall
}
EOF
    then
        echo -e "${RED}创建 OpenRC 服务脚本失败${RESET}"
        exit 1
    fi

    execute_privileged chmod +x "${service_file}"
    execute_privileged rc-update add "${SERVICE_NAME}" default 2>/dev/null

    echo -e "${GREEN}OpenRC 服务创建成功${RESET}"
}

# 创建 SysVinit 服务脚本
create_sysvinit_service() {
    local port=$1
    local user=$2
    local password=$3
    local whitelist=$4
    local service_file="/etc/init.d/${SERVICE_NAME}"

    echo "正在创建 SysVinit 服务脚本..."

    # 构建命令参数
    local cmd_args="-p ${port}"
    if [ -n "${user}" ] && [ -n "${password}" ]; then
        cmd_args="${cmd_args} -user ${user} -pwd ${password}"
    fi
    if [ -n "${whitelist}" ]; then
        cmd_args="${cmd_args} --whitelist ${whitelist}"
    fi

    if ! execute_privileged tee "${service_file}" > /dev/null <<'OUTER_EOF'
#!/bin/bash
### BEGIN INIT INFO
# Provides:          brume
# Required-Start:    $network $remote_fs
# Required-Stop:     $network $remote_fs
# Default-Start:     2 3 4 5
# Default-Stop:      0 1 6
# Short-Description: Brume Proxy Server
# Description:       Brume Proxy Server
### END INIT INFO

DAEMON="PLACEHOLDER_INSTALL_DIR/brume"
DAEMON_ARGS="PLACEHOLDER_CMD_ARGS"
PIDFILE="/var/run/brume.pid"
NAME="brume"

start() {
    echo "Starting $NAME..."
    if [ -f "$PIDFILE" ] && kill -0 "$(cat "$PIDFILE")" 2>/dev/null; then
        echo "$NAME is already running."
        return 1
    fi
    nohup $DAEMON $DAEMON_ARGS > /dev/null 2>&1 &
    echo $! > "$PIDFILE"
    echo "$NAME started."
}

stop() {
    echo "Stopping $NAME..."
    if [ ! -f "$PIDFILE" ] || ! kill -0 "$(cat "$PIDFILE")" 2>/dev/null; then
        echo "$NAME is not running."
        return 1
    fi
    kill "$(cat "$PIDFILE")"
    rm -f "$PIDFILE"
    echo "$NAME stopped."
}

restart() {
    stop
    sleep 1
    start
}

status() {
    if [ -f "$PIDFILE" ] && kill -0 "$(cat "$PIDFILE")" 2>/dev/null; then
        echo "$NAME is running (PID: $(cat "$PIDFILE"))"
    else
        echo "$NAME is not running."
        return 1
    fi
}

case "$1" in
    start)   start ;;
    stop)    stop ;;
    restart) restart ;;
    status)  status ;;
    *)
        echo "Usage: $0 {start|stop|restart|status}"
        exit 1
        ;;
esac
OUTER_EOF
    then
        echo -e "${RED}创建 SysVinit 服务脚本失败${RESET}"
        exit 1
    fi

    # 替换占位符为实际值
    execute_privileged sed -i "s|PLACEHOLDER_INSTALL_DIR|${INSTALL_DIR}|g" "${service_file}"
    execute_privileged sed -i "s|PLACEHOLDER_CMD_ARGS|${cmd_args}|g" "${service_file}"

    execute_privileged chmod +x "${service_file}"

    # 尝试使用 update-rc.d 或 chkconfig 注册开机自启
    if command -v update-rc.d &> /dev/null; then
        execute_privileged update-rc.d "${SERVICE_NAME}" defaults 2>/dev/null
    elif command -v chkconfig &> /dev/null; then
        execute_privileged chkconfig --add "${SERVICE_NAME}" 2>/dev/null
        execute_privileged chkconfig "${SERVICE_NAME}" on 2>/dev/null
    fi

    echo -e "${GREEN}SysVinit 服务创建成功${RESET}"
}

# 根据检测到的 init 系统创建服务
create_service() {
    local init_system=$1
    local port=$2
    local user=$3
    local password=$4
    local whitelist=$5

    case "${init_system}" in
        systemd)
            create_systemd_service "${port}" "${user}" "${password}" "${whitelist}"
            ;;
        openrc)
            create_openrc_service "${port}" "${user}" "${password}" "${whitelist}"
            ;;
        sysvinit)
            create_sysvinit_service "${port}" "${user}" "${password}" "${whitelist}"
            ;;
        *)
            echo -e "${RED}不支持的服务管理器: ${init_system}${RESET}"
            exit 1
            ;;
    esac
}

# 根据 init 系统启动服务
start_service_by_init() {
    local init_system=$1

    echo "正在启动服务..."

    case "${init_system}" in
        systemd)
            if ! execute_privileged systemctl start "${SERVICE_NAME}"; then
                echo -e "${RED}启动服务失败，请运行 'systemctl status ${SERVICE_NAME}' 查看详情${RESET}"
                exit 1
            fi
            echo -e "${GREEN}服务启动成功${RESET}"
            echo "服务状态:"
            execute_privileged systemctl status "${SERVICE_NAME}" --no-pager
            ;;
        openrc)
            if ! execute_privileged rc-service "${SERVICE_NAME}" start; then
                echo -e "${RED}启动服务失败${RESET}"
                exit 1
            fi
            echo -e "${GREEN}服务启动成功${RESET}"
            ;;
        sysvinit)
            if ! execute_privileged /etc/init.d/"${SERVICE_NAME}" start; then
                echo -e "${RED}启动服务失败${RESET}"
                exit 1
            fi
            echo -e "${GREEN}服务启动成功${RESET}"
            ;;
    esac
}

# 根据 init 系统停止服务
stop_service_by_init() {
    local init_system=$1

    echo "正在停止服务..."

    case "${init_system}" in
        systemd)
            execute_privileged systemctl stop "${SERVICE_NAME}" 2>/dev/null
            ;;
        openrc)
            execute_privileged rc-service "${SERVICE_NAME}" stop 2>/dev/null
            ;;
        sysvinit)
            if [ -f "/etc/init.d/${SERVICE_NAME}" ]; then
                execute_privileged /etc/init.d/"${SERVICE_NAME}" stop 2>/dev/null
            fi
            ;;
    esac
}

# 根据 init 系统重启服务
restart_service_by_init() {
    local init_system=$1

    echo "正在重启服务..."

    case "${init_system}" in
        systemd)
            if ! execute_privileged systemctl restart "${SERVICE_NAME}"; then
                echo -e "${RED}重启服务失败，请运行 'systemctl status ${SERVICE_NAME}' 查看详情${RESET}"
                exit 1
            fi
            ;;
        openrc)
            if ! execute_privileged rc-service "${SERVICE_NAME}" restart; then
                echo -e "${RED}重启服务失败${RESET}"
                exit 1
            fi
            ;;
        sysvinit)
            if ! execute_privileged /etc/init.d/"${SERVICE_NAME}" restart; then
                echo -e "${RED}重启服务失败${RESET}"
                exit 1
            fi
            ;;
    esac

    echo -e "${GREEN}服务重启成功${RESET}"
}

# 根据 init 系统卸载服务
remove_service_by_init() {
    local init_system=$1

    case "${init_system}" in
        systemd)
            echo "正在禁用 systemd 服务..."
            execute_privileged systemctl disable "${SERVICE_NAME}" 2>/dev/null
            local service_file="/etc/systemd/system/${SERVICE_NAME}.service"
            if [ -f "${service_file}" ]; then
                echo "正在删除服务文件..."
                execute_privileged rm -f "${service_file}"
                execute_privileged systemctl daemon-reload
            fi
            ;;
        openrc)
            echo "正在移除 OpenRC 服务..."
            execute_privileged rc-update del "${SERVICE_NAME}" default 2>/dev/null
            local service_file="/etc/init.d/${SERVICE_NAME}"
            if [ -f "${service_file}" ]; then
                execute_privileged rm -f "${service_file}"
            fi
            ;;
        sysvinit)
            echo "正在移除 SysVinit 服务..."
            if command -v update-rc.d &> /dev/null; then
                execute_privileged update-rc.d -f "${SERVICE_NAME}" remove 2>/dev/null
            elif command -v chkconfig &> /dev/null; then
                execute_privileged chkconfig --del "${SERVICE_NAME}" 2>/dev/null
            fi
            local service_file="/etc/init.d/${SERVICE_NAME}"
            if [ -f "${service_file}" ]; then
                execute_privileged rm -f "${service_file}"
            fi
            ;;
    esac
}

# ============================================================
# 防火墙检测与管理
# ============================================================

# 检测可用的防火墙工具
# 返回: firewalld / ufw / nftables / iptables / none
detect_firewall() {
    # 按优先级检测：firewalld > ufw > nftables > iptables
    if command -v firewall-cmd &> /dev/null && execute_privileged firewall-cmd --state &> /dev/null; then
        echo "firewalld"
    elif command -v ufw &> /dev/null && execute_privileged ufw status &> /dev/null; then
        echo "ufw"
    elif command -v nft &> /dev/null; then
        echo "nftables"
    elif command -v iptables &> /dev/null; then
        echo "iptables"
    else
        echo "none"
    fi
}

# 使用 firewalld 设置防火墙规则
# 仅允许白名单 IP 访问指定端口，拒绝其他所有连接
setup_firewall_firewalld() {
    local port=$1
    local whitelist=$2

    echo -e "${CYAN}使用 firewalld 配置防火墙规则...${RESET}"

    # 先移除已有的 Brume zone 和规则（幂等操作）
    remove_firewall_firewalld "${port}" 2>/dev/null

    # 创建专用 zone
    execute_privileged firewall-cmd --permanent --new-zone=brume 2>/dev/null

    # 解析白名单并添加 rich rules
    IFS=',' read -ra IPS <<< "${whitelist}"
    for ip in "${IPS[@]}"; do
        ip=$(echo "${ip}" | xargs)  # 去除空格
        [ -z "${ip}" ] && continue
        echo "  允许 ${ip} 访问端口 ${port}..."
        execute_privileged firewall-cmd --permanent --zone=brume \
            --add-rich-rule="rule family=\"ipv4\" source address=\"${ip}\" port port=\"${port}\" protocol=\"tcp\" accept"
    done

    # 将接口绑定到 brume zone（如果需要可以指定接口）
    # 使用默认 zone 来添加拒绝规则
    execute_privileged firewall-cmd --permanent --zone=public \
        --add-rich-rule="rule family=\"ipv4\" port port=\"${port}\" protocol=\"tcp\" drop"

    # 在 public zone 中添加白名单 IP 的放行规则（优先级更高）
    for ip in "${IPS[@]}"; do
        ip=$(echo "${ip}" | xargs)
        [ -z "${ip}" ] && continue
        execute_privileged firewall-cmd --permanent --zone=public \
            --add-rich-rule="rule family=\"ipv4\" source address=\"${ip}\" port port=\"${port}\" protocol=\"tcp\" accept"
    done

    execute_privileged firewall-cmd --reload

    echo -e "${GREEN}firewalld 防火墙规则配置完成${RESET}"
}

# 使用 ufw 设置防火墙规则
setup_firewall_ufw() {
    local port=$1
    local whitelist=$2

    echo -e "${CYAN}使用 ufw 配置防火墙规则...${RESET}"

    # 先清除已有规则
    remove_firewall_ufw "${port}" 2>/dev/null

    # 添加白名单 IP 的放行规则
    IFS=',' read -ra IPS <<< "${whitelist}"
    for ip in "${IPS[@]}"; do
        ip=$(echo "${ip}" | xargs)
        [ -z "${ip}" ] && continue
        echo "  允许 ${ip} 访问端口 ${port}..."
        # 添加注释标识规则来源
        execute_privileged ufw allow from "${ip}" to any port "${port}" proto tcp comment "brume-whitelist"
    done

    # 拒绝其他所有连接到该端口
    execute_privileged ufw deny to any port "${port}" proto tcp comment "brume-deny-default"

    # 确保 ufw 已启用
    echo "y" | execute_privileged ufw enable 2>/dev/null

    echo -e "${GREEN}ufw 防火墙规则配置完成${RESET}"
}

# 使用 nftables 设置防火墙规则
setup_firewall_nftables() {
    local port=$1
    local whitelist=$2

    echo -e "${CYAN}使用 nftables 配置防火墙规则...${RESET}"

    # 先清除已有规则
    remove_firewall_nftables 2>/dev/null

    # 创建 Brume 专用规则表
    execute_privileged nft add table inet brume
    execute_privileged nft add chain inet brume input '{ type filter hook input priority 0; policy accept; }'

    # 添加白名单 IP 的放行规则
    IFS=',' read -ra IPS <<< "${whitelist}"
    for ip in "${IPS[@]}"; do
        ip=$(echo "${ip}" | xargs)
        [ -z "${ip}" ] && continue
        echo "  允许 ${ip} 访问端口 ${port}..."
        execute_privileged nft add rule inet brume input ip saddr "${ip}" tcp dport "${port}" accept
    done

    # 拒绝其他所有到该端口的连接
    execute_privileged nft add rule inet brume input tcp dport "${port}" drop

    # 持久化 nftables 规则
    if [ -f /etc/nftables.conf ]; then
        execute_privileged nft list ruleset > /tmp/brume_nft_backup.conf
        execute_privileged cp /tmp/brume_nft_backup.conf /etc/nftables.conf
        rm -f /tmp/brume_nft_backup.conf
    fi

    echo -e "${GREEN}nftables 防火墙规则配置完成${RESET}"
}

# 使用 iptables 设置防火墙规则
setup_firewall_iptables() {
    local port=$1
    local whitelist=$2

    echo -e "${CYAN}使用 iptables 配置防火墙规则...${RESET}"

    # 先清除已有规则
    remove_firewall_iptables "${port}" 2>/dev/null

    # 创建自定义 chain
    execute_privileged iptables -N "${IPTABLES_CHAIN}" 2>/dev/null

    # 将针对目标端口的流量导向自定义 chain
    execute_privileged iptables -I INPUT -p tcp --dport "${port}" -j "${IPTABLES_CHAIN}"

    # 添加白名单 IP 的放行规则
    IFS=',' read -ra IPS <<< "${whitelist}"
    for ip in "${IPS[@]}"; do
        ip=$(echo "${ip}" | xargs)
        [ -z "${ip}" ] && continue
        echo "  允许 ${ip} 访问端口 ${port}..."
        execute_privileged iptables -A "${IPTABLES_CHAIN}" -s "${ip}" -p tcp --dport "${port}" -j ACCEPT
    done

    # 拒绝其他所有到该端口的连接
    execute_privileged iptables -A "${IPTABLES_CHAIN}" -p tcp --dport "${port}" -j DROP

    # 持久化 iptables 规则
    persist_iptables_rules

    echo -e "${GREEN}iptables 防火墙规则配置完成${RESET}"
}

# 持久化 iptables 规则
persist_iptables_rules() {
    if command -v netfilter-persistent &> /dev/null; then
        execute_privileged netfilter-persistent save 2>/dev/null
    elif command -v iptables-save &> /dev/null; then
        # Debian/Ubuntu
        if [ -d /etc/iptables ]; then
            execute_privileged sh -c "iptables-save > /etc/iptables/rules.v4"
        # CentOS/RHEL
        elif [ -d /etc/sysconfig ]; then
            execute_privileged sh -c "iptables-save > /etc/sysconfig/iptables"
        fi
    fi
}

# ============================================================
# 防火墙规则移除
# ============================================================

# 移除 firewalld 规则
remove_firewall_firewalld() {
    local port=$1

    echo -e "${CYAN}移除 firewalld 防火墙规则...${RESET}"

    # 如果不知道端口，尝试从服务中获取
    if [ -z "${port}" ]; then
        port=$(get_current_port)
    fi

    if [ -n "${port}" ]; then
        # 移除 public zone 中所有 brume 相关的 rich rules
        local rules
        rules=$(execute_privileged firewall-cmd --permanent --zone=public --list-rich-rules 2>/dev/null | grep "port=\"${port}\"")
        while IFS= read -r rule; do
            [ -z "${rule}" ] && continue
            execute_privileged firewall-cmd --permanent --zone=public --remove-rich-rule="${rule}" 2>/dev/null
        done <<< "${rules}"
    fi

    # 删除 brume zone
    execute_privileged firewall-cmd --permanent --delete-zone=brume 2>/dev/null
    execute_privileged firewall-cmd --reload 2>/dev/null

    echo -e "${GREEN}firewalld 规则已清除${RESET}"
}

# 移除 ufw 规则
remove_firewall_ufw() {
    local port=$1

    echo -e "${CYAN}移除 ufw 防火墙规则...${RESET}"

    if [ -z "${port}" ]; then
        port=$(get_current_port)
    fi

    if [ -n "${port}" ]; then
        # 删除带有 brume 标识的规则（通过规则编号倒序删除避免索引偏移）
        # 先获取所有规则编号
        local rule_nums
        rule_nums=$(execute_privileged ufw status numbered 2>/dev/null | grep -E "(brume|${port}/tcp)" | grep -oP '^\[\s*\K[0-9]+' | sort -rn)
        for num in ${rule_nums}; do
            echo "y" | execute_privileged ufw delete "${num}" 2>/dev/null
        done

        # 备选方案：直接按规则内容删除
        execute_privileged ufw delete deny to any port "${port}" proto tcp 2>/dev/null
    fi

    echo -e "${GREEN}ufw 规则已清除${RESET}"
}

# 移除 nftables 规则
remove_firewall_nftables() {
    echo -e "${CYAN}移除 nftables 防火墙规则...${RESET}"

    # 删除 brume 表
    execute_privileged nft delete table inet brume 2>/dev/null

    # 更新持久化配置
    if [ -f /etc/nftables.conf ]; then
        execute_privileged nft list ruleset > /tmp/brume_nft_cleanup.conf 2>/dev/null
        execute_privileged cp /tmp/brume_nft_cleanup.conf /etc/nftables.conf 2>/dev/null
        rm -f /tmp/brume_nft_cleanup.conf
    fi

    echo -e "${GREEN}nftables 规则已清除${RESET}"
}

# 移除 iptables 规则
remove_firewall_iptables() {
    local port=$1

    echo -e "${CYAN}移除 iptables 防火墙规则...${RESET}"

    if [ -z "${port}" ]; then
        port=$(get_current_port)
    fi

    # 移除 INPUT 链中指向自定义 chain 的引用
    if [ -n "${port}" ]; then
        execute_privileged iptables -D INPUT -p tcp --dport "${port}" -j "${IPTABLES_CHAIN}" 2>/dev/null
    else
        # 尝试通过 chain 名称查找并删除所有引用
        while execute_privileged iptables -D INPUT -j "${IPTABLES_CHAIN}" 2>/dev/null; do :; done
    fi

    # 清空并删除自定义 chain
    execute_privileged iptables -F "${IPTABLES_CHAIN}" 2>/dev/null
    execute_privileged iptables -X "${IPTABLES_CHAIN}" 2>/dev/null

    # 更新持久化规则
    persist_iptables_rules

    echo -e "${GREEN}iptables 规则已清除${RESET}"
}

# 根据检测到的防火墙类型设置规则
setup_firewall() {
    local port=$1
    local whitelist=$2
    local fw_type

    # 白名单为空时，不需要设置防火墙规则
    if [ -z "${whitelist}" ]; then
        echo -e "${YELLOW}未设置IP白名单，跳过防火墙配置${RESET}"
        return 0
    fi

    fw_type=$(detect_firewall)
    echo -e "${CYAN}检测到防火墙类型: ${fw_type}${RESET}"

    case "${fw_type}" in
        firewalld)
            setup_firewall_firewalld "${port}" "${whitelist}"
            ;;
        ufw)
            setup_firewall_ufw "${port}" "${whitelist}"
            ;;
        nftables)
            setup_firewall_nftables "${port}" "${whitelist}"
            ;;
        iptables)
            setup_firewall_iptables "${port}" "${whitelist}"
            ;;
        none)
            echo -e "${YELLOW}未检测到可用的防火墙工具${RESET}"
            echo -e "${YELLOW}建议安装 iptables 或 ufw 以增强安全性${RESET}"
            echo -e "${YELLOW}IP白名单已通过 Brume 应用层生效，但未进行网络层限制${RESET}"
            ;;
    esac
}

# 移除所有防火墙规则
remove_firewall() {
    local port=$1
    local fw_type

    fw_type=$(detect_firewall)

    case "${fw_type}" in
        firewalld)
            remove_firewall_firewalld "${port}"
            ;;
        ufw)
            remove_firewall_ufw "${port}"
            ;;
        nftables)
            remove_firewall_nftables
            ;;
        iptables)
            remove_firewall_iptables "${port}"
            ;;
        none)
            echo -e "${YELLOW}未检测到防火墙工具，无需清理${RESET}"
            ;;
    esac
}

# ============================================================
# 工具函数
# ============================================================

# 检测系统架构
check_architecture() {
    local arch
    arch=$(uname -m)
    case "$arch" in
        x86_64) echo "amd64" ;;
        aarch64) echo "arm64" ;;
        *) echo -e "${RED}不支持的系统架构: $arch${RESET}" >&2 ; exit 1 ;;
    esac
}

# 获取最新版本号
get_latest_version() {
    echo "正在获取最新版本号..." >&2
    local latest_version
    latest_version=$(curl -s "https://api.github.com/repos/${GITHUB_REPO}/releases/latest" | grep '"tag_name":' | sed -E 's/.*"tag_name": "([^"]+)".*/\1/')
    if [ -z "$latest_version" ]; then
        echo -e "${RED}获取最新版本号失败，使用默认版本 1.0.0${RESET}" >&2
        latest_version="1.0.0"
    fi
    echo "最新版本: $latest_version" >&2
    echo "$latest_version"
}

# 下载并安装二进制文件
download_and_install() {
    local version=$1
    local arch=$2
    # shellcheck disable=SC2155
    local temp_dir=$(mktemp -d)
    local download_url="https://github.com/${GITHUB_REPO}/releases/download/${version}/brume-${version}-linux-${arch}.tar.gz"
    local tar_file="${temp_dir}/brume-${version}-linux-${arch}.tar.gz"

    echo "正在下载 ${download_url}..."
    if ! curl -L -o "${tar_file}" "${download_url}"; then
        echo -e "${RED}下载失败，请检查网络连接或版本是否存在${RESET}"
        rm -rf "${temp_dir}"
        exit 1
    fi

    echo "解压安装包..."
    if ! tar -xzf "${tar_file}" -C "${temp_dir}"; then
        echo -e "${RED}解压失败${RESET}"
        rm -rf "${temp_dir}"
        exit 1
    fi

    echo "安装到 ${INSTALL_DIR}..."
    if ! execute_privileged mv "${temp_dir}/brume" "${INSTALL_DIR}"; then
        echo -e "${RED}安装失败，请检查权限${RESET}"
        rm -rf "${temp_dir}"
        exit 1
    fi

    execute_privileged chmod +x "${INSTALL_DIR}/brume"
    rm -rf "${temp_dir}"
    echo -e "${GREEN}安装成功${RESET}"
}

# 从当前服务配置中获取端口号
get_current_port() {
    local port=""
    local init_system
    init_system=$(detect_init_system)

    case "${init_system}" in
        systemd)
            local exec_start
            exec_start=$(execute_privileged grep '^ExecStart=' "/etc/systemd/system/${SERVICE_NAME}.service" 2>/dev/null | head -n1)
            port=$(echo "${exec_start}" | grep -oP '(?<=-p\s)\d+')
            ;;
        openrc|sysvinit)
            local cmd_line
            cmd_line=$(execute_privileged grep -E '(command_args|DAEMON_ARGS)' "/etc/init.d/${SERVICE_NAME}" 2>/dev/null | head -n1)
            port=$(echo "${cmd_line}" | grep -oP '(?<=-p\s)\d+')
            ;;
    esac

    # 如果解析失败，使用默认端口
    if [ -z "${port}" ]; then
        port="${DEFAULT_PORT}"
    fi
    echo "${port}"
}

# 从当前服务配置中获取白名单
get_current_whitelist() {
    local whitelist=""
    local init_system
    init_system=$(detect_init_system)

    case "${init_system}" in
        systemd)
            local exec_start
            exec_start=$(execute_privileged grep '^ExecStart=' "/etc/systemd/system/${SERVICE_NAME}.service" 2>/dev/null | head -n1)
            whitelist=$(echo "${exec_start}" | grep -oP '(?<=--whitelist\s)\S+')
            ;;
        openrc|sysvinit)
            local cmd_line
            cmd_line=$(execute_privileged grep -E '(command_args|DAEMON_ARGS)' "/etc/init.d/${SERVICE_NAME}" 2>/dev/null | head -n1)
            whitelist=$(echo "${cmd_line}" | grep -oP '(?<=--whitelist\s)\S+')
            ;;
    esac

    echo "${whitelist}"
}

# ============================================================
# 启动服务（安装后展示信息）
# ============================================================

start_service() {
    local init_system=$1

    start_service_by_init "${init_system}"

    echo -e "${BLUE}Brume 服务器安装完成！${RESET}"
    echo "配置信息:"
    echo "  端口: ${port}"
    echo "  认证: $(if [ -n "${user}" ] && [ -n "${password}" ]; then echo "启用"; else echo "禁用"; fi)"
    echo "  白名单: $(if [ -n "${whitelist}" ]; then echo "${whitelist}"; else echo "无（允许所有IP）"; fi)"
    echo "  服务管理器: ${init_system}"
    echo "  防火墙: $(detect_firewall)"

    echo -e "${YELLOW}管理命令:${RESET}"
    case "${init_system}" in
        systemd)
            echo "  查看服务状态: systemctl status ${SERVICE_NAME}"
            echo "  停止服务: systemctl stop ${SERVICE_NAME}"
            echo "  重启服务: systemctl restart ${SERVICE_NAME}"
            echo "  禁用开机自启: systemctl disable ${SERVICE_NAME}"
            ;;
        openrc)
            echo "  查看服务状态: rc-service ${SERVICE_NAME} status"
            echo "  停止服务: rc-service ${SERVICE_NAME} stop"
            echo "  重启服务: rc-service ${SERVICE_NAME} restart"
            echo "  禁用开机自启: rc-update del ${SERVICE_NAME} default"
            ;;
        sysvinit)
            echo "  查看服务状态: /etc/init.d/${SERVICE_NAME} status"
            echo "  停止服务: /etc/init.d/${SERVICE_NAME} stop"
            echo "  重启服务: /etc/init.d/${SERVICE_NAME} restart"
            ;;
    esac
}

# ============================================================
# 修改配置函数
# ============================================================

modify() {
    local init_system=$1
    local old_port
    local old_whitelist

    # 获取旧配置用于清理防火墙规则
    old_port=$(get_current_port)
    old_whitelist=$(get_current_whitelist)

    # 停止服务
    stop_service_by_init "${init_system}"

    # 如果之前有白名单配置，先清理旧的防火墙规则
    if [ -n "${old_whitelist}" ]; then
        echo "清理旧的防火墙规则..."
        remove_firewall "${old_port}"
    fi

    # 更新服务文件
    create_service "${init_system}" "${port}" "${user}" "${password}" "${whitelist}"

    # 设置新的防火墙规则
    setup_firewall "${port}" "${whitelist}"

    # 重启服务
    restart_service_by_init "${init_system}"

    echo -e "${GREEN}Brume 服务器配置修改完成！${RESET}"
}

# ============================================================
# 卸载函数
# ============================================================

uninstall() {
    local init_system=$1
    local current_port
    local current_whitelist

    echo -e "${BLUE}=== Brume 服务器卸载 ===${RESET}"

    # 获取当前配置用于清理
    current_port=$(get_current_port)
    current_whitelist=$(get_current_whitelist)

    # 停止服务
    stop_service_by_init "${init_system}"

    # 移除服务
    remove_service_by_init "${init_system}"

    # 清理防火墙规则
    if [ -n "${current_whitelist}" ]; then
        echo "正在清理防火墙规则..."
        remove_firewall "${current_port}"
    else
        # 即使没有白名单配置，也尝试清理可能残留的规则
        echo "正在检查并清理防火墙规则..."
        remove_firewall "${current_port}"
    fi

    # 删除二进制文件
    local binary_path="${INSTALL_DIR}/brume"
    if [ -f "${binary_path}" ]; then
        echo "正在删除二进制文件..."
        execute_privileged rm -f "${binary_path}"
    fi

    echo -e "${GREEN}Brume 服务器卸载完成${RESET}"
}

# ============================================================
# 交互式菜单
# ============================================================

# 交互式获取操作类型
get_action() {
    while true; do
        echo -e "${BLUE}=== Brume 服务器管理 ===${RESET}"
        echo "请选择要执行的操作："
        echo "1. 安装Brume服务器"
        echo "2. 卸载Brume服务器"
        echo "3. 修改Brume服务器配置"
        echo "4. 一键更新Brume服务器"
        echo "5. 显示帮助信息"

        read -p "请输入选项 (1-5): " choice

        case $choice in
            1)
                action="install"
                break
                ;;
            2)
                action="uninstall"
                break
                ;;
            3)
                action="modify"
                break
                ;;
            4)
                action="upgrade"
                break
                ;;
            5)
                show_help
                ;;
            *)
                echo -e "${RED}无效的选项，请重新输入${RESET}"
                ;;
        esac
    done
}

# 交互式获取安装配置参数
get_install_config() {
    # 重置参数为默认值
    port="${DEFAULT_PORT}"
    user="${DEFAULT_USER}"
    password="${DEFAULT_PASSWORD}"
    whitelist="${DEFAULT_WHITELIST}"
    version=""

    # 获取端口号
    while true; do
        read -p "请输入Brume服务器端口号 [默认: ${DEFAULT_PORT}]: " input_port
        if [ -z "${input_port}" ]; then
            port=${DEFAULT_PORT}
            break
        elif [[ "${input_port}" =~ ^[0-9]+$ ]] && [ "${input_port}" -ge 1 ] && [ "${input_port}" -le 65535 ]; then
            port=${input_port}
            break
        else
            echo -e "${RED}无效的端口号，请输入1-65535之间的数字${RESET}"
        fi
    done

    # 获取是否启用认证
    while true; do
        read -p "是否启用用户认证? (y/n) [默认: n]: " enable_auth
        enable_auth=${enable_auth:-n}

        case ${enable_auth} in
            [Yy])
                read -p "请输入用户名: " user
                while [ -z "${user}" ]; do
                    read -p "用户名不能为空，请重新输入: " user
                done

                # shellcheck disable=SC2162
                read -s -p "请输入密码: " password
                echo
                while [ -z "${password}" ]; do
                    read -s -p "密码不能为空，请重新输入: " password
                    echo
                done
                break
                ;;
            [Nn])
                user=""
                password=""
                break
                ;;
            *)
                echo -e "${RED}无效的选项，请输入y或n${RESET}"
                ;;
        esac
    done

    # 获取IP白名单
    echo -e "${YELLOW}提示: 设置IP白名单后，脚本将自动配置防火墙规则，仅允许白名单IP访问服务端口${RESET}"
    read -p "请输入IP白名单（多个IP用逗号分隔，留空表示不限制）: " whitelist

    # 获取版本信息
    read -p "请输入要安装的版本（留空表示最新版本）: " version

    # 显示确认信息
    echo -e "${GREEN}安装配置确认：${RESET}"
    echo "端口: ${port}"
    echo "认证: $(if [ -n "${user}" ] && [ -n "${password}" ]; then echo "启用 (用户名: ${user})"; else echo "禁用"; fi)"
    echo "白名单: $(if [ -n "${whitelist}" ]; then echo "${whitelist}"; else echo "无（允许所有IP）"; fi)"
    echo "版本: $(if [ -n "${version}" ]; then echo "${version}"; else echo "最新版本"; fi)"
    if [ -n "${whitelist}" ]; then
        echo -e "防火墙: ${CYAN}将自动配置防火墙规则${RESET}"
    fi

    while true; do
        read -p "确认以上配置是否正确? (y/n) [默认: y]: " confirm
        confirm=${confirm:-y}

        case ${confirm} in
            [Yy])
                break
                ;;
            [Nn])
                echo -e "${YELLOW}配置已取消，将重新开始${RESET}"
                get_install_config
                break
                ;;
            *)
                echo -e "${RED}无效的选项，请输入y或n${RESET}"
                ;;
        esac
    done
}

# 交互式获取修改配置参数
get_modify_config() {
    local init_system=$1
    local service_file

    # 根据 init 系统确定服务文件位置
    case "${init_system}" in
        systemd)
            service_file="/etc/systemd/system/${SERVICE_NAME}.service"
            ;;
        openrc|sysvinit)
            service_file="/etc/init.d/${SERVICE_NAME}"
            ;;
    esac

    # 检查服务是否存在
    if [ ! -f "${service_file}" ]; then
        echo -e "${RED}Brume 服务未安装，请先安装服务${RESET}"
        exit 1
    fi

    # 读取当前配置参数
    local cmd_args=""

    case "${init_system}" in
        systemd)
            local exec_start
            exec_start=$(execute_privileged grep '^ExecStart=' "${service_file}" 2>/dev/null | head -n1)
            cmd_args=$(echo "$exec_start" | sed "s|^ExecStart=${INSTALL_DIR}/brume ||")
            ;;
        openrc)
            cmd_args=$(execute_privileged grep '^command_args=' "${service_file}" 2>/dev/null | head -n1 | sed 's/^command_args="//' | sed 's/"$//')
            ;;
        sysvinit)
            cmd_args=$(execute_privileged grep '^DAEMON_ARGS=' "${service_file}" 2>/dev/null | head -n1 | sed 's/^DAEMON_ARGS="//' | sed 's/"$//')
            ;;
    esac

    # 初始化默认值
    port="${DEFAULT_PORT}"
    user=""
    password=""
    whitelist=""

    # 解析参数
    set -- $cmd_args
    while [ $# -gt 0 ]; do
        case "$1" in
            -p)
                if [ -n "$2" ] && [ "$2" -ge 1 ] && [ "$2" -le 65535 ] 2>/dev/null; then
                    port="$2"
                    shift 2
                else
                    shift
                fi
                ;;
            -user)
                user="$2"
                shift 2
                ;;
            -pwd)
                password="$2"
                shift 2
                ;;
            --whitelist)
                whitelist="$2"
                shift 2
                ;;
            *)
                shift
                ;;
        esac
    done

    # 显示当前配置
    echo -e "${GREEN}当前配置:${RESET}"
    echo "  端口: ${port}"
    echo "  认证: $(if [ -n "${user}" ]; then echo "启用 (用户名: ${user})"; else echo "禁用"; fi)"
    echo "  白名单: $(if [ -n "${whitelist}" ]; then echo "${whitelist}"; else echo "无"; fi)"

    # 获取新的配置参数
    echo -e "${BLUE}请输入新的配置参数（留空表示保持当前配置）${RESET}"

    # 获取端口号
    while true; do
        read -p "请输入Brume服务器端口号 [当前: ${port}]: " input_port
        if [ -z "${input_port}" ]; then
            break
        elif [[ "${input_port}" =~ ^[0-9]+$ ]] && [ "${input_port}" -ge 1 ] && [ "${input_port}" -le 65535 ]; then
            port=${input_port}
            break
        else
            echo -e "${RED}无效的端口号，请输入1-65535之间的数字${RESET}"
        fi
    done

    # 获取是否修改用户认证
    while true; do
        read -p "是否修改用户认证设置? (y/n) [默认: n]: " change_auth
        change_auth=${change_auth:-n}

        case ${change_auth} in
            [Yy])
                while true; do
                    read -p "是否启用用户认证? (y/n): " enable_auth
                    case ${enable_auth} in
                        [Yy])
                            read -p "请输入用户名: " user
                            while [ -z "${user}" ]; do
                                read -p "用户名不能为空，请重新输入: " user
                            done

                            read -s -p "请输入密码: " password
                            echo
                            while [ -z "${password}" ]; do
                                read -s -p "密码不能为空，请重新输入: " password
                                echo
                            done
                            break
                            ;;
                        [Nn])
                            user=""
                            password=""
                            break
                            ;;
                        *)
                            echo -e "${RED}无效的选项，请输入y或n${RESET}"
                            ;;
                    esac
                done
                break
                ;;
            [Nn])
                # 保持当前设置
                if [ -n "${user}" ]; then
                    read -s -p "请重新输入密码 (留空表示保留当前密码): " input_password
                    echo
                    if [ -n "${input_password}" ]; then
                        password="${input_password}"
                    fi
                else
                    user=""
                    password=""
                fi
                break
                ;;
            *)
                echo -e "${RED}无效的选项，请输入y或n${RESET}"
                ;;
        esac
    done

    # 获取IP白名单
    echo -e "${YELLOW}提示: 修改白名单将自动更新防火墙规则${RESET}"
    read -p "请输入IP白名单（多个IP用逗号分隔，留空表示保持当前设置，输入 'clear' 清除白名单）: " input_whitelist
    if [ "${input_whitelist}" = "clear" ]; then
        whitelist=""
    elif [ -n "${input_whitelist}" ]; then
        whitelist="${input_whitelist}"
    fi

    # 显示新配置
    echo -e "${GREEN}新配置:${RESET}"
    echo "  端口: ${port}"
    echo "  认证: $(if [ -n "${user}" ] && [ -n "${password}" ]; then echo "启用 (用户名: ${user})"; else echo "禁用"; fi)"
    echo "  白名单: $(if [ -n "${whitelist}" ]; then echo "${whitelist}"; else echo "无"; fi)"
    if [ -n "${whitelist}" ]; then
        echo -e "  防火墙: ${CYAN}将自动更新防火墙规则${RESET}"
    fi

    # 确认修改
    while true; do
        read -p "确认修改配置? (y/n) [默认: y]: " confirm
        confirm=${confirm:-y}

        case ${confirm} in
            [Yy])
                break
                ;;
            [Nn])
                echo -e "${YELLOW}配置修改已取消${RESET}"
                exit 0
                ;;
            *)
                echo -e "${RED}无效的选项，请输入y或n${RESET}"
                ;;
        esac
    done
}

# ============================================================
# 更新函数
# ============================================================

# 自动获取当前所有参数（非交互式）
get_current_config_auto() {
    local init_system=$1
    local service_file

    case "${init_system}" in
        systemd) service_file="/etc/systemd/system/${SERVICE_NAME}.service" ;;
        openrc|sysvinit) service_file="/etc/init.d/${SERVICE_NAME}" ;;
    esac

    if [ ! -f "${service_file}" ]; then
        echo -e "${RED}Brume 服务未安装，无法更新${RESET}"
        exit 1
    fi

    local cmd_args=""
    case "${init_system}" in
        systemd)
            local exec_start
            exec_start=$(execute_privileged grep '^ExecStart=' "${service_file}" 2>/dev/null | head -n1)
            cmd_args=$(echo "$exec_start" | sed "s|^ExecStart=${INSTALL_DIR}/brume ||")
            ;;
        openrc)
            cmd_args=$(execute_privileged grep '^command_args=' "${service_file}" 2>/dev/null | head -n1 | sed 's/^command_args="//' | sed 's/"$//')
            ;;
        sysvinit)
            cmd_args=$(execute_privileged grep '^DAEMON_ARGS=' "${service_file}" 2>/dev/null | head -n1 | sed 's/^DAEMON_ARGS="//' | sed 's/"$//')
            ;;
    esac

    # 初始化配置
    port="${DEFAULT_PORT}"
    user=""
    password=""
    whitelist=""

    # 简单解析已有的 cmd_args
    set -- $cmd_args
    while [ $# -gt 0 ]; do
        case "$1" in
            -p) port="$2"; shift 2 ;;
            -user) user="$2"; shift 2 ;;
            -pwd) password="$2"; shift 2 ;;
            --whitelist) whitelist="$2"; shift 2 ;;
            *) shift ;;
        esac
    done
}

upgrade() {
    local init_system=$1
    echo -e "${BLUE}=== Brume 服务器一键更新 ===${RESET}"

    # 1. 自动提取参数
    get_current_config_auto "${init_system}"
    echo -e "已提取当前配置: 端口 ${port}, 用户 ${user:-无}, 白名单 ${whitelist:-无限制}"

    # 2. 检测系统架构
    arch=$(check_architecture)
    
    # 3. 获取最新版本
    version=$(get_latest_version)
    echo -e "准备更新至版本: ${version}"

    # 4. 停服并清理旧规则（为了重新安全挂载）
    stop_service_by_init "${init_system}"
    if [ -n "${whitelist}" ]; then
        remove_firewall "${port}"
    fi

    # 5. 下载最新二进制覆盖
    download_and_install "${version}" "${arch}"

    # 6. 重建服务与防火墙项（确保配置更新）
    create_service "${init_system}" "${port}" "${user}" "${password}" "${whitelist}"
    setup_firewall "${port}" "${whitelist}"

    # 7. 重启服务
    restart_service_by_init "${init_system}"

    echo -e "${GREEN}Brume 服务器一键更新完成！${RESET}"
}

# ============================================================
# 主函数
# ============================================================

main() {
    # 检查是否为root用户
    if [ "$(id -u)" != "0" ]; then
        echo -e "${RED}请以root用户或使用sudo运行此脚本${RESET}"
        exit 1
    fi

    # 检查依赖工具
    if ! command -v curl &> /dev/null; then
        echo -e "${RED}未找到curl命令，请先安装curl${RESET}"
        exit 1
    fi

    # 检测服务管理器类型
    local init_system
    init_system=$(detect_init_system)

    if [ "${init_system}" = "unknown" ]; then
        echo -e "${RED}无法检测到支持的服务管理器 (systemd/openrc/sysvinit)${RESET}"
        echo -e "${RED}请确保系统使用 systemd、OpenRC 或 SysVinit 作为初始化系统${RESET}"
        exit 1
    fi

    echo -e "${CYAN}检测到服务管理器: ${init_system}${RESET}"
    echo -e "${CYAN}检测到防火墙工具: $(detect_firewall)${RESET}"

    # 交互式获取操作类型
    get_action

    # 根据操作类型执行相应的功能
    case "${action}" in
        install)
            echo -e "${BLUE}=== Brume 服务器安装 ===${RESET}"

            # 交互式获取安装配置
            get_install_config

            # 检测系统架构
            arch=$(check_architecture)
            echo "系统架构: $arch"

            # 获取版本号
            if [ -z "${version}" ]; then
                version=$(get_latest_version)
            else
                echo "指定版本: ${version}"
            fi

            # 下载并安装
            download_and_install "${version}" "${arch}"

            # 创建服务
            create_service "${init_system}" "${port}" "${user}" "${password}" "${whitelist}"

            # 设置防火墙规则
            setup_firewall "${port}" "${whitelist}"

            # 启动服务
            start_service "${init_system}"
            ;;
        uninstall)
            # 确认卸载
            while true; do
                read -p "确定要卸载Brume服务器吗? 此操作将删除所有相关文件、配置和防火墙规则。(y/n) [默认: n]: " confirm
                confirm=${confirm:-n}

                case ${confirm} in
                    [Yy])
                        uninstall "${init_system}"
                        break
                        ;;
                    [Nn])
                        echo -e "${YELLOW}卸载已取消${RESET}"
                        exit 0
                        ;;
                    *)
                        echo -e "${RED}无效的选项，请输入y或n${RESET}"
                        ;;
                esac
            done
            ;;
        modify)
            echo -e "${BLUE}=== Brume 服务器配置修改 ===${RESET}"

            # 交互式获取修改配置
            get_modify_config "${init_system}"

            # 执行修改操作（包含防火墙规则更新）
            modify "${init_system}"
            ;;
        upgrade)
            # 执行一键更新
            upgrade "${init_system}"
            ;;
        *)
            echo -e "${RED}未知操作: ${action}${RESET}"
            show_help
            ;;
    esac
}

# 执行主函数
main "$@"