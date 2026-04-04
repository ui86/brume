#!/bin/bash

# Brume 服务器安装脚本
# 支持 linux/amd64 和 linux/arm64 架构
# 功能：安装、卸载、修改配置

# 默认参数
DEFAULT_PORT=1080
DEFAULT_USER=""
DEFAULT_PASSWORD=""
DEFAULT_WHITELIST=""
INSTALL_DIR="/usr/local/bin"
SERVICE_NAME="brume"
GITHUB_REPO="ui86/brume"

# 检测是否有sudo命令并设置执行特权命令的方式
has_sudo=false
if command -v sudo &> /dev/null; then
    has_sudo=true
fi

# 执行特权命令的辅助函数
execute_privileged() {
    if [ "$has_sudo" = true ]; then
        sudo "$@"
    else
        "$@"
    fi
}

# 颜色定义
RED="\033[31m"
GREEN="\033[32m"
YELLOW="\033[33m"
BLUE="\033[34m"
RESET="\033[0m"

# 显示帮助信息
show_help() {
    echo -e "${BLUE}Brume 服务器安装脚本${RESET}"
    echo "使用方法: 运行脚本后根据提示进行交互操作"
    echo ""
    echo "此脚本支持以下操作："
    echo "  1. 安装Brume服务器"
    echo "  2. 卸载Brume服务器"
    echo "  3. 修改Brume服务器配置"
    echo ""
    echo "注意：此脚本需要以root用户或使用sudo运行"
    exit 0
}

# 检测系统架构
check_architecture() {
    local arch
    arch=$(uname -m)
    case "$arch" in
        x86_64) echo "amd64" ;;
        aarch64) echo "arm64" ;;
        *) echo -e "${RED}不支持的系统架构: $arch${RESET}" >&2 ; exit 1 ;;
    esac
}

# 获取最新版本号
get_latest_version() {
    # 将提示信息输出到标准错误
    echo "正在获取最新版本号..." >&2
    local latest_version
    # 使用GitHub API获取最新版本号
    # 使用更通用的方法提取版本号，不依赖grep -P
    latest_version=$(curl -s "https://api.github.com/repos/${GITHUB_REPO}/releases/latest" | grep '"tag_name":' | sed -E 's/.*"tag_name": "([^"]+)".*/\1/')
    if [ -z "$latest_version" ]; then
        echo -e "${RED}获取最新版本号失败，使用默认版本 1.0.0${RESET}" >&2
        latest_version="1.0.0"
    fi
    echo "最新版本: $latest_version" >&2
    # 只将纯版本号输出到标准输出，供命令替换捕获
    echo "$latest_version"
}

# 下载并安装二进制文件
download_and_install() {
    local version=$1
    local arch=$2
    # shellcheck disable=SC2155
    local temp_dir=$(mktemp -d)
    local download_url="https://github.com/${GITHUB_REPO}/releases/download/${version}/brume-${version}-linux-${arch}.tar.gz"
    local tar_file="${temp_dir}/brume-${version}-linux-${arch}.tar.gz"
    
    echo "正在下载 ${download_url}..."
    if ! curl -L -o "${tar_file}" "${download_url}"; then
        echo -e "${RED}下载失败，请检查网络连接或版本是否存在${RESET}"
        rm -rf "${temp_dir}"
        exit 1
    fi
    
    echo "解压安装包..."
    if ! tar -xzf "${tar_file}" -C "${temp_dir}"; then
        echo -e "${RED}解压失败${RESET}"
        rm -rf "${temp_dir}"
        exit 1
    fi
    
    echo "安装到 ${INSTALL_DIR}..."
    if ! execute_privileged mv "${temp_dir}/brume" "${INSTALL_DIR}"; then
        echo -e "${RED}安装失败，请检查权限${RESET}"
        rm -rf "${temp_dir}"
        exit 1
    fi
    
    execute_privileged chmod +x "${INSTALL_DIR}/brume"
    rm -rf "${temp_dir}"
    echo -e "${GREEN}安装成功${RESET}"
}

