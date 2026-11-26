#!/bin/bash

# Brume 服务器本地安装脚本，基于 install.sh 的服务、配置和防火墙逻辑
# 将本脚本和适配当前 Linux 架构的 brume 放在同一目录，无需联网下载
# 功能：本地安装、卸载、修改配置、使用本地二进制更新
# 自动适配服务管理器：systemd / OpenRC / SysVinit，或安装为前台程序
# 自动适配防火墙：firewalld / ufw / nftables / iptables

# 默认参数
DEFAULT_PORT=1080
DEFAULT_USER=""
DEFAULT_PASSWORD=""
DEFAULT_WHITELIST=""
DEFAULT_TCP_TIMEOUT=0
DEFAULT_UDP_TIMEOUT=60
DEFAULT_DNS_SERVERS="8.8.8.8,1.1.1.1"
SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P) || exit 1
# 独立工作进程运行事务目录中的脚本副本，必须继承最初的安装目录。
INSTALL_DIR="${BRUME_LOCAL_INSTALL_DIR:-${SCRIPT_DIR}}"
LOCAL_BINARY="${BRUME_LOCAL_BINARY:-${SCRIPT_DIR}/brume}"
SERVICE_NAME="brume-local"
CONFIG_DIR="${INSTALL_DIR}/.brume"
CONFIG_FILE="${CONFIG_DIR}/brume.conf"

# iptables chain 名称（用于标识 Brume 添加的规则）
IPTABLES_CHAIN="BRUME_LOCAL_WHITELIST"

# 检测是否有sudo命令并设置执行特权命令的方式
has_sudo=false
if command -v sudo &> /dev/null; then
    has_sudo=true
fi

# 执行特权命令的辅助函数
execute_privileged() (
    # 服务管理器可能启动常驻进程，不能让其继承安装事务锁
    if [ -n "${mutation_lock_fd:-}" ]; then
        exec {mutation_lock_fd}>&-
    fi
    if [ "${EUID}" -ne 0 ] && [ "$has_sudo" = true ]; then
        sudo "$@"
    else
        "$@"
    fi
)

# 修改与升级共用同一把锁，持锁描述符可交给独立工作进程继续保有
acquire_mutation_lock() {
    if ! command -v flock >/dev/null; then
        echo "修改和升级需要 flock，请先安装对应系统工具。" >&2
        return 1
    fi
    mkdir -p -m 700 "${CONFIG_DIR}" && chmod 700 "${CONFIG_DIR}" || return 1
    exec {mutation_lock_fd}>"${CONFIG_DIR}/.mutation.lock" || return 1
    if ! flock -n "${mutation_lock_fd}"; then
        exec {mutation_lock_fd}>&-
        echo "另一个配置修改或升级事务正在执行，请稍后重试。" >&2
        return 1
    fi
}

validate_mutation_lock() {
    [[ "${mutation_lock_fd:-}" =~ ^[0-9]+$ ]] && flock -n "${mutation_lock_fd}"
}

# 将完整配置原子写入仅 root 可读的文件
write_config_file() {
    local value temp_file
    for value in "$@"; do
        if [[ "${value}" == *$'\n'* || "${value}" == *$'\r'* ]]; then
            echo "配置值不能包含换行符" >&2
            return 1
        fi
    done
    mkdir -p -m 700 "${CONFIG_DIR}" || return 1
    chmod 700 "${CONFIG_DIR}" || return 1
    temp_file=$(mktemp "${CONFIG_DIR}/.brume.conf.XXXXXX") || return 1
    if ! {
        printf 'port=%s\n' "$1"
        printf 'username=%s\n' "$2"
        printf 'password=%s\n' "$3"
        printf 'whitelist=%s\n' "$4"
        printf 'tcp_timeout=%s\n' "$5"
        printf 'udp_timeout=%s\n' "$6"
        if [ "${7:-${DEFAULT_DNS_SERVERS}}" != "${DEFAULT_DNS_SERVERS}" ]; then
            printf 'dns_servers=%s\n' "$7"
        fi
    } > "${temp_file}"; then
        rm -f "${temp_file}"
        return 1
    fi
    chmod 600 "${temp_file}" || { rm -f "${temp_file}"; return 1; }
    mv -f "${temp_file}" "${CONFIG_FILE}" || { rm -f "${temp_file}"; return 1; }
}

# 按字面值读取配置，避免将密码作为 Shell 代码执行
load_config_file() {
    local line key value content file_content
    local config_file=${1:-${CONFIG_FILE}}
    # 先确认整份文件读取成功，避免读取错误被当成空配置
    if [ ! -f "${config_file}" ] || ! file_content=$(cat -- "${config_file}"); then
        echo "读取服务配置文件失败" >&2
        return 1
    fi
    while IFS= read -r line || [ -n "${line}" ]; do
        line=${line%$'\r'}
        content=${line#"${line%%[![:space:]]*}"}
        case "${content}" in
            ''|\#*) continue ;;
            *=*) ;;
            *) echo "配置文件格式无效" >&2; return 1 ;;
        esac
        key=${line%%=*}
        key=${key#"${key%%[![:space:]]*}"}
        key=${key%"${key##*[![:space:]]}"}
        value=${line#*=}
        case "${key}" in
            port) port=${value} ;;
            username) user=${value} ;;
            password) password=${value} ;;
            whitelist) whitelist=${value} ;;
            tcp_timeout) tcp_timeout=${value} ;;
            udp_timeout) udp_timeout=${value} ;;
            dns_servers) dns_servers=${value} ;;
            *) echo "配置文件包含未知配置项" >&2; return 1 ;;
        esac
    done <<< "${file_content}"
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
    echo -e "${BLUE}Brume 服务器本地安装脚本${RESET}"
    echo "使用方法: bash install-local.sh [--binary 本地二进制路径]"
    echo "将 install-local.sh 和 brume 放在同一目录后运行，无需 install.sh 或联网下载。"
    echo "程序安装目录: ${INSTALL_DIR}（默认是脚本所在目录）"
    echo "本地二进制: ${LOCAL_BINARY}"
    echo ""
    echo "此脚本支持以下操作："
    echo "  1. 安装Brume服务器"
    echo "  2. 卸载Brume服务器"
    echo "  3. 修改Brume服务器配置"
    echo "  4. 使用本地二进制更新（可用 --binary 指定 brume.new）"
    echo ""
    echo "支持的服务管理器: systemd / OpenRC / SysVinit"
    echo "没有运行服务管理器时，安装为由容器或外部管理器启动的前台程序"
    echo "支持的防火墙: firewalld / ufw / nftables / iptables"
    echo "服务配置文件: ${CONFIG_FILE}"
    echo ""
    echo "注意：此脚本需要以root用户或使用sudo运行"
    echo "Alpine 需预先安装 Bash；修改和更新还需要 flock、setsid、nohup。"
    exit 0
}

# ============================================================
# 服务管理器检测与适配
# ============================================================

# 检测正在运行的服务管理器；容器里存在管理命令不代表初始化系统已启动。
detect_init_system() {
    local pid1="" manager_version
    if [ -r /proc/1/comm ]; then
        IFS= read -r pid1 < /proc/1/comm || true
    fi

    if [[ "${pid1}" == systemd || "${pid1}" == init ]] && command -v systemctl >/dev/null \
        && manager_version=$(execute_privileged systemctl show --property=Version 2>/dev/null) \
        && [[ "${manager_version}" == Version=?* ]]; then
        echo "systemd"
    # 旧版 rc-status 在 OpenRC 未启动时也会报告 sysinit，以实际启动标记为准。
    elif command -v rc-service >/dev/null && command -v rc-update >/dev/null \
        && { [ -e /run/openrc/softlevel ] || [ -e /lib/rc/init.d/softlevel ]; }; then
        echo "openrc"
    elif [[ "${pid1}" == init || "${pid1}" == sysvinit ]] && [ -d /etc/init.d ] \
        && command -v runlevel >/dev/null && runlevel >/dev/null 2>&1; then
        echo "sysvinit"
    else
        echo "standalone"
    fi
}

# 创建 systemd 服务文件
create_systemd_service() {
    local service_file="/etc/systemd/system/${SERVICE_NAME}.service"

    echo "正在创建 systemd 服务文件..."

    # 创建服务文件
    if ! execute_privileged tee "${service_file}" > /dev/null <<EOF
[Unit]
Description=Brume Proxy Server
After=network.target

[Service]
Type=simple
ExecStart="${INSTALL_DIR}/brume" --config "${CONFIG_FILE}"
Restart=on-failure
RestartSec=5s
TimeoutStopSec=5s
KillMode=control-group

[Install]
WantedBy=multi-user.target
EOF
    then
        echo -e "${RED}创建服务文件失败，请检查权限${RESET}"
        exit 1
    fi

    execute_privileged systemctl daemon-reload || return 1
    execute_privileged systemctl enable "${SERVICE_NAME}" || return 1

    echo -e "${GREEN}systemd 服务创建成功${RESET}"
}

