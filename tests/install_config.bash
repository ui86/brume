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

write_config_file 26547 admin 'test#= $value' '127.0.0.1' 4 90 '9.9.9.9,127.0.0.1:5353'
# Windows 的 Git Bash 不提供可靠的 POSIX 权限位，权限检查由 Linux CI 执行
case "$(uname -s)" in
    MINGW*|MSYS*) ;;
    *)
        [[ $(stat -c %a "${CONFIG_DIR}") == 700 ]]
        [[ $(stat -c %a "${CONFIG_FILE}") == 600 ]]
        ;;
esac

printf '  # 允许缩进的注释\n' >> "${CONFIG_FILE}"
port= user= password= whitelist= tcp_timeout= udp_timeout= dns_servers=
load_config_file
[[ "${port}" == 26547 ]]
[[ "${user}" == admin ]]
[[ "${password}" == 'test#= $value' ]]
[[ "${whitelist}" == 127.0.0.1 ]]
[[ "${tcp_timeout}" == 4 ]]
[[ "${udp_timeout}" == 90 ]]
[[ "${dns_servers}" == '9.9.9.9,127.0.0.1:5353' ]]

write_config_file 26547 admin 'test#= $value' '127.0.0.1' 4 90
if grep -q '^dns_servers=' "${CONFIG_FILE}"; then
    echo '默认 DNS 不应破坏旧版程序的配置文件' >&2
    exit 1
fi

if write_config_file 26547 admin $'bad\nvalue' '' 0 60 2>/dev/null; then
    echo '配置文件接受了换行符' >&2
    exit 1
fi
