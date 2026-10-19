#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
source "${script_dir}/../install.sh"

events=$(mktemp)
trap 'rm -f -- "${events}"' EXIT
failure=""
absent=false
ip6tables() { :; }

# 所有系统命令均使用替身，测试不修改真实防火墙和持久化文件。
execute_privileged() {
    printf '%s\n' "$*" >> "${events}"
    case "$1" in
        firewall-cmd)
            case "$*" in
                *--list-rich-rules)
                    [ "${failure}" != firewalld-query ] || return 1
                    if [ "${absent}" != true ]; then
                        printf '%s\n' 'rule family="ipv4" port port="26547" protocol="tcp" drop'
                    fi
                    ;;
                *--get-zones)
                    [ "${failure}" != firewalld-zones ] || return 1
                    if [ "${absent}" = true ]; then echo public; else echo 'public brume'; fi
                    ;;
                *--remove-rich-rule=*) [ "${failure}" != firewalld-rule ] || return 1 ;;
                *--delete-zone=brume) [ "${failure}" != firewalld-delete ] || return 1 ;;
                *--reload) [ "${failure}" != firewalld-reload ] || return 1 ;;
            esac
            ;;
        env)
            [ "${failure}" != ufw-query ] || return 1
            if [ "${absent}" != true ]; then
                printf '%s\n' \
                    '[ 2] 26547/tcp ALLOW IN 192.0.2.1 # brume-whitelist' \
                    '[10] 26547/udp DENY IN Anywhere # brume-deny-default' \
                    '[11] 26547/tcp ALLOW IN 192.0.2.5 # administrator' \
                    '[12] 26548/udp DENY IN Anywhere # brume-deny-default'
            fi
            ;;
        ufw) [ "${failure}" != ufw-delete ] || return 1 ;;
        nft)
            if [ "$2" = list ]; then
                [ "${failure}" != nft-query ] || return 1
                if [ "${absent}" != true ]; then echo 'table inet brume'; fi
                echo 'table inet administrator'
            else
                [ "${failure}" != nft-delete ] || return 1
            fi
            ;;
        iptables|ip6tables)
            [ "${failure}" != "$1:$2" ] || return 1
            if [ "$2" = -S ] && [ "${absent}" != true ]; then
                printf '%s\n' '-P INPUT ACCEPT' '-N BRUME_WHITELIST' \
                    '-A INPUT -p tcp -m tcp --dport 26547 -j BRUME_WHITELIST' \
                    '-A INPUT -p udp -m udp --dport 26547 -j BRUME_WHITELIST' \
                    '-A INPUT -p tcp --dport 22 -j ACCEPT'
            fi
            ;;
    esac
    return 0
}

persist_nftables_rules() {
    printf '%s\n' persist-nft >> "${events}"
    [ "${failure}" != nft-persist ]
}
persist_iptables_rules() {
    printf '%s\n' persist-iptables >> "${events}"
    [ "${failure}" != iptables-persist ]
}

expect_failure() {
    if "$@" >/dev/null 2>&1; then
        echo "防火墙清理错误被忽略：${failure}" >&2
        exit 1
    fi
}

# 对象确实存在时，每个查询、删除、清空和持久化错误都必须上报。
for failure in firewalld-query firewalld-zones firewalld-rule firewalld-delete firewalld-reload; do
    expect_failure remove_firewall_firewalld 26547
done
for failure in ufw-query ufw-delete; do
    expect_failure remove_firewall_ufw 26547
done
for failure in nft-query nft-delete nft-persist; do
    expect_failure remove_firewall_nftables
done
for failure in iptables:-S iptables:-D iptables:-F iptables:-X \
    ip6tables:-S ip6tables:-D ip6tables:-F ip6tables:-X iptables-persist; do
    expect_failure remove_firewall_iptables 26547
done

# 查询成功且目标对象不存在属于幂等成功，不能执行删除命令。
failure="" absent=true
: > "${events}"
remove_firewall_firewalld 26547 >/dev/null
remove_firewall_ufw 26547 >/dev/null
remove_firewall_nftables >/dev/null
remove_firewall_iptables 26547 >/dev/null
if grep -qE -- '--remove-rich-rule|--delete-zone|ufw --force delete|nft delete|tables -[DFX]' "${events}"; then
    echo '不存在的对象不应执行删除' >&2
    exit 1
fi

# UFW 按编号倒序删除，仅匹配本端口并带 Brume 注释的规则。
absent=false
: > "${events}"
remove_firewall_ufw 26547 >/dev/null
[[ "$(grep '^ufw ' "${events}")" == $'ufw --force delete 10\nufw --force delete 2' ]]

# IPv4 和 IPv6 的引用及链均要移除，其他 INPUT 规则必须保留。
: > "${events}"
remove_firewall_iptables 26547 >/dev/null
grep -qx 'iptables -D INPUT -p tcp -m tcp --dport 26547 -j BRUME_WHITELIST' "${events}"
grep -qx 'ip6tables -D INPUT -p udp -m udp --dport 26547 -j BRUME_WHITELIST' "${events}"
grep -qx 'iptables -X BRUME_WHITELIST' "${events}"
grep -qx 'ip6tables -X BRUME_WHITELIST' "${events}"
if grep -q -- '-D INPUT.*--dport 22' "${events}"; then
    echo '清理 Brume 规则误删了其他 INPUT 规则' >&2
    exit 1
fi

# setup 不能吞掉其内部清理阶段的失败。
for backend in firewalld ufw nftables iptables; do
    (
        case "${backend}" in
            firewalld) remove_firewall_firewalld() { return 1; } ;;
            ufw) remove_firewall_ufw() { return 1; } ;;
            nftables) remove_firewall_nftables() { return 1; } ;;
            iptables) remove_firewall_iptables() { return 1; } ;;
        esac
        is_ufw_active() { return 0; }
        if "setup_firewall_${backend}" 26547 192.0.2.1 >/dev/null 2>&1; then
            echo "${backend} 配置吞掉了清理失败" >&2
            exit 1
        fi
    )
done
