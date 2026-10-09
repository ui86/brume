#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
source "${script_dir}/../install.sh"

SSH_CONNECTION='198.51.100.10 52133 203.0.113.8 2222'
export SSH_CONNECTION
ports="$(get_ssh_ports)"
grep -qx '22' <<< "${ports}"
grep -qx '2222' <<< "${ports}"

if validate_firewall_port 22 >/dev/null 2>&1; then
    echo '标准 SSH 端口未受到保护' >&2
    exit 1
fi
if validate_firewall_port 2222 >/dev/null 2>&1; then
    echo '当前 SSH 会话端口未受到保护' >&2
    exit 1
fi
validate_firewall_port 26547
