#!/usr/bin/env bash
set -euo pipefail

# 在 Docker 的默认权限下验证真实命令探测，并用本地安装包隔离发布服务。
grep -Eq '^[^ ]+ /tmp [^ ]+ ([^ ]*,)?noexec(,| )' /proc/mounts
case "${1:?需要容器发行版}" in
    debian) systemctl --version >/dev/null ;;
    alpine) command -v rc-service >/dev/null ;;
    *) exit 1 ;;
esac
command -v nft >/dev/null
command -v iptables >/dev/null

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
source "${script_dir}/../install.sh"
test_dir=$(mktemp -d /root/brume-install-ci.XXXXXX)
trap 'rm -rf -- "${test_dir}"' EXIT
INSTALL_DIR="${test_dir}/bin"
CONFIG_DIR="${test_dir}/etc"
CONFIG_FILE="${CONFIG_DIR}/brume.conf"

# 即使容器内安装了 sudo，已经是 root 的安装脚本也不需要它。
has_sudo=true
sudo() { echo 'root 安装不应调用 sudo' >&2; return 1; }
init_system=$(detect_init_system)
firewall=$(detect_firewall)
printf '容器检测结果：服务管理器 %s，防火墙 %s\n' "${init_system}" "${firewall}"
[[ "${init_system}" == standalone && "${firewall}" == none ]]

cat > "${test_dir}/brume" <<'EOF'
#!/bin/bash
case "$1" in
    --help) echo '--config --check-config --dns-servers' ;;
    --config)
        [ "$3" = --check-config ] && grep -Eq '^port=2654[78]$' "$2"
        ;;
    *) exit 1 ;;
esac
EOF
chmod 755 "${test_dir}/brume"
tar -czf "${test_dir}/release.tar.gz" -C "${test_dir}" brume
curl() {
    case "$1" in
        -s) echo '{"tag_name": "9.9.9"}' ;;
        -fL) cp -- "${test_dir}/release.tar.gz" "$3" ;;
        *) return 1 ;;
    esac
}

if main </dev/null 2>"${test_dir}/no-input.log"; then
    echo '无菜单输入时应结束安装' >&2
    exit 1
fi
grep -q '未收到操作选项' "${test_dir}/no-input.log"
if main <<< '1'; then
    echo '安装配置输入不完整时应结束安装' >&2
    exit 1
fi
[[ ! -e "${INSTALL_DIR}" && ! -e "${CONFIG_FILE}" ]]

printf '1\n26547\ny\nadmin\nsecret\\pass\n127.0.0.1\n9.9.9\ny\n' | main >"${test_dir}/install.log"
grep -Fq "exec ${INSTALL_DIR}/brume --config ${CONFIG_FILE}" "${test_dir}/install.log"
[[ -x "${INSTALL_DIR}/brume" && "$(stat -c '%a' "${CONFIG_FILE}")" == 600 ]]
grep -Fxq 'password=secret\pass' "${CONFIG_FILE}"
grep -Fxq 'whitelist=127.0.0.1' "${CONFIG_FILE}"
[[ ! -e /etc/systemd/system/brume.service && ! -e /etc/init.d/brume ]]

printf '3\n26548\nn\n\n\ny\n' | main
grep -Fxq 'port=26548' "${CONFIG_FILE}"
if printf '3\n26549\nn\n\n\ny\n' | main; then
    echo '无效配置不应覆盖容器的现有配置' >&2
    exit 1
fi
grep -Fxq 'port=26548' "${CONFIG_FILE}"
printf '4\n' | main
grep -Fxq 'port=26548' "${CONFIG_FILE}"
grep -Fxq 'password=secret\pass' "${CONFIG_FILE}"
printf '2\ny\n' | main
[[ ! -e "${INSTALL_DIR}/brume" && ! -e "${CONFIG_FILE}" ]]
echo '容器内安装、配置修改及校验、更新和卸载均通过。'