# 创建 OpenRC 服务脚本
create_openrc_service() {
    local service_file="/etc/init.d/${SERVICE_NAME}"

    echo "正在创建 OpenRC 服务脚本..."

    if ! execute_privileged tee "${service_file}" > /dev/null <<EOF
#!/sbin/openrc-run

name="brume-local"
description="Brume Proxy Server"
command="${INSTALL_DIR}/brume"
command_args="--config '${CONFIG_FILE}'"
command_background=true
pidfile="/run/\${RC_SVCNAME}.pid"

# OpenRC 默认启动函数会再次展开 command；本地目录可能含空格，需显式引用路径。
start() {
    ebegin "Starting \${RC_SVCNAME}"
    eval "set -- \${command_args}"
    start-stop-daemon --start --exec "\${command}" --pidfile "\${pidfile}" \
        --background --make-pidfile -- "\$@"
    eend \$?
}

stop() {
    ebegin "Stopping \${RC_SVCNAME}"
    start-stop-daemon --stop --exec "\${command}" --pidfile "\${pidfile}"
    eend \$?
}

depend() {
    need net
    after firewall
}
EOF
    then
        echo -e "${RED}创建 OpenRC 服务脚本失败${RESET}"
        exit 1
    fi

    execute_privileged chmod +x "${service_file}" || return 1
    execute_privileged rc-update add "${SERVICE_NAME}" default 2>/dev/null || return 1

    echo -e "${GREEN}OpenRC 服务创建成功${RESET}"
}

# 创建 SysVinit 服务脚本
create_sysvinit_service() {
    local service_file="/etc/init.d/${SERVICE_NAME}"

    echo "正在创建 SysVinit 服务脚本..."

    if ! execute_privileged tee "${service_file}" > /dev/null <<'OUTER_EOF'
#!/bin/bash
### BEGIN INIT INFO
# Provides:          brume-local
# Required-Start:    $network $remote_fs
# Required-Stop:     $network $remote_fs
# Default-Start:     2 3 4 5
# Default-Stop:      0 1 6
# Short-Description: Brume Proxy Server
# Description:       Brume Proxy Server
### END INIT INFO

DAEMON="PLACEHOLDER_INSTALL_DIR/brume"
DAEMON_ARGS="--config 'PLACEHOLDER_CONFIG_FILE'"
DAEMON_CONFIG="PLACEHOLDER_CONFIG_FILE"
PIDFILE="/var/run/brume-local.pid"
NAME="brume-local"

start() {
    echo "Starting $NAME..."
    if [ -f "$PIDFILE" ] && kill -0 "$(cat "$PIDFILE")" 2>/dev/null; then
        echo "$NAME is already running."
        return 1
    fi
    nohup "$DAEMON" --config "$DAEMON_CONFIG" > /dev/null 2>&1 &
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
    execute_privileged sed -i "s|PLACEHOLDER_INSTALL_DIR|${INSTALL_DIR}|g" "${service_file}" || return 1
    execute_privileged sed -i "s|PLACEHOLDER_CONFIG_FILE|${CONFIG_FILE}|g" "${service_file}" || return 1

    execute_privileged chmod +x "${service_file}" || return 1

    # 尝试使用 update-rc.d 或 chkconfig 注册开机自启
    if command -v update-rc.d &> /dev/null; then
        execute_privileged update-rc.d "${SERVICE_NAME}" defaults 2>/dev/null || return 1
    elif command -v chkconfig &> /dev/null; then
        execute_privileged chkconfig --add "${SERVICE_NAME}" 2>/dev/null || return 1
        execute_privileged chkconfig "${SERVICE_NAME}" on 2>/dev/null || return 1
    fi

    echo -e "${GREEN}SysVinit 服务创建成功${RESET}"
}

# 根据检测到的 init 系统创建服务
create_service() {
    local init_system=$1

    case "${init_system}" in
        systemd)
            create_systemd_service
            ;;
        openrc)
            create_openrc_service
            ;;
        sysvinit)
            create_sysvinit_service
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

# 独立工作进程完成升级重启及状态检查后才释放事务锁
schedule_upgrade_restart() (
    local init_system=$1 transaction_dir log_file scheduled=false
    transaction_dir=$(mktemp -d "${CONFIG_DIR}/.upgrade.XXXXXX") || return 1
    trap 'if [ "${scheduled}" != true ] && [ ! -f "${transaction_dir}/go" ]; then cleanup_modify_transaction "${transaction_dir}"; fi' EXIT
    cp -- "${BASH_SOURCE[0]}" "${transaction_dir}/installer.sh" || return 1
    log_file=$(mktemp "${CONFIG_DIR}/upgrade.log.XXXXXX") || return 1
    schedule_independent_worker "${transaction_dir}" "${init_system}" "${log_file}" --restart-upgrade || return 1
    scheduled=true
    echo "正在安排服务重启；经过 Brume 的 SSH 连接可能短暂断开，请稍后重连。"
    echo "升级重启结果日志：${log_file}"
)

