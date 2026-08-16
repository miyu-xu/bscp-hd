#!/bin/sh
set -eu

LABEL="com.bscp.hd-network"
CONTRACT_VERSION="2"
HELPER="/Library/PrivilegedHelperTools/${LABEL}"
PLIST="/Library/LaunchDaemons/${LABEL}.plist"
PF_CONF="/etc/pf.conf"
PF_BACKUP="/etc/pf.conf.bscp-hd.backup"
PF_ANCHOR="${LABEL}/vpn-nat"
PF_NAT_LINE='nat-anchor "com.bscp.hd-network/*"'
PF_FILTER_LINE='anchor "com.bscp.hd-network/*"'
SOCKET_VMNET="/var/run/socket_vmnet"
VM_SUBNET="192.168.105.0/24"
ROUTE_PROBE="1.1.1.1"
STATE_FILE="/var/run/${LABEL}.egress"
PF_TOKEN_FILE="/var/run/${LABEL}.pf-token"

require_root() {
    if [ "$(id -u)" -ne 0 ]; then
        echo "macOS network setup requires root" >&2
        exit 77
    fi
}

is_regular_nosymlink() {
    [ -f "$1" ] && [ ! -L "$1" ]
}

sha256_file() {
    /usr/bin/shasum -a 256 "$1" | /usr/bin/awk '{print $1}'
}

validate_system_paths() {
    if ! is_regular_nosymlink "${PF_CONF}"; then
        echo "unsafe or missing PF configuration: ${PF_CONF}" >&2
        exit 66
    fi
    for path in "${HELPER}" "${PLIST}" "${PF_BACKUP}" "${STATE_FILE}" "${PF_TOKEN_FILE}"; do
        if [ -e "${path}" ] && ! is_regular_nosymlink "${path}"; then
            echo "refusing unsafe network service path: ${path}" >&2
            exit 66
        fi
    done
}

default_interface() {
    /sbin/route -n get "${ROUTE_PROBE}" 2>/dev/null |
        /usr/bin/awk '/interface:/{print $2; exit}'
}

clear_anchor() {
    /sbin/pfctl -a "${PF_ANCHOR}" -F all >/dev/null 2>&1 || true
}

ensure_pf_enabled() {
    if /sbin/pfctl -s info 2>/dev/null | /usr/bin/grep -q '^Status: Enabled'; then
        return
    fi
    token="$(
        /sbin/pfctl -E 2>&1 |
            /usr/bin/awk '/Token :/{print $3; exit}'
    )"
    if [ -z "${token}" ]; then
        echo "failed to acquire PF enable token" >&2
        exit 70
    fi
    /usr/bin/printf "%s\n" "${token}" >"${PF_TOKEN_FILE}"
    /usr/sbin/chown root:wheel "${PF_TOKEN_FILE}"
    /bin/chmod 0600 "${PF_TOKEN_FILE}"
}

release_pf_token() {
    if [ -f "${PF_TOKEN_FILE}" ]; then
        token="$(/bin/cat "${PF_TOKEN_FILE}")"
        /sbin/pfctl -X "${token}" >/dev/null 2>&1 || true
        /bin/rm -f "${PF_TOKEN_FILE}"
    fi
}

reconcile() {
    require_root
    validate_system_paths
    if [ ! -S "${SOCKET_VMNET}" ]; then
        clear_anchor
        /bin/rm -f "${STATE_FILE}"
        return
    fi

    interface="$(default_interface || true)"
    case "${interface}" in
        utun[0-9]*)
            rules="nat on ${interface} inet from ${VM_SUBNET} to any -> (${interface})
pass quick on ${interface} inet from ${VM_SUBNET} to any keep state
"
            current=""
            if [ -f "${STATE_FILE}" ]; then
                current="$(/bin/cat "${STATE_FILE}")"
            fi
            if [ "${current}" != "${interface}" ]; then
                clear_anchor
                /sbin/pfctl -k "${VM_SUBNET}" >/dev/null 2>&1 || true
            fi
            /usr/bin/printf "%s" "${rules}" |
                /sbin/pfctl -a "${PF_ANCHOR}" -f - >/dev/null
            /usr/bin/printf "%s\n" "${interface}" >"${STATE_FILE}"
            /usr/sbin/chown root:wheel "${STATE_FILE}"
            /bin/chmod 0600 "${STATE_FILE}"
            ;;
        *)
            clear_anchor
            /bin/rm -f "${STATE_FILE}"
            ;;
    esac
}

