#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
source "${script_dir}/../install.sh"

CONFIG_DIR=$(mktemp -d)
CONFIG_FILE="${CONFIG_DIR}/brume.conf"
events="${CONFIG_DIR}/events"
touch "${CONFIG_FILE}" "${events}"
cleanup() {
    rm -f -- "${CONFIG_FILE}" "${events}"
    rmdir -- "${CONFIG_DIR}"
}
trap cleanup EXIT

get_current_config_auto() {
    port=26547 user=admin password=secret whitelist=127.0.0.1
    tcp_timeout=0 udp_timeout=60 dns_servers=9.9.9.9 uses_config_file=true
}
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
    execute_privileged() { printf '%s\n' "$*" >> "${events}"; }
    schedule_upgrade_restart systemd
)
[[ "$(tail -n 1 "${events}")" == 'systemctl restart --no-block brume' ]]