require_independent_worker_tools() {
    if ! command -v setsid >/dev/null || ! command -v nohup >/dev/null; then
        echo "修改和升级需要 setsid 和 nohup，请先安装对应系统工具。" >&2
        return 1
    fi
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

# 检查 UFW 是否已经由管理员启用
is_ufw_active() {
    local status

    command -v ufw &> /dev/null || return 1
    status=$(execute_privileged env LC_ALL=C ufw status 2>/dev/null | head -n1)
    [[ "${status}" =~ ^Status:[[:space:]]+active$ ]]
}

# 获取 SSH 服务使用的端口，始终保护标准 SSH 端口
get_ssh_ports() {
    {
        echo "22"

        # 优先读取当前远程会话实际连接的服务端口
        if [ -n "${SSH_CONNECTION:-}" ]; then
            echo "${SSH_CONNECTION}" | awk 'NF >= 4 && $4 ~ /^[0-9]+$/ { print $4 }'
        elif [ -n "${SSH_CLIENT:-}" ]; then
            echo "${SSH_CLIENT}" | awk 'NF >= 3 && $3 ~ /^[0-9]+$/ { print $3 }'
        fi

        # 部分发行版的 root PATH 不含 /usr/sbin，仍需读取 SSH 配置端口
        if command -v sshd &> /dev/null; then
            execute_privileged sshd -T 2>/dev/null | awk '$1 == "port" { print $2 }'
        elif [ -x /usr/sbin/sshd ]; then
            execute_privileged /usr/sbin/sshd -T 2>/dev/null | awk '$1 == "port" { print $2 }'
        elif [ -x /sbin/sshd ]; then
            execute_privileged /sbin/sshd -T 2>/dev/null | awk '$1 == "port" { print $2 }'
        fi

        if command -v ss &> /dev/null; then
            execute_privileged ss -H -ltnp 2>/dev/null | awk '
                /sshd|dropbear/ {
                    port = $4
                    sub(/^.*:/, "", port)
                    if (port ~ /^[0-9]+$/) print port
                }
            '
        fi
    } | sort -nu
}

# 防止防火墙规则误伤 SSH 服务
validate_firewall_port() {
    local port=$1
    local ssh_port

    while IFS= read -r ssh_port; do
        if [ "${port}" = "${ssh_port}" ]; then
            echo -e "${RED}端口 ${port} 是标准 SSH 端口或已被 SSH 服务使用${RESET}"
            echo -e "${YELLOW}请为 Brume 选择其他端口，避免服务器失去远程连接${RESET}"
            return 1
        fi
    done < <(get_ssh_ports)

    return 0
}

# 检测可用的防火墙工具
# 返回: firewalld / ufw / nftables / iptables / none
detect_firewall() {
    # 仅使用已启用的 UFW，避免脚本激活全局入站策略
    if command -v firewall-cmd &> /dev/null && execute_privileged firewall-cmd --state &> /dev/null; then
        echo "firewalld"
    elif is_ufw_active; then
        echo "ufw"
    elif command -v nft >/dev/null && execute_privileged nft list tables &>/dev/null; then
        echo "nftables"
    elif command -v iptables >/dev/null && execute_privileged iptables -S &>/dev/null \
        && { ! command -v ip6tables >/dev/null || execute_privileged ip6tables -S &>/dev/null; }; then
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
    remove_firewall_firewalld "${port}" 2>/dev/null || return 1

    # 创建专用 zone
    execute_privileged firewall-cmd --permanent --new-zone=brume-local 2>/dev/null || return 1

    # 解析白名单并添加 rich rules
    IFS=',' read -ra IPS <<< "${whitelist}"
    for ip in "${IPS[@]}"; do
        ip=$(echo "${ip}" | xargs)  # 去除空格
        [ -z "${ip}" ] && continue
        local family=ipv4
        [[ "${ip}" == *:* ]] && family=ipv6
        echo "  允许 ${ip} 访问端口 ${port}..."
        execute_privileged firewall-cmd --permanent --zone=brume-local \
            --add-rich-rule="rule family=\"${family}\" source address=\"${ip}\" port port=\"${port}\" protocol=\"tcp\" accept" || return 1
        execute_privileged firewall-cmd --permanent --zone=brume-local \
            --add-rich-rule="rule family=\"${family}\" source address=\"${ip}\" port port=\"${port}\" protocol=\"udp\" accept" || return 1
    done

    # 将接口绑定到 brume zone（如果需要可以指定接口）
    # 使用默认 zone 来添加拒绝规则
    for family in ipv4 ipv6; do
        execute_privileged firewall-cmd --permanent --zone=public \
            --add-rich-rule="rule priority=\"10\" family=\"${family}\" port port=\"${port}\" protocol=\"tcp\" drop" || return 1
        execute_privileged firewall-cmd --permanent --zone=public \
            --add-rich-rule="rule priority=\"10\" family=\"${family}\" port port=\"${port}\" protocol=\"udp\" drop" || return 1
    done

    # 在 public zone 中添加白名单 IP 的放行规则（优先级更高）
    for ip in "${IPS[@]}"; do
        ip=$(echo "${ip}" | xargs)
        [ -z "${ip}" ] && continue
        local family=ipv4
        [[ "${ip}" == *:* ]] && family=ipv6
        execute_privileged firewall-cmd --permanent --zone=public \
            --add-rich-rule="rule priority=\"-10\" family=\"${family}\" source address=\"${ip}\" port port=\"${port}\" protocol=\"tcp\" accept" || return 1
        execute_privileged firewall-cmd --permanent --zone=public \
            --add-rich-rule="rule priority=\"-10\" family=\"${family}\" source address=\"${ip}\" port port=\"${port}\" protocol=\"udp\" accept" || return 1
    done

    execute_privileged firewall-cmd --reload || return 1

    echo -e "${GREEN}firewalld 防火墙规则配置完成${RESET}"
}

# 使用 ufw 设置防火墙规则
setup_firewall_ufw() {
    local port=$1
    local whitelist=$2

    echo -e "${CYAN}使用 ufw 配置防火墙规则...${RESET}"

    # 不主动启用 UFW，避免其全局默认策略阻断 SSH 等现有服务
    if ! is_ufw_active; then
        echo -e "${YELLOW}UFW 当前未启用，跳过 UFW 防火墙配置${RESET}"
        return 1
    fi

    # 先清除已有规则
    remove_firewall_ufw "${port}" 2>/dev/null || return 1

    # 添加白名单 IP 的放行规则
    IFS=',' read -ra IPS <<< "${whitelist}"
    for ip in "${IPS[@]}"; do
        ip=$(echo "${ip}" | xargs)
        [ -z "${ip}" ] && continue
        echo "  允许 ${ip} 访问端口 ${port}..."
        # 添加注释标识规则来源
        execute_privileged ufw allow from "${ip}" to any port "${port}" proto tcp comment "brume-local-whitelist" || return 1
        execute_privileged ufw allow from "${ip}" to any port "${port}" proto udp comment "brume-local-whitelist" || return 1
    done

    # 拒绝其他所有连接到该端口
    execute_privileged ufw deny to any port "${port}" proto tcp comment "brume-local-deny-default" || return 1
    execute_privileged ufw deny to any port "${port}" proto udp comment "brume-local-deny-default" || return 1

    echo -e "${GREEN}ufw 防火墙规则配置完成${RESET}"
}

# 使用 nftables 设置防火墙规则
setup_firewall_nftables() {
    local port=$1
    local whitelist=$2

    echo -e "${CYAN}使用 nftables 配置防火墙规则...${RESET}"

    # 先清除已有规则
    remove_firewall_nftables 2>/dev/null || return 1

    # 创建 Brume 专用规则表
    execute_privileged nft add table inet brume_local || return 1
    execute_privileged nft add chain inet brume_local input '{ type filter hook input priority 0; policy accept; }' || return 1

    # 添加白名单 IP 的放行规则
    IFS=',' read -ra IPS <<< "${whitelist}"
    for ip in "${IPS[@]}"; do
        ip=$(echo "${ip}" | xargs)
        [ -z "${ip}" ] && continue
        local address_family=ip
        [[ "${ip}" == *:* ]] && address_family=ip6
        echo "  允许 ${ip} 访问端口 ${port}..."
        execute_privileged nft add rule inet brume_local input "${address_family}" saddr "${ip}" tcp dport "${port}" accept || return 1
        execute_privileged nft add rule inet brume_local input "${address_family}" saddr "${ip}" udp dport "${port}" accept || return 1
    done

    # 拒绝其他所有到该端口的连接
    execute_privileged nft add rule inet brume_local input tcp dport "${port}" drop || return 1
    execute_privileged nft add rule inet brume_local input udp dport "${port}" drop || return 1

    # 导出失败时保留原配置，临时文件只允许当前用户访问
    persist_nftables_rules /etc/nftables.conf || return 1

    echo -e "${GREEN}nftables 防火墙规则配置完成${RESET}"
}

# 使用 iptables 设置防火墙规则
setup_firewall_iptables() {
    local port=$1
    local whitelist=$2
    local has_ip6tables=false

    if command -v ip6tables &> /dev/null; then
        has_ip6tables=true
    elif [[ "${whitelist}" == *:* ]]; then
        echo -e "${RED}IPv6 白名单需要 ip6tables${RESET}" >&2
        return 1
    else
        echo -e "${YELLOW}未找到 ip6tables，无法配置 IPv6 网络层规则${RESET}" >&2
    fi

    echo -e "${CYAN}使用 iptables 配置防火墙规则...${RESET}"

    # 先清除已有规则
    remove_firewall_iptables "${port}" 2>/dev/null || return 1

    # 创建自定义 chain
    execute_privileged iptables -N "${IPTABLES_CHAIN}" 2>/dev/null || return 1
    if [ "${has_ip6tables}" = true ]; then
        execute_privileged ip6tables -N "${IPTABLES_CHAIN}" 2>/dev/null || return 1
    fi

    # 将针对目标端口的流量导向自定义 chain
    execute_privileged iptables -I INPUT -p tcp --dport "${port}" -j "${IPTABLES_CHAIN}" || return 1
    execute_privileged iptables -I INPUT -p udp --dport "${port}" -j "${IPTABLES_CHAIN}" || return 1
    if [ "${has_ip6tables}" = true ]; then
        execute_privileged ip6tables -I INPUT -p tcp --dport "${port}" -j "${IPTABLES_CHAIN}" || return 1
        execute_privileged ip6tables -I INPUT -p udp --dport "${port}" -j "${IPTABLES_CHAIN}" || return 1
    fi

    # 添加白名单 IP 的放行规则
    IFS=',' read -ra IPS <<< "${whitelist}"
    for ip in "${IPS[@]}"; do
        ip=$(echo "${ip}" | xargs)
        [ -z "${ip}" ] && continue
        echo "  允许 ${ip} 访问端口 ${port}..."
        if [[ "${ip}" == *:* ]]; then
            execute_privileged ip6tables -A "${IPTABLES_CHAIN}" -s "${ip}" -p tcp --dport "${port}" -j ACCEPT || return 1
            execute_privileged ip6tables -A "${IPTABLES_CHAIN}" -s "${ip}" -p udp --dport "${port}" -j ACCEPT || return 1
        else
            execute_privileged iptables -A "${IPTABLES_CHAIN}" -s "${ip}" -p tcp --dport "${port}" -j ACCEPT || return 1
            execute_privileged iptables -A "${IPTABLES_CHAIN}" -s "${ip}" -p udp --dport "${port}" -j ACCEPT || return 1
        fi
    done

    # 拒绝其他所有到该端口的连接
    execute_privileged iptables -A "${IPTABLES_CHAIN}" -p tcp --dport "${port}" -j DROP || return 1
    execute_privileged iptables -A "${IPTABLES_CHAIN}" -p udp --dport "${port}" -j DROP || return 1
    if [ "${has_ip6tables}" = true ]; then
        execute_privileged ip6tables -A "${IPTABLES_CHAIN}" -p tcp --dport "${port}" -j DROP || return 1
        execute_privileged ip6tables -A "${IPTABLES_CHAIN}" -p udp --dport "${port}" -j DROP || return 1
    fi

    # 持久化 iptables 规则
    persist_iptables_rules || return 1

    echo -e "${GREEN}iptables 防火墙规则配置完成${RESET}"
}

# 使用私有临时文件保存规则，退出或中断时均清理文件
persist_nftables_rules() (
    local config_file=$1
    local temp_file

    [ -f "${config_file}" ] || return 0
    temp_file=$(mktemp) || return 1
    trap 'rm -f -- "${temp_file}"' EXIT
    trap 'exit 1' HUP INT TERM

    if ! execute_privileged nft list ruleset > "${temp_file}"; then
        echo "导出 nftables 规则失败，已保留原持久化配置" >&2
        return 1
    fi
    execute_privileged cp -- "${temp_file}" "${config_file}"
)

# 持久化 iptables 规则
persist_iptables_rules() {
    if command -v netfilter-persistent &> /dev/null; then
        execute_privileged netfilter-persistent save 2>/dev/null || return 1
    elif command -v iptables-save &> /dev/null; then
        # Debian/Ubuntu
        if [ -d /etc/iptables ]; then
            execute_privileged sh -c "iptables-save > /etc/iptables/rules.v4" || return 1
            if command -v ip6tables-save &> /dev/null; then
                execute_privileged sh -c "ip6tables-save > /etc/iptables/rules.v6" || return 1
            fi
        # CentOS/RHEL
        elif [ -d /etc/sysconfig ]; then
            execute_privileged sh -c "iptables-save > /etc/sysconfig/iptables" || return 1
            if command -v ip6tables-save &> /dev/null; then
                execute_privileged sh -c "ip6tables-save > /etc/sysconfig/ip6tables" || return 1
            fi
        fi
    fi
}

# ============================================================
# 防火墙规则移除
# ============================================================

# 移除 firewalld 规则
remove_firewall_firewalld() {
    local port=$1 rules rule zones zone

    echo -e "${CYAN}移除 firewalld 防火墙规则...${RESET}"

    # 如果不知道端口，尝试从服务中获取
    if [ -z "${port}" ]; then
        port=$(get_current_port) || return 1
    fi

    if [ -n "${port}" ]; then
        # 移除 public zone 中所有 brume 相关的 rich rules
        rules=$(execute_privileged firewall-cmd --permanent --zone=public --list-rich-rules) || return 1
        while IFS= read -r rule; do
            [[ "${rule}" == *"port=\"${port}\""* ]] || continue
            execute_privileged firewall-cmd --permanent --zone=public --remove-rich-rule="${rule}" || return 1
        done <<< "${rules}"
    fi

    # 先查询再删除，只有确认区域不存在时才按幂等成功处理。
    zones=$(execute_privileged firewall-cmd --permanent --get-zones) || return 1
    for zone in ${zones}; do
        if [ "${zone}" = brume-local ]; then
            execute_privileged firewall-cmd --permanent --delete-zone=brume-local || return 1
            break
        fi
    done
    execute_privileged firewall-cmd --reload || return 1

    echo -e "${GREEN}firewalld 规则已清除${RESET}"
}

# 移除 ufw 规则
remove_firewall_ufw() {
    local port=$1 status line number rule_nums
    local -a matching_rules=()

    echo -e "${CYAN}移除 ufw 防火墙规则...${RESET}"

    if [ -z "${port}" ]; then
        port=$(get_current_port) || return 1
    fi

    if [ -n "${port}" ]; then
        # 仅删除带有 Brume 注释的规则，避免误删同端口的现有规则
        status=$(execute_privileged env LC_ALL=C ufw status numbered) || return 1
        while IFS= read -r line; do
            if [[ "${line}" =~ ^\[[[:space:]]*([0-9]+)\][[:space:]]+${port}/(tcp|udp)([[:space:]]|$) ]]; then
                number=${BASH_REMATCH[1]}
                if [[ "${line}" =~ \#\ brume-local-(whitelist|deny-default)([[:space:]]|$) ]]; then
                    matching_rules+=("${number}")
                fi
            fi
        done <<< "${status}"
        if [ "${#matching_rules[@]}" -gt 0 ]; then
            rule_nums=$(printf '%s\n' "${matching_rules[@]}" | sort -rn) || return 1
            for number in ${rule_nums}; do
                execute_privileged ufw --force delete "${number}" || return 1
            done
        fi
    fi

    echo -e "${GREEN}ufw 规则已清除${RESET}"
}

# 移除 nftables 规则
remove_firewall_nftables() {
    local tables kind family name extra
    echo -e "${CYAN}移除 nftables 防火墙规则...${RESET}"

    # 查询失败与不存在不同，只有查询成功且没有目标表时才跳过删除。
    tables=$(execute_privileged nft list tables) || return 1
    while read -r kind family name extra; do
        if [ "${kind}" = table ] && [ "${family}" = inet ] && [ "${name}" = brume_local ]; then
            execute_privileged nft delete table inet brume_local || return 1
            break
        fi
    done <<< "${tables}"

    # 更新持久化配置，导出失败时不覆盖原文件
    persist_nftables_rules /etc/nftables.conf || return 1

    echo -e "${GREEN}nftables 规则已清除${RESET}"
}

# 移除 iptables 规则
remove_firewall_iptables() {
    local port=$1 backend rules line chain_exists jump_matches port_matches index
    local -a arguments

    echo -e "${CYAN}移除 iptables 防火墙规则...${RESET}"

    if [ -z "${port}" ]; then
        port=$(get_current_port) || return 1
    fi

    for backend in iptables ip6tables; do
        if [ "${backend}" = ip6tables ] && ! command -v ip6tables >/dev/null; then
            continue
        fi
        # 一次查询同时确认规则及链是否存在，避免把权限错误当成规则缺失。
        rules=$(execute_privileged "${backend}" -S) || return 1
        chain_exists=false
        while IFS= read -r line; do
            if [ "${line}" = "-N ${IPTABLES_CHAIN}" ]; then
                chain_exists=true
                continue
            fi
            read -r -a arguments <<< "${line}"
            [ "${#arguments[@]}" -ge 2 ] || continue
            [ "${arguments[0]}" = -A ] && [ "${arguments[1]}" = INPUT ] || continue
            jump_matches=false port_matches=false
            [ -n "${port}" ] || port_matches=true
            for ((index=2; index+1<${#arguments[@]}; index++)); do
                if [ "${arguments[index]}" = -j ] && [ "${arguments[index+1]}" = "${IPTABLES_CHAIN}" ]; then
                    jump_matches=true
                elif [ "${arguments[index]}" = --dport ] && [ "${arguments[index+1]}" = "${port}" ]; then
                    port_matches=true
                fi
            done
            if [ "${jump_matches}" = true ] && [ "${port_matches}" = true ]; then
                # 参数只按字面值传递；模糊的自定义规则会明确失败，不执行任何展开。
                arguments[0]=-D
                execute_privileged "${backend}" "${arguments[@]}" || return 1
            fi
        done <<< "${rules}"
        if [ "${chain_exists}" = true ]; then
            execute_privileged "${backend}" -F "${IPTABLES_CHAIN}" || return 1
            execute_privileged "${backend}" -X "${IPTABLES_CHAIN}" || return 1
        fi
    done

    # 更新持久化规则
    persist_iptables_rules || return 1

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

    if ! validate_firewall_port "${port}"; then
        return 1
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
            echo -e "${YELLOW}当前环境无法配置系统防火墙，IP 白名单由 Brume 执行。${RESET}"
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

binary_supports_check_config() {
    local help
    help=$("$1" --help 2>&1) || return 1
    [[ "${help}" == *--check-config* ]]
}

prepare_binary() {
    local help
    if [ ! -f "${LOCAL_BINARY}" ]; then
        echo -e "${RED}未找到本地二进制：${LOCAL_BINARY}${RESET}" >&2
        echo "请将 brume 与本脚本放在同一目录，或使用 --binary 指定文件。" >&2
        return 1
    fi
    # 精简镜像可能缺少安装目录，容器的 /tmp 也可能挂载为 noexec。
    execute_privileged mkdir -p "${INSTALL_DIR}" || return 1
    prepared_dir=$(mktemp -d "${INSTALL_DIR}/.brume-local.XXXXXX") || return 1
    echo "读取本地二进制：${LOCAL_BINARY}"
    # 先复制到安装目录再校验，兼容源文件与目标相同及 /tmp 的 noexec 挂载。
    if ! cp -- "${LOCAL_BINARY}" "${prepared_dir}/brume" \
        || ! chmod 755 "${prepared_dir}/brume"; then
        echo -e "${RED}准备本地二进制失败${RESET}" >&2
        rm -rf "${prepared_dir}"
        return 1
    fi
    if ! help=$("${prepared_dir}/brume" --help 2>&1); then
        echo -e "${RED}本地 Brume 程序无法执行：${help}${RESET}" >&2
        echo "请检查 Linux 架构和运行库；Debian、Alpine 建议使用 Release 中的 musl 版本。" >&2
        rm -rf "${prepared_dir}"
        return 1
    fi
    # 拒绝不支持配置文件的旧版程序，避免回退到命令行传递密码
    if [[ "${help}" != *--config* ]]; then
        echo -e "${RED}该版本不支持配置文件，请选择包含 --config 功能的新版本${RESET}"
        rm -rf "${prepared_dir}"
        return 1
    fi

    # 当前配置显式使用自定义 DNS 时，拒绝安装不兼容的旧版本
    if [ "${dns_servers:-${DEFAULT_DNS_SERVERS}}" != "${DEFAULT_DNS_SERVERS}" ] \
        && [[ "${help}" != *--dns-servers* ]]; then
        echo -e "${RED}该版本不支持 dns_servers，请选择包含 --dns-servers 功能的新版本${RESET}"
        rm -rf "${prepared_dir}"
        return 1
    fi
}

# 安装已经验证过的二进制文件
install_prepared_binary() {
    local staged_binary
    echo "安装到 ${INSTALL_DIR}..."
    staged_binary=$(mktemp "${INSTALL_DIR}/.brume.XXXXXX") || { rm -rf "${prepared_dir}"; return 1; }
    if ! execute_privileged cp "${prepared_dir}/brume" "${staged_binary}" \
        || ! execute_privileged chmod 755 "${staged_binary}" \
        || ! execute_privileged mv -f "${staged_binary}" "${INSTALL_DIR}/brume"; then
        echo -e "${RED}安装失败，请检查权限${RESET}"
        rm -f "${staged_binary}"
        rm -rf "${prepared_dir}"
        return 1
    fi

    rm -rf "${prepared_dir}"
    prepared_dir=""
    echo -e "${GREEN}安装成功${RESET}"
}

install_local_binary() {
    prepare_binary || return 1
    install_prepared_binary
}

# 从当前服务配置中获取端口号
get_current_port() {
    local port user password whitelist tcp_timeout udp_timeout uses_config_file
    get_current_config_auto "$(detect_init_system)" || return 1
    echo "${port}"
}

# ============================================================
# 启动服务（安装后展示信息）
# ============================================================

start_service() {
    local init_system=$1

    if [ "${init_system}" != standalone ]; then
        start_service_by_init "${init_system}" || return 1
    elif binary_supports_check_config "${INSTALL_DIR}/brume"; then
        "${INSTALL_DIR}/brume" --config "${CONFIG_FILE}" --check-config || return 1
    fi

    echo -e "${BLUE}Brume 服务器安装完成！${RESET}"
    echo "配置信息:"
    echo "  端口: ${port}"
    echo "  认证: $(if [ -n "${user}" ] && [ -n "${password}" ]; then echo "启用"; else echo "禁用"; fi)"
    echo "  白名单: $(if [ -n "${whitelist}" ]; then echo "${whitelist}"; else echo "无（允许所有IP）"; fi)"
    echo "  服务管理器: ${init_system}"
    if [ "${init_system}" = standalone ]; then
        echo "  防火墙: 由容器或宿主管理"
    else
        echo "  防火墙: $(detect_firewall)"
    fi
    echo "  配置文件: ${CONFIG_FILE}"

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
        standalone)
            echo "  前台启动（可用作容器启动命令）: exec '${INSTALL_DIR}/brume' --config '${CONFIG_FILE}'"
            ;;
    esac
}

# ============================================================
# 修改配置函数
# ============================================================

# 事务目录只包含已知文件，避免递归删除不可确认的路径
cleanup_modify_transaction() {
    local transaction_dir=$1
    [ -d "${transaction_dir}" ] || return 0
    if [ -f "${transaction_dir}/recovery-needed" ]; then
        echo "恢复未完成，已保留私有备份目录：${transaction_dir}" >&2
        return 1
    fi
    rm -f -- "${transaction_dir}/new.conf" "${transaction_dir}/old-effective.conf" \
        "${transaction_dir}/previous.conf" "${transaction_dir}/previous.service" \
        "${transaction_dir}/had-config" "${transaction_dir}/installer.sh" \
        "${transaction_dir}/ready" "${transaction_dir}/go"
    rmdir -- "${transaction_dir}"
}

check_service_running() {
    case "$1" in
        systemd) execute_privileged systemctl is-active --quiet "${SERVICE_NAME}" ;;
        openrc) execute_privileged rc-service "${SERVICE_NAME}" status ;;
        sysvinit) execute_privileged /etc/init.d/"${SERVICE_NAME}" status ;;
        *) return 1 ;;
    esac
}

# 每个恢复阶段单独尝试，单项失败不会阻断其余恢复操作
rollback_modify_transaction() {
    local transaction_dir=$1 init_system=$2 service_file=$3
    local firewall_changed=$4 restart_attempted=$5 new_port=$6 old_port=$7 old_whitelist=$8
    local failed=false
    echo "修改配置失败，正在恢复原配置和服务。" >&2
    if [ -f "${transaction_dir}/had-config" ]; then
        cp -- "${transaction_dir}/previous.conf" "${CONFIG_FILE}" \
            || { failed=true; echo "恢复配置文件失败，请手动检查。" >&2; }
    else
        rm -f -- "${CONFIG_FILE}" || { failed=true; echo "清理新配置文件失败。" >&2; }
    fi
    cp -p -- "${transaction_dir}/previous.service" "${service_file}" \
        || { failed=true; echo "恢复服务定义失败，请手动检查。" >&2; }
    if [ "${init_system}" = systemd ]; then
        execute_privileged systemctl daemon-reload || { failed=true; echo "重新加载原服务定义失败。" >&2; }
    fi
    if [ "${firewall_changed}" = true ]; then
        (
            trap - EXIT HUP INT TERM
            local restore_failed=false
            remove_firewall "${new_port}" || { restore_failed=true; echo "清理候选防火墙规则失败，继续恢复旧规则。" >&2; }
            setup_firewall "${old_port}" "${old_whitelist}" || restore_failed=true
            [ "${restore_failed}" = false ]
        ) || { failed=true; echo "恢复防火墙失败，请手动检查。" >&2; }
    fi
    if [ "${restart_attempted}" = true ]; then
        (
            trap - EXIT HUP INT TERM
            restart_service_by_init "${init_system}" || return 1
            sleep 1
            check_service_running "${init_system}"
        ) || { failed=true; echo "恢复原服务失败，请手动检查。" >&2; }
    fi
    [ "${failed}" = false ]
}

# 应用与回滚均由独立进程执行，当前 SSH 断开不影响事务完成
apply_modify_transaction() (
    local transaction_dir=$1 init_system=$2 service_file
    local old_port old_whitelist new_port new_whitelist
    local firewall_changed=false restart_attempted=false config_changed=false committed=false result preserve_transaction=false
    trap 'result=$?; trap - EXIT HUP INT TERM;
        if [ "${committed}" != true ] && [ "${config_changed}" = true ]; then
            if ! rollback_modify_transaction "${transaction_dir}" "${init_system}" "${service_file}" \
                "${firewall_changed}" "${restart_attempted}" "${new_port}" "${old_port}" "${old_whitelist}"; then
                preserve_transaction=true;
                touch "${transaction_dir}/recovery-needed";
                echo "恢复未完成，私有备份保留于：${transaction_dir}" >&2;
            fi;
            result=1;
        fi;
        if [ "${preserve_transaction}" != true ]; then cleanup_modify_transaction "${transaction_dir}" || result=1; fi;
        exit "${result}"' EXIT
    trap 'exit 1' HUP INT TERM

    service_file=$(get_service_file "${init_system}") || return 1
    dns_servers=${DEFAULT_DNS_SERVERS}
    load_config_file "${transaction_dir}/old-effective.conf" || return 1
    old_port=${port} old_whitelist=${whitelist}
    dns_servers=${DEFAULT_DNS_SERVERS}
    load_config_file "${transaction_dir}/new.conf" || return 1
    new_port=${port} new_whitelist=${whitelist}
    config_changed=true
    write_config_file "${port}" "${user}" "${password}" "${whitelist}" "${tcp_timeout}" "${udp_timeout}" "${dns_servers}" || return 1
    create_service "${init_system}" || return 1
    if [ "${old_port}" != "${new_port}" ] || [ "${old_whitelist}" != "${new_whitelist}" ]; then
        firewall_changed=true
        if [ -n "${old_whitelist}" ]; then
            remove_firewall "${old_port}" || return 1
        fi
        setup_firewall "${new_port}" "${new_whitelist}" || return 1
    fi
    restart_attempted=true
    restart_service_by_init "${init_system}" || return 1
    sleep 1
    check_service_running "${init_system}" || return 1
    committed=true
    echo "Brume 配置修改完成，服务已重新启动。"
)

# 调度进程与工作进程握手后才允许开始应用，失败时不会触碰现有配置
schedule_independent_worker() {
    local transaction_dir=$1 init_system=$2 log_file=$3 worker_action=$4 worker_pid attempt
    require_independent_worker_tools || return 1
    BRUME_LOCAL_INSTALL_DIR="${INSTALL_DIR}" BRUME_LOCAL_BINARY="${LOCAL_BINARY}" \
        nohup setsid bash "${transaction_dir}/installer.sh" "${worker_action}" "${transaction_dir}" "${init_system}" "${mutation_lock_fd}" \
        </dev/null >"${log_file}" 2>&1 &
    worker_pid=$!
    for ((attempt=0; attempt<50; attempt++)); do
        if [ -f "${transaction_dir}/ready" ]; then
            touch "${transaction_dir}/go" && return 0
            break
        fi
        kill -0 "${worker_pid}" 2>/dev/null || break
        sleep 0.1
    done
    kill "${worker_pid}" 2>/dev/null || true
    wait "${worker_pid}" 2>/dev/null || true
    echo "无法启动独立配置工作进程，现有服务保持运行。" >&2
    return 1
}

schedule_modify_apply() {
    schedule_independent_worker "$1" "$2" "$3" --apply-modify
}

run_independent_worker() {
    local transaction_dir=$1 init_system=$2 worker_action=$4 prefix attempt
    mutation_lock_fd=$3
    case "${worker_action}" in
        --apply-modify) prefix=.modify. ;;
        --restart-upgrade) prefix=.upgrade. ;;
        *) return 1 ;;
    esac
    # 工作目录必须是配置目录内由 root 独占的真实目录
    if [ "$(id -u)" != 0 ] || [[ "${transaction_dir}" != "${CONFIG_DIR}/${prefix}"* ]] \
        || [ "${transaction_dir%/*}" != "${CONFIG_DIR}" ] \
        || [ ! -d "${transaction_dir}" ] || [ -L "${transaction_dir}" ] \
        || [ "$(stat -c '%u:%a' "${transaction_dir}")" != 0:700 ]; then
        echo "配置事务目录不安全，拒绝应用。" >&2
        return 1
    fi
    validate_mutation_lock || { echo "工作进程没有继承有效事务锁，拒绝应用。" >&2; return 1; }
    touch "${transaction_dir}/ready" || return 1
    for ((attempt=0; attempt<100; attempt++)); do
        if [ -f "${transaction_dir}/go" ]; then
            if [ "${worker_action}" = --apply-modify ]; then
                apply_modify_transaction "${transaction_dir}" "${init_system}"
            else
                (
                    trap 'result=$?; cleanup_modify_transaction "${transaction_dir}"; exit "${result}"' EXIT
                    restart_service_by_init "${init_system}" || return 1
                    sleep 1
                    check_service_running "${init_system}" || return 1
                    echo "升级服务已重启并通过状态检查。"
                )
            fi
            return
        fi
        sleep 0.1
    done
    cleanup_modify_transaction "${transaction_dir}"
    echo "配置事务未获调度确认，已取消。" >&2
    return 1
}