daemon() {
    require_root
    trap 'clear_anchor; exit 0' TERM INT HUP
    while :; do
        reconcile
        /bin/sleep 5
    done
}

write_pf_config() {
    temporary="$(/usr/bin/mktemp /tmp/hd-pf.XXXXXX)"
    if ! /usr/bin/awk -v nat_line="${PF_NAT_LINE}" -v filter_line="${PF_FILTER_LINE}" '
        $0 == nat_line || $0 == filter_line { next }
        {
            print
            if (!nat_inserted && $0 ~ /^nat-anchor[[:space:]]/) {
                print nat_line
                nat_inserted = 1
            }
            if (!filter_inserted && $0 ~ /^anchor[[:space:]]/) {
                print filter_line
                filter_inserted = 1
            }
        }
        END {
            if (!nat_inserted || !filter_inserted) {
                exit 65
            }
        }
    ' "${PF_CONF}" >"${temporary}"; then
        /bin/rm -f "${temporary}"
        return 65
    fi
    if ! /sbin/pfctl -nf "${temporary}" >/dev/null ||
        ! /usr/sbin/chown root:wheel "${temporary}" ||
        ! /bin/chmod 0644 "${temporary}" ||
        ! /bin/mv "${temporary}" "${PF_CONF}"; then
        /bin/rm -f "${temporary}"
        return 70
    fi
}

write_launch_daemon() {
    temporary="$(/usr/bin/mktemp /tmp/hd-network-plist.XXXXXX)"
    if ! /bin/cat >"${temporary}" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>${LABEL}</string>
  <key>ProgramArguments</key>
  <array><string>${HELPER}</string><string>daemon</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ProcessType</key><string>Background</string>
  <key>StandardOutPath</key><string>/var/log/${LABEL}.log</string>
  <key>StandardErrorPath</key><string>/var/log/${LABEL}.log</string>
</dict>
</plist>
EOF
    then
        /bin/rm -f "${temporary}"
        return 70
    fi
    if ! /usr/bin/plutil -lint "${temporary}" >/dev/null ||
        ! /usr/sbin/chown root:wheel "${temporary}" ||
        ! /bin/chmod 0644 "${temporary}" ||
        ! /bin/mv "${temporary}" "${PLIST}"; then
        /bin/rm -f "${temporary}"
        return 70
    fi
}

