#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
source "${script_dir}/../install.sh"

test_dir=$(mktemp -d)
CONFIG_DIR="${test_dir}/standard"
CONFIG_FILE="${CONFIG_DIR}/brume.conf"
test_service_file="${test_dir}/brume.service"
actual_config="${test_dir}/custom config.conf"
test_dropin_directory="${test_dir}/brume.service.d"
cleanup() {
    rm -f -- "${CONFIG_FILE}" "${test_service_file}" "${actual_config}"
    rmdir -- "${test_dropin_directory}" 2>/dev/null || true
    rmdir -- "${CONFIG_DIR}" 2>/dev/null || true
    rmdir -- "${test_dir}"
}
trap cleanup EXIT

# 只替换服务文件位置，配置提取、分词和迁移均使用真实实现
get_service_file() { printf '%s\n' "${test_service_file}"; }

# 系统目录检查可以独立替换，测试无需写入真实 systemd 配置
dropin_directories=$(get_systemd_dropin_directories)
grep -qx '/etc/systemd/system/brume.service.d' <<< "${dropin_directories}"
grep -qx '/run/systemd/system/brume.service.d' <<< "${dropin_directories}"
grep -qx '/usr/lib/systemd/system/service.d' <<< "${dropin_directories}"
get_systemd_dropin_directories() { printf '%s\n' "${test_dropin_directory}"; }
cat > "${actual_config}" <<'EOF'
port=26547
username=原有用户
password=原有密码#=$value
whitelist=127.0.0.1,192.0.2.0/24
tcp_timeout=7
udp_timeout=80
dns_servers=9.9.9.9
EOF

printf 'ExecStart=%s/brume "--config=%s"\n' "${INSTALL_DIR}" "${actual_config}" > "${test_service_file}"
get_current_config_auto systemd
[[ "${port}" == 26547 && "${user}" == 原有用户 && "${password}" == '原有密码#=$value' ]]
[[ "${whitelist}" == '127.0.0.1,192.0.2.0/24' && "${dns_servers}" == 9.9.9.9 ]]
[[ "${tcp_timeout}" == 7 && "${udp_timeout}" == 80 && "${uses_config_file}" == true ]]

# 配置文件前后的命令行覆盖优先级均与 Rust 实现一致
printf 'ExecStart=%s/brume --port=12345 --user="新用户" --config "%s" --pwd="新 密码" --whitelist=198.51.100.1 --tcp-timeout=4 --udp-timeout 90 --dns-servers=1.0.0.1 --port 26548\n' \
    "${INSTALL_DIR}" "${actual_config}" > "${test_service_file}"
get_current_config_auto systemd
[[ "${port}" == 26548 && "${user}" == 新用户 && "${password}" == '新 密码' ]]
[[ "${whitelist}" == 198.51.100.1 && "${dns_servers}" == 1.0.0.1 ]]
[[ "${tcp_timeout}" == 4 && "${udp_timeout}" == 90 ]]

# 短参数和等号语法也必须保留认证及来源限制
printf 'ExecStart=%s/brume -p=26549 -user admin -pwd=secret --whitelist 127.0.0.1\n' "${INSTALL_DIR}" > "${test_service_file}"
get_current_config_auto systemd
[[ "${port}" == 26549 && "${user}" == admin && "${password}" == secret && "${whitelist}" == 127.0.0.1 ]]

for init_system in openrc sysvinit; do
    if [ "${init_system}" = openrc ]; then
        executable=command
        arguments=command_args
    else
        executable=DAEMON
        arguments=DAEMON_ARGS
    fi
    printf '%s="%s/brume"\n%s="--config=%s --port=26550 --user=admin --pwd=secret"\n' \
        "${executable}" "${INSTALL_DIR}" "${arguments}" "${CONFIG_FILE}" > "${test_service_file}"
    mkdir -p "${CONFIG_DIR}"
    cp -- "${actual_config}" "${CONFIG_FILE}"
    get_current_config_auto "${init_system}"
    [[ "${port}" == 26550 && "${user}" == admin && "${password}" == secret ]]
    [[ "${whitelist}" == '127.0.0.1,192.0.2.0/24' ]]
