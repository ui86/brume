#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
source "${script_dir}/../install.sh"

events=$(mktemp)
trap 'rm -f -- "${events}"' EXIT

execute_privileged() { printf '%s\n' "$*" >> "${events}"; }
remove_firewall_firewalld() { :; }
remove_firewall_ufw() { :; }
remove_firewall_nftables() { :; }
remove_firewall_iptables() { :; }
persist_iptables_rules() { :; }
is_ufw_active() { return 0; }
ip6tables() { :; }

for backend in firewalld ufw nftables iptables; do
    : > "${events}"
    "setup_firewall_${backend}" 26547 192.0.2.1 >/dev/null
    case "${backend}" in
        firewalld)
            grep -q 'protocol="udp" accept' "${events}"
            grep -q 'protocol="udp" drop' "${events}"
            ;;
        ufw)
            grep -q 'proto udp comment brume-whitelist' "${events}"
            grep -q 'proto udp comment brume-deny-default' "${events}"
            ;;
        nftables)
            grep -q 'ip saddr 192.0.2.1 udp dport 26547 accept' "${events}"
            grep -q 'udp dport 26547 drop' "${events}"
            ;;
        iptables)
            grep -q 'iptables -I INPUT -p udp --dport 26547' "${events}"
            grep -q 'iptables -A BRUME' "${events}"
            grep -q -- '-p udp --dport 26547 -j ACCEPT' "${events}"
            grep -q -- '-p udp --dport 26547 -j DROP' "${events}"
            ;;
    esac
done

for backend in firewalld ufw nftables iptables; do
    : > "${events}"
    "setup_firewall_${backend}" 26547 '192.0.2.1,2001:db8::1' >/dev/null
    case "${backend}" in
        firewalld)
            grep -q 'family="ipv6" source address="2001:db8::1".*protocol="udp" accept' "${events}"
            grep -q 'family="ipv6".*protocol="udp" drop' "${events}"
            ;;
        ufw)
            grep -q 'allow from 2001:db8::1 to any port 26547 proto udp' "${events}"
            ;;
        nftables)
            grep -q 'ip6 saddr 2001:db8::1 udp dport 26547 accept' "${events}"
            ;;
        iptables)
            grep -q 'ip6tables -I INPUT -p udp --dport 26547' "${events}"
            grep -q 'ip6tables -A BRUME_WHITELIST -s 2001:db8::1 -p udp' "${events}"
            grep -q 'ip6tables -A BRUME_WHITELIST -p udp --dport 26547 -j DROP' "${events}"
            ;;
    esac
done

(
    source "${script_dir}/../install.sh"
    execute_privileged() { printf '%s\n' "$*" >> "${events}"; }
    persist_iptables_rules() { :; }
    ip6tables() { :; }
    : > "${events}"
    remove_firewall_iptables 26547 >/dev/null
    grep -q 'ip6tables -D INPUT -p udp --dport 26547' "${events}"
    grep -q 'ip6tables -X BRUME_WHITELIST' "${events}"
)
