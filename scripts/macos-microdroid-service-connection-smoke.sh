#!/bin/sh
set -eu
umask 077

usage() {
  cat >&2 <<'EOF'
Usage: scripts/macos-microdroid-service-connection-smoke.sh \
  --app <HD.app> \
  --output <fresh-absolute-evidence-directory> \
  --development-package
EOF
}

fail() {
  echo "$*" >&2
  exit 1
}

require_contains() {
  expected=$1
  file=$2
  message=$3
  grep -Fq "$expected" "$file" || fail "$message (evidence: $file)"
}

APP=
OUTPUT=
DEVELOPMENT_PACKAGE=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    --app) APP=$2; shift 2 ;;
    --output) OUTPUT=$2; shift 2 ;;
    --development-package) DEVELOPMENT_PACKAGE=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage; exit 2 ;;
  esac
done

case "$APP:$OUTPUT" in /*:/*) ;; *) fail "--app and --output must be absolute" ;; esac
[ -d "$APP" ] && [ ! -L "$APP" ] || fail "--app must be a real directory"
[ "$DEVELOPMENT_PACKAGE" -eq 1 ] ||
  fail "current infrastructure fault injection requires --development-package"
[ ! -e "$OUTPUT" ] || fail "refusing to replace evidence output: $OUTPUT"
FAILURE_OUTPUT="$OUTPUT.failed"
[ ! -e "$FAILURE_OUTPUT" ] || fail "refusing to replace failure evidence: $FAILURE_OUTPUT"
[ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ] ||
  fail "Microdroid service-connection smoke requires Apple Silicon macOS"

CTL="$APP/Contents/MacOS/hdctl"
[ -x "$CTL" ] && [ ! -L "$CTL" ] || fail "HD.app does not contain a real hdctl"
codesign --verify --deep --strict "$APP" || fail "HD.app codesign verification failed"

OUTPUT_PARENT=$(dirname -- "$OUTPUT")
mkdir -p -- "$OUTPUT_PARENT"
STAGE=$(mktemp -d "$OUTPUT_PARENT/.hd-microdroid-service-connection.XXXXXX")
DATA=$(mktemp -d /private/tmp/hd-microdroid-service-connection-data.XXXXXX)
INSTANCE_ID=$(uuidgen | tr '[:upper:]' '[:lower:]')
COMPLETED=0
START_PID=

matching_pids() {
  ps ax -o pid=,command= | grep -F -- "$DATA" | grep -v grep | awk '{print $1}'
}

capture_runtime_evidence() {
  if [ -d "$DATA/logs" ]; then
    mkdir -p "$STAGE/logs"
    cp -Rp "$DATA/logs/." "$STAGE/logs/" 2>/dev/null || true
  fi
  run_root="$DATA/runs/$INSTANCE_ID"
  [ -d "$run_root" ] || return 0
  for run_dir in "$run_root"/*; do
    [ -d "$run_dir" ] || continue
    run_id=$(basename -- "$run_dir")
    destination="$STAGE/runs/$run_id"
    mkdir -p "$destination"
    for name in result.json microdroid.stdout.log microdroid.stderr.log \
      microdroid-console.txt microdroid-guest.log microdroid-virtmgr-trace.log \
      microdroid-vmclient-trace.log; do
      [ ! -f "$run_dir/$name" ] || cp -p "$run_dir/$name" "$destination/$name"
    done
  done
}

terminate_test_processes() {
  pids=$(matching_pids || true)
  [ -z "$pids" ] || kill $pids 2>/dev/null || true
  sleep 1
  pids=$(matching_pids || true)
  [ -z "$pids" ] || kill -KILL $pids 2>/dev/null || true
}

cleanup() {
  status=$?
  [ -z "$START_PID" ] || kill "$START_PID" 2>/dev/null || true
  HD_MICRODROID_DEV_BYPASS=1 "$CTL" --data-root "$DATA" --no-start-host \
    shutdown --stop-all >/dev/null 2>&1 || true
  capture_runtime_evidence || true
  terminate_test_processes
  case "$DATA" in /private/tmp/hd-microdroid-service-connection-data.*) rm -rf -- "$DATA" ;; esac
  case "$STAGE" in
    "$OUTPUT_PARENT"/.hd-microdroid-service-connection.*)
      if [ "$COMPLETED" -eq 0 ] && [ -d "$STAGE" ]; then
        printf '%s\n' "$status" >"$STAGE/exit.code"
        mv -- "$STAGE" "$FAILURE_OUTPUT"
        echo "failure_evidence=$FAILURE_OUTPUT" >&2
      else
        rm -rf -- "$STAGE"
      fi
      ;;
  esac
}
trap cleanup EXIT HUP INT TERM

run_ctl() {
  HD_MICRODROID_DEV_BYPASS=1 "$CTL" --data-root "$DATA" "$@"
}

jq -n --arg id "$INSTANCE_ID" \
  '{schema_version:2,id:$id,name:"Microdroid Service Connection Failure",
    guest_kind:"microdroid",
    microdroid:{debug_level:"full",payload:{kind:"empty"},encrypted_storage_mib:null},
    cpu_count:1,memory_mib:512,
    display:{width:1080,height:1920,dpi:420,refresh_rate_hz:60,
      orientation:"portrait",vsync:"on",show_host_fps:false},
    adb:{mode:"disabled",host_port:null,executable:null},artifacts:null,
    boot:{kernel_log_level:4,panic_timeout_seconds:5,boot_animation:true},
    devices:{bluetooth:false,nfc:false,uwb:false,modem:false,gnss:false,sensors:false,
      network:false,audio:false,camera:false,power:false},restart_policy:"never",
    labels:{purpose:"microdroid-service-connection"}}' >"$STAGE/spec.json"
run_ctl create --spec "$STAGE/spec.json" >"$STAGE/create.json"

# virtmgr creates one owner-only /tmp/binder_rpc_vsock_<cid>_<cid>.sock before crosvm starts.
# Remove only the socket newly created for this isolated data root, before the Guest manager boots,
# so the real Guest connection attempt and failure serial own the resulting AOSP death reason.
# `/tmp` is a symlink on macOS and BSD find does not descend through a symlink start path, so the
# discovery root must be the canonical `/private/tmp` while referring to the same socket inode.
find /private/tmp -maxdepth 1 -type s -name 'binder_rpc_vsock_*_*.sock' -print | LC_ALL=C sort \
  >"$STAGE/binder-sockets-before.txt"
set +e
run_ctl start "$INSTANCE_ID" >"$STAGE/start.stdout" 2>"$STAGE/start.stderr" &
START_PID=$!
set -e
INJECTED_SOCKET=
attempt=0
while [ "$attempt" -lt 500 ]; do
  find /private/tmp -maxdepth 1 -type s -name 'binder_rpc_vsock_*_*.sock' -print | LC_ALL=C sort \
    >"$STAGE/binder-sockets-current.txt"
  comm -13 "$STAGE/binder-sockets-before.txt" "$STAGE/binder-sockets-current.txt" \
    >"$STAGE/binder-sockets-new.txt"
  count=$(grep -c . "$STAGE/binder-sockets-new.txt" || true)
  if [ "$count" -eq 1 ]; then
    INJECTED_SOCKET=$(cat "$STAGE/binder-sockets-new.txt")
    break
  fi
  [ "$count" -eq 0 ] || fail "more than one new Binder-RPC socket appeared"
  attempt=$((attempt + 1))
  sleep 0.01
done
[ -n "$INJECTED_SOCKET" ] || fail "isolated Binder-RPC socket did not appear"
case "$INJECTED_SOCKET" in /private/tmp/binder_rpc_vsock_[0-9]*_[0-9]*.sock) ;;
  *) fail "unsafe Binder-RPC socket path: $INJECTED_SOCKET" ;;
esac
SOCKET_NAME=$(basename -- "$INJECTED_SOCKET")
CID_PAIR=${SOCKET_NAME#binder_rpc_vsock_}
CID_PAIR=${CID_PAIR%.sock}
CID_LEFT=${CID_PAIR%%_*}
CID_RIGHT=${CID_PAIR#*_}
[ "$CID_LEFT" = "$CID_RIGHT" ] || fail "Binder-RPC socket did not use cid=port"
case "$CID_LEFT" in ''|*[!0-9]*) fail "Binder-RPC socket CID was not numeric" ;; esac
[ -S "$INJECTED_SOCKET" ] && [ ! -L "$INJECTED_SOCKET" ] ||
  fail "injection target was not a real Unix socket"
stat -f 'owner=%Su mode=%Sp type=%HT inode=%i' "$INJECTED_SOCKET" \
  >"$STAGE/injected-socket-stat.txt"
[ "$(stat -f %Su "$INJECTED_SOCKET")" = "$(id -un)" ] ||
  fail "new Binder-RPC socket was not owned by the current user"
printf '%s\n' "$INJECTED_SOCKET" >"$STAGE/injected-socket.txt"
rm -f -- "$INJECTED_SOCKET"
[ ! -e "$INJECTED_SOCKET" ] || fail "Binder-RPC fault injection did not remove the socket path"

set +e
wait "$START_PID"
START_STATUS=$?
set -e
START_PID=
[ "$START_STATUS" -ne 0 ] || fail "Microdroid unexpectedly started without its RPC socket"
require_contains 'code: "microdroid_service_connection_failed"' "$STAGE/start.stderr" \
  "CLI did not return the typed service-connection code"
require_contains 'VirtualizationService' "$STAGE/start.stderr" \
  "CLI error did not provide an actionable service repair message"
run_ctl show "$INSTANCE_ID" >"$STAGE/blocked-instance.json"
jq -e '.status.observed == "blocked" and
  .status.error_code == "microdroid_service_connection_failed" and
  (.status.reason | contains("VirtualizationService")) and
  (.active_run_id | type == "string")' "$STAGE/blocked-instance.json" >/dev/null ||
  fail "instance did not preserve the typed service-connection blocked state"
RUN_ID=$(jq -r .active_run_id "$STAGE/blocked-instance.json")
RUN_DIR="$DATA/runs/$INSTANCE_ID/$RUN_ID"
[ -f "$RUN_DIR/result.json" ] || fail "failed run did not publish result.json"
jq -e '.final_state == "blocked" and
  .error_code == "microdroid_service_connection_failed"' "$RUN_DIR/result.json" >/dev/null ||
  fail "run result lost the service-connection reason"
require_contains 'VM ended: MicrodroidFailedToConnectToVirtualizationService' \
  "$RUN_DIR/microdroid.stdout.log" "vm client did not report the AOSP death reason"
require_contains 'reason=MicrodroidFailedToConnectToVirtualizationService' \
  "$RUN_DIR/microdroid-vmclient-trace.log" "vmclient trace lost the AOSP death reason"
require_contains 'MICRODROID_FAILED_TO_CONNECT_TO_VIRTUALIZATION_SERVICE' \
  "$RUN_DIR/microdroid-virtmgr-trace.log" "virtmgr trace lost the Guest service error"
require_contains "create_vm_internal vm_context cid=$CID_LEFT" \
  "$RUN_DIR/microdroid-virtmgr-trace.log" \
  "injected Binder-RPC socket CID did not belong to the isolated virtmgr VM"
capture_runtime_evidence

run_ctl stop "$INSTANCE_ID" --force >"$STAGE/stop.json"
run_ctl delete "$INSTANCE_ID" >"$STAGE/delete.json"
run_ctl shutdown --stop-all >"$STAGE/shutdown.json"
attempt=0
while [ "$attempt" -lt 100 ] && matching_pids | grep -q .; do
  attempt=$((attempt + 1))
  sleep 0.05
done
if matching_pids | grep -q .; then
  matching_pids >"$STAGE/process-leaks.txt"
  fail "service-connection smoke left an isolated process running"
fi

VERSION=$(plutil -extract CFBundleShortVersionString raw -o - "$APP/Contents/Info.plist")
BUILD=$(plutil -extract CFBundleVersion raw -o - "$APP/Contents/Info.plist")
GENERATED_AT=$(date -u +%Y-%m-%dT%H:%M:%SZ)
jq -n --arg generated_at "$GENERATED_AT" --arg version "$VERSION" --arg build "$BUILD" \
  --arg instance_id "$INSTANCE_ID" --arg run_id "$RUN_ID" --argjson guest_cid "$CID_LEFT" \
  '{schema_version:1,profile:"macos-arm64-microdroid-service-connection-v1",
    status:"pass",generated_at:$generated_at,version:$version,build:$build,
    instance_id:$instance_id,run_id:$run_id,fixture:{guest_cid:$guest_cid,
      rpc_socket_owner:"virtmgr",rpc_socket_removed_before_guest_connect:true},
    guest_death_reason:"MicrodroidFailedToConnectToVirtualizationService",
    api_error_code:"microdroid_service_connection_failed",observed_state:"blocked",
    process_cleanup:"pass"}' >"$STAGE/result.json"
jq -n --arg generated_at "$GENERATED_AT" --arg log_path "$OUTPUT/result.json" \
  '{schema_version:2,generated_at:$generated_at,
    source:"scripts/macos-microdroid-service-connection-smoke.sh",
    gates:[{name:"macos-microdroid-service-connection",
      command:"macos-microdroid-service-connection-smoke.sh --app <HD.app> --output <fresh-dir> --development-package",
      status:"pass",duration_ms:null,log_path:$log_path,
      summary:"The isolated virtmgr-owned Binder-RPC socket was removed before Guest boot; the real microdroid_manager failed its host service connection, emitted MicrodroidFailedToConnectToVirtualizationService, and HD remained Blocked with the actionable microdroid_service_connection_failed API code; cleanup left no test process."}]}' \
  >"$STAGE/service-connection-gate.json"
chmod 600 "$STAGE/result.json" "$STAGE/service-connection-gate.json"

case "$DATA" in /private/tmp/hd-microdroid-service-connection-data.*) rm -rf -- "$DATA" ;; esac
mv -- "$STAGE" "$OUTPUT"
COMPLETED=1
trap - EXIT HUP INT TERM
echo "evidence=$OUTPUT"
echo "gate_report=$OUTPUT/service-connection-gate.json"