# 创建systemd服务文件
create_systemd_service() {
    local port=$1
    local user=$2
    local password=$3
    local whitelist=$4
    local service_file="/etc/systemd/system/${SERVICE_NAME}.service"
    
    echo "正在创建systemd服务文件..."
    
    # 构建命令参数
    local cmd_args="-p ${port}"
    if [ -n "${user}" ] && [ -n "${password}" ]; then
        cmd_args="${cmd_args} -user ${user} -pwd ${password}"
    fi
    if [ -n "${whitelist}" ]; then
        cmd_args="${cmd_args} --whitelist ${whitelist}"
    fi
    
    # 创建服务文件
    if ! execute_privileged tee "${service_file}" > /dev/null << EOF
[Unit]
Description=Brume Proxy Server
After=network.target

[Service]
Type=simple
ExecStart=${INSTALL_DIR}/brume ${cmd_args}
Restart=on-failure
RestartSec=5s

[Install]
WantedBy=multi-user.target
EOF
    then
        echo -e "${RED}创建服务文件失败，请检查权限${RESET}"
        exit 1
    fi
    
    # 重新加载systemd配置
execute_privileged systemctl daemon-reload
    # 启用服务（开机自启）
execute_privileged systemctl enable "${SERVICE_NAME}"
    
    echo -e "${GREEN}systemd服务创建成功${RESET}"
}

# 启动服务
start_service() {
    echo "正在启动服务..."
    if ! execute_privileged systemctl start "${SERVICE_NAME}"; then
        echo -e "${RED}启动服务失败，请运行 'systemctl status ${SERVICE_NAME}' 查看详情${RESET}"
        exit 1
    fi
    
    echo -e "${GREEN}服务启动成功${RESET}"
    echo "服务状态:"
    execute_privileged systemctl status "${SERVICE_NAME}" --no-pager

    echo -e "${BLUE}Brume 服务器安装完成！${RESET}"

    echo "配置信息:"
    echo "  端口: ${port}"
    echo "  认证: $(if [ -n "${user}" ] && [ -n "${password}" ]; then echo "启用"; else echo "禁用"; fi)"
    echo "  白名单: $(if [ -n "${whitelist}" ]; then echo "${whitelist}"; else echo "无（允许所有IP）"; fi)"

    echo -e "${YELLOW}管理命令:${RESET}"
    echo "  查看服务状态: systemctl status ${SERVICE_NAME}"
    echo "  停止服务: systemctl stop ${SERVICE_NAME}"
    echo "  重启服务: systemctl restart ${SERVICE_NAME}"
    echo "  禁用开机自启: systemctl disable ${SERVICE_NAME}"
}

# 修改配置函数
modify() {
    local service_file="/etc/systemd/system/${SERVICE_NAME}.service"
    
    # 停止服务
    echo "正在停止服务..."
    execute_privileged systemctl stop "${SERVICE_NAME}" 2>/dev/null
    
    # 更新systemd服务文件
    create_systemd_service "${port}" "${user}" "${password}" "${whitelist}"
    
    # 重启服务
    echo "正在重启服务..."
    if ! execute_privileged systemctl restart "${SERVICE_NAME}"; then
        echo -e "${RED}重启服务失败，请运行 'systemctl status ${SERVICE_NAME}' 查看详情${RESET}"
        exit 1
    fi
    
    echo -e "${GREEN}Brume 服务器配置修改完成！${RESET}"
}



