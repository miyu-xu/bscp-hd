#!/bin/sh
set -eu

usage() {
  cat <<'EOF'
Usage: scripts/macos-android-readiness-soak-smoke.sh \
  --app <HD.app> \
  --output <fresh-absolute-evidence-directory> \
  [--cycles <3..100>] \
  [--ready-budget-seconds <1..180>] \
  [--development-package]

Starts one packaged Android instance repeatedly and requires adb_ready to imply
two stable interactive samples after boot and startup policy on every cycle.
Both the signed artifact-store and explicit direct-development package contracts
are accepted, but they must never be mixed.
EOF
}

fail() {
  echo "$*" >&2
  exit 1
}

APP=
OUTPUT=
CYCLES=20
READY_BUDGET_SECONDS=90
DEVELOPMENT_PACKAGE=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    --app) [ "$#" -ge 2 ] || { usage >&2; exit 2; }; APP=$2; shift 2 ;;
    --output) [ "$#" -ge 2 ] || { usage >&2; exit 2; }; OUTPUT=$2; shift 2 ;;
    --cycles) [ "$#" -ge 2 ] || { usage >&2; exit 2; }; CYCLES=$2; shift 2 ;;
    --ready-budget-seconds) [ "$#" -ge 2 ] || { usage >&2; exit 2; }; READY_BUDGET_SECONDS=$2; shift 2 ;;
    --development-package) DEVELOPMENT_PACKAGE=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

