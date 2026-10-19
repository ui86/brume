#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
source "${script_dir}/../install.sh"

# Git Bash 没有 flock，真实文件锁与进程继承由 Linux CI 验证
if ! command -v flock >/dev/null; then
    echo '当前环境没有 flock，跳过真实文件锁回归。'
    exit 0
fi

test_dir=$(mktemp -d)
CONFIG_DIR="${test_dir}/config"
has_sudo=false
cleanup() {
    local pid_file
    for pid_file in "${test_dir}/daemon.pid" "${test_dir}/holder.pid"; do
        if [ -f "${pid_file}" ] && [ ! -f "${pid_file%.pid}-completed" ]; then
            kill "$(cat "${pid_file}")" 2>/dev/null || true
        fi
    done
    rm -f -- "${CONFIG_DIR}/.mutation.lock" "${test_dir}/daemon.sh" "${test_dir}/daemon.pid" \
        "${test_dir}/stop-daemon" "${test_dir}/daemon-completed" "${test_dir}/holder.sh" \
        "${test_dir}/holder.pid" "${test_dir}/holder-ready" "${test_dir}/release-holder" "${test_dir}/holder-completed"
    rmdir -- "${CONFIG_DIR}" "${test_dir}"
}
trap cleanup EXIT

cat > "${test_dir}/daemon.sh" <<'EOF'
#!/usr/bin/env bash
while [ ! -f "$1/stop-daemon" ]; do sleep 0.05; done
touch "$1/daemon-completed"
EOF

(
    acquire_mutation_lock
    validate_mutation_lock
    # 竞争者关闭继承的描述符后重新打开锁文件，必须被现有事务拒绝
    if (
        exec {mutation_lock_fd}>&-
        acquire_mutation_lock >/dev/null 2>&1
    ); then
        echo '并发事务获取了已被占用的锁' >&2
        exit 1
    fi
    # 服务管理命令及其启动的常驻进程不能看到锁描述符
    execute_privileged bash -c 'if { : <&"$1"; } 2>/dev/null; then exit 1; fi' -- "${mutation_lock_fd}"
    execute_privileged bash -c 'bash "$1/daemon.sh" "$1" </dev/null >/dev/null 2>&1 & echo "$!" > "$1/daemon.pid"' -- "${test_dir}"
)
kill -0 "$(cat "${test_dir}/daemon.pid")"
(acquire_mutation_lock)
touch "${test_dir}/stop-daemon"
for ((attempt=0; attempt<100; attempt++)); do
    [ ! -f "${test_dir}/daemon-completed" ] || break
    sleep 0.05
done
[[ -f "${test_dir}/daemon-completed" ]]

cat > "${test_dir}/holder.sh" <<'EOF'
#!/usr/bin/env bash
flock -n "$1" || exit 1
touch "$2/holder-ready"
while [ ! -f "$2/release-holder" ]; do sleep 0.05; done
touch "$2/holder-completed"
EOF
(
    acquire_mutation_lock
    bash "${test_dir}/holder.sh" "${mutation_lock_fd}" "${test_dir}" </dev/null >/dev/null 2>&1 &
    echo "$!" > "${test_dir}/holder.pid"
)
for ((attempt=0; attempt<100; attempt++)); do
    [ ! -f "${test_dir}/holder-ready" ] || break
    sleep 0.05
done
[[ -f "${test_dir}/holder-ready" ]]
if (acquire_mutation_lock >/dev/null 2>&1); then
    echo '父进程退出后，独立工作进程没有继续持锁' >&2
    exit 1
fi
touch "${test_dir}/release-holder"
for ((attempt=0; attempt<100; attempt++)); do
    if (acquire_mutation_lock >/dev/null 2>&1); then
        break
    fi
    sleep 0.05
done
[[ "${attempt}" -lt 100 && -f "${test_dir}/holder-completed" ]]