# 卸载函数
uninstall() {
    echo -e "${BLUE}=== Brume 服务器卸载 ===${RESET}"
    
    # 停止服务
    echo "正在停止服务..."
    execute_privileged systemctl stop "${SERVICE_NAME}" 2>/dev/null
    
    # 禁用服务
    echo "正在禁用服务..."
    execute_privileged systemctl disable "${SERVICE_NAME}" 2>/dev/null
    
    # 删除服务文件
    local service_file="/etc/systemd/system/${SERVICE_NAME}.service"
    if [ -f "${service_file}" ]; then
        echo "正在删除服务文件..."
        execute_privileged rm -f "${service_file}"
        execute_privileged systemctl daemon-reload
    fi
    
    # 删除二进制文件
    local binary_path="${INSTALL_DIR}/brume"
    if [ -f "${binary_path}" ]; then
        echo "正在删除二进制文件..."
        execute_privileged rm -f "${binary_path}"
    fi
    
    echo -e "${GREEN}Brume 服务器卸载完成${RESET}"
}

# 交互式获取操作类型
get_action() {
    while true; do
        echo -e "${BLUE}=== Brume 服务器管理 ===${RESET}"
        echo "请选择要执行的操作："
        echo "1. 安装Brume服务器"
        echo "2. 卸载Brume服务器"
        echo "3. 修改Brume服务器配置"
        echo "4. 显示帮助信息"
        
        read -p "请输入选项 (1-4): " choice
        
        case $choice in
            1)
                action="install"
                break
                ;;
            2)
                action="uninstall"
                break
                ;;
            3)
                action="modify"
                break
                ;;
            4)
                show_help
                ;;
            *)
                echo -e "${RED}无效的选项，请重新输入${RESET}"
                ;;
        esac
    done
}

# 交互式获取安装配置参数
get_install_config() {
    # 重置参数为默认值
    port="${DEFAULT_PORT}"
    user="${DEFAULT_USER}"
    password="${DEFAULT_PASSWORD}"
    whitelist="${DEFAULT_WHITELIST}"
    version=""

    # 获取端口号
    while true; do
        read -p "请输入Brume服务器端口号 [默认: ${DEFAULT_PORT}]: " input_port
        if [ -z "${input_port}" ]; then
            port=${DEFAULT_PORT}
            break
        elif [[ "${input_port}" =~ ^[0-9]+$ ]] && [ "${input_port}" -ge 1 ] && [ "${input_port}" -le 65535 ]; then
            port=${input_port}
            break
        else
            echo -e "${RED}无效的端口号，请输入1-65535之间的数字${RESET}"
        fi
    done

    # 获取是否启用认证
    while true; do
        read -p "是否启用用户认证? (y/n) [默认: n]: " enable_auth
        enable_auth=${enable_auth:-n}

        case ${enable_auth} in
            [Yy])
                read -p "请输入用户名: " user
                while [ -z "${user}" ]; do
                    read -p "用户名不能为空，请重新输入: " user
                done

                # shellcheck disable=SC2162
                read -s -p "请输入密码: " password
                echo
                while [ -z "${password}" ]; do
                    read -s -p "密码不能为空，请重新输入: " password
                    echo
                done
                break
                ;;
            [Nn])
                user=""
                password=""
                break
                ;;
            *)
                echo -e "${RED}无效的选项，请输入y或n${RESET}"
                ;;
        esac
    done

    # 获取IP白名单
    read -p "请输入IP白名单（多个IP用逗号分隔，留空表示不限制）: " whitelist

    # 获取版本信息
    read -p "请输入要安装的版本（留空表示最新版本）: " version

    # 显示确认信息
    echo -e "${GREEN}安装配置确认：${RESET}"
    echo "端口: ${port}"
    echo "认证: $(if [ -n "${user}" ] && [ -n "${password}" ]; then echo "启用 (用户名: ${user})"; else echo "禁用"; fi)"
    echo "白名单: $(if [ -n "${whitelist}" ]; then echo "${whitelist}"; else echo "无（允许所有IP）"; fi)"
    echo "版本: $(if [ -n "${version}" ]; then echo "${version}"; else echo "最新版本"; fi)"

    while true; do
        read -p "确认以上配置是否正确? (y/n) [默认: y]: " confirm
        confirm=${confirm:-y}

        case ${confirm} in
            [Yy])
                break
                ;;
            [Nn])
                echo -e "${YELLOW}配置已取消，将重新开始${RESET}"
                get_install_config
                break
                ;;
            *)
                echo -e "${RED}无效的选项，请输入y或n${RESET}"
                ;;
        esac
    done
}