case "$APP" in /*) ;; *) fail "--app must be absolute" ;; esac
[ -d "$APP" ] && [ ! -L "$APP" ] || fail "--app must be a real non-symlink directory"
case "$OUTPUT" in /*) ;; *) fail "--output must be absolute" ;; esac
case "$CYCLES" in *[!0-9]*|'') fail "--cycles must be an integer" ;; esac
case "$READY_BUDGET_SECONDS" in *[!0-9]*|'') fail "--ready-budget-seconds must be an integer" ;; esac
[ "$CYCLES" -ge 3 ] && [ "$CYCLES" -le 100 ] || fail "--cycles must be between 3 and 100"
[ "$READY_BUDGET_SECONDS" -ge 1 ] && [ "$READY_BUDGET_SECONDS" -le 180 ] ||
  fail "--ready-budget-seconds must be between 1 and 180"
[ "$DEVELOPMENT_PACKAGE" -eq 1 ] || fail "current Android soak requires --development-package"
[ ! -e "$OUTPUT" ] || fail "refusing to replace evidence output: $OUTPUT"
FAILURE_OUTPUT="$OUTPUT.failed"
[ ! -e "$FAILURE_OUTPUT" ] || fail "refusing to replace failure evidence: $FAILURE_OUTPUT"
[ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ] ||
  fail "Android readiness soak requires Apple Silicon macOS"

UI="$APP/Contents/MacOS/HD"
CTL="$APP/Contents/MacOS/hdctl"
ADB="$APP/Contents/MacOS/adb"
SIGNED_STORE="$APP/Contents/Resources/products/android/artifact-store-v2"
DIRECT_MARKER="$APP/Contents/Resources/products/android/development-direct-v1.plist"
STORE=
TRUST=
ARTIFACT_DISTRIBUTION=
for binary in "$UI" "$CTL" "$ADB"; do
  [ -f "$binary" ] && [ ! -L "$binary" ] && [ -x "$binary" ] ||
    fail "HD.app is missing a real executable: $binary"
done
if [ -d "$SIGNED_STORE" ] && [ ! -L "$SIGNED_STORE" ]; then
  [ ! -e "$DIRECT_MARKER" ] ||
    fail "HD.app must not mix signed and direct-development Android distributions"
  STORE=$SIGNED_STORE
  TRUST="$STORE/trusted-keys-v2.json"
  [ -f "$TRUST" ] && [ ! -L "$TRUST" ] ||
    fail "HD.app has no embedded Android trust store"
  ARTIFACT_DISTRIBUTION=signed-artifact-store-v2
else
  [ -f "$DIRECT_MARKER" ] && [ ! -L "$DIRECT_MARKER" ] ||
    fail "HD.app has neither a signed nor direct-development Android distribution"
  ARTIFACT_DISTRIBUTION=direct-development-v1
fi
codesign --verify --deep --strict "$APP" || fail "HD.app codesign verification failed"

OUTPUT_PARENT=$(dirname -- "$OUTPUT")
mkdir -p -- "$OUTPUT_PARENT"
STAGE=$(mktemp -d "$OUTPUT_PARENT/.hd-android-readiness-soak.XXXXXX")
DATA=$(mktemp -d /private/tmp/hd-android-readiness-soak-data.XXXXXX)
UI_PID=
INSTANCE_ID=
COMPLETED=0
unset ADB_SERVER_SOCKET ANDROID_ADB_SERVER_ADDRESS
ANDROID_ADB_SERVER_PORT=$((20000 + ($$ % 20000)))
export ANDROID_ADB_SERVER_PORT

run_ctl() {
  stdout=$1
  stderr=$2
  shift 2
  "$CTL" --data-root "$DATA" --no-start-host "$@" >"$stdout" 2>"$stderr"
}

matching_pids() {
  ps ax -o pid=,command= | grep -F -- "--data-root $DATA" | grep -v grep |
    awk '{print $1}'
}

terminate_test_processes() {
  pids=$(matching_pids || true)
  [ -z "$pids" ] || kill $pids 2>/dev/null || true
  sleep 1
  pids=$(matching_pids || true)
  [ -z "$pids" ] || kill -KILL $pids 2>/dev/null || true
}

capture_failure_logs() {
  [ -d "$DATA/logs" ] || return 0
  mkdir -p "$STAGE/runtime-failure"
  cp -Rp "$DATA/logs" "$STAGE/runtime-failure/" 2>/dev/null || true
  [ -n "$INSTANCE_ID" ] || return 0
  run_root="$DATA/runs/$INSTANCE_ID"
  [ -d "$run_root" ] || return 0
  find "$run_root" -type f \( -name '*.json' -o -name '*.jsonl' -o -name '*.log' -o -name '*.txt' \) \
    -size -16M -print | while IFS= read -r source; do
      relative=${source#"$DATA/"}
      destination="$STAGE/runtime-failure/$relative"
      mkdir -p "$(dirname -- "$destination")"
      cp -p "$source" "$destination" 2>/dev/null || true
    done
}

cleanup() {
  status=$?
  if [ "$status" -ne 0 ]; then
    capture_failure_logs || true
  fi
  if [ -x "$CTL" ]; then
    "$CTL" --data-root "$DATA" --no-start-host shutdown --stop-all >/dev/null 2>&1 || true
  fi
  "$ADB" kill-server >/dev/null 2>&1 || true
  [ -z "$UI_PID" ] || kill "$UI_PID" 2>/dev/null || true
  terminate_test_processes
  case "$DATA" in /private/tmp/hd-android-readiness-soak-data.*) rm -rf -- "$DATA" ;; esac
  case "$STAGE" in
    "$OUTPUT_PARENT"/.hd-android-readiness-soak.*)
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

mkdir -p "$STAGE/cycles" "$STAGE/logs"
GUEST_DIGEST=
HOST_DIGEST=
if [ "$ARTIFACT_DISTRIBUTION" = signed-artifact-store-v2 ]; then
  "$CTL" verify-android-artifact-store \
    --store-root "$STORE" --trust-store "$TRUST" --channel development \
    >"$STAGE/signed-android-verification.json"
  grep -Fq '"exact_closure": true' "$STAGE/signed-android-verification.json" ||
    fail "signed Android artifact store did not have an exact closure"
  grep -Fq '"signature_verified": true' "$STAGE/signed-android-verification.json" ||
    fail "signed Android artifact store did not verify"
  GUEST_DIGEST=$(jq -r '.guest_bundle_digest // empty' "$STAGE/signed-android-verification.json")
  HOST_DIGEST=$(jq -r '.host_bundle_digest // empty' "$STAGE/signed-android-verification.json")
fi

"$UI" --data-root "$DATA" >"$STAGE/logs/ui.stdout" 2>"$STAGE/logs/ui.stderr" &
UI_PID=$!
printf '%s\n' "$UI_PID" >"$STAGE/ui.pid"
attempt=0
while ! run_ctl "$STAGE/health.json" "$STAGE/health.stderr" health; do
  kill -0 "$UI_PID" 2>/dev/null || fail "packaged UI exited before Host became healthy"
  [ "$attempt" -lt 300 ] || fail "packaged Host did not become healthy within 30 seconds"
  attempt=$((attempt + 1))
  sleep 0.1
done
HOST_PID=$(jq -r '.pid // empty' "$STAGE/health.json")
[ -n "$HOST_PID" ] && kill -0 "$HOST_PID" 2>/dev/null || fail "isolated Host PID is not alive"

if [ "$ARTIFACT_DISTRIBUTION" = direct-development-v1 ]; then
  STORE="$DATA/cache/direct-dev-artifacts-v2"
  MANIFEST_COUNT=0
  for manifest in "$STORE"/bundles/*/manifest-v2.json; do
    [ -f "$manifest" ] && [ ! -L "$manifest" ] || continue
    MANIFEST_COUNT=$((MANIFEST_COUNT + 1))
    kind=$(jq -r '.kind // empty' "$manifest")
    digest=$(jq -r '.digest // empty' "$manifest")
    [ "${#digest}" -eq 64 ] || fail "direct-development manifest has an invalid digest"
    case "$kind" in
      guest)
        [ -z "$GUEST_DIGEST" ] || fail "direct-development store has multiple Guest bundles"
        GUEST_DIGEST=$digest
        ;;
      host_tools)
        [ -z "$HOST_DIGEST" ] || fail "direct-development store has multiple Host bundles"
        HOST_DIGEST=$digest
        ;;
      *) fail "direct-development store contains unexpected bundle kind: $kind" ;;
    esac
  done
  [ "$MANIFEST_COUNT" -eq 2 ] ||
    fail "direct-development store must contain exactly two bundle manifests"
