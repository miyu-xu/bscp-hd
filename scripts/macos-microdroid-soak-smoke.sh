#!/bin/sh
set -eu

usage() {
  cat <<'EOF'
Usage: scripts/macos-microdroid-soak-smoke.sh \
  --app <HD.app> \
  --output <fresh-absolute-evidence-directory> \
  [--cycles <3..100>] \
  [--ready-budget-seconds <1..180>] \
  [--dwell-seconds <0..30>] \
  [--development-package]
EOF
}

fail() {
  echo "$*" >&2
  exit 1
}

APP=
OUTPUT=
CYCLES=10
READY_BUDGET_SECONDS=90
DWELL_SECONDS=1
DEVELOPMENT_PACKAGE=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    --app) [ "$#" -ge 2 ] || { usage >&2; exit 2; }; APP=$2; shift 2 ;;
    --output) [ "$#" -ge 2 ] || { usage >&2; exit 2; }; OUTPUT=$2; shift 2 ;;
    --cycles) [ "$#" -ge 2 ] || { usage >&2; exit 2; }; CYCLES=$2; shift 2 ;;
    --ready-budget-seconds) [ "$#" -ge 2 ] || { usage >&2; exit 2; }; READY_BUDGET_SECONDS=$2; shift 2 ;;
    --dwell-seconds) [ "$#" -ge 2 ] || { usage >&2; exit 2; }; DWELL_SECONDS=$2; shift 2 ;;
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
case "$DWELL_SECONDS" in *[!0-9]*|'') fail "--dwell-seconds must be an integer" ;; esac
[ "$CYCLES" -ge 3 ] && [ "$CYCLES" -le 100 ] || fail "--cycles must be between 3 and 100"
[ "$READY_BUDGET_SECONDS" -ge 1 ] && [ "$READY_BUDGET_SECONDS" -le 180 ] ||
  fail "--ready-budget-seconds must be between 1 and 180"
[ "$DWELL_SECONDS" -le 30 ] || fail "--dwell-seconds must be between 0 and 30"

CTL="$APP/Contents/MacOS/hdctl"
HOST="$APP/Contents/MacOS/hd-host"
[ -x "$CTL" ] && [ -f "$CTL" ] && [ ! -L "$CTL" ] || fail "HD.app has no real executable hdctl"
[ -x "$HOST" ] && [ -f "$HOST" ] && [ ! -L "$HOST" ] || fail "HD.app has no real executable hd-host"
codesign --verify --deep --strict "$APP" || fail "HD.app codesign verification failed"
[ ! -e "$OUTPUT" ] || fail "refusing to replace evidence output: $OUTPUT"
FAILURE_OUTPUT="$OUTPUT.failed"
[ ! -e "$FAILURE_OUTPUT" ] || fail "refusing to replace failure evidence: $FAILURE_OUTPUT"
[ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ] ||
  fail "Microdroid soak requires Apple Silicon macOS"

OUTPUT_PARENT=$(dirname -- "$OUTPUT")
mkdir -p -- "$OUTPUT_PARENT"
STAGE=$(mktemp -d "$OUTPUT_PARENT/.hd-microdroid-soak.XXXXXX")
DATA=$(mktemp -d /private/tmp/hd-microdroid-soak-data.XXXXXX)
COMPLETED=0

run_ctl() {
  stdout=$1
  stderr=$2
  shift 2
  if [ "$DEVELOPMENT_PACKAGE" -eq 1 ]; then
    HD_MICRODROID_DEV_BYPASS=1 "$CTL" --data-root "$DATA" "$@" >"$stdout" 2>"$stderr"
  else
    "$CTL" --data-root "$DATA" "$@" >"$stdout" 2>"$stderr"
  fi
}

terminate_test_processes() {
  pids=$(pgrep -f "$DATA" 2>/dev/null || true)
  [ -z "$pids" ] || kill $pids 2>/dev/null || true
  sleep 1
  pids=$(pgrep -f "$DATA" 2>/dev/null || true)
  [ -z "$pids" ] || kill -KILL $pids 2>/dev/null || true
}

