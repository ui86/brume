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

CONFIG_DIR=$(mktemp -d)
CONFIG_FILE="${CONFIG_DIR}/brume.conf"
test_config_dir=${CONFIG_DIR}
events="${CONFIG_DIR}/events"
touch "${CONFIG_FILE}" "${events}"
cleanup() {
    rm -f -- "${CONFIG_FILE}" "${events}"
    rm -f -- "${CONFIG_DIR}"/upgrade.log.*
    rmdir -- "${CONFIG_DIR}"
}
trap cleanup EXIT

get_current_config_auto() {
    port=26547 user=admin password=secret whitelist=127.0.0.1
    tcp_timeout=0 udp_timeout=60 dns_servers=9.9.9.9 uses_config_file=true
}
acquire_mutation_lock() { :; }
require_independent_worker_tools() { :; }
check_architecture() { echo amd64; }
get_latest_version() { echo 1.0.3; }
prepare_binary() { echo prepare >> "${events}"; }
install_prepared_binary() { echo install >> "${events}"; }
create_service() { echo service >> "${events}"; }
schedule_upgrade_restart() { echo restart >> "${events}"; }
stop_service_by_init() { echo '升级中不应提前停服' >&2; exit 1; }
remove_firewall() { echo '升级中不应清理防火墙' >&2; exit 1; }
setup_firewall() { echo '升级中不应重建防火墙' >&2; exit 1; }

upgrade systemd
expected=$(printf 'prepare\ninstall\nservice\nrestart\n')
[[ "$(cat "${events}")" == "${expected}" ]]

(
    source "${script_dir}/../install.sh"
    CONFIG_DIR=${test_config_dir}
    schedule_independent_worker() {
        printf '%s\n' "$2 $4" >> "${events}"
        cleanup_modify_transaction "$1"
    }
    schedule_upgrade_restart systemd
)
[[ "$(tail -n 1 "${events}")" == 'systemd --restart-upgrade' ]]

# 修改事务已持锁时，升级不能进入下载或写配置阶段
(
    acquire_mutation_lock() { return 1; }
    get_current_config_auto() { echo '持锁失败不应读取或修改配置' >&2; exit 1; }
    if upgrade systemd >/dev/null 2>&1; then
        echo '并发升级未被拒绝' >&2
        exit 1
    fi
)