fi
[ "${#GUEST_DIGEST}" -eq 64 ] && [ "${#HOST_DIGEST}" -eq 64 ] ||
  fail "packaged Android distribution did not publish both bundle digests"

INSTANCE_ID=$(uuidgen | tr '[:upper:]' '[:lower:]')
jq -n --arg id "$INSTANCE_ID" --arg store "$STORE" \
  --arg guest "$GUEST_DIGEST" --arg host "$HOST_DIGEST" \
  --arg artifact_distribution "$ARTIFACT_DISTRIBUTION" '{
  schema_version:2, id:$id, name:"Android Readiness Soak", guest_kind:"android",
  microdroid:null, cpu_count:4, memory_mib:4096,
  display:{width:1080,height:1920,dpi:420,refresh_rate_hz:60,
    orientation:"portrait",vsync:"on",show_host_fps:true},
  adb:{mode:"loopback",host_port:null,executable:null},
  artifacts:{store_root:$store,guest_bundle_digest:$guest,host_bundle_digest:$host},
  boot:{kernel_log_level:4,panic_timeout_seconds:5,boot_animation:true},
  devices:{bluetooth:true,nfc:true,uwb:true,modem:true,gnss:true,sensors:true,
    network:true,audio:true,camera:true,power:true},
  restart_policy:"never", labels:{purpose:"installed-android-readiness-soak",
    artifact_distribution:$artifact_distribution,data_profile:"development-unencrypted"}
}' >"$STAGE/spec.json"
run_ctl "$STAGE/create.json" "$STAGE/create.stderr" create --spec "$STAGE/spec.json"
run_ctl "$STAGE/capabilities.json" "$STAGE/capabilities.stderr" capabilities "$INSTANCE_ID"
grep -Fq '"development_bypass": true' "$STAGE/capabilities.json" ||
  fail "development Android capability bypass was not explicit"
grep -Fq '"id": "display.zero_copy"' "$STAGE/capabilities.json" ||
  fail "Android readiness soak has no zero-copy display capability"