cleanup() {
  status=$?
  if [ -x "$CTL" ]; then
    run_ctl /dev/null /dev/null shutdown --stop-all || true
  fi
  terminate_test_processes
  case "$DATA" in /private/tmp/hd-microdroid-soak-data.*) rm -rf -- "$DATA" ;; esac
  case "$STAGE" in
    "$OUTPUT_PARENT"/.hd-microdroid-soak.*)
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

mkdir -p -- "$STAGE/cycles"
run_ctl "$STAGE/health.json" "$STAGE/health.stderr" health
HOST_PID=$(pgrep -f "$HOST --data-root $DATA" | head -1)
[ -n "$HOST_PID" ] || fail "isolated Host process was not found"
COLD_RSS_KIB=$(ps -o rss= -p "$HOST_PID" | tr -d ' ')
COLD_FDS=$(lsof -p "$HOST_PID" 2>/dev/null | wc -l | tr -d ' ')
COLD_THREADS=$(ps -M "$HOST_PID" | wc -l | tr -d ' ')
COLD_DATA_KIB=$(du -sk "$DATA" | awk '{print $1}')

TOTAL_READY_SECONDS=0
MAX_READY_SECONDS=0
WARM_RSS_KIB=
WARM_FDS=
WARM_THREADS=
WARM_DATA_KIB=
cycle=1
while [ "$cycle" -le "$CYCLES" ]; do
  CYCLE_DIR="$STAGE/cycles/$cycle"
  mkdir -p -- "$CYCLE_DIR"
  INSTANCE_ID=$(uuidgen | tr '[:upper:]' '[:lower:]')
  jq -n --arg id "$INSTANCE_ID" --arg cycle "$cycle" '{
    schema_version: 2,
    id: $id,
    name: ("Microdroid Soak " + $cycle),
    guest_kind: "microdroid",
    microdroid: {
      debug_level: "full",
      payload: { kind: "empty" },
      encrypted_storage_mib: 64
    },
    cpu_count: 1,
    memory_mib: 512,
    display: {
      width: 1080, height: 1920, dpi: 420, refresh_rate_hz: 60,
      orientation: "portrait", vsync: "on", show_host_fps: false
    },
    adb: { mode: "disabled", host_port: null, executable: null },
    artifacts: null,
    boot: { kernel_log_level: 4, panic_timeout_seconds: 5, boot_animation: true },
    devices: {
      bluetooth: false, nfc: false, uwb: false, modem: false, gnss: false,
      sensors: false, network: false, audio: false, camera: false, power: false
    },
    restart_policy: "never",
    labels: { purpose: "installed-microdroid-soak" }
  }' >"$CYCLE_DIR/spec.json"

  run_ctl "$CYCLE_DIR/create.json" "$CYCLE_DIR/create.stderr" create --spec "$CYCLE_DIR/spec.json"
  if [ "$cycle" -eq 1 ]; then
    run_ctl "$CYCLE_DIR/capabilities.json" "$CYCLE_DIR/capabilities.stderr" capabilities "$INSTANCE_ID"
    if [ "$DEVELOPMENT_PACKAGE" -eq 1 ]; then
      grep -Fq '"development_bypass": true' "$CYCLE_DIR/capabilities.json" ||
        fail "development capability bypass was not explicit"
    else
      grep -Fq '"certified": true' "$CYCLE_DIR/capabilities.json" ||
        fail "release capability was not certified"
    fi
  fi

  STARTED=$(date +%s)
  run_ctl "$CYCLE_DIR/start.json" "$CYCLE_DIR/start.stderr" start "$INSTANCE_ID"
  FINISHED=$(date +%s)
  READY_SECONDS=$((FINISHED - STARTED))
  [ "$READY_SECONDS" -le "$READY_BUDGET_SECONDS" ] ||
    fail "cycle $cycle exceeded Ready budget: ${READY_SECONDS}s"
  TOTAL_READY_SECONDS=$((TOTAL_READY_SECONDS + READY_SECONDS))
  [ "$READY_SECONDS" -le "$MAX_READY_SECONDS" ] || MAX_READY_SECONDS=$READY_SECONDS
  grep -Fq '"state": "succeeded"' "$CYCLE_DIR/start.json" ||
    fail "cycle $cycle start did not succeed"
  run_ctl "$CYCLE_DIR/ready.json" "$CYCLE_DIR/ready.stderr" show "$INSTANCE_ID"
  grep -Fq '"observed": "ready"' "$CYCLE_DIR/ready.json" ||
    fail "cycle $cycle did not reach Ready"
  [ "$DWELL_SECONDS" -eq 0 ] || sleep "$DWELL_SECONDS"
  run_ctl "$CYCLE_DIR/stop.json" "$CYCLE_DIR/stop.stderr" stop --force "$INSTANCE_ID"
  run_ctl "$CYCLE_DIR/delete.json" "$CYCLE_DIR/delete.stderr" delete "$INSTANCE_ID"

  worker_wait=0
  while pgrep -f "$APP/Contents/MacOS/hd-worker --data-root $DATA" >/dev/null 2>&1; do
    [ "$worker_wait" -lt 50 ] || fail "cycle $cycle left an idle Worker after delete"
    worker_wait=$((worker_wait + 1))
    sleep 0.1
  done
  pgrep -f "$DATA.*crosvm|crosvm.*$DATA" >"$CYCLE_DIR/process-leaks.txt" 2>/dev/null &&
    fail "cycle $cycle left crosvm running"

  RSS_KIB=$(ps -o rss= -p "$HOST_PID" | tr -d ' ')
  FDS=$(lsof -p "$HOST_PID" 2>/dev/null | wc -l | tr -d ' ')
  THREADS=$(ps -M "$HOST_PID" | wc -l | tr -d ' ')
  DATA_KIB=$(du -sk "$DATA" | awk '{print $1}')
  jq -cn \
    --argjson cycle "$cycle" \
    --arg instance_id "$INSTANCE_ID" \
    --argjson ready_seconds "$READY_SECONDS" \
    --argjson host_rss_kib "$RSS_KIB" \
    --argjson host_fds "$FDS" \
    --argjson host_threads "$THREADS" \
    --argjson data_kib "$DATA_KIB" \
    '{cycle:$cycle, instance_id:$instance_id, ready_seconds:$ready_seconds,
      host_rss_kib:$host_rss_kib, host_fds:$host_fds,
      host_threads:$host_threads, data_kib:$data_kib,
      observed:"ready", stop:"pass", delete:"pass", process_cleanup:"pass"}' \
    >>"$STAGE/cycles.jsonl"
  if [ "$cycle" -eq 1 ]; then
    WARM_RSS_KIB=$RSS_KIB
    WARM_FDS=$FDS
    WARM_THREADS=$THREADS
    WARM_DATA_KIB=$DATA_KIB
  fi
  cycle=$((cycle + 1))
