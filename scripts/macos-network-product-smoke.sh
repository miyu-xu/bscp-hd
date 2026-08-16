#!/bin/sh
set -eu

SCRIPT=""
OUTPUT=""

usage() {
    echo "usage: $0 --script PATH --output PATH" >&2
    exit 64
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --script)
            [ "$#" -ge 2 ] || usage
            SCRIPT="$2"
            shift 2
            ;;
        --output)
            [ "$#" -ge 2 ] || usage
            OUTPUT="$2"
            shift 2
            ;;
        *)
            usage
            ;;
    esac
done

[ -n "${SCRIPT}" ] && [ -n "${OUTPUT}" ] || usage
[ -f "${SCRIPT}" ] && [ ! -L "${SCRIPT}" ] || {
    echo "network setup script must be a regular non-symlink file" >&2
    exit 66
}

work_dir="$(/usr/bin/mktemp -d /tmp/hd-network-product-smoke.XXXXXX)"
output_tmp="${OUTPUT}.tmp.$$"
cleanup() {
    /bin/rm -rf -- "${work_dir}"
    /bin/rm -f -- "${output_tmp}"
}
trap cleanup EXIT HUP INT TERM

status_file="${work_dir}/status"
"${SCRIPT}" status >"${status_file}"

required_keys="schema_version label health service_action network_usable installed package_match plist_installed loaded pf_configured egress vpn_nat_required socket_vmnet nat source_sha256 installed_sha256 unsafe detail"
for key in ${required_keys}; do
    count="$(/usr/bin/awk -F= -v key="${key}" '$1 == key { count += 1 } END { print count + 0 }' "${status_file}")"
    [ "${count}" = 1 ] || {
        echo "status contract key ${key} appeared ${count} times" >&2
        exit 65
    }
done

actual_keys="$(/usr/bin/awk -F= 'NF >= 2 { print $1 }' "${status_file}" | /usr/bin/sort | /usr/bin/uniq | /usr/bin/wc -l | /usr/bin/tr -d ' ')"
[ "${actual_keys}" = 18 ] || {
    echo "status contract contains unexpected or malformed fields" >&2
    exit 65
}
if /usr/bin/awk -F= 'NF < 2 || $1 !~ /^[a-z][a-z0-9_]*$/ { invalid = 1 } END { exit invalid ? 0 : 1 }' "${status_file}"; then
    echo "status contract contains malformed lines" >&2
    exit 65
fi

value() {
    /usr/bin/awk -F= -v key="$1" '$1 == key { sub(/^[^=]*=/, ""); print; exit }' "${status_file}"
}

schema_version="$(value schema_version)"
label="$(value label)"
health="$(value health)"
service_action="$(value service_action)"
network_usable="$(value network_usable)"
installed="$(value installed)"
package_match="$(value package_match)"
plist_installed="$(value plist_installed)"
loaded="$(value loaded)"
pf_configured="$(value pf_configured)"
egress="$(value egress)"
vpn_nat_required="$(value vpn_nat_required)"
socket_vmnet="$(value socket_vmnet)"
nat="$(value nat)"
source_sha256="$(value source_sha256)"
installed_sha256="$(value installed_sha256)"
unsafe="$(value unsafe)"
detail="$(value detail)"

[ "${schema_version}" = 2 ] || {
    echo "unsupported status contract version: ${schema_version}" >&2
    exit 65
}
[ "${label}" = com.bscp.hd-network ] || {
    echo "unexpected service label: ${label}" >&2
    exit 65
}
case "${health}" in ready|maintenance|degraded|offline) ;; *) exit 65 ;; esac
case "${service_action}" in none|install|upgrade|repair|manual_repair) ;; *) exit 65 ;; esac
for boolean in "${network_usable}" "${installed}" "${package_match}" \
    "${plist_installed}" "${loaded}" "${pf_configured}" \
    "${vpn_nat_required}" "${socket_vmnet}" "${unsafe}"; do
    case "${boolean}" in true|false) ;; *) exit 65 ;; esac
done
case "${nat}" in active|inactive) ;; *) exit 65 ;; esac

expected_source_sha256="$(/usr/bin/shasum -a 256 "${SCRIPT}" | /usr/bin/awk '{print $1}')"
[ "${source_sha256}" = "${expected_source_sha256}" ] || {
    echo "status source digest does not match the inspected package" >&2
    exit 65
}
case "${source_sha256}" in
    [0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]*)
        [ "${#source_sha256}" = 64 ] || exit 65
        ;;
    *)
        exit 65
        ;;