TOTAL_READY_SECONDS=0
MAX_READY_SECONDS=0
PREVIOUS_GENERATION=0
STABLE_WORKER_PID=
WARM_RSS_KIB=
WARM_FDS=
WARM_THREADS=
WARM_DATA_KIB=
cycle=1
while [ "$cycle" -le "$CYCLES" ]; do
  CYCLE_DIR="$STAGE/cycles/$cycle"
  mkdir -p "$CYCLE_DIR"
  STARTED=$(date +%s)
  run_ctl "$CYCLE_DIR/start.json" "$CYCLE_DIR/start.stderr" start "$INSTANCE_ID"
  grep -Fq '"state": "succeeded"' "$CYCLE_DIR/start.json" ||
    fail "cycle $cycle start operation did not succeed"
  attempt=0
  while :; do
    run_ctl "$CYCLE_DIR/ready.json" "$CYCLE_DIR/ready.stderr" show "$INSTANCE_ID"
    if grep -Fq '"observed": "ready"' "$CYCLE_DIR/ready.json" &&
       grep -Fq '"adb_ready": true' "$CYCLE_DIR/ready.json"; then
      break
    fi
    kill -0 "$UI_PID" 2>/dev/null || fail "cycle $cycle UI exited before ADB Ready"
    if [ "$attempt" -ge $((READY_BUDGET_SECONDS * 10)) ]; then
      DIAGNOSTIC_SERIAL=$(jq -r '.adb_serial // empty' "$CYCLE_DIR/ready.json")
      if [ -n "$DIAGNOSTIC_SERIAL" ]; then
        "$ADB" -s "$DIAGNOSTIC_SERIAL" get-state \
          >"$CYCLE_DIR/pending-adb-state.txt" 2>&1 || true
        for property in sys.boot_completed service.bootanim.exit init.svc.bootanim; do
          "$ADB" -s "$DIAGNOSTIC_SERIAL" shell getprop "$property" \
            >"$CYCLE_DIR/pending-$property.txt" 2>&1 || true
        done
        if "$ADB" -s "$DIAGNOSTIC_SERIAL" shell pidof bootanimation \
          >"$CYCLE_DIR/pending-bootanimation-pid.txt" 2>"$CYCLE_DIR/pending-bootanimation-pid.stderr"; then
          echo running >"$CYCLE_DIR/pending-bootanimation-process.txt"
        elif [ "$?" -eq 1 ]; then
          echo stopped >"$CYCLE_DIR/pending-bootanimation-process.txt"
        else
          echo probe-failed >"$CYCLE_DIR/pending-bootanimation-process.txt"
        fi
        "$ADB" -s "$DIAGNOSTIC_SERIAL" shell cmd package path android \
          >"$CYCLE_DIR/pending-package.txt" 2>&1 || true
        for service in SurfaceFlinger input window; do
          "$ADB" -s "$DIAGNOSTIC_SERIAL" shell service check "$service" \
            >"$CYCLE_DIR/pending-service-$service.txt" 2>&1 || true
        done
      fi
      fail "cycle $cycle did not reach ADB Ready within ${READY_BUDGET_SECONDS}s"
    fi
    attempt=$((attempt + 1))
    sleep 0.1
  done
  FINISHED=$(date +%s)
  READY_SECONDS=$((FINISHED - STARTED))
  [ "$READY_SECONDS" -le "$READY_BUDGET_SECONDS" ] ||
    fail "cycle $cycle exceeded Ready budget: ${READY_SECONDS}s"
  TOTAL_READY_SECONDS=$((TOTAL_READY_SECONDS + READY_SECONDS))
  [ "$READY_SECONDS" -le "$MAX_READY_SECONDS" ] || MAX_READY_SECONDS=$READY_SECONDS

  ADB_SERIAL=$(jq -r '.adb_serial // empty' "$CYCLE_DIR/ready.json")
  GENERATION=$(jq -r '.frame_generation // 0' "$CYCLE_DIR/ready.json")
  WORKER_PID=$(jq -r '.worker.pid // empty' "$CYCLE_DIR/ready.json")
  [ -n "$ADB_SERIAL" ] || fail "cycle $cycle has no ADB serial"
  [ -n "$WORKER_PID" ] && kill -0 "$WORKER_PID" 2>/dev/null ||
    fail "cycle $cycle has no live Worker identity"
  if [ -z "$STABLE_WORKER_PID" ]; then
    STABLE_WORKER_PID=$WORKER_PID
  else
    [ "$WORKER_PID" = "$STABLE_WORKER_PID" ] ||
      fail "cycle $cycle replaced the healthy Worker identity"
  fi
  [ "$GENERATION" -gt "$PREVIOUS_GENERATION" ] ||
    fail "cycle $cycle frame generation did not increase"
  PREVIOUS_GENERATION=$GENERATION
  sample=1
  while [ "$sample" -le 2 ]; do
    "$ADB" -s "$ADB_SERIAL" shell getprop sys.boot_completed |
      tr -d '\r' >"$CYCLE_DIR/sys-boot-completed-$sample.txt"
    "$ADB" -s "$ADB_SERIAL" shell getprop service.bootanim.exit |
      tr -d '\r' >"$CYCLE_DIR/bootanim-exit-$sample.txt"
    "$ADB" -s "$ADB_SERIAL" shell getprop init.svc.bootanim |
      tr -d '\r' >"$CYCLE_DIR/bootanim-service-$sample.txt"
    if "$ADB" -s "$ADB_SERIAL" shell pidof bootanimation \
      >"$CYCLE_DIR/bootanim-pid-$sample.txt" 2>"$CYCLE_DIR/bootanim-pid-$sample.stderr"; then
      echo running >"$CYCLE_DIR/bootanim-process-$sample.txt"
    elif [ "$?" -eq 1 ]; then
      echo stopped >"$CYCLE_DIR/bootanim-process-$sample.txt"
    else
      echo probe-failed >"$CYCLE_DIR/bootanim-process-$sample.txt"
    fi
    "$ADB" -s "$ADB_SERIAL" shell service check SurfaceFlinger |
      tr -d '\r' >"$CYCLE_DIR/surfaceflinger-$sample.txt"
    "$ADB" -s "$ADB_SERIAL" shell service check input |
      tr -d '\r' >"$CYCLE_DIR/input-$sample.txt"
    "$ADB" -s "$ADB_SERIAL" shell service check window |
      tr -d '\r' >"$CYCLE_DIR/window-$sample.txt"
    grep -Fxq 1 "$CYCLE_DIR/sys-boot-completed-$sample.txt" ||
      fail "cycle $cycle sample $sample published ADB Ready before sys.boot_completed=1"
    grep -Fxq 1 "$CYCLE_DIR/bootanim-exit-$sample.txt" ||
      fail "cycle $cycle sample $sample published ADB Ready before boot animation exit"
    grep -Fxq stopped "$CYCLE_DIR/bootanim-service-$sample.txt" ||
      fail "cycle $cycle sample $sample published ADB Ready before boot animation service stopped"
    grep -Fxq stopped "$CYCLE_DIR/bootanim-process-$sample.txt" ||
      fail "cycle $cycle sample $sample published ADB Ready while bootanimation was alive"
    grep -Fq found "$CYCLE_DIR/surfaceflinger-$sample.txt" ||
      fail "cycle $cycle sample $sample published ADB Ready without SurfaceFlinger"
    grep -Fq found "$CYCLE_DIR/input-$sample.txt" ||
      fail "cycle $cycle sample $sample published ADB Ready without InputManager"
    grep -Fq found "$CYCLE_DIR/window-$sample.txt" ||
      fail "cycle $cycle sample $sample published ADB Ready without WindowManager"
    [ "$sample" -eq 2 ] || sleep 0.5
    sample=$((sample + 1))
  done

  UWB_DISTANCE_CM=$((320 + cycle))
  run_ctl "$CYCLE_DIR/uwb-action.json" "$CYCLE_DIR/uwb-action.stderr" \
    action "$INSTANCE_ID" uwb-ranging "$UWB_DISTANCE_CM"
  [ "$(jq -r '.uwb_ranging.distance_cm // 0' "$CYCLE_DIR/uwb-action.json")" \
      -eq "$UWB_DISTANCE_CM" ] ||
    fail "cycle $cycle UWB action did not return its instance-owned ranging state"
  run_ctl "$CYCLE_DIR/uwb-show.json" "$CYCLE_DIR/uwb-show.stderr" show "$INSTANCE_ID"
  [ "$(jq -r '.uwb_ranging.distance_cm // 0' "$CYCLE_DIR/uwb-show.json")" \
      -eq "$UWB_DISTANCE_CM" ] ||
    fail "cycle $cycle Host did not persist the Worker UWB ranging state"

  BLUETOOTH_BEACON_ID=00000000-0000-4000-8000-000000000113
  run_ctl "$CYCLE_DIR/bluetooth-beacon-action.json" \
    "$CYCLE_DIR/bluetooth-beacon-action.stderr" action "$INSTANCE_ID" \
    bluetooth-beacon "$BLUETOOTH_BEACON_ID" "HD Readiness Beacon" 02010605FF4C000215
  [ "$(jq -r --arg peer_id "$BLUETOOTH_BEACON_ID" \
      '.bluetooth_peers[] | select(.peer_id == $peer_id) | .kind' \
      "$CYCLE_DIR/bluetooth-beacon-action.json")" = beacon ] &&
    [ "$(jq -r --arg peer_id "$BLUETOOTH_BEACON_ID" \
      '.bluetooth_peers[] | select(.peer_id == $peer_id) | .advertising' \
      "$CYCLE_DIR/bluetooth-beacon-action.json")" = true ] ||
    fail "cycle $cycle Bluetooth Beacon action did not return its instance-owned state"
  run_ctl "$CYCLE_DIR/bluetooth-beacon-stop.json" \
    "$CYCLE_DIR/bluetooth-beacon-stop.stderr" action "$INSTANCE_ID" \
    bluetooth-advertise "$BLUETOOTH_BEACON_ID" false
  run_ctl "$CYCLE_DIR/bluetooth-beacon-show.json" \
    "$CYCLE_DIR/bluetooth-beacon-show.stderr" show "$INSTANCE_ID"
  [ "$(jq -r --arg peer_id "$BLUETOOTH_BEACON_ID" \
      '.bluetooth_peers[] | select(.peer_id == $peer_id) | .advertising' \
      "$CYCLE_DIR/bluetooth-beacon-show.json")" = false ] ||
    fail "cycle $cycle Host did not persist the stopped Bluetooth Beacon state"
  run_ctl "$CYCLE_DIR/bluetooth-beacon-remove.json" \
    "$CYCLE_DIR/bluetooth-beacon-remove.stderr" action "$INSTANCE_ID" \
    bluetooth-remove "$BLUETOOTH_BEACON_ID"
  [ "$(jq -r '.bluetooth_peers | length' \
      "$CYCLE_DIR/bluetooth-beacon-remove.json")" -eq 0 ] ||
    fail "cycle $cycle Bluetooth Beacon was not removed from instance state"

  BLUETOOTH_SCRIPTED_BEACON_ID=00000000-0000-4000-8000-000000000114
  BLUETOOTH_SCRIPTED_FRAMES='[{"advertising_data_hex":"02010605FF4C000215","duration_ms":20},{"advertising_data_hex":"02010605FF4C000216","duration_ms":20}]'
  run_ctl "$CYCLE_DIR/bluetooth-scripted-beacon-action.json" \
    "$CYCLE_DIR/bluetooth-scripted-beacon-action.stderr" action "$INSTANCE_ID" \
    bluetooth-scripted-beacon "$BLUETOOTH_SCRIPTED_BEACON_ID" \
    "HD Readiness Scripted Beacon" --frames-json "$BLUETOOTH_SCRIPTED_FRAMES" --repeat
  [ "$(jq -r --arg peer_id "$BLUETOOTH_SCRIPTED_BEACON_ID" \
      '.bluetooth_peers[] | select(.peer_id == $peer_id) | .kind' \
      "$CYCLE_DIR/bluetooth-scripted-beacon-action.json")" = scripted_beacon ] &&
    [ "$(jq -r --arg peer_id "$BLUETOOTH_SCRIPTED_BEACON_ID" \
      '.bluetooth_peers[] | select(.peer_id == $peer_id) | .scripted_frame_count' \
      "$CYCLE_DIR/bluetooth-scripted-beacon-action.json")" -eq 2 ] &&
    [ "$(jq -r --arg peer_id "$BLUETOOTH_SCRIPTED_BEACON_ID" \
      '.bluetooth_peers[] | select(.peer_id == $peer_id) | .repeat' \
      "$CYCLE_DIR/bluetooth-scripted-beacon-action.json")" = true ] ||
    fail "cycle $cycle scripted Beacon action did not return its bounded timeline state"
  sleep 0.1
  run_ctl "$CYCLE_DIR/bluetooth-scripted-beacon-show.json" \
    "$CYCLE_DIR/bluetooth-scripted-beacon-show.stderr" show "$INSTANCE_ID"
  [ "$(jq -r --arg peer_id "$BLUETOOTH_SCRIPTED_BEACON_ID" \
      '.bluetooth_peers[] | select(.peer_id == $peer_id) | .advertising' \
      "$CYCLE_DIR/bluetooth-scripted-beacon-show.json")" = true ] ||
    fail "cycle $cycle Host lost the active scripted Beacon state"
  run_ctl "$CYCLE_DIR/bluetooth-scripted-beacon-remove.json" \
    "$CYCLE_DIR/bluetooth-scripted-beacon-remove.stderr" action "$INSTANCE_ID" \
    bluetooth-remove "$BLUETOOTH_SCRIPTED_BEACON_ID"
  [ "$(jq -r '.bluetooth_peers | length' \
      "$CYCLE_DIR/bluetooth-scripted-beacon-remove.json")" -eq 0 ] ||
    fail "cycle $cycle scripted Beacon was not removed from instance state"

  MODEM_SIGNAL_STRENGTH=$((16 + cycle))
  run_ctl "$CYCLE_DIR/modem-action.json" "$CYCLE_DIR/modem-action.stderr" \
    action "$INSTANCE_ID" modem-state 310260 "$MODEM_SIGNAL_STRENGTH" \
    --operator-long-name "HD Test Mobile" --operator-short-name HDT --registered false
  [ "$(jq -r '.modem_state.signal_strength // -1' "$CYCLE_DIR/modem-action.json")" \
      -eq "$MODEM_SIGNAL_STRENGTH" ] ||
    fail "cycle $cycle modem action did not return its instance-owned signal state"
  [ "$(jq -r '.modem_state.operator_numeric // empty' "$CYCLE_DIR/modem-action.json")" \
      = 310260 ] &&
    [ "$(jq -r '.modem_state.registered' "$CYCLE_DIR/modem-action.json")" = false ] ||
    fail "cycle $cycle modem action did not return its operator and registration state"
  run_ctl "$CYCLE_DIR/modem-show.json" "$CYCLE_DIR/modem-show.stderr" show "$INSTANCE_ID"
  [ "$(jq -r '.modem_state.signal_strength // -1' "$CYCLE_DIR/modem-show.json")" \
      -eq "$MODEM_SIGNAL_STRENGTH" ] ||
    fail "cycle $cycle Host did not persist the Worker modem state"

  run_ctl "$CYCLE_DIR/stop.json" "$CYCLE_DIR/stop.stderr" stop --force "$INSTANCE_ID"
  run_ctl "$CYCLE_DIR/stopped.json" "$CYCLE_DIR/stopped.stderr" show "$INSTANCE_ID"
  grep -Fq '"observed": "stopped"' "$CYCLE_DIR/stopped.json" ||
    fail "cycle $cycle did not reach Stopped"
  WORKER_COUNT=$(ps ax -o command= |
    grep -F -- "$APP/Contents/MacOS/hd-worker --data-root $DATA" |
    grep -v grep | wc -l | tr -d ' ')
  [ "$WORKER_COUNT" -eq 1 ] && kill -0 "$STABLE_WORKER_PID" 2>/dev/null ||
    fail "cycle $cycle did not retain exactly one authenticated Worker"
  ps ax -o command= | grep -F -- "$DATA" | grep -F crosvm | grep -v grep \
    >"$CYCLE_DIR/process-leaks.txt" 2>/dev/null && fail "cycle $cycle left crosvm running"

  RSS_KIB=$(ps -o rss= -p "$HOST_PID" | tr -d ' ')
  FDS=$(lsof -p "$HOST_PID" 2>/dev/null | wc -l | tr -d ' ')
  THREADS=$(ps -M "$HOST_PID" | wc -l | tr -d ' ')
  DATA_KIB=$(du -sk "$DATA" | awk '{print $1}')
  jq -cn --argjson cycle "$cycle" --argjson ready_seconds "$READY_SECONDS" \
    --argjson generation "$GENERATION" --argjson worker_pid "$WORKER_PID" \
    --argjson host_rss_kib "$RSS_KIB" \
    --argjson host_fds "$FDS" --argjson host_threads "$THREADS" \
    --argjson data_kib "$DATA_KIB" \
    '{cycle:$cycle,ready_seconds:$ready_seconds,frame_generation:$generation,
      worker_pid:$worker_pid,worker_identity_stable:true,
      sys_boot_completed:"1",bootanim_exit:"1",bootanim_service:"stopped",
      bootanim_process:"absent",surfaceflinger:"found",input_manager:"found",
      window_manager:"found",stable_external_samples:2,host_rss_kib:$host_rss_kib,
      host_fds:$host_fds,host_threads:$host_threads,data_kib:$data_kib,
      bluetooth_beacon:"pass",bluetooth_scripted_beacon:"pass",
      stop:"pass",process_cleanup:"pass"}' \
    >>"$STAGE/cycles.jsonl"
  if [ "$cycle" -eq 1 ]; then
    WARM_RSS_KIB=$RSS_KIB
    WARM_FDS=$FDS
    WARM_THREADS=$THREADS
    WARM_DATA_KIB=$DATA_KIB
  fi
  echo "cycle=$cycle ready_seconds=$READY_SECONDS frame_generation=$GENERATION worker_pid=$WORKER_PID"
  cycle=$((cycle + 1))