# 交互式获取修改配置参数
get_modify_config() {
    local service_file="/etc/systemd/system/${SERVICE_NAME}.service"
    
    # 检查服务是否存在
    if [ ! -f "${service_file}" ]; then
        echo -e "${RED}Brume 服务未安装，请先安装服务${RESET}"
        exit 1
    fi
    
    # 读取 ExecStart 行
    local exec_start
    exec_start=$(execute_privileged grep '^ExecStart=' "${service_file}" 2>/dev/null | head -n1)
    if [ -z "$exec_start" ]; then
        echo -e "${RED}无法读取服务配置，请检查服务文件${RESET}"
        exit 1
    fi

    # 提取命令参数部分（去掉 'ExecStart=/usr/local/bin/brume '）
    local cmd_args
    cmd_args=$(echo "$exec_start" | sed 's|^ExecStart=/usr/local/bin/brume ||')

    # 初始化默认值
    port="${DEFAULT_PORT}"
    user=""
    password=""
    whitelist=""

    # 使用 while + case 安全解析参数
    set -- $cmd_args  # 将参数拆分为位置参数（注意：不加引号，按空格分割）
    while [ $# -gt 0 ]; do
        case "$1" in
            -p)
                if [ -n "$2" ] && [ "$2" -ge 1 ] && [ "$2" -le 65535 ] 2>/dev/null; then
                    port="$2"
                    shift 2
                else
                    shift
                fi
                ;;
            -user)
                user="$2"
                shift 2
                ;;
            -pwd)
                password="$2"
                shift 2
                ;;
            --whitelist)
                whitelist="$2"
                shift 2
                ;;
            *)
                shift
                ;;
        esac
    done

    # 显示当前配置
    echo -e "${GREEN}当前配置:${RESET}"
    echo "  端口: ${port}"
    echo "  认证: $(if [ -n "${user}" ]; then echo "启用 (用户名: ${user})"; else echo "禁用"; fi)"
    echo "  白名单: $(if [ -n "${whitelist}" ]; then echo "${whitelist}"; else echo "无"; fi)"

    # 获取新的配置参数
    echo -e "${BLUE}请输入新的配置参数（留空表示保持当前配置）${RESET}"

    # 获取端口号
    while true; do
        read -p "请输入Brume服务器端口号 [当前: ${port}]: " input_port
        if [ -z "${input_port}" ]; then
            break
        elif [[ "${input_port}" =~ ^[0-9]+$ ]] && [ "${input_port}" -ge 1 ] && [ "${input_port}" -le 65535 ]; then
            port=${input_port}
            break
        else
            echo -e "${RED}无效的端口号，请输入1-65535之间的数字${RESET}"
        fi
    done

    # 获取是否修改用户认证
    while true; do
        read -p "是否修改用户认证设置? (y/n) [默认: n]: " change_auth
        change_auth=${change_auth:-n}

        case ${change_auth} in
            [Yy])
                while true; do
                    read -p "是否启用用户认证? (y/n): " enable_auth
                    case ${enable_auth} in
                        [Yy])
                            read -p "请输入用户名: " user
                            while [ -z "${user}" ]; do
                                read -p "用户名不能为空，请重新输入: " user
                            done

                            read -s -p "请输入密码: " password
                            echo
                            while [ -z "${password}" ]; do
                                read -s -p "密码不能为空，请重新输入: " password
                                echo
                            done
                            break
                            ;;
                        [Nn])
                            user=""
                            password=""
                            break
                            ;;
                        *)
                            echo -e "${RED}无效的选项，请输入y或n${RESET}"
                            ;;
                    esac
                done
                break
                ;;
            [Nn])
                # 保持当前设置
                if [ -n "${user}" ]; then
                    # 当前有用户，提示是否更改密码
                    read -s -p "请重新输入密码 (留空表示保留当前密码): " input_password
                    echo
                    if [ -n "${input_password}" ]; then
                        password="${input_password}"
                    fi
                    # 注意：如果留空，保留原 password（已在上面解析）
                else
                    user=""
                    password=""
                fi
                break
                ;;
            *)
                echo -e "${RED}无效的选项，请输入y或n${RESET}"
                ;;
        esac
    done

    # 获取IP白名单
    read -p "请输入IP白名单（多个IP用逗号分隔，留空表示保持当前设置）: " input_whitelist
    if [ -n "${input_whitelist}" ]; then
        whitelist="${input_whitelist}"
    fi
    
    # 显示新配置
    echo -e "${GREEN}新配置:${RESET}"
    echo "  端口: ${port}"
    echo "  认证: $(if [ -n "${user}" ] && [ -n "${password}" ]; then echo "启用 (用户名: ${user})"; else echo "禁用"; fi)"
    echo "  白名单: $(if [ -n "${whitelist}" ]; then echo "${whitelist}"; else echo "无"; fi)"
    
    # 确认修改
    while true; do
        read -p "确认修改配置? (y/n) [默认: y]: " confirm
        confirm=${confirm:-y}
        
        case ${confirm} in
            [Yy])
                break
                ;;
            [Nn])
                echo -e "${YELLOW}配置修改已取消${RESET}"
                exit 0
                ;;
            *)
                echo -e "${RED}无效的选项，请输入y或n${RESET}"
                ;;
        esac
    done
}

