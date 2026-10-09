#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
source "${script_dir}/../install.sh"

CONFIG_DIR=$(mktemp -d)
CONFIG_FILE="${CONFIG_DIR}/brume.conf"
cleanup() {
    rm -f -- "${CONFIG_FILE}"
    rmdir -- "${CONFIG_DIR}"
}
trap cleanup EXIT

write_config_file 26547 admin 'test#= $value' '127.0.0.1' 4 90
[[ $(stat -c %a "${CONFIG_DIR}") == 700 ]]
[[ $(stat -c %a "${CONFIG_FILE}") == 600 ]]

printf '  # 允许缩进的注释\n' >> "${CONFIG_FILE}"
port= user= password= whitelist= tcp_timeout= udp_timeout=
load_config_file
[[ "${port}" == 26547 ]]
[[ "${user}" == admin ]]
[[ "${password}" == 'test#= $value' ]]
[[ "${whitelist}" == 127.0.0.1 ]]
[[ "${tcp_timeout}" == 4 ]]
[[ "${udp_timeout}" == 90 ]]

if write_config_file 26547 admin $'bad\nvalue' '' 0 60 2>/dev/null; then
    echo '配置文件接受了换行符' >&2
    exit 1
fi