done

FINAL_RSS_KIB=$RSS_KIB
FINAL_FDS=$FDS
FINAL_THREADS=$THREADS
FINAL_DATA_KIB=$DATA_KIB
RSS_GROWTH_KIB=$((FINAL_RSS_KIB - WARM_RSS_KIB))
FD_GROWTH=$((FINAL_FDS - WARM_FDS))
THREAD_GROWTH=$((FINAL_THREADS - WARM_THREADS))
DATA_GROWTH_KIB=$((FINAL_DATA_KIB - WARM_DATA_KIB))
[ "$RSS_GROWTH_KIB" -le 131072 ] || fail "Host RSS grew more than 128 MiB after warmup"
[ "$FD_GROWTH" -le 16 ] || fail "Host file descriptors grew by more than 16 after warmup"
[ "$THREAD_GROWTH" -le 8 ] || fail "Host threads grew by more than 8 after warmup"
[ "$DATA_GROWTH_KIB" -le 524288 ] || fail "data root grew more than 512 MiB after warmup"

run_ctl "$STAGE/delete.json" "$STAGE/delete.stderr" delete "$INSTANCE_ID"
attempt=0
while kill -0 "$STABLE_WORKER_PID" 2>/dev/null; do
  [ "$attempt" -lt 100 ] || fail "deleted Android instance retained its Worker"
  attempt=$((attempt + 1))
  sleep 0.1
