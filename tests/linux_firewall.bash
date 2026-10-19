#!/usr/bin/env bash
set -euo pipefail

# 必须由 CI 在独立网络命名空间启动，禁止触碰宿主网络规则。
if [ "$(uname -s)" != Linux ] || [ "${EUID}" -ne 0 ] \
    || [ "$(readlink /proc/self/ns/net)" = "$(readlink /proc/1/ns/net)" ]; then
    echo '真实防火墙测试必须在 root 的独立网络命名空间内执行。' >&2
    exit 1
fi

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
source "${script_dir}/../install.sh"
has_sudo=false
test_dir=$(mktemp -d)
echo_pid=
cleanup() {
    if [ -n "${echo_pid}" ]; then
        kill "${echo_pid}" 2>/dev/null || true
        wait "${echo_pid}" 2>/dev/null || true
    fi
    rm -f -- "${test_dir}/ready" "${test_dir}/nftables.conf" \
        "${test_dir}/rules.v4" "${test_dir}/rules.v6"
    rmdir -- "${test_dir}"
}
trap cleanup EXIT

# 只把持久化文件重定向至临时目录，规则生成和内核应用均使用真实实现。
persist_nftables_rules() { nft list ruleset > "${test_dir}/nftables.conf"; }
persist_iptables_rules() {
    iptables-save > "${test_dir}/rules.v4" && ip6tables-save > "${test_dir}/rules.v6"
}

ip link set lo up
ip -6 address add ::2/128 dev lo nodad
ip -6 address add ::3/128 dev lo nodad
python3 "${script_dir}/linux_firewall.py" serve "${test_dir}/ready" &
echo_pid=$!
for ((attempt=0; attempt<100; attempt++)); do
    [ ! -f "${test_dir}/ready" ] || break
    kill -0 "${echo_pid}"
    sleep 0.05
done
[[ -f "${test_dir}/ready" ]]

for backend in nftables iptables; do
    "setup_firewall_${backend}" 26547 '127.0.0.2,::2'
    python3 "${script_dir}/linux_firewall.py" allowed
    python3 "${script_dir}/linux_firewall.py" denied
    # 重复应用必须清理旧规则，且不改变实际允许及拒绝行为。
    "setup_firewall_${backend}" 26547 '127.0.0.2,::2'
    python3 "${script_dir}/linux_firewall.py" allowed
    python3 "${script_dir}/linux_firewall.py" denied
    "remove_firewall_${backend}" 26547
    python3 "${script_dir}/linux_firewall.py" removed
done
echo 'Linux IPv4/IPv6 TCP/UDP 真实防火墙应用、重复配置与移除回归通过。'
