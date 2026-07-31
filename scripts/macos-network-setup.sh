#!/bin/sh
set -eu

LABEL="com.bscp.hd-network"
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
    trap '/bin/rm -f "${temporary}"' EXIT
    /usr/bin/awk -v nat_line="${PF_NAT_LINE}" -v filter_line="${PF_FILTER_LINE}" '
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
    ' "${PF_CONF}" >"${temporary}"
    /sbin/pfctl -nf "${temporary}" >/dev/null
    /usr/sbin/chown root:wheel "${temporary}"
    /bin/chmod 0644 "${temporary}"
    /bin/mv "${temporary}" "${PF_CONF}"
    trap - EXIT
}

write_launch_daemon() {
    temporary="$(/usr/bin/mktemp /tmp/hd-network-plist.XXXXXX)"
    trap '/bin/rm -f "${temporary}"' EXIT
    /bin/cat >"${temporary}" <<EOF
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
    /usr/bin/plutil -lint "${temporary}" >/dev/null
    /usr/sbin/chown root:wheel "${temporary}"
    /bin/chmod 0644 "${temporary}"
    /bin/mv "${temporary}" "${PLIST}"
    trap - EXIT
}

install_helper() {
    require_root
    source_path="$(cd "$(dirname "$0")" && pwd -P)/$(basename "$0")"
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
    echo "installed ${LABEL}"
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
    /bin/launchctl bootout "system/${LABEL}" >/dev/null 2>&1 || true
    clear_anchor
    remove_pf_config
    /sbin/pfctl -f "${PF_CONF}" >/dev/null
    release_pf_token
    /bin/rm -f "${PLIST}" "${HELPER}" "${STATE_FILE}"
    echo "uninstalled ${LABEL}; backup retained at ${PF_BACKUP}"
}

status() {
    require_root
    interface="$(default_interface || true)"
    loaded="false"
    if /bin/launchctl print "system/${LABEL}" >/dev/null 2>&1; then
        loaded="true"
    fi
    anchor_rules="$(/sbin/pfctl -a "${PF_ANCHOR}" -sn 2>/dev/null || true)"
    /usr/bin/printf "label=%s\nloaded=%s\negress=%s\nsocket_vmnet=%s\n" \
        "${LABEL}" "${loaded}" "${interface:-none}" "$([ -S "${SOCKET_VMNET}" ] && echo true || echo false)"
    if [ -n "${anchor_rules}" ]; then
        /usr/bin/printf "nat=active\n%s\n" "${anchor_rules}"
    else
        /usr/bin/printf "nat=inactive\n"
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