# 主函数
main() {
    # 检查是否为root用户
    if [ "$(id -u)" != "0" ]; then
        echo -e "${RED}请以root用户或使用sudo运行此脚本${RESET}"
        exit 1
    fi
    
    # 检查依赖工具
    if ! command -v curl &> /dev/null; then
        echo -e "${RED}未找到curl命令，请先安装curl${RESET}"
        exit 1
    fi
    
    if ! command -v systemctl &> /dev/null; then
        echo -e "${RED}未找到systemctl命令，此脚本仅支持使用systemd的Linux系统${RESET}"
        exit 1
    fi
    
    # 交互式获取操作类型
    get_action
    
    # 根据操作类型执行相应的功能
    case "${action}" in
        install)
            echo -e "${BLUE}=== Brume 服务器安装 ===${RESET}"
            
            # 交互式获取安装配置
            get_install_config
            
            # 检测系统架构
            arch=$(check_architecture)
            echo "系统架构: $arch"
            
            # 获取版本号
            if [ -z "${version}" ]; then
                version=$(get_latest_version)
            else
                echo "指定版本: ${version}"
            fi
            
            # 下载并安装
            download_and_install "${version}" "${arch}"
            
            # 创建systemd服务
            create_systemd_service "${port}" "${user}" "${password}" "${whitelist}"
            
            # 启动服务
            start_service
            ;;
        uninstall)
            # 确认卸载
            while true; do
                read -p "确定要卸载Brume服务器吗? 此操作将删除所有相关文件和配置。(y/n) [默认: n]: " confirm
                confirm=${confirm:-n}
                
                case ${confirm} in
                    [Yy])
                        uninstall
                        break
                        ;;
                    [Nn])
                        echo -e "${YELLOW}卸载已取消${RESET}"
                        exit 0
                        ;;
                    *)
                        echo -e "${RED}无效的选项，请输入y或n${RESET}"
                        ;;
                esac
            done
            ;;
        modify)
            echo -e "${BLUE}=== Brume 服务器配置修改 ===${RESET}"
            
            # 交互式获取修改配置
            get_modify_config
            
            # 执行修改操作
            modify
            ;;
        *)
            echo -e "${RED}未知操作: ${action}${RESET}"
            show_help
            ;;
    esac
}

# 执行主函数
main "$@"