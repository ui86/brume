#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
source "${script_dir}/../install.sh"

# 按 BusyBox 的模板规则限制替身，使 GNU mktemp 环境也能发现后缀不兼容。
mktemp() {
    if [ "$#" -eq 0 ] || { [ "$#" -eq 1 ] && [ "$1" = -d ]; } \
        || { [ "$#" -eq 1 ] && [[ "$1" == *XXXXXX ]]; } \
        || { [ "$#" -eq 2 ] && [ "$1" = -d ] && [[ "$2" == *XXXXXX ]]; }; then
        command mktemp "$@"
    else
        echo '临时文件模板必须以 XXXXXX 结尾' >&2
        return 1
    fi
}

test_dir=$(mktemp -d)
CONFIG_DIR="${test_dir}/config"
CONFIG_FILE="${CONFIG_DIR}/brume.conf"
INSTALL_DIR="${test_dir}/bin"
test_service_file="${test_dir}/brume.service"
events="${test_dir}/events"
failure_stage=""
cleanup() {
    local directory
    for directory in "${CONFIG_DIR}"/.modify.*; do
        if [ -d "${directory}" ]; then
            rm -f -- "${directory}/recovery-needed"
            cleanup_modify_transaction "${directory}"
        fi
    done
    rm -f -- "${CONFIG_DIR}"/* "${INSTALL_DIR}/brume" "${INSTALL_DIR}/unsupported" "${INSTALL_DIR}/setsid" \
        "${test_service_file}" "${events}" "${test_dir}/transaction" "${test_dir}/fake.lock" "${test_dir}/worker.log"
    if [ -d "${test_dir}/schedule" ]; then
        rm -f -- "${test_dir}/schedule"/*
        rmdir -- "${test_dir}/schedule"
    fi
    rmdir -- "${CONFIG_DIR}" "${INSTALL_DIR}" "${test_dir}"
}
trap cleanup EXIT
mkdir "${CONFIG_DIR}" "${INSTALL_DIR}"
touch "${events}"

# 替身程序只模拟帮助及配置校验，不建立监听也不执行配置内容
cat > "${INSTALL_DIR}/brume" <<'EOF'
#!/usr/bin/env bash
if [ "$1" = --help ]; then
    if [ ! -f "$(dirname -- "$0")/unsupported" ]; then
        echo '--config --check-config'
    fi
    exit 0
fi
[[ "$1" == --config && "$3" == --check-config ]] || exit 1
if grep -q '^whitelist=invalid' "$2"; then
    exit 1
fi
EOF
chmod +x "${INSTALL_DIR}/brume"
get_service_file() { printf '%s\n' "${test_service_file}"; }
get_systemd_dropin_directories() { printf '%s\n' "${test_dir}/missing.service.d"; }
acquire_mutation_lock() { [ "${failure_stage}" != lock ]; }
require_independent_worker_tools() { :; }
sleep() { :; }
stop_service_by_init() { echo '修改配置不应提前停服' >&2; exit 1; }
create_service() {
    printf 'service\n' >> "${events}"
    printf 'ExecStart=候选服务\n' > "${test_service_file}"
    [ "${failure_stage}" != service ] || exit 1
}
remove_firewall() { printf 'remove:%s\n' "$1" >> "${events}"; }
setup_firewall() {
    printf 'firewall:%s:%s\n' "$1" "$2" >> "${events}"
    if { [ "${failure_stage}" = firewall ] && [ "$1" = 26548 ]; } || [ "${failure_stage}" = rollback ]; then
        return 1
    fi
}
restart_service_by_init() {
    local active_port
    active_port=$(sed -n 's/^port=//p' "${CONFIG_FILE}")
    printf 'restart:%s\n' "${active_port}" >> "${events}"
    if [ "${failure_stage}" = restart ] && [ "${active_port}" = 26548 ]; then
        exit 1
    fi
}
check_service_running() {
    [ "${failure_stage}" != health ] || ! grep -q '^port=26548$' "${CONFIG_FILE}"
}
execute_privileged() { printf 'privileged:%s\n' "$*" >> "${events}"; }
schedule_modify_apply() {
    printf '%s\n' "$1" > "${test_dir}/transaction"
    if [ "${failure_stage}" = schedule ]; then
        return 1
    fi
    if [ "${failure_stage}" != deferred ]; then
        apply_modify_transaction "$1" "$2"
    fi
}

reset_case() {
    : > "${events}"
    rm -f -- "${test_dir}/transaction"
    write_config_file 26547 admin '旧密码#=$value' 192.0.2.1 0 60 9.9.9.9
    printf 'ExecStart=%s/brume --config=%s\n' "${INSTALL_DIR}" "${CONFIG_FILE}" > "${test_service_file}"
    port=26548 user=admin password='新密码$(touch 不应执行)' whitelist=198.51.100.1
    tcp_timeout=0 udp_timeout=60 dns_servers=${DEFAULT_DNS_SERVERS}
}

# 调度前只能校验和备份，不能修改配置、服务或防火墙
failure_stage=deferred
reset_case
before=$(cat "${CONFIG_FILE}")
before_service=$(cat "${test_service_file}")
modify systemd >/dev/null
[[ "$(cat "${CONFIG_FILE}")" == "${before}" && ! -s "${events}" ]]
transaction_dir=$(cat "${test_dir}/transaction")
failure_stage=""
apply_modify_transaction "${transaction_dir}" systemd >/dev/null
[[ ! -d "${transaction_dir}" ]]
load_config_file
[[ "${port}" == 26548 && "${password}" == '新密码$(touch 不应执行)' ]]
if grep -q '^dns_servers=' "${CONFIG_FILE}"; then
    echo '候选默认 DNS 被旧配置污染' >&2
    exit 1
fi
grep -qx 'restart:26548' "${events}"

for failure_stage in service firewall restart health schedule lock; do
    reset_case
    before=$(cat "${CONFIG_FILE}")
    before_service=$(cat "${test_service_file}")
    if modify systemd >/dev/null 2>&1; then
        echo "${failure_stage} 失败未被报告" >&2
        exit 1
    fi
    [[ "$(cat "${CONFIG_FILE}")" == "${before}" ]]
    [[ "$(cat "${test_service_file}")" == "${before_service}" ]]
    if [ "${failure_stage}" = firewall ] || [ "${failure_stage}" = restart ] || [ "${failure_stage}" = health ]; then
        grep -qx 'firewall:26547:192.0.2.1' "${events}"
    fi
    if [ "${failure_stage}" = restart ] || [ "${failure_stage}" = health ]; then
        grep -qx 'restart:26547' "${events}"
    fi
    if [ -f "${test_dir}/transaction" ]; then
        [[ ! -d "$(cat "${test_dir}/transaction")" ]]
    fi
done

# 恢复规则失败时必须保留完整的私有备份，供管理员继续恢复
failure_stage=rollback
reset_case
if modify systemd >/dev/null 2>&1; then
    echo '回滚失败未被报告' >&2
    exit 1
fi
transaction_dir=$(cat "${test_dir}/transaction")
[[ -f "${transaction_dir}/recovery-needed" && -f "${transaction_dir}/previous.conf" ]]
[[ -f "${transaction_dir}/previous.service" && -f "${transaction_dir}/old-effective.conf" ]]
case "$(uname -s)" in
    MINGW*|MSYS*) ;;
    *) [[ "$(stat -c %a "${transaction_dir}")" == 700 ]] ;;
esac

# 只修改认证时无需重建防火墙
failure_stage=""
reset_case
port=26547 whitelist=192.0.2.1
modify systemd >/dev/null
if grep -qE '^(remove|firewall):' "${events}"; then
    echo '认证变更不应重建未改变的防火墙规则' >&2
    exit 1
fi

# 校验失败和旧版不支持完整校验时，现有服务不得受到影响
failure_stage=""
reset_case
before=$(cat "${CONFIG_FILE}")
whitelist=invalid
if modify systemd >/dev/null 2>&1; then
    echo '无效候选配置仍然进入应用阶段' >&2
    exit 1
fi
[[ "$(cat "${CONFIG_FILE}")" == "${before}" && ! -s "${events}" ]]
touch "${INSTALL_DIR}/unsupported"
if modify systemd >/dev/null 2>&1; then
    echo '不支持完整校验的旧版仍然修改配置' >&2
    exit 1
fi
[[ "$(cat "${CONFIG_FILE}")" == "${before}" && ! -s "${events}" ]]

# 实际服务创建步骤也必须传播服务管理器错误
execute_privileged() {
    case "$1" in
        tee) cat >/dev/null ;;
        systemctl) return 1 ;;
        *) return 0 ;;
    esac
}
if create_systemd_service >/dev/null 2>&1; then
    echo '服务管理器失败被忽略' >&2
    exit 1
fi

# 用替身 setsid 验证真实调度握手及锁描述符继承，兼容 Git Bash
(
    source "${script_dir}/../install.sh"
    unset -f sleep
    mkdir "${test_dir}/schedule"
    cat > "${test_dir}/bin/setsid" <<'EOF'
#!/usr/bin/env bash
exec "$@"
EOF
    cat > "${test_dir}/schedule/installer.sh" <<'EOF'
#!/usr/bin/env bash
transaction_dir=$2
: <&"$4" || exit 1
touch "${transaction_dir}/ready"
for ((attempt=0; attempt<100; attempt++)); do
    if [ -f "${transaction_dir}/go" ]; then
        printf '%s\n' "$3" > "${transaction_dir}/completed"
        exit 0
    fi
    sleep 0.1
done
exit 1
EOF
    chmod +x "${test_dir}/bin/setsid"
    export PATH="${test_dir}/bin:${PATH}"
    exec {mutation_lock_fd}>"${test_dir}/fake.lock"
    schedule_modify_apply "${test_dir}/schedule" systemd "${test_dir}/worker.log"
    for ((attempt=0; attempt<100; attempt++)); do
        [ ! -f "${test_dir}/schedule/completed" ] || break
        command sleep 0.1
    done
    grep -qx systemd "${test_dir}/schedule/completed"

    # 缺少独立进程工具时，在启动任何工作进程前失败
    command() {
        if [ "$#" -eq 2 ] && [ "$1" = -v ] && [ "$2" = setsid ]; then
            return 1
        fi
        builtin command "$@"
    }
    if schedule_modify_apply "${test_dir}/schedule" systemd "${test_dir}/worker.log" >/dev/null 2>&1; then
        echo '缺少 setsid 时仍然调度成功' >&2
        exit 1
    fi
)