modify() (
    local init_system=$1 transaction_dir="" log_file="" service_file scheduled=false
    local -a new_config=("${port}" "${user}" "${password}" "${whitelist}" "${tcp_timeout}" "${udp_timeout}" "${dns_servers}") old_config
    acquire_mutation_lock || return 1
    if ! binary_supports_check_config "${INSTALL_DIR}/brume"; then
        echo "当前程序不支持完整配置校验，请先升级到包含 --check-config 的版本。" >&2
        return 1
    fi
    get_current_config_auto "${init_system}" || return 1
    if [ "${init_system}" != standalone ]; then
        require_independent_worker_tools || return 1
        old_config=("${port}" "${user}" "${password}" "${whitelist}" "${tcp_timeout}" "${udp_timeout}" "${dns_servers}")
        service_file=$(get_service_file "${init_system}") || return 1
    fi
    mkdir -p -m 700 "${CONFIG_DIR}" && chmod 700 "${CONFIG_DIR}" || return 1
    transaction_dir=$(mktemp -d "${CONFIG_DIR}/.modify.XXXXXX") || return 1
    trap 'if [ "${scheduled}" != true ] && [ ! -f "${transaction_dir}/go" ]; then cleanup_modify_transaction "${transaction_dir}"; fi' EXIT
    (
        CONFIG_DIR=${transaction_dir} CONFIG_FILE="${transaction_dir}/new.conf"
        write_config_file "${new_config[@]}" || return 1
        if [ "${init_system}" != standalone ]; then
            CONFIG_FILE="${transaction_dir}/old-effective.conf"
            write_config_file "${old_config[@]}"
        fi
    ) || return 1
    if ! "${INSTALL_DIR}/brume" --config "${transaction_dir}/new.conf" --check-config; then
        echo "候选配置校验失败，保留原配置。" >&2
        return 1
    fi
    if [ "${init_system}" = standalone ]; then
        mv -f -- "${transaction_dir}/new.conf" "${CONFIG_FILE}" || return 1
        echo "Brume 配置已保存，请通过容器或进程管理器重启后生效。"
        return 0
    fi
    cp -p -- "${service_file}" "${transaction_dir}/previous.service" || return 1
    if [ -f "${CONFIG_FILE}" ]; then
        cp -p -- "${CONFIG_FILE}" "${transaction_dir}/previous.conf" \
            && touch "${transaction_dir}/had-config" || return 1
    fi
    cp -- "${BASH_SOURCE[0]}" "${transaction_dir}/installer.sh" || return 1
    log_file=$(mktemp "${CONFIG_DIR}/modify.log.XXXXXX") || return 1
    schedule_modify_apply "${transaction_dir}" "${init_system}" "${log_file}" || return 1
    scheduled=true
    echo "已安排独立进程应用配置；经过 Brume 的 SSH 连接可能短暂断开，请稍后重连。"
    echo "应用结果日志：${log_file}"
)