done
run_ctl "$STAGE/shutdown.json" "$STAGE/shutdown.stderr" shutdown --stop-all
attempt=0
while kill -0 "$HOST_PID" 2>/dev/null; do
  [ "$attempt" -lt 100 ] || fail "isolated Host did not exit within 10 seconds"
  attempt=$((attempt + 1))
  sleep 0.1
done
kill "$UI_PID" 2>/dev/null || true
UI_PID=
sleep 1
[ -z "$(matching_pids || true)" ] || fail "Android readiness soak left an isolated process"

AVERAGE_READY_SECONDS=$((TOTAL_READY_SECONDS / CYCLES))
VERSION=$(plutil -extract CFBundleShortVersionString raw -o - "$APP/Contents/Info.plist")
BUILD=$(plutil -extract CFBundleVersion raw -o - "$APP/Contents/Info.plist")
jq -n --arg version "$VERSION" --arg build "$BUILD" \
  --arg artifact_distribution "$ARTIFACT_DISTRIBUTION" --argjson cycles "$CYCLES" \
  --argjson average "$AVERAGE_READY_SECONDS" --argjson maximum "$MAX_READY_SECONDS" \
  --argjson rss_growth "$RSS_GROWTH_KIB" --argjson fd_growth "$FD_GROWTH" \
  --argjson thread_growth "$THREAD_GROWTH" --argjson data_growth "$DATA_GROWTH_KIB" \
  '{schema_version:1,profile:"hd-macos-android-readiness-soak-v1",status:"pass",
    version:$version,build:$build,artifact_distribution:$artifact_distribution,
    cycles:$cycles,successful_boots:$cycles,
    sys_boot_completed_consistent:true,boot_animation_exit_consistent:true,
    boot_animation_stopped_consistent:true,boot_animation_process_absent:true,
    interactive_services_consistent:true,stable_external_samples:2,
    startup_policy_revalidated:true,display_policy_revalidated:true,
    frame_generation_monotonic:true,worker_identity_stable:true,
    average_ready_seconds:$average,
    maximum_ready_seconds:$maximum,host_rss_growth_kib:$rss_growth,
    host_fd_growth:$fd_growth,host_thread_growth:$thread_growth,
    data_growth_kib:$data_growth,zero_copy:true,swiftshader:false,
    bluetooth_beacon_instance_control:"pass",
    bluetooth_scripted_beacon_instance_control:"pass",process_cleanup:"pass"}' \
  >"$STAGE/result.json"
GENERATED_AT=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
jq -n --arg generated_at "$GENERATED_AT" \
  --arg command "macos-android-readiness-soak-smoke.sh --cycles $CYCLES" \
  --arg summary "$CYCLES packaged Android starts kept adb_ready aligned with stable post-policy boot, animation-process and interactive-service state; frame generations, Host resource bounds and cleanup passed" \
  '{schema_version:2,generated_at:$generated_at,source:"scripts/macos-android-readiness-soak-smoke.sh",
  gates:[{name:"macos-android-readiness-soak",command:$command,
    status:"pass",duration_ms:null,log_path:"cycles.jsonl",summary:$summary}]}' \
  >"$STAGE/android-readiness-soak-gates.json"

mv -- "$STAGE" "$OUTPUT"
COMPLETED=1
echo "evidence=$OUTPUT"
echo "gate_report=$OUTPUT/android-readiness-soak-gates.json"
echo "cycles=$CYCLES"
echo "average_ready_seconds=$AVERAGE_READY_SECONDS"
echo "maximum_ready_seconds=$MAX_READY_SECONDS"