esac
if [ "${installed}" = true ]; then
    [ "${installed_sha256}" != unavailable ] || exit 65
else
    [ "${installed_sha256}" = unavailable ] || exit 65
fi
if [ "${package_match}" = true ]; then
    [ "${installed}" = true ] &&
        [ "${source_sha256}" = "${installed_sha256}" ] || exit 65
fi
if [ "${unsafe}" = true ]; then
    [ "${service_action}" = manual_repair ] || exit 65
fi
if [ "${service_action}" = none ]; then
    [ "${installed}" = true ] &&
        [ "${package_match}" = true ] &&
        [ "${plist_installed}" = true ] &&
        [ "${loaded}" = true ] &&
        [ "${pf_configured}" = true ] || exit 65
fi
if [ "${socket_vmnet}" = false ]; then
    [ "${network_usable}" = false ] && [ "${health}" = offline ] || exit 65
elif [ "${vpn_nat_required}" = false ]; then
    [ "${network_usable}" = true ] || exit 65
elif [ "${nat}" = inactive ]; then
    [ "${network_usable}" = false ] || exit 65
fi

for token in \
    "validate_system_paths" \
    "rollback_install" \
    "trap rollback_install EXIT HUP INT TERM" \
    "network service installation failed; restoring previous state"; do
    /usr/bin/grep -Fq "${token}" "${SCRIPT}" || {
        echo "missing installation safety contract: ${token}" >&2
        exit 65
    }
done
/usr/bin/grep -Eq '^[[:space:]]*status\)[[:space:]]+status[[:space:]]*;;' "${SCRIPT}" || {
    echo "status must remain available without administrator privileges" >&2
    exit 65
}

fixture_root="${work_dir}/fixture"
mock_bin="${fixture_root}/mock-bin"
fixture_script="${fixture_root}/macos-network-setup.sh"
/bin/mkdir -p \
    "${fixture_root}/Library/PrivilegedHelperTools" \
    "${fixture_root}/Library/LaunchDaemons" \
    "${fixture_root}/etc" \
    "${fixture_root}/var/run" \
    "${mock_bin}"
/usr/bin/printf '%s\n' \
    'nat-anchor "com.apple/*"' \
    'anchor "com.apple/*"' >"${fixture_root}/etc/pf.conf"
/usr/bin/printf 'old-helper\n' \
    >"${fixture_root}/Library/PrivilegedHelperTools/com.bscp.hd-network"
/usr/bin/printf 'old-plist\n' \
    >"${fixture_root}/Library/LaunchDaemons/com.bscp.hd-network.plist"
/bin/cp -p "${fixture_root}/etc/pf.conf" "${fixture_root}/pf.conf.expected"
/bin/cp -p \
    "${fixture_root}/Library/PrivilegedHelperTools/com.bscp.hd-network" \
    "${fixture_root}/helper.expected"
/bin/cp -p \
    "${fixture_root}/Library/LaunchDaemons/com.bscp.hd-network.plist" \
    "${fixture_root}/plist.expected"

/usr/bin/awk -v root="${fixture_root}" '
    /^HELPER=/ {
        print "HELPER=\"" root "/Library/PrivilegedHelperTools/com.bscp.hd-network\""
        next
    }
    /^PLIST=/ {
        print "PLIST=\"" root "/Library/LaunchDaemons/com.bscp.hd-network.plist\""
        next
    }
    /^PF_CONF=/ {
        print "PF_CONF=\"" root "/etc/pf.conf\""
        next
    }
    /^PF_BACKUP=/ {
        print "PF_BACKUP=\"" root "/etc/pf.conf.bscp-hd.backup\""
        next
    }
    /^SOCKET_VMNET=/ {
        print "SOCKET_VMNET=\"" root "/var/run/socket_vmnet\""
        next
    }
    /^STATE_FILE=/ {
        print "STATE_FILE=\"" root "/var/run/com.bscp.hd-network.egress\""
        next
    }
    /^PF_TOKEN_FILE=/ {
        print "PF_TOKEN_FILE=\"" root "/var/run/com.bscp.hd-network.pf-token\""
        next
    }
    $0 == "    if [ \"$(id -u)\" -ne 0 ]; then" {
        print "    if false; then"
        next
    }
    { print }