# ============================================================
# 卸载函数
# ============================================================

uninstall() {
    local init_system=$1
    local current_port

    echo -e "${BLUE}=== Brume 服务器卸载 ===${RESET}"

    if [ "${init_system}" = standalone ]; then
        echo "若 Brume 仍在运行，请通过容器或进程管理器停止。"
    else
        current_port=$(get_current_port) || return 1
        stop_service_by_init "${init_system}"
        remove_service_by_init "${init_system}"
        echo "正在检查并清理防火墙规则..."
        remove_firewall "${current_port}"
    fi

    # 删除二进制文件
    local binary_path="${INSTALL_DIR}/brume"
    if [ -f "${binary_path}" ]; then
        echo "正在删除二进制文件..."
        execute_privileged rm -f "${binary_path}"
    fi

    if [ -f "${CONFIG_FILE}" ]; then
        rm -f "${CONFIG_FILE}"
        rmdir "${CONFIG_DIR}" 2>/dev/null || true
    fi

    echo -e "${GREEN}Brume 服务器卸载完成${RESET}"
}

# ============================================================
# 交互式菜单
# ============================================================

# 交互式获取操作类型
get_action() {
    while true; do
        echo -e "${BLUE}=== Brume 服务器本地管理 ===${RESET}"
        echo "请选择要执行的操作："
        echo "1. 安装Brume服务器"
        echo "2. 卸载Brume服务器"
        echo "3. 修改Brume服务器配置"
        echo "4. 使用本地二进制更新Brume服务器"
        echo "5. 显示帮助信息"

        if ! read -r -p "请输入选项 (1-5): " choice; then
            echo "未收到操作选项，请通过终端或标准输入提供菜单选项。" >&2
            return 1
        fi

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
    local candidate_port

    # 重置参数为默认值
    port="${DEFAULT_PORT}"
    user="${DEFAULT_USER}"
    password="${DEFAULT_PASSWORD}"
    whitelist="${DEFAULT_WHITELIST}"
    tcp_timeout="${DEFAULT_TCP_TIMEOUT}"
    udp_timeout="${DEFAULT_UDP_TIMEOUT}"
    dns_servers="${DEFAULT_DNS_SERVERS}"

    # 获取端口号
    while true; do
        read -r -p "请输入Brume服务器端口号 [默认: ${DEFAULT_PORT}]: " input_port || return 1
        if [ -z "${input_port}" ]; then
            candidate_port=${DEFAULT_PORT}
        elif [[ "${input_port}" =~ ^[0-9]+$ ]] && [ "${input_port}" -ge 1 ] && [ "${input_port}" -le 65535 ]; then
            candidate_port=${input_port}
        else
            echo -e "${RED}无效的端口号，请输入1-65535之间的数字${RESET}"
            continue
        fi

        if validate_firewall_port "${candidate_port}"; then
            port=${candidate_port}
            break
        fi
    done

    # 获取是否启用认证
    while true; do
        read -r -p "是否启用用户认证? (y/n) [默认: n]: " enable_auth || return 1
        enable_auth=${enable_auth:-n}

        case ${enable_auth} in
            [Yy])
                read -r -p "请输入用户名: " user || return 1
                while [ -z "${user}" ]; do
                    read -r -p "用户名不能为空，请重新输入: " user || return 1
                done

                read -rs -p "请输入密码: " password || return 1
                echo
                while [ -z "${password}" ]; do
                    read -rs -p "密码不能为空，请重新输入: " password || return 1
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
    echo -e "${YELLOW}提示: Brume 按白名单限制连接来源，有可用的防火墙时也可应用网络层规则。${RESET}"
    read -r -p "请输入IP白名单（多个IP用逗号分隔，留空表示不限制）: " whitelist || return 1

    # 显示确认信息
    echo -e "${GREEN}安装配置确认：${RESET}"
    echo "端口: ${port}"
    echo "认证: $(if [ -n "${user}" ] && [ -n "${password}" ]; then echo "启用 (用户名: ${user})"; else echo "禁用"; fi)"
    echo "白名单: $(if [ -n "${whitelist}" ]; then echo "${whitelist}"; else echo "无（允许所有IP）"; fi)"
    echo "本地二进制: ${LOCAL_BINARY}"
    echo "安装目录: ${INSTALL_DIR}"
    while true; do
        read -r -p "确认以上配置是否正确? (y/n) [默认: y]: " confirm || return 1
        confirm=${confirm:-y}

        case ${confirm} in
            [Yy])
                break
                ;;
            [Nn])
                echo -e "${YELLOW}配置已取消，将重新开始${RESET}"
                get_install_config
                return
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
    local candidate_port

    # 同时兼容旧服务参数与当前配置文件
    get_current_config_auto "${init_system}" || exit 1

    # 显示当前配置
    echo -e "${GREEN}当前配置:${RESET}"
    echo "  端口: ${port}"
    echo "  认证: $(if [ -n "${user}" ]; then echo "启用 (用户名: ${user})"; else echo "禁用"; fi)"
    echo "  白名单: $(if [ -n "${whitelist}" ]; then echo "${whitelist}"; else echo "无"; fi)"

    # 获取新的配置参数
    echo -e "${BLUE}请输入新的配置参数（留空表示保持当前配置）${RESET}"

    # 获取端口号
    while true; do
        read -r -p "请输入Brume服务器端口号 [当前: ${port}]: " input_port || return 1
        if [ -z "${input_port}" ]; then
            candidate_port=${port}
        elif [[ "${input_port}" =~ ^[0-9]+$ ]] && [ "${input_port}" -ge 1 ] && [ "${input_port}" -le 65535 ]; then
            candidate_port=${input_port}
        else
            echo -e "${RED}无效的端口号，请输入1-65535之间的数字${RESET}"
            continue
        fi

        if validate_firewall_port "${candidate_port}"; then
            port=${candidate_port}
            break
        fi
    done

    # 获取是否修改用户认证
    while true; do
        read -r -p "是否修改用户认证设置? (y/n) [默认: n]: " change_auth || return 1
        change_auth=${change_auth:-n}

        case ${change_auth} in
            [Yy])
                while true; do
                    read -r -p "是否启用用户认证? (y/n): " enable_auth || return 1
                    case ${enable_auth} in
                        [Yy])
                            read -r -p "请输入用户名: " user || return 1
                            while [ -z "${user}" ]; do
                                read -r -p "用户名不能为空，请重新输入: " user || return 1
                            done

                            read -rs -p "请输入密码: " password || return 1
                            echo
                            while [ -z "${password}" ]; do
                                read -rs -p "密码不能为空，请重新输入: " password || return 1
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
                    read -rs -p "请重新输入密码 (留空表示保留当前密码): " input_password || return 1
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
    read -r -p "请输入IP白名单（多个IP用逗号分隔，留空表示保持当前设置，输入 'clear' 清除白名单）: " input_whitelist || return 1
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
    # 确认修改
    while true; do
        read -r -p "确认修改配置? (y/n) [默认: y]: " confirm || return 1
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

