#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
source "${script_dir}/../install.sh"

test_dir=$(mktemp -d)
trap 'rm -f -- "${test_dir}"/*; rmdir -- "${test_dir}"' EXIT
config_file="${test_dir}/nftables.conf"
export TMPDIR="${test_dir}"
printf '原有规则\n' > "${config_file}"

execute_privileged() {
    case "$1" in
        nft)
            if [ "${export_fails:-false}" = true ]; then
                printf '不完整的规则\n'
                return 1
            fi
            printf '新的规则\n'
            ;;
        cp)
            # Linux 中确认实际复制源为仅当前用户可读写的私有文件
            case "$(uname -s)" in
                MINGW*|MSYS*) ;;
                *) [[ "$(stat -c %a "$3")" == 600 ]] || return 1 ;;
            esac
            if [ "${copy_fails:-false}" = true ]; then
                return 1
            fi
            "$@"
            ;;
        *) echo '出现了未预期的命令' >&2; return 1 ;;
    esac
}

persist_nftables_rules "${config_file}"
[[ "$(cat "${config_file}")" == 新的规则 ]]
[[ "$(find "${test_dir}" -maxdepth 1 -type f | wc -l)" -eq 1 ]]

# 导出失败时保留旧配置，并清理已写入部分内容的私有文件
export_fails=true
if persist_nftables_rules "${config_file}" 2>/dev/null; then
    echo '导出失败仍然覆盖了持久化配置' >&2
    exit 1
fi
[[ "$(cat "${config_file}")" == 新的规则 ]]
[[ "$(find "${test_dir}" -maxdepth 1 -type f | wc -l)" -eq 1 ]]

export_fails=false
copy_fails=true
if persist_nftables_rules "${config_file}"; then
    echo '复制失败未向上报告' >&2
    exit 1
fi
[[ "$(find "${test_dir}" -maxdepth 1 -type f | wc -l)" -eq 1 ]]

# 未安装持久化配置时不创建文件
persist_nftables_rules "${test_dir}/missing.conf"
[[ ! -e "${test_dir}/missing.conf" ]]