done

FINAL_RSS_KIB=$(ps -o rss= -p "$HOST_PID" | tr -d ' ')
FINAL_FDS=$(lsof -p "$HOST_PID" 2>/dev/null | wc -l | tr -d ' ')
FINAL_THREADS=$(ps -M "$HOST_PID" | wc -l | tr -d ' ')
FINAL_DATA_KIB=$(du -sk "$DATA" | awk '{print $1}')
RSS_GROWTH_KIB=$((FINAL_RSS_KIB - WARM_RSS_KIB))
FD_GROWTH=$((FINAL_FDS - WARM_FDS))
THREAD_GROWTH=$((FINAL_THREADS - WARM_THREADS))
DATA_GROWTH_KIB=$((FINAL_DATA_KIB - WARM_DATA_KIB))
[ "$RSS_GROWTH_KIB" -le 131072 ] || fail "Host RSS grew more than 128 MiB after warm-up"
[ "$FD_GROWTH" -le 16 ] || fail "Host file descriptors grew by more than 16 after warm-up"
[ "$THREAD_GROWTH" -le 8 ] || fail "Host threads grew by more than 8 after warm-up"
[ "$DATA_GROWTH_KIB" -le 16384 ] || fail "data root grew more than 16 MiB after warm-up"

run_ctl "$STAGE/shutdown.json" "$STAGE/shutdown.stderr" shutdown --stop-all
sleep 1
if pgrep -f "$DATA" >"$STAGE/final-process-leaks.txt" 2>/dev/null; then
  fail "soak left an isolated HD process running"
fi