# 获取服务定义路径，读取参数时始终检查实际执行文件
get_service_file() {
    case "$1" in
        systemd) printf '%s\n' "/etc/systemd/system/${SERVICE_NAME}.service" ;;
        openrc|sysvinit) printf '%s\n' "/etc/init.d/${SERVICE_NAME}" ;;
        *) echo "不支持的服务管理器" >&2; return 1 ;;
    esac
}

# 检查服务专用、名称前缀及所有服务共用的扩展目录
get_systemd_dropin_directories() {
    local directory prefix
    for directory in /etc/systemd/system /run/systemd/system /usr/local/lib/systemd/system /usr/lib/systemd/system /lib/systemd/system; do
        printf '%s\n' "${directory}/${SERVICE_NAME}.service.d" "${directory}/service.d"
        prefix=${SERVICE_NAME}
        while [[ "${prefix}" == *-* ]]; do
            prefix=${prefix%-*}
            printf '%s\n' "${directory}/${prefix}-.service.d"
        done
    done
}

# 扩展定义可能覆盖实际启动命令，不能只根据主文件推断有效配置
validate_systemd_service_definition() {
    local directory directories
    directories=$(get_systemd_dropin_directories) || return 1
    while IFS= read -r directory; do
        if [ -e "${directory}" ] || [ -L "${directory}" ]; then
            echo "systemd 服务存在扩展定义，请先手动确认有效配置后再迁移" >&2
            return 1
        fi
    done <<< "${directories}"
}

