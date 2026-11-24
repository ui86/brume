#!/bin/sh
set -eu

umask 077
config=/run/brume/brume.conf
staged=$(mktemp /run/brume/.brume.conf.XXXXXX)
trap 'rm -f "$staged"' EXIT
trap 'exit 1' HUP INT TERM

cr=$(printf '\r')
lf='
'

# 配置按行读取，拒绝换行以防一个环境变量写入多个配置项。
write_setting() {
    case "$2" in
        *"$cr"*|*"$lf"*)
            printf 'Invalid value for %s: line breaks are not allowed.\n' "$1" >&2
            exit 1
            ;;
    esac
    printf '%s=%s\n' "$1" "$2"
}

{
    write_setting port "${BRUME_PORT:-1080}"
    write_setting username "${BRUME_USERNAME-}"
    write_setting password "${BRUME_PASSWORD-}"
    write_setting whitelist "${BRUME_WHITELIST-}"
    write_setting tcp_timeout "${BRUME_TCP_TIMEOUT:-0}"
    write_setting udp_timeout "${BRUME_UDP_TIMEOUT:-60}"
    if [ -n "${BRUME_DNS_SERVERS-}" ]; then
        write_setting dns_servers "$BRUME_DNS_SERVERS"
    fi
} > "$staged"

mv -f "$staged" "$config"
trap - EXIT HUP INT TERM
unset BRUME_USERNAME BRUME_PASSWORD
exec /usr/local/bin/brume --config "$config" "$@"