done

# 升级必须把自定义配置与覆盖项完整迁移到标准配置，不能恢复默认值
printf 'ExecStart=%s/brume "--config=%s" --port=26551\n' "${INSTALL_DIR}" "${actual_config}" > "${test_service_file}"
check_architecture() { echo amd64; }
get_latest_version() { echo 1.0.7; }
prepare_binary() { :; }
install_prepared_binary() { :; }
create_service() { :; }
schedule_upgrade_restart() { :; }
upgrade systemd >/dev/null
port= user= password= whitelist= tcp_timeout= udp_timeout= dns_servers=
load_config_file
[[ "${port}" == 26551 && "${user}" == 原有用户 && "${password}" == '原有密码#=$value' ]]
[[ "${whitelist}" == '127.0.0.1,192.0.2.0/24' && "${dns_servers}" == 9.9.9.9 ]]

for arguments in \
    '--port' \
    '--unknown=secret' \
    '--user=admin' \
    '--port=65536' \
    '--config=relative.conf' \
    '--config=/missing-brume.conf' \
    "--config='${actual_config}' --config='${actual_config}'" \
    '--pwd="unterminated' \
    '--user=$USER --pwd=secret' \
    '--user=admin --pwd=back\slash' \
    '--user=admin --pwd=%n'; do
    printf 'ExecStart=%s/brume %s\n' "${INSTALL_DIR}" "${arguments}" > "${test_service_file}"
    if get_current_config_auto systemd >/dev/null 2>&1; then
        echo "不安全的服务参数被接受：${arguments}" >&2
        exit 1
    fi
done

printf 'ExecStart=/other/brume --port=26547\n' > "${test_service_file}"
if get_current_config_auto systemd >/dev/null 2>&1; then
    echo '未检查服务实际执行文件' >&2
    exit 1
fi
printf 'ExecStart=%s/brume\nExecStart=%s/brume\n' "${INSTALL_DIR}" "${INSTALL_DIR}" > "${test_service_file}"
if get_current_config_auto systemd >/dev/null 2>&1; then
    echo '重复的服务命令被接受' >&2
    exit 1
fi

# 无法安全解析时，升级不能写配置、替换程序或重启服务
before=$(cat "${CONFIG_FILE}")
prepare_binary() { echo '无效配置不应进入下载阶段' >&2; exit 1; }
if upgrade systemd >/dev/null 2>&1; then
    echo '无效服务配置仍然升级成功' >&2
    exit 1
fi
[[ "$(cat "${CONFIG_FILE}")" == "${before}" ]]

# 扩展目录存在时，即使主服务文件看似有效也不能迁移
printf 'ExecStart=%s/brume "--config=%s"\n' "${INSTALL_DIR}" "${actual_config}" > "${test_service_file}"
mkdir "${test_dropin_directory}"
if get_current_config_auto systemd >/dev/null 2>&1; then
    echo '存在 systemd 扩展定义时仍然提取了主文件配置' >&2
    exit 1
fi
if upgrade systemd >/dev/null 2>&1; then
    echo '存在 systemd 扩展定义时仍然执行升级' >&2
    exit 1
fi
[[ "$(cat "${CONFIG_FILE}")" == "${before}" ]]
rmdir "${test_dropin_directory}"

# 缺失文件、目录和实际读取失败都必须可靠返回失败
if load_config_file "${test_dir}/missing.conf" >/dev/null 2>&1 \
    || load_config_file "${test_dir}" >/dev/null 2>&1; then
    echo '无效配置文件被当成空配置' >&2
    exit 1
fi
(
    # 模拟权限或输入输出错误，不依赖测试进程是否具有 root 权限
    cat() { printf 'username=\npassword=\n'; return 1; }
    user=admin password=secret
    if load_config_file "${actual_config}" >/dev/null 2>&1; then
        echo '读取失败的配置仍被接受' >&2
        exit 1
    fi
    [[ "${user}" == admin && "${password}" == secret ]]
)
