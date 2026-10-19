#!/usr/bin/env bash
set -euo pipefail

# 临时服务只允许在 GitHub 托管的短期运行器中创建，禁止直接操作用户宿主。
if [ "$(uname -s)" != Linux ] || [ "${EUID}" -ne 0 ] \
    || [ "${GITHUB_ACTIONS:-}" != true ] || [ "${RUNNER_ENVIRONMENT:-}" != github-hosted ] \
    || [ "${BRUME_CI_SYSTEMD:-}" != 1 ] || [ "$(cat /proc/1/comm)" != systemd ]; then
    echo '真实 systemd 测试仅允许在显式启用的 GitHub 托管 Linux 运行器中执行。' >&2
    exit 1
fi

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
source "${script_dir}/../install.sh"
has_sudo=false
binary=$(realpath -- "${1:?需要已构建的 brume 程序}")
[[ -x "${binary}" && "${binary##*/}" == brume ]]
test_dir=$(mktemp -d /tmp/brume-service-ci.XXXXXX)
SERVICE_NAME="brume-ci-${test_dir##*.}"
service_file="/etc/systemd/system/${SERVICE_NAME}.service"
[[ ! -e "${service_file}" ]]
INSTALL_DIR=${binary%/*}
CONFIG_DIR="${test_dir}/config"
CONFIG_FILE="${CONFIG_DIR}/brume.conf"
failure_stage=
cleanup() {
    local transaction
    systemctl stop "${SERVICE_NAME}" 2>/dev/null || true
    systemctl disable "${SERVICE_NAME}" 2>/dev/null || true
    rm -f -- "${service_file}"
    systemctl daemon-reload || true
    systemctl reset-failed "${SERVICE_NAME}" 2>/dev/null || true
    for transaction in "${CONFIG_DIR}"/.modify.*; do
        if [ -d "${transaction}" ]; then
            rm -f -- "${transaction}/recovery-needed"
            cleanup_modify_transaction "${transaction}" || true
        fi
    done
    rm -f -- "${CONFIG_FILE}"
    rmdir -- "${CONFIG_DIR}" "${test_dir}"
}
trap cleanup EXIT

port=$(python3 - <<'PY'
import socket
with socket.socket() as stream:
    stream.bind(('127.0.0.1', 0))
    print(stream.getsockname()[1])
PY
)
write_config_file "${port}" '' '' 127.0.0.1 0 60 "${DEFAULT_DNS_SERVERS}"
create_test_service() {
    create_systemd_service
    # 故障注入会连续重启多次，仅临时测试服务关闭启动频率限制。
    printf '\n[Unit]\nStartLimitIntervalSec=0\n' >> "${service_file}"
    systemctl daemon-reload
}
create_test_service
start_service_by_init systemd

probe_service() {
    check_service_running systemd
    python3 - "${port}" <<'PY'
import socket
import sys
import time

deadline = time.monotonic() + 3
while True:
    try:
        with socket.create_connection(('127.0.0.1', int(sys.argv[1])), timeout=1) as stream:
            stream.sendall(b'\x05\x01\x00')
            result = b''
            while len(result) < 2:
                part = stream.recv(2 - len(result))
                assert part, '服务关闭了协商连接'
                result += part
            assert result == b'\x05\x00', '服务未完成 SOCKS5 协商'
            break
    except OSError:
        if time.monotonic() >= deadline:
            raise
        time.sleep(0.05)
PY
}

prepare_transaction() {
    local timeout=$1
    transaction_dir=$(mktemp -d "${CONFIG_DIR}/.modify.XXXXXX")
    cp -- "${CONFIG_FILE}" "${transaction_dir}/old-effective.conf"
    cp -p -- "${CONFIG_FILE}" "${transaction_dir}/previous.conf"
    cp -p -- "${service_file}" "${transaction_dir}/previous.service"
    touch "${transaction_dir}/had-config"
    sed "s/^tcp_timeout=.*/tcp_timeout=${timeout}/" "${CONFIG_FILE}" > "${transaction_dir}/new.conf"
    "${binary}" --config "${transaction_dir}/new.conf" --check-config
}

# 注入候选服务故障，其余生成、重启、检查和恢复均调用真实安装器及 systemd。
create_service() {
    create_test_service
    case "${failure_stage}" in
        restart)
            printf '\n[Service]\nExecStartPre=/bin/false\nRestart=no\n' >> "${service_file}"
            systemctl daemon-reload
            ;;
        health)
            # 启动任务先成功，再于安装器的一秒状态检查前退出。
            printf '\n[Service]\nExecStart=\nExecStart=/bin/sleep 0.2\nRestart=no\n' >> "${service_file}"
            systemctl daemon-reload
            ;;
    esac
}

probe_service
previous_pid=$(systemctl show "${SERVICE_NAME}" --property=MainPID --value)
prepare_transaction 1
apply_modify_transaction "${transaction_dir}" systemd
[[ ! -d "${transaction_dir}" ]]
grep -qx 'tcp_timeout=1' "${CONFIG_FILE}"
[[ "$(systemctl show "${SERVICE_NAME}" --property=MainPID --value)" != "${previous_pid}" ]]
probe_service

for failure_stage in restart health; do
    before_config=$(cat "${CONFIG_FILE}")
    before_service=$(cat "${service_file}")
    prepare_transaction 2
    if apply_modify_transaction "${transaction_dir}" systemd; then
        echo "真实 systemd ${failure_stage} 失败未被识别" >&2
        exit 1
    fi
    [[ ! -d "${transaction_dir}" ]]
    [[ "$(cat "${CONFIG_FILE}")" == "${before_config}" ]]
    [[ "$(cat "${service_file}")" == "${before_service}" ]]
    probe_service
done
echo 'Linux 真实 systemd 配置应用、重启失败和状态检查失败恢复回归通过。'