# 只接受唯一的直接赋值，拒绝重置、多行命令及其他模糊定义
read_service_assignment() {
    local service_file=$1 key=$2 line value="" found=false
    while IFS= read -r line || [ -n "${line}" ]; do
        line=${line%$'\r'}
        if [[ "${line}" == "${key}="* ]]; then
            if [ "${found}" = true ]; then
                echo "服务定义包含重复的 ${key}，无法安全迁移" >&2
                return 1
            fi
            value=${line#*=}
            found=true
        fi
    done < "${service_file}"
    if [ "${found}" != true ] || [ -z "${value}" ]; then
        echo "服务定义缺少有效的 ${key}，无法安全迁移" >&2
        return 1
    fi
    printf '%s\n' "${value}"
}

# 按字面值安全分词，无法还原的转义、变量和命令展开直接拒绝
split_service_arguments() {
    local text=$1 char quote="" token="" started=false index
    service_args=()
    for ((index=0; index<${#text}; index++)); do
        char=${text:index:1}
        case "${char}" in
            '$'|'`'|'\'|'%'|$'\n'|$'\r')
                echo "服务参数包含无法安全迁移的转义或展开" >&2
                return 1
                ;;
        esac
        if [ -n "${quote}" ]; then
            if [ "${char}" = "${quote}" ]; then
                quote=""
            else
                token+=${char}
            fi
        else
            case "${char}" in
                "'"|'"') quote=${char}; started=true ;;
                ' '|$'\t')
                    if [ "${started}" = true ]; then
                        service_args+=("${token}")
                        token=""
                        started=false
                    fi
                    ;;
                ';'|'&'|'|'|'<'|'>'|'('|')')
                    echo "服务参数包含不支持的命令语法" >&2
                    return 1
                    ;;
                *) token+=${char}; started=true ;;
            esac
        fi
    done
    if [ -n "${quote}" ]; then
        echo "服务参数包含未闭合的引号" >&2
        return 1
    fi
    if [ "${started}" = true ]; then
        service_args+=("${token}")
    fi
}

