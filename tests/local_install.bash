#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
installer="${script_dir}/../install-local.sh"
# Git Bash 可能启用 igncr，单独检查字节才能发现 Linux 上的 CRLF 错误。
if LC_ALL=C grep -q $'\r' "${installer}"; then
    echo '本地安装脚本必须使用 LF 换行。' >&2
    exit 1
fi
bash -n "${installer}"
source "${installer}"
if [ "${BRUME_LOCAL_CONTAINER:-}" = 1 ]; then
    [[ "$(detect_init_system)" == standalone ]]
fi

temp_base=$(cd -- "${TMPDIR:-/tmp}" && pwd -P)
test_dir=$(mktemp -d "${temp_base}/brume-local-test.XXXXXX")
cleanup() {
    # 只删除本测试创建的临时目录，拒绝不符合约定的清理路径。
    case "${test_dir}" in
        "${temp_base}"/brume-local-test.*) rm -rf -- "${test_dir}" ;;
        *) echo '拒绝清理未知测试目录。' >&2; return 1 ;;
    esac
}
trap cleanup EXIT
INSTALL_DIR="${test_dir}/local install"
CONFIG_DIR="${INSTALL_DIR}/.brume"
CONFIG_FILE="${CONFIG_DIR}/brume.conf"
LOCAL_BINARY="${INSTALL_DIR}/brume"
test_service_file="${test_dir}/brume-local.service"
mkdir "${INSTALL_DIR}"
cp -- "${installer}" "${INSTALL_DIR}/install-local.sh"