' "${SCRIPT}" |
    /usr/bin/sed \
        -e "s|/sbin/pfctl|${mock_bin}/pfctl|g" \
        -e "s|/bin/launchctl|${mock_bin}/launchctl|g" \
        -e 's|/usr/sbin/chown root:wheel|/usr/bin/true|g' \
        -e 's|/usr/bin/install -o root -g wheel -m|/usr/bin/install -m|g' \
        >"${fixture_script}"
/bin/chmod 0755 "${fixture_script}"

/bin/cat >"${mock_bin}/pfctl" <<'EOF'
#!/bin/sh
if [ "$*" = "-s info" ]; then
    echo "Status: Enabled"
fi
exit 0
EOF
/bin/cat >"${mock_bin}/launchctl" <<EOF
#!/bin/sh
echo "\$*" >>"${fixture_root}/launchctl.log"
if [ "\${1:-}" = bootstrap ] && [ -f "${fixture_root}/fail-bootstrap-once" ]; then
    /bin/rm -f "${fixture_root}/fail-bootstrap-once"
    exit 70
fi
exit 0
EOF
/bin/chmod 0755 "${mock_bin}/pfctl" "${mock_bin}/launchctl"
touch "${fixture_root}/fail-bootstrap-once"

if "${fixture_script}" install >"${fixture_root}/install.stdout" \
    2>"${fixture_root}/install.stderr"; then
    echo "fault-injected install unexpectedly succeeded" >&2
    exit 65
fi
/usr/bin/grep -Fq \
    "network service installation failed; restoring previous state" \
    "${fixture_root}/install.stderr" || {
    echo "fault-injected install did not report rollback" >&2
    /bin/cat "${fixture_root}/install.stderr" >&2
    exit 65
}
/usr/bin/cmp -s "${fixture_root}/pf.conf.expected" "${fixture_root}/etc/pf.conf" &&
    /usr/bin/cmp -s "${fixture_root}/helper.expected" \
        "${fixture_root}/Library/PrivilegedHelperTools/com.bscp.hd-network" &&
    /usr/bin/cmp -s "${fixture_root}/plist.expected" \
        "${fixture_root}/Library/LaunchDaemons/com.bscp.hd-network.plist" || {
    echo "fault-injected install did not restore the previous state" >&2
    exit 65
}
bootstrap_count="$(/usr/bin/awk '$1 == "bootstrap" { count += 1 } END { print count + 0 }' \
    "${fixture_root}/launchctl.log")"
[ "${bootstrap_count}" = 2 ] || {
    echo "rollback did not restore the previous launchd service" >&2
    exit 65
}

escape_json() {
    /usr/bin/printf "%s" "$1" |
        /usr/bin/awk 'BEGIN { ORS = "" } { gsub(/\\/, "\\\\"); gsub(/"/, "\\\""); if (NR > 1) printf "\\n"; printf "%s", $0 }'
}

/bin/mkdir -p "$(/usr/bin/dirname "${OUTPUT}")"
{
    /usr/bin/printf '{\n'
    /usr/bin/printf '  "schema_version": 1,\n'
    /usr/bin/printf '  "verdict": "pass",\n'
    /usr/bin/printf '  "network_contract_version": %s,\n' "${schema_version}"
    /usr/bin/printf '  "health": "%s",\n' "$(escape_json "${health}")"
    /usr/bin/printf '  "service_action": "%s",\n' "$(escape_json "${service_action}")"
    /usr/bin/printf '  "network_usable": %s,\n' "${network_usable}"
    /usr/bin/printf '  "egress": "%s",\n' "$(escape_json "${egress}")"
    /usr/bin/printf '  "vpn_nat_required": %s,\n' "${vpn_nat_required}"
    /usr/bin/printf '  "socket_vmnet": %s,\n' "${socket_vmnet}"
    /usr/bin/printf '  "nat": "%s",\n' "${nat}"
    /usr/bin/printf '  "source_sha256": "%s",\n' "${source_sha256}"
    /usr/bin/printf '  "installed_sha256": "%s",\n' "${installed_sha256}"
    /usr/bin/printf '  "unsafe": %s,\n' "${unsafe}"
    /usr/bin/printf '  "detail": "%s",\n' "$(escape_json "${detail}")"
    /usr/bin/printf '  "checks": ["non_root_status", "strict_schema", "digest_binding", "state_consistency", "transactional_install_fault_injection"]\n'
    /usr/bin/printf '}\n'
} >"${output_tmp}"
/bin/mv -f "${output_tmp}" "${OUTPUT}"
trap - EXIT HUP INT TERM
cleanup

echo "macOS network product smoke passed: ${OUTPUT}"