# 自动读取真实配置文件，再按程序规则应用全部命令行覆盖项
get_current_config_auto() {
    local init_system=$1
    local service_file command_args executable assignment key value config_path=""
    local -a service_args override_keys=() override_values=()
    local LC_ALL=C

    if [ "${init_system}" != standalone ]; then
        service_file=$(get_service_file "${init_system}") || return 1
        if [ ! -f "${service_file}" ]; then
            echo -e "${RED}Brume 服务未安装，请先安装服务${RESET}" >&2
            return 1
        fi
    fi

    case "${init_system}" in
        standalone)
            service_args=(--config "${CONFIG_FILE}")
            ;;
        systemd)
            validate_systemd_service_definition || return 1
            if [ -L "${service_file}" ]; then
                echo "systemd 服务文件是符号链接，无法安全确认有效配置" >&2
                return 1
            fi
            command_args=$(read_service_assignment "${service_file}" ExecStart) || return 1
            split_service_arguments "${command_args}" || return 1
            if [ "${#service_args[@]}" -eq 0 ] || [ "${service_args[0]}" != "${INSTALL_DIR}/brume" ]; then
                echo "服务执行文件不是 ${INSTALL_DIR}/brume，无法安全迁移" >&2
                return 1
            fi
            service_args=("${service_args[@]:1}")
            ;;
        openrc|sysvinit)
            if [ "${init_system}" = openrc ]; then
                executable=command
                assignment=command_args
            else
                executable=DAEMON
                assignment=DAEMON_ARGS
            fi
            command_args=$(read_service_assignment "${service_file}" "${executable}") || return 1
            split_service_arguments "${command_args}" || return 1
            if [ "${#service_args[@]}" -ne 1 ] || [ "${service_args[0]}" != "${INSTALL_DIR}/brume" ]; then
                echo "服务执行文件不是 ${INSTALL_DIR}/brume，无法安全迁移" >&2
                return 1
            fi
            command_args=$(read_service_assignment "${service_file}" "${assignment}") || return 1
            split_service_arguments "${command_args}" || return 1
            if [ "${#service_args[@]}" -ne 1 ]; then
                echo "服务参数赋值无法安全迁移" >&2
                return 1
            fi
            command_args=${service_args[0]}
            split_service_arguments "${command_args}" || return 1
            ;;
    esac

    set -- "${service_args[@]}"
    while [ $# -gt 0 ]; do
        key=${1%%=*}
        case "${key}" in
            --config|-p|--port|-user|--user|-pwd|--pwd|--whitelist|--tcp-timeout|--udp-timeout|--dns-servers) ;;
            *) echo "服务包含不支持的参数，无法安全迁移" >&2; return 1 ;;
        esac
        if [[ "$1" == *=* ]]; then
            value=${1#*=}
            shift
        else
            if [ $# -lt 2 ]; then
                echo "服务参数 ${key} 缺少值" >&2
                return 1
            fi
            value=$2
            shift 2
        fi
        if [ "${key}" = --config ]; then
            if [ -n "${config_path}" ] || [[ "${value}" != /* ]]; then
                echo "服务必须使用唯一的绝对配置文件路径" >&2
                return 1
            fi
            config_path=${value}
        else
            override_keys+=("${key}")
            override_values+=("${value}")
        fi
    done

    port="${DEFAULT_PORT}" user="" password="" whitelist=""
    tcp_timeout="${DEFAULT_TCP_TIMEOUT}" udp_timeout="${DEFAULT_UDP_TIMEOUT}"
    dns_servers="${DEFAULT_DNS_SERVERS}" uses_config_file=false
    if [ -n "${config_path}" ]; then
        if [ ! -f "${config_path}" ]; then
            echo "服务配置文件不存在，无法安全迁移" >&2
            return 1
        fi
        load_config_file "${config_path}" || return 1
        uses_config_file=true
    fi

    local index
    for ((index=0; index<${#override_keys[@]}; index++)); do
        value=${override_values[index]}
        case "${override_keys[index]}" in
            -p|--port) port=${value} ;;
            -user|--user) user=${value} ;;
            -pwd|--pwd) password=${value} ;;
            --whitelist) whitelist=${value} ;;
            --tcp-timeout) tcp_timeout=${value} ;;
            --udp-timeout) udp_timeout=${value} ;;
            --dns-servers) dns_servers=${value} ;;
        esac
    done
    if [[ ! "${port}" =~ ^[0-9]+$ ]] || [ "${#port}" -gt 5 ] \
        || [ "${port}" -lt 1 ] || [ "${port}" -gt 65535 ] \
        || [[ ! "${tcp_timeout}" =~ ^[0-9]+$ || ! "${udp_timeout}" =~ ^[0-9]+$ ]] \
        || { [ -z "${user}" ] && [ -n "${password}" ]; } \
        || { [ -n "${user}" ] && [ -z "${password}" ]; } \
        || [ "${#user}" -gt 255 ] || [ "${#password}" -gt 255 ]; then
        echo "服务配置无效，无法安全迁移" >&2
        return 1
    fi
}

upgrade() (
    local init_system=$1
    acquire_mutation_lock || return 1
    if [ "${init_system}" != standalone ]; then
        require_independent_worker_tools || return 1
    fi
    echo -e "${BLUE}=== Brume 服务器本地更新 ===${RESET}"

    # 1. 自动提取参数
    get_current_config_auto "${init_system}" || return 1
    echo -e "已提取当前配置: 端口 ${port}, 用户 ${user:-无}, 白名单 ${whitelist:-无限制}"

    # 2. 读取并检查本地新程序，整个过程不访问发布服务。
    prepare_binary || return 1

    # 3. 保存完整有效配置，保留自定义文件和命令行覆盖后的实际行为
    if ! write_config_file "${port}" "${user}" "${password}" "${whitelist}" "${tcp_timeout}" "${udp_timeout}" "${dns_servers}"; then
        rm -rf "${prepared_dir}"
        return 1
    fi

    # 4. 保持旧进程运行，原子替换程序并更新服务定义
    install_prepared_binary || return 1
    if [ "${init_system}" = standalone ]; then
        echo "Brume 程序已更新，请通过容器或进程管理器重启后生效。"
        return 0
    fi
    create_service "${init_system}" || return 1

    # 5. 端口和白名单未改变，无需重建防火墙规则
    echo -e "${GREEN}新版本已安装，正在安排服务自动重启${RESET}"
    schedule_upgrade_restart "${init_system}"
)

# ============================================================
# 主函数
# ============================================================

# 限制服务定义和 SysVinit 模板无法按字面值还原的路径，普通空格目录可正常使用。
validate_local_paths() {
    if [[ "${INSTALL_DIR}" != /* ]]; then
        echo "安装目录必须是绝对路径。" >&2
        return 1
    fi
    case "${INSTALL_DIR}" in
        *\"*|*\'*|*\\*|*\$*|*\`*|*%*|*\|*|*\&*|*$'\n'*|*$'\r'*)
            echo '安装目录不能包含引号、反斜杠、$、反引号、%、|、& 或换行，请移动脚本后重试。' >&2
            return 1
            ;;
    esac
}

parse_local_options() {
    while [ $# -gt 0 ]; do
        case "$1" in
            --binary)
                if [ $# -lt 2 ] || [ -z "$2" ]; then
                    echo "--binary 需要指定本地二进制路径。" >&2
                    return 1
                fi
                LOCAL_BINARY=$2
                [[ "${LOCAL_BINARY}" == /* ]] || LOCAL_BINARY="${PWD}/${LOCAL_BINARY}"
                shift 2
                ;;
            -h|--help) show_help ;;
            *) echo "未知参数：$1" >&2; return 1 ;;
        esac
    done
    validate_local_paths
}

# 固定服务名只管理这一份本地安装，防止另一目录的安装覆盖现有服务。
verify_local_service_owner() {
    local init_system=$1 service_file
    [ "${init_system}" != standalone ] || return 0
    service_file=$(get_service_file "${init_system}") || return 1
    if [ -e "${service_file}" ] || [ -L "${service_file}" ]; then
        if ! (get_current_config_auto "${init_system}"); then
            echo "无法确认现有 ${SERVICE_NAME} 服务属于本目录，请在原安装目录管理。" >&2
            return 1
        fi
    fi
}

main() {
    parse_local_options "$@" || return 1
    # 检查是否为root用户
    if [ "$(id -u)" != "0" ]; then
        echo -e "${RED}请以root用户或使用sudo运行此脚本${RESET}"
        exit 1
    fi

    # 检测服务管理器类型
    local init_system
    init_system=$(detect_init_system)

    echo -e "${CYAN}检测到服务管理器: ${init_system}${RESET}"
    if [ "${init_system}" != standalone ]; then
        echo -e "${CYAN}检测到防火墙工具: $(detect_firewall)${RESET}"
    fi

    # 交互式获取操作类型
    get_action || return 1
    verify_local_service_owner "${init_system}" || return 1

    # 根据操作类型执行相应的功能
    case "${action}" in
        install)
            echo -e "${BLUE}=== Brume 服务器安装 ===${RESET}"

            # 重复安装时保留已有配置，不重置认证、端口或白名单。
            if [ -f "${CONFIG_FILE}" ]; then
                get_current_config_auto standalone || return 1
                echo "保留已有配置：${CONFIG_FILE}"
            else
                get_install_config || { echo "未收到完整的安装配置。" >&2; return 1; }
            fi

            # 读取本地程序并原子安装到脚本所在目录。
            install_local_binary || return 1

            write_config_file "${port}" "${user}" "${password}" "${whitelist}" "${tcp_timeout}" "${udp_timeout}" "${dns_servers}" || return 1
            if [ "${init_system}" != standalone ]; then
                create_service "${init_system}" || return 1
                setup_firewall "${port}" "${whitelist}" || return 1
            fi

            # 启动服务
            start_service "${init_system}"
            ;;
        uninstall)
            # 确认卸载
            while true; do
                read -r -p "确定要卸载本地Brume服务器吗? 将删除程序、配置及对应服务和防火墙规则，保留脚本和历史日志。(y/n) [默认: n]: " confirm || return 1
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
            get_modify_config "${init_system}" || { echo "未收到完整的修改配置。" >&2; return 1; }

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

# 直接运行脚本时进入交互式菜单
if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
    if { [ "${1:-}" = --apply-modify ] || [ "${1:-}" = --restart-upgrade ]; } && [ $# -eq 4 ]; then
        run_independent_worker "$2" "$3" "$4" "$1"
    else
        main "$@"
    fi
fi