# 所有特权操作都限制在测试目录，不接触宿主服务、防火墙或网络。
id() { if [ "${1:-}" = -u ]; then echo 0; else command id "$@"; fi; }
detect_init_system() { echo standalone; }
detect_firewall() { echo none; }
curl() { echo '本地安装不应调用 curl' >&2; exit 1; }
wget() { echo '本地安装不应调用 wget' >&2; exit 1; }
acquire_mutation_lock() { :; }
execute_privileged() {
    case "$1" in
        tee) command tee "${test_service_file}" ;;
        sed) command sed "$2" "$3" "${test_service_file}" ;;
        chmod)
            if [[ "$3" == /etc/* ]]; then
                command chmod "$2" "${test_service_file}"
            else
                command "$@"
            fi
            ;;
        systemctl|rc-update|update-rc.d|chkconfig) : ;;
        *) command "$@" ;;
    esac
}

# 不要求源文件有执行位；安装器应复制后赋权，再校验目标目录中的程序。
cat > "${LOCAL_BINARY}" <<'EOF'
#!/bin/sh
# local-test-v1
case "$1" in
    --help) echo '--config --check-config --dns-servers' ;;
    --config)
        [ "${3:-}" = --check-config ] || exit 1
        ! grep -q '^whitelist=invalid$' "$2"
        ;;
    *) exit 1 ;;
esac
EOF
chmod 600 "${LOCAL_BINARY}"
validate_local_paths
bash "${INSTALL_DIR}/install-local.sh" --help > "${test_dir}/help.log"
grep -Fq "${INSTALL_DIR}" "${test_dir}/help.log"
if main --binary > "${test_dir}/missing-arg.log" 2>&1; then
    echo '--binary 缺少路径时应拒绝执行。' >&2
    exit 1
fi
if main </dev/null > "${test_dir}/no-input.log" 2>&1; then
    echo '未提供菜单输入时应拒绝执行。' >&2
    exit 1
fi
if printf '1\n' | main > "${test_dir}/partial-input.log" 2>&1; then
    echo '安装配置输入不完整时应拒绝执行。' >&2
    exit 1
fi
[[ ! -e "${CONFIG_FILE}" ]]

# Git Bash 无法可靠地对新目录应用 POSIX mode；Linux 保留真实创建路径。
case "$(uname -s)" in MINGW*|MSYS*) mkdir "${CONFIG_DIR}" ;; esac
secret='test#= $value \pass'
printf '1\n26547\ny\nadmin\n%s\n127.0.0.1\ny\n' "${secret}" | main > "${test_dir}/install.log"
[[ -x "${INSTALL_DIR}/brume" ]]
grep -Fxq "password=${secret}" "${CONFIG_FILE}"
grep -Fxq 'whitelist=127.0.0.1' "${CONFIG_FILE}"
grep -Fq "exec '${INSTALL_DIR}/brume' --config '${CONFIG_FILE}'" "${test_dir}/install.log"
case "$(uname -s)" in
    MINGW*|MSYS*) ;;
    *)
        [[ "$(stat -c %a "${CONFIG_DIR}")" == 700 ]]
        [[ "$(stat -c %a "${CONFIG_FILE}")" == 600 ]]
        ;;
esac

# 重复安装只需选择菜单，不再次提示配置，也不能丢失密码等字面值。
printf '1\n' | main > "${test_dir}/reinstall.log"
grep -Fxq "password=${secret}" "${CONFIG_FILE}"
grep -Fxq 'port=26547' "${CONFIG_FILE}"

# 本地更新使用另一文件，保留实际生效的所有配置。
write_config_file 26547 admin "${secret}" 127.0.0.1 4 90 9.9.9.9
candidate="${test_dir}/brume.new"
sed 's/local-test-v1/local-test-v2/' "${INSTALL_DIR}/brume" > "${candidate}"
printf '4\n' | main --binary "${candidate}" > "${test_dir}/upgrade.log"
grep -q 'local-test-v2' "${INSTALL_DIR}/brume"
grep -Fxq "password=${secret}" "${CONFIG_FILE}"
grep -Fxq 'dns_servers=9.9.9.9' "${CONFIG_FILE}"
grep -Fxq 'tcp_timeout=4' "${CONFIG_FILE}"

printf '3\n26548\nn\n\n\ny\n' | main > "${test_dir}/modify.log"
grep -Fxq 'port=26548' "${CONFIG_FILE}"
grep -Fxq "password=${secret}" "${CONFIG_FILE}"
cp -- "${CONFIG_FILE}" "${test_dir}/previous.conf"
if printf '3\n26549\nn\n\ninvalid\ny\n' | main > "${test_dir}/invalid.log" 2>&1; then
    echo '候选配置校验失败时应保留现有配置。' >&2
    exit 1
fi
cmp "${CONFIG_FILE}" "${test_dir}/previous.conf"

# 缺失、无法执行和不支持配置文件的候选程序都不能替换已安装文件。
cp -- "${INSTALL_DIR}/brume" "${test_dir}/previous-binary"
for kind in missing broken incompatible; do
    LOCAL_BINARY="${test_dir}/${kind}"
    case "${kind}" in
        broken) printf '#!/bin/sh\nexit 1\n' > "${LOCAL_BINARY}" ;;
        incompatible) printf '#!/bin/sh\necho old-version\n' > "${LOCAL_BINARY}" ;;
    esac
    if prepare_binary > "${test_dir}/${kind}.log" 2>&1; then
        echo "错误的候选程序未被拒绝：${kind}" >&2
        exit 1
    fi
    cmp "${INSTALL_DIR}/brume" "${test_dir}/previous-binary"
    [[ -z "$(find "${INSTALL_DIR}" -maxdepth 1 -type d -name '.brume-local.*' -print)" ]]
done
LOCAL_BINARY=${candidate}

# 服务生成和配置解析共用真实实现，覆盖包含空格的本地目录。
get_service_file() { printf '%s\n' "${test_service_file}"; }
get_systemd_dropin_directories() { printf '%s\n' "${test_dir}/missing.service.d"; }
for init_system in systemd openrc sysvinit; do
    create_service "${init_system}" > "${test_dir}/${init_system}.log"
    bash -n "${test_service_file}"
    get_current_config_auto "${init_system}"
    [[ "${port}" == 26548 && "${password}" == "${secret}" ]]
    verify_local_service_owner "${init_system}"
    if [ "${init_system}" = openrc ]; then
        (
            # 调用生成的真实启动/停止函数，确认路径按单个参数传给 OpenRC 工具。
            RC_SVCNAME=brume-local
            source "${test_service_file}"
            ebegin() { :; }
            eend() { return "$1"; }
            start-stop-daemon() { printf '%s\0' "$@" > "${test_dir}/daemon-args"; }
            start
            mapfile -d '' -t daemon_args < "${test_dir}/daemon-args"
            [[ "${daemon_args[2]}" == "${INSTALL_DIR}/brume" ]]
            [[ "${daemon_args[8]}" == --config && "${daemon_args[9]}" == "${CONFIG_FILE}" ]]
            stop
            mapfile -d '' -t daemon_args < "${test_dir}/daemon-args"
            [[ "${daemon_args[0]}" == --stop && "${daemon_args[2]}" == "${INSTALL_DIR}/brume" ]]
        )
    fi
done
printf 'ExecStart=/other/brume --config /other/brume.conf\n' > "${test_service_file}"
if verify_local_service_owner systemd > "${test_dir}/foreign-service.log" 2>&1; then
    echo '不应接管其他安装目录的服务。' >&2
    exit 1
fi

# 脚本复制到事务目录后，独立进程仍应使用原安装目录和指定的二进制。
transaction_dir=$(mktemp -d "${CONFIG_DIR}/.modify.XXXXXX")
mkdir "${test_dir}/tools"
printf '#!/bin/sh\nexec "$@"\n' > "${test_dir}/tools/setsid"
chmod 755 "${test_dir}/tools/setsid"
{
    printf '#!/usr/bin/env bash\nsource %q\n' "${installer}"
    cat <<'EOF'
printf '%s\n%s\n%s\n' "${INSTALL_DIR}" "${CONFIG_DIR}" "${LOCAL_BINARY}" > "$2/paths"
touch "$2/ready"
while [ ! -f "$2/go" ]; do sleep 0.05; done
touch "$2/done"
EOF
} > "${transaction_dir}/installer.sh"
exec {mutation_lock_fd}>"${test_dir}/worker.lock"
PATH="${test_dir}/tools:${PATH}" schedule_independent_worker "${transaction_dir}" systemd "${test_dir}/worker.log" --apply-modify
exec {mutation_lock_fd}>&-
expected=$(printf '%s\n%s\n%s\n' "${INSTALL_DIR}" "${CONFIG_DIR}" "${LOCAL_BINARY}")
[[ "$(cat "${transaction_dir}/paths")" == "${expected}" ]]
for ((attempt=0; attempt<50; attempt++)); do
    [ ! -f "${transaction_dir}/done" ] || break
    sleep 0.05
done
[[ -f "${transaction_dir}/done" ]]

printf '2\ny\n' | main > "${test_dir}/uninstall.log"
[[ ! -e "${INSTALL_DIR}/brume" && ! -e "${CONFIG_FILE}" ]]
[[ -f "${INSTALL_DIR}/install-local.sh" && -f "${candidate}" ]]
echo '本地安装、重复安装、更新、配置修改、服务路径及事务目录回归通过。'
