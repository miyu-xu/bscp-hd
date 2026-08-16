#!/bin/sh
set -eu
umask 077

usage() {
  cat >&2 <<'EOF'
Usage: scripts/macos-microdroid-fault-injection-smoke.sh \
  --app <HD.app> \
  --output <fresh-absolute-evidence-directory> \
  [--development-package]
EOF
}

fail() {
  echo "$*" >&2
  exit 1
}

APP=
OUTPUT=
DEVELOPMENT_PACKAGE=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    --app) [ "$#" -ge 2 ] || { usage; exit 2; }; APP=$2; shift 2 ;;
    --output) [ "$#" -ge 2 ] || { usage; exit 2; }; OUTPUT=$2; shift 2 ;;
    --development-package) DEVELOPMENT_PACKAGE=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage; exit 2 ;;
  esac
done

case "$APP:$OUTPUT" in /*:/*) ;; *) fail "--app and --output must be absolute" ;; esac
[ -d "$APP" ] && [ ! -L "$APP" ] || fail "--app must be a real non-symlink directory"
[ ! -e "$OUTPUT" ] || fail "refusing to replace evidence output: $OUTPUT"
FAILURE_OUTPUT="$OUTPUT.failed"
[ ! -e "$FAILURE_OUTPUT" ] || fail "refusing to replace failure evidence: $FAILURE_OUTPUT"
[ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ] ||
  fail "Microdroid fault injection requires Apple Silicon macOS"

CTL="$APP/Contents/MacOS/hdctl"
HOST="$APP/Contents/MacOS/hd-host"
for binary in "$CTL" "$HOST"; do
  [ -f "$binary" ] && [ ! -L "$binary" ] && [ -x "$binary" ] ||
    fail "HD.app is missing a real executable: $binary"
done
codesign --verify --deep --strict "$APP" || fail "HD.app codesign verification failed"
if [ "$DEVELOPMENT_PACKAGE" -eq 0 ]; then
  fail "current Microdroid fault injection requires --development-package"
fi

OUTPUT_PARENT=$(dirname -- "$OUTPUT")
mkdir -p -- "$OUTPUT_PARENT"
STAGE=$(mktemp -d "$OUTPUT_PARENT/.hd-microdroid-fault.XXXXXX")
DATA=$(mktemp -d /private/tmp/hd-microdroid-fault-data.XXXXXX)
INSTANCE_ID=$(uuidgen | tr '[:upper:]' '[:lower:]')
COMPLETED=0
HOST_PID=
WORKER_PID=
VM_PID=
CROSVM_PID=

matching_pids() {
  ps ax -o pid=,command= | grep -F -- "$DATA" | grep -v grep | awk '{print $1}'
}

terminate_test_processes() {
  pids=$(matching_pids || true)
  [ -z "$pids" ] || kill $pids 2>/dev/null || true
  sleep 1
  pids=$(matching_pids || true)
  [ -z "$pids" ] || kill -KILL $pids 2>/dev/null || true
}

capture_logs() {
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

cleanup() {
  status=$?
  HD_MICRODROID_DEV_BYPASS=1 "$CTL" --data-root "$DATA" --no-start-host \
    shutdown --stop-all >/dev/null 2>&1 || true
  capture_logs || true
  terminate_test_processes
  case "$DATA" in /private/tmp/hd-microdroid-fault-data.*) rm -rf -- "$DATA" ;; esac
  case "$STAGE" in
    "$OUTPUT_PARENT"/.hd-microdroid-fault.*)
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

process_alive() {
  [ -n "$1" ] && kill -0 "$1" 2>/dev/null
}

process_running() {
  process_alive "$1" || return 1
  state=$(ps -p "$1" -o state= 2>/dev/null | tr -d '[:space:]')
  [ -n "$state" ] && [ "${state#Z}" = "$state" ]
}

capture_process_snapshot() {
  output=$1
  ps ax -o pid=,ppid=,pgid=,state=,command= | grep -F -- "$DATA" | grep -v grep \
    >"$output" || true
}

capture_process_group() {
  pid=$1
  output=$2
  : >"$output"
  pgid=$(ps -p "$pid" -o pgid= 2>/dev/null | tr -d '[:space:]')
  [ -n "$pgid" ] || return 0
  ps ax -o pid=,ppid=,pgid=,state=,command= | \
    awk -v expected="$pgid" '$3 == expected' >"$output" || true
}

current_guest_vm() {
  pid=$(grep -h -F '"event":"process.spawn.succeeded"' \
    "$DATA"/logs/workers/"$INSTANCE_ID".jsonl* 2>/dev/null | tail -1 | \
    jq -r '.fields.pid // empty')
  [ -n "$pid" ] || return 1
  command=$(ps -p "$pid" -o command= 2>/dev/null || true)
  printf '%s' "$command" | grep -Eq '/(vm|crosvm)( |$)' || return 1
  printf '%s\n' "$pid"
}

current_guest_crosvm() {
  ps ax -o pid=,command= | grep -F -- "$DATA" | grep -F '/crosvm ' | \
    grep -F ' run ' | grep -v grep | tail -1 | awk '{print $1}'
}

wait_recovered() {
  label=$1
  previous_run=$2
  previous_worker=$3
  worker_mode=$4
  output=$5
  attempt=0
  while [ "$attempt" -lt 1800 ]; do
    if run_ctl show "$INSTANCE_ID" >"$output.tmp" 2>"$output.stderr"; then
      observed=$(jq -r '.status.observed // empty' "$output.tmp")
      desired=$(jq -r '.status.desired // empty' "$output.tmp")
      worker=$(jq -r '.worker.pid // empty' "$output.tmp")
      run=$(jq -r '.active_run_id // empty' "$output.tmp")
      [ "$observed" != blocked ] || fail "$label recovery entered Blocked"
      worker_ok=false
      case "$worker_mode" in
        same) [ "$worker" = "$previous_worker" ] && worker_ok=true ;;
        new) [ -n "$worker" ] && [ "$worker" != "$previous_worker" ] && worker_ok=true ;;
        *) fail "invalid worker recovery mode: $worker_mode" ;;
      esac
      if [ "$desired" = running ] && [ "$observed" = ready ] &&
          [ "$run" != "$previous_run" ] && [ "$worker_ok" = true ]; then
        mv -- "$output.tmp" "$output"
        return 0
      fi
    fi
    attempt=$((attempt + 1))
    sleep 0.1
  done
  fail "$label did not recover within 180 seconds"
}

mkdir -p "$STAGE/scenarios"
cat >"$STAGE/spec.json" <<EOF
{
  "schema_version": 2,
  "id": "$INSTANCE_ID",
  "name": "Microdroid Fault Injection",
  "guest_kind": "microdroid",
  "microdroid": {
    "debug_level": "full",
    "payload": { "kind": "empty" },
    "encrypted_storage_mib": 64
  },
  "cpu_count": 1,
  "memory_mib": 512,
  "display": {
    "width": 1080, "height": 1920, "dpi": 420, "refresh_rate_hz": 60,
    "orientation": "portrait", "vsync": "on", "show_host_fps": false
  },
  "adb": { "mode": "disabled", "host_port": null, "executable": null },
  "artifacts": null,
  "boot": { "kernel_log_level": 4, "panic_timeout_seconds": 5, "boot_animation": true },
  "devices": {
    "bluetooth": false, "nfc": false, "uwb": false, "modem": false,
    "gnss": false, "sensors": false, "network": false, "audio": false,
    "camera": false, "power": false
  },
  "restart_policy": "on_failure",
  "labels": { "purpose": "microdroid-fault-injection" }
}
EOF

run_ctl health >"$STAGE/initial-health.json"
run_ctl create --spec "$STAGE/spec.json" >"$STAGE/create.json"
run_ctl start "$INSTANCE_ID" >"$STAGE/start.json"
run_ctl show "$INSTANCE_ID" >"$STAGE/initial-ready.json"
jq -e '.status.desired == "running" and .status.observed == "ready" and
  (.worker.pid | type == "number") and (.active_run_id | type == "string")' \
  "$STAGE/initial-ready.json" >/dev/null || fail "initial Microdroid did not reach Ready"

HOST_PID=$(jq -r .pid "$STAGE/initial-health.json")
WORKER_PID=$(jq -r .worker.pid "$STAGE/initial-ready.json")
VM_PID=$(current_guest_vm)
CROSVM_PID=$(current_guest_crosvm)
INITIAL_RUN=$(jq -r .active_run_id "$STAGE/initial-ready.json")
process_alive "$HOST_PID" || fail "initial Host is not alive"
process_alive "$WORKER_PID" || fail "initial Worker is not alive"
process_alive "$VM_PID" || fail "initial Microdroid VM process is not alive"
process_alive "$CROSVM_PID" || fail "initial Microdroid crosvm process is not alive"

VM_STARTED=$(date +%s)
# Kill the actual VMM rather than its `vm` client. This makes AVF publish a real Killed death reason
# through the surviving client, which the Worker must preserve as a typed product error before
# restart policy creates the replacement run.
kill -KILL "$CROSVM_PID"
wait_recovered guest-vm "$INITIAL_RUN" "$WORKER_PID" same \
  "$STAGE/scenarios/vm-recovered.json"
VM_FINISHED=$(date +%s)
process_alive "$CROSVM_PID" && fail "injected Microdroid crosvm PID survived SIGKILL"
INITIAL_RESULT="$DATA/runs/$INSTANCE_ID/$INITIAL_RUN/result.json"
[ -f "$INITIAL_RESULT" ] || fail "crashed Microdroid run did not publish result.json"
cp "$INITIAL_RESULT" "$STAGE/scenarios/vm-crash-result.json"
jq -e '.final_state == "failed" and .error_code == "microdroid_runtime_failed"' \
  "$INITIAL_RESULT" >/dev/null ||
  fail "Microdroid VMM Killed reason was not preserved as microdroid_runtime_failed"
RECOVERED_WORKER=$(jq -r .worker.pid "$STAGE/scenarios/vm-recovered.json")
RECOVERED_RUN=$(jq -r .active_run_id "$STAGE/scenarios/vm-recovered.json")
RECOVERED_VM=$(current_guest_vm)
process_alive "$RECOVERED_VM" || fail "VM recovery did not publish a live replacement"

# A transient virtmgr death after Ready is not the Guest's startup-time RPC connection failure.
# libvmclient must publish VirtualizationServiceDied, HD must classify the finished run as a
# runtime infrastructure failure, and restart policy must recover without leaving the old VMM.
RECOVERED_CROSVM=$(current_guest_crosvm)
process_alive "$RECOVERED_CROSVM" || fail "recovered Microdroid crosvm is not alive"
RECOVERED_RUN_DIR="$DATA/runs/$INSTANCE_ID/$RECOVERED_RUN"
SERVICE_PID=$(grep -F 'transient virtmgr spawned pid=' \
  "$RECOVERED_RUN_DIR/microdroid-vmclient-trace.log" | tail -1 | sed 's/.*pid=//')
case "$SERVICE_PID" in ''|*[!0-9]*) fail "could not resolve transient virtmgr PID" ;; esac
SERVICE_COMMAND=$(ps -p "$SERVICE_PID" -o command= 2>/dev/null || true)
printf '%s\n' "$SERVICE_COMMAND" >"$STAGE/scenarios/service-process.txt"
printf '%s' "$SERVICE_COMMAND" | grep -F "$APP/Contents/MacOS/virtmgr" >/dev/null ||
  fail "resolved service PID was not this package's transient virtmgr"
SERVICE_STARTED=$(date +%s)
kill -KILL "$SERVICE_PID"
wait_recovered virtualization-service "$RECOVERED_RUN" "$RECOVERED_WORKER" same \
  "$STAGE/scenarios/service-recovered.json"
SERVICE_FINISHED=$(date +%s)
process_alive "$SERVICE_PID" && fail "injected transient virtmgr PID survived SIGKILL"
SERVICE_RESULT="$RECOVERED_RUN_DIR/result.json"
[ -f "$SERVICE_RESULT" ] || fail "service-death run did not publish result.json"
cp "$SERVICE_RESULT" "$STAGE/scenarios/service-death-result.json"
jq -e '.final_state == "failed" and .error_code == "microdroid_runtime_failed"' \
  "$SERVICE_RESULT" >/dev/null ||
  fail "VirtualizationServiceDied was not preserved as microdroid_runtime_failed"
grep -Fq 'VirtualizationServiceDied' "$RECOVERED_RUN_DIR/microdroid-vmclient-trace.log" ||
  fail "vmclient trace lost VirtualizationServiceDied"
attempt=0
while process_running "$RECOVERED_CROSVM" && [ "$attempt" -lt 100 ]; do
  attempt=$((attempt + 1))
  sleep 0.1
done
process_running "$RECOVERED_CROSVM" &&
  fail "old crosvm survived its transient virtmgr death and recovery"
SERVICE_RECOVERED_RUN=$(jq -r .active_run_id "$STAGE/scenarios/service-recovered.json")
SERVICE_RECOVERED_VM=$(current_guest_vm)
process_alive "$SERVICE_RECOVERED_VM" ||
  fail "service-death recovery did not publish a live replacement VM"
jq -n --argjson injected_pid "$SERVICE_PID" --arg previous_run "$RECOVERED_RUN" \
  --arg recovered_run "$SERVICE_RECOVERED_RUN" \
  --argjson duration_seconds "$((SERVICE_FINISHED - SERVICE_STARTED))" \
  '{name:"virtualization-service-exit",status:"pass",injected_pid:$injected_pid,
    death_reason:"VirtualizationServiceDied",api_error_code:"microdroid_runtime_failed",
    previous_run_id:$previous_run,recovered_run_id:$recovered_run,
    old_crosvm_cleanup:"pass",duration_seconds:$duration_seconds}' \
  >"$STAGE/scenarios/service.json"
RECOVERED_RUN=$SERVICE_RECOVERED_RUN
RECOVERED_VM=$SERVICE_RECOVERED_VM
capture_process_snapshot "$STAGE/scenarios/pre-worker-exit-processes.txt"
capture_process_group "$RECOVERED_VM" \
  "$STAGE/scenarios/pre-worker-exit-process-group.txt"
jq -n --argjson injected_pid "$CROSVM_PID" --argjson vm_client_pid "$VM_PID" \
  --argjson worker_pid "$RECOVERED_WORKER" \
  --arg previous_run "$INITIAL_RUN" --arg recovered_run "$RECOVERED_RUN" \
  --argjson duration_seconds "$((VM_FINISHED - VM_STARTED))" \
  '{name:"guest-vm-exit",status:"pass",injected_pid:$injected_pid,
    injected_process:"crosvm",vm_client_pid:$vm_client_pid,
    typed_error_code:"microdroid_runtime_failed",
    worker_preserved:true,worker_pid:$worker_pid,previous_run_id:$previous_run,
    recovered_run_id:$recovered_run,duration_seconds:$duration_seconds}' \
  >"$STAGE/scenarios/vm.json"

WORKER_STARTED=$(date +%s)
kill -KILL "$RECOVERED_WORKER"
wait_recovered worker "$RECOVERED_RUN" "$RECOVERED_WORKER" new \
  "$STAGE/scenarios/worker-recovered.json"
WORKER_FINISHED=$(date +%s)
process_alive "$RECOVERED_WORKER" && fail "injected Worker PID survived SIGKILL"
attempt=0
while process_running "$RECOVERED_VM" && [ "$attempt" -lt 100 ]; do
  attempt=$((attempt + 1))
  sleep 0.05
done
capture_process_snapshot "$STAGE/scenarios/post-worker-recovery-processes.txt"
capture_process_group "$RECOVERED_VM" \
  "$STAGE/scenarios/post-worker-recovery-process-group.txt"
process_running "$RECOVERED_VM" && fail "Guest VM survived its owning Worker SIGKILL"
NEW_WORKER=$(jq -r .worker.pid "$STAGE/scenarios/worker-recovered.json")
NEW_RUN=$(jq -r .active_run_id "$STAGE/scenarios/worker-recovered.json")
NEW_VM=$(current_guest_vm)
process_alive "$NEW_WORKER" || fail "Worker recovery did not publish a live replacement"
process_alive "$NEW_VM" || fail "Worker recovery did not publish a live Guest VM"
jq -n --argjson injected_pid "$RECOVERED_WORKER" --argjson recovered_pid "$NEW_WORKER" \
  --arg previous_run "$RECOVERED_RUN" --arg recovered_run "$NEW_RUN" \
  --argjson duration_seconds "$((WORKER_FINISHED - WORKER_STARTED))" \
  '{name:"worker-exit",status:"pass",injected_pid:$injected_pid,
    recovered_worker_pid:$recovered_pid,previous_run_id:$previous_run,
    recovered_run_id:$recovered_run,duration_seconds:$duration_seconds}' \
  >"$STAGE/scenarios/worker.json"

run_ctl health >"$STAGE/pre-host-exit-health.json"
HOST_PID=$(jq -r .pid "$STAGE/pre-host-exit-health.json")
HOST_STARTED=$(date +%s)
kill -KILL "$HOST_PID"
attempt=0
while process_alive "$HOST_PID" && [ "$attempt" -lt 100 ]; do
  attempt=$((attempt + 1))
  sleep 0.05
done
process_alive "$HOST_PID" && fail "injected Host PID survived SIGKILL"
process_alive "$NEW_WORKER" || fail "Worker exited with the injected Host"
process_alive "$NEW_VM" || fail "Guest VM exited with the injected Host"
run_ctl health >"$STAGE/host-recovered-health.json"
RECOVERED_HOST=$(jq -r .pid "$STAGE/host-recovered-health.json")
[ "$RECOVERED_HOST" != "$HOST_PID" ] || fail "Host recovery reused the injected PID"
process_alive "$RECOVERED_HOST" || fail "replacement Host is not alive"
attempt=0
while [ "$attempt" -lt 1200 ]; do
  if run_ctl show "$INSTANCE_ID" >"$STAGE/scenarios/host-recovered.json.tmp" 2>/dev/null; then
    observed=$(jq -r '.status.observed // empty' "$STAGE/scenarios/host-recovered.json.tmp")
    worker=$(jq -r '.worker.pid // empty' "$STAGE/scenarios/host-recovered.json.tmp")
    run=$(jq -r '.active_run_id // empty' "$STAGE/scenarios/host-recovered.json.tmp")
    if [ "$observed" = ready ] && [ "$worker" = "$NEW_WORKER" ] && [ "$run" = "$NEW_RUN" ]; then
      mv -- "$STAGE/scenarios/host-recovered.json.tmp" \
        "$STAGE/scenarios/host-recovered.json"
      break
    fi
  fi
  attempt=$((attempt + 1))
  sleep 0.1
done
[ -f "$STAGE/scenarios/host-recovered.json" ] ||
  fail "replacement Host did not reconnect the surviving Worker and Guest"
HOST_FINISHED=$(date +%s)
jq -n --argjson injected_pid "$HOST_PID" --argjson recovered_pid "$RECOVERED_HOST" \
  --argjson worker_pid "$NEW_WORKER" --arg run_id "$NEW_RUN" \
  --argjson duration_seconds "$((HOST_FINISHED - HOST_STARTED))" \
  '{name:"host-exit",status:"pass",injected_pid:$injected_pid,
    recovered_host_pid:$recovered_pid,worker_preserved:true,worker_pid:$worker_pid,
    run_preserved:true,run_id:$run_id,duration_seconds:$duration_seconds}' \
  >"$STAGE/scenarios/host.json"

run_ctl stop "$INSTANCE_ID" --force >"$STAGE/stop.json"
run_ctl delete "$INSTANCE_ID" >"$STAGE/delete.json"
run_ctl shutdown --stop-all >"$STAGE/shutdown.json"
attempt=0
while [ "$attempt" -lt 100 ] && matching_pids | grep -q .; do
  attempt=$((attempt + 1))
  sleep 0.05
done
if matching_pids | grep -q .; then
  matching_pids >"$STAGE/final-process-leaks.txt"
  fail "fault injection left an isolated process running"
fi
capture_logs

VERSION=$(plutil -extract CFBundleShortVersionString raw -o - "$APP/Contents/Info.plist")
BUILD=$(plutil -extract CFBundleVersion raw -o - "$APP/Contents/Info.plist")
GENERATED_AT=$(date -u +%Y-%m-%dT%H:%M:%SZ)
jq -s --arg version "$VERSION" --arg build "$BUILD" --arg generated_at "$GENERATED_AT" \
  '{schema_version:1,profile:"macos-arm64-microdroid-fault-injection-v3",
    status:"pass",version:$version,build:$build,generated_at:$generated_at,
    scenarios:.,process_cleanup:"pass"}' \
  "$STAGE/scenarios/vm.json" "$STAGE/scenarios/service.json" \
  "$STAGE/scenarios/worker.json" \
  "$STAGE/scenarios/host.json" >"$STAGE/result.json"
jq -n --arg generated_at "$GENERATED_AT" --arg log_path "$OUTPUT/result.json" \
  '{schema_version:2,generated_at:$generated_at,
    source:"scripts/macos-microdroid-fault-injection-smoke.sh",
    gates:[{name:"macos-microdroid-fault-injection",
      command:"macos-microdroid-fault-injection-smoke.sh --app <HD.app> --output <fresh-dir>",
      status:"pass",duration_ms:null,log_path:$log_path,
      summary:"An isolated Ready Microdroid had its crosvm VMM killed and then its recovered transient virtmgr killed; HD persisted Killed and VirtualizationServiceDied as microdroid_runtime_failed, restart policy created new runs without an old VMM leak, Worker SIGKILL produced a replacement Worker, Host SIGKILL preserved the active Worker/run, and cleanup left no test process."}]}' \
  >"$STAGE/fault-injection-gate.json"
chmod 600 "$STAGE/result.json" "$STAGE/fault-injection-gate.json"

case "$DATA" in /private/tmp/hd-microdroid-fault-data.*) rm -rf -- "$DATA" ;; esac
mv -- "$STAGE" "$OUTPUT"
COMPLETED=1
trap - EXIT HUP INT TERM
echo "evidence=$OUTPUT"
echo "gate_report=$OUTPUT/fault-injection-gate.json"