install_helper() {
    require_root
    source_path="$(cd "$(dirname "$0")" && pwd -P)/$(basename "$0")"
    if ! is_regular_nosymlink "${source_path}"; then
        echo "network setup source must be a regular non-symlink file" >&2
        exit 66
    fi
    validate_system_paths
    install_stage="$(/usr/bin/mktemp -d /tmp/hd-network-install.XXXXXX)"
    /bin/chmod 0700 "${install_stage}"
    /bin/cp -p "${PF_CONF}" "${install_stage}/pf.conf"
    helper_existed=false
    plist_existed=false
    token_existed=false
    if is_regular_nosymlink "${HELPER}"; then
        /bin/cp -p "${HELPER}" "${install_stage}/helper"
        helper_existed=true
    fi
    if is_regular_nosymlink "${PLIST}"; then
        /bin/cp -p "${PLIST}" "${install_stage}/launchd.plist"
        plist_existed=true
    fi
    if is_regular_nosymlink "${PF_TOKEN_FILE}"; then
        token_existed=true
    fi
    rollback_pending=true
    rollback_install() {
        if [ "${rollback_pending}" = true ]; then
            echo "network service installation failed; restoring previous state" >&2
            /bin/launchctl bootout "system/${LABEL}" >/dev/null 2>&1 || true
            /usr/bin/install -o root -g wheel -m 0644 \
                "${install_stage}/pf.conf" "${PF_CONF}" >/dev/null 2>&1 || true
            if [ "${helper_existed}" = true ]; then
                /usr/bin/install -o root -g wheel -m 0755 \
                    "${install_stage}/helper" "${HELPER}" >/dev/null 2>&1 || true
            else
                /bin/rm -f "${HELPER}"
            fi
            if [ "${plist_existed}" = true ]; then
                /usr/bin/install -o root -g wheel -m 0644 \
                    "${install_stage}/launchd.plist" "${PLIST}" >/dev/null 2>&1 || true
            else
                /bin/rm -f "${PLIST}"
            fi
            /sbin/pfctl -f "${PF_CONF}" >/dev/null 2>&1 || true
            if [ "${token_existed}" = false ]; then
                release_pf_token
            fi
            if [ "${plist_existed}" = true ]; then
                /bin/launchctl bootstrap system "${PLIST}" >/dev/null 2>&1 || true
                /bin/launchctl kickstart -k "system/${LABEL}" >/dev/null 2>&1 || true
            fi
        fi
        /bin/rm -rf -- "${install_stage}"
    }
    trap rollback_install EXIT HUP INT TERM
    if [ ! -f "${PF_BACKUP}" ]; then
        /bin/cp -p "${PF_CONF}" "${PF_BACKUP}"
        /usr/sbin/chown root:wheel "${PF_BACKUP}"
        /bin/chmod 0600 "${PF_BACKUP}"
    fi
    /usr/bin/install -o root -g wheel -m 0755 "${source_path}" "${HELPER}"
    write_pf_config
    write_launch_daemon
    /sbin/pfctl -f "${PF_CONF}" >/dev/null
    ensure_pf_enabled
    /bin/launchctl bootout "system/${LABEL}" >/dev/null 2>&1 || true
    /bin/launchctl bootstrap system "${PLIST}"
    /bin/launchctl kickstart -k "system/${LABEL}"
    rollback_pending=false
    rollback_install
    trap - EXIT HUP INT TERM
    echo "installed ${LABEL} contract=${CONTRACT_VERSION}"
}

remove_pf_config() {
    temporary="$(/usr/bin/mktemp /tmp/hd-pf.XXXXXX)"
    trap '/bin/rm -f "${temporary}"' EXIT
    /usr/bin/awk -v nat_line="${PF_NAT_LINE}" -v filter_line="${PF_FILTER_LINE}" '
        $0 != nat_line && $0 != filter_line { print }
    ' "${PF_CONF}" >"${temporary}"
    /sbin/pfctl -nf "${temporary}" >/dev/null
    /usr/sbin/chown root:wheel "${temporary}"
    /bin/chmod 0644 "${temporary}"
    /bin/mv "${temporary}" "${PF_CONF}"
    trap - EXIT
}

uninstall_helper() {
    require_root
    validate_system_paths
    /bin/launchctl bootout "system/${LABEL}" >/dev/null 2>&1 || true
    clear_anchor
    remove_pf_config
    /sbin/pfctl -f "${PF_CONF}" >/dev/null
    release_pf_token
    /bin/rm -f "${PLIST}" "${HELPER}" "${STATE_FILE}"
    echo "uninstalled ${LABEL}; backup retained at ${PF_BACKUP}"
}