VERSION=$(plutil -extract CFBundleShortVersionString raw -o - "$APP/Contents/Info.plist")
BUILD=$(plutil -extract CFBundleVersion raw -o - "$APP/Contents/Info.plist")
APP_HOST_SHA256=$(shasum -a 256 "$HOST" | awk '{print $1}')
AVERAGE_READY_SECONDS=$((TOTAL_READY_SECONDS / CYCLES))
GENERATED_AT=$(date -u +%Y-%m-%dT%H:%M:%SZ)
jq -n \
  --arg version "$VERSION" \
  --arg build "$BUILD" \
  --arg app_host_sha256 "$APP_HOST_SHA256" \
  --argjson development_bypass "$([ "$DEVELOPMENT_PACKAGE" -eq 1 ] && echo true || echo false)" \
  --argjson cycles "$CYCLES" \
  --argjson ready_budget_seconds "$READY_BUDGET_SECONDS" \
  --argjson max_ready_seconds "$MAX_READY_SECONDS" \
  --argjson average_ready_seconds "$AVERAGE_READY_SECONDS" \
  --argjson cold_rss_kib "$COLD_RSS_KIB" \
  --argjson warm_rss_kib "$WARM_RSS_KIB" \
  --argjson final_rss_kib "$FINAL_RSS_KIB" \
  --argjson rss_growth_kib "$RSS_GROWTH_KIB" \
  --argjson cold_fds "$COLD_FDS" \
  --argjson fd_growth "$FD_GROWTH" \
  --argjson cold_threads "$COLD_THREADS" \
  --argjson thread_growth "$THREAD_GROWTH" \
  --argjson cold_data_kib "$COLD_DATA_KIB" \
  --argjson data_growth_kib "$DATA_GROWTH_KIB" \
  '{schema_version:1, profile:"macos-arm64-installed-microdroid-soak-v1",
    status:"pass", version:$version, build:$build, app_host_sha256:$app_host_sha256,
    development_bypass:$development_bypass, cycles:$cycles,
    ready_budget_seconds:$ready_budget_seconds, max_ready_seconds:$max_ready_seconds,
    average_ready_seconds:$average_ready_seconds,
    resources:{cold_rss_kib:$cold_rss_kib, warm_rss_kib:$warm_rss_kib,
      final_rss_kib:$final_rss_kib, rss_growth_kib:$rss_growth_kib,
      cold_fds:$cold_fds, fd_growth:$fd_growth,
      cold_threads:$cold_threads, thread_growth:$thread_growth,
      cold_data_kib:$cold_data_kib, data_growth_kib:$data_growth_kib},
    lifecycle:{ready:"pass", stop:"pass", delete:"pass", process_cleanup:"pass"}}' \
  >"$STAGE/result.json"
jq -n \
  --arg generated_at "$GENERATED_AT" \
  --arg log_path "$OUTPUT/result.json" \
  --argjson cycles "$CYCLES" \
  --argjson max_ready_seconds "$MAX_READY_SECONDS" \
  --argjson rss_growth_kib "$RSS_GROWTH_KIB" \
  --argjson fd_growth "$FD_GROWTH" \
  --argjson thread_growth "$THREAD_GROWTH" \
  --argjson data_growth_kib "$DATA_GROWTH_KIB" \
  '{schema_version:2, generated_at:$generated_at,
    source:"scripts/macos-microdroid-soak-smoke.sh",
    gates:[{name:"macos-microdroid-soak",
      command:"macos-microdroid-soak-smoke.sh --app <HD.app> --output <fresh-dir>",
      status:"pass", duration_ms:null, log_path:$log_path,
      summary:("Completed " + ($cycles|tostring) + " installed Microdroid create/Ready/force-stop/delete cycles; max Ready " + ($max_ready_seconds|tostring) + "s; post-warm-up RSS growth " + ($rss_growth_kib|tostring) + " KiB, file descriptor growth " + ($fd_growth|tostring) + ", thread growth " + ($thread_growth|tostring) + ", data growth " + ($data_growth_kib|tostring) + " KiB; no Worker, crosvm or Host leak.")}]} ' \
  >"$STAGE/microdroid-soak-gate.json"

mv -- "$STAGE" "$OUTPUT"
COMPLETED=1
trap - EXIT HUP INT TERM
case "$DATA" in /private/tmp/hd-microdroid-soak-data.*) rm -rf -- "$DATA" ;; esac
echo "evidence=$OUTPUT"
echo "gate_report=$OUTPUT/microdroid-soak-gate.json"