status() {
    source_path="$(cd "$(dirname "$0")" && pwd -P)/$(basename "$0")"
    interface="$(default_interface || true)"
    loaded="false"
    if /bin/launchctl print "system/${LABEL}" >/dev/null 2>&1; then
        loaded="true"
    fi
    socket_vmnet=false
    [ ! -S "${SOCKET_VMNET}" ] || socket_vmnet=true
    installed=false
    plist_installed=false
    pf_configured=false
    state_present=false
    unsafe=false
    source_sha256=unavailable
    installed_sha256=unavailable
    package_match=false
    is_regular_nosymlink "${source_path}" || unsafe=true
    if is_regular_nosymlink "${source_path}"; then
        source_sha256="$(sha256_file "${source_path}")"
    fi
    if [ -e "${HELPER}" ] && ! is_regular_nosymlink "${HELPER}"; then
        unsafe=true
    elif is_regular_nosymlink "${HELPER}"; then
        installed=true
        installed_sha256="$(sha256_file "${HELPER}")"
    fi
    if [ -e "${PLIST}" ] && ! is_regular_nosymlink "${PLIST}"; then
        unsafe=true
    elif is_regular_nosymlink "${PLIST}"; then
        plist_installed=true
    fi
    for runtime_path in "${PF_BACKUP}" "${STATE_FILE}" "${PF_TOKEN_FILE}"; do
        if [ -e "${runtime_path}" ] && ! is_regular_nosymlink "${runtime_path}"; then
            unsafe=true
        fi
    done
    if is_regular_nosymlink "${PF_CONF}" &&
        [ "$(/usr/bin/grep -Fxc "${PF_NAT_LINE}" "${PF_CONF}" || true)" = 1 ] &&
        [ "$(/usr/bin/grep -Fxc "${PF_FILTER_LINE}" "${PF_CONF}" || true)" = 1 ]; then
        pf_configured=true
    elif [ -e "${PF_CONF}" ] && ! is_regular_nosymlink "${PF_CONF}"; then
        unsafe=true
    fi
    if is_regular_nosymlink "${STATE_FILE}"; then
        state_present=true
    fi
    if [ "${source_sha256}" != unavailable ] &&
        [ "${source_sha256}" = "${installed_sha256}" ]; then
        package_match=true
    fi
    vpn_nat_required=false
    case "${interface}" in utun[0-9]*) vpn_nat_required=true ;; esac
    service_action=none
    if [ "${unsafe}" = true ]; then
        service_action=manual_repair
    elif [ "${installed}" = false ]; then
        service_action=install
    elif [ "${package_match}" = false ]; then
        service_action=upgrade
    elif [ "${plist_installed}" = false ] || [ "${pf_configured}" = false ] ||
        [ "${loaded}" = false ]; then
        service_action=repair
    fi
    network_usable="${socket_vmnet}"
    nat=inactive
    if [ "${vpn_nat_required}" = true ]; then
        nat=inactive
        if [ "${service_action}" = none ] && [ "${state_present}" = true ]; then
            nat=active
        else
            network_usable=false
        fi
    fi
    health=ready
    if [ "${socket_vmnet}" = false ]; then
        health=offline
    elif [ "${network_usable}" = false ]; then
        health=degraded
    elif [ "${service_action}" != none ]; then
        health=maintenance
    fi
    /usr/bin/printf \
        "schema_version=%s\nlabel=%s\nhealth=%s\nservice_action=%s\nnetwork_usable=%s\ninstalled=%s\npackage_match=%s\nplist_installed=%s\nloaded=%s\npf_configured=%s\negress=%s\nvpn_nat_required=%s\nsocket_vmnet=%s\nnat=%s\nsource_sha256=%s\ninstalled_sha256=%s\nunsafe=%s\n" \
        "${CONTRACT_VERSION}" "${LABEL}" "${health}" "${service_action}" \
        "${network_usable}" "${installed}" "${package_match}" \
        "${plist_installed}" "${loaded}" "${pf_configured}" \
        "${interface:-none}" "${vpn_nat_required}" "${socket_vmnet}" \
        "${nat}" "${source_sha256}" "${installed_sha256}" "${unsafe}"
    if [ "${unsafe}" = true ]; then
        /usr/bin/printf "detail=检测到不安全的系统路径，需要管理员手动修复\n"
    elif [ "${network_usable}" = false ] && [ "${vpn_nat_required}" = true ]; then
        /usr/bin/printf "detail=当前 VPN 出口需要安装或修复 HD 网络兼容服务\n"
    elif [ "${socket_vmnet}" = false ]; then
        /usr/bin/printf "detail=socket_vmnet 未运行，Android 将使用离线网络配置\n"
    elif [ "${service_action}" = none ]; then
        /usr/bin/printf "detail=共享 NAT 上行链路与当前 HD 网络服务已就绪\n"
    else
        case "${service_action}" in
            install) action_label="安装" ;;
            upgrade) action_label="升级" ;;
            repair) action_label="修复" ;;
            *) action_label="管理员处理" ;;
        esac
        /usr/bin/printf "detail=当前网络可用，但 HD 网络兼容服务需要%s\n" "${action_label}"
    fi
}

case "${1:-}" in
    install) install_helper ;;
    uninstall) uninstall_helper ;;
    reconcile) reconcile ;;
    daemon) daemon ;;
    status) status ;;
    *)
        echo "usage: $0 install|uninstall|reconcile|daemon|status" >&2
        exit 64
        ;;
esac
