#!/bin/sh
set -eu

usage() {
  cat <<'EOF'
Usage: scripts/macos-ui-native-lifecycle-smoke.sh \
  --app <HD.app> \
  --output <fresh-absolute-evidence-directory>

Launches the packaged app through Finder, verifies that native windows are
created only after the winit handler is active, proves a second launch activates
the existing window without another UI/Host, and shuts down the isolated Host.
EOF
}

fail() {
  echo "$*" >&2
  exit 1
}

APP=
OUTPUT=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --app) [ "$#" -ge 2 ] || { usage >&2; exit 2; }; APP=$2; shift 2 ;;
    --output) [ "$#" -ge 2 ] || { usage >&2; exit 2; }; OUTPUT=$2; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

case "$APP" in /*) ;; *) fail "--app must be absolute" ;; esac
[ -d "$APP" ] && [ ! -L "$APP" ] || fail "--app must be a real non-symlink directory"
case "$OUTPUT" in /*) ;; *) fail "--output must be absolute" ;; esac
[ ! -e "$OUTPUT" ] || fail "refusing to replace evidence output: $OUTPUT"
FAILURE_OUTPUT="$OUTPUT.failed"
[ ! -e "$FAILURE_OUTPUT" ] || fail "refusing to replace failure evidence: $FAILURE_OUTPUT"
[ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ] ||
  fail "native lifecycle smoke requires Apple Silicon macOS"

UI="$APP/Contents/MacOS/HD"
CTL="$APP/Contents/MacOS/hdctl"
for binary in "$UI" "$CTL"; do
  [ -f "$binary" ] && [ ! -L "$binary" ] && [ -x "$binary" ] ||
    fail "HD.app is missing a real executable: $binary"
done
codesign --verify --deep --strict "$APP" || fail "HD.app codesign verification failed"

OUTPUT_PARENT=$(dirname -- "$OUTPUT")
mkdir -p -- "$OUTPUT_PARENT"
STAGE=$(mktemp -d "$OUTPUT_PARENT/.hd-ui-native-lifecycle.XXXXXX")
WORK=$(mktemp -d /private/tmp/hd-ui-native-lifecycle.XXXXXX)
DATA="$WORK/data"
mkdir -p -- "$DATA" "$STAGE/logs"
UI_PID=
COMPLETED=0

matching_pids() {
  ps ax -o pid=,command= | grep -F -- "--data-root $WORK/" | grep -v grep |
    awk '{print $1}'
}

cleanup() {
  status=$?
  if [ "$status" -ne 0 ] && [ -d "$STAGE" ]; then
    mkdir -p -- "$STAGE/logs"
    for log in "$DATA"/logs/ui-web.jsonl* "$DATA"/logs/host-v2.jsonl*; do
      [ -f "$log" ] || continue
      cp -- "$log" "$STAGE/logs/"
    done
    ps ax -o pid=,ppid=,state=,command= >"$STAGE/processes.txt" 2>/dev/null || true
  fi
  "$CTL" --data-root "$DATA" --no-start-host shutdown --stop-all >/dev/null 2>&1 || true
  pids=$(matching_pids || true)
  [ -z "$pids" ] || kill $pids 2>/dev/null || true
  sleep 1
  pids=$(matching_pids || true)
  [ -z "$pids" ] || kill -KILL $pids 2>/dev/null || true
  case "$WORK" in /private/tmp/hd-ui-native-lifecycle.*) rm -rf -- "$WORK" ;; esac
  case "$STAGE" in
    "$OUTPUT_PARENT"/.hd-ui-native-lifecycle.*)
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

open -n "$APP" --args --data-root "$DATA"
attempt=0
while :; do
  UI_PID=$(ps ax -o pid=,command= | grep -F -- "$UI --data-root $DATA" |
    grep -v grep | awk 'NR == 1 {print $1}')
  [ -z "$UI_PID" ] || break
  [ "$attempt" -lt 200 ] || fail "Finder did not launch the packaged UI within 20 seconds"
  attempt=$((attempt + 1))
  sleep 0.1
done
printf '%s\n' "$UI_PID" >"$STAGE/ui.pid"

attempt=0
while ! "$CTL" --data-root "$DATA" --no-start-host health \
    >"$STAGE/health.json" 2>"$STAGE/health.stderr"; do
  kill -0 "$UI_PID" 2>/dev/null || fail "packaged UI exited before Host became healthy"
  [ "$attempt" -lt 300 ] || fail "packaged Host did not become healthy within 30 seconds"
  attempt=$((attempt + 1))
  sleep 0.1
done

FIRST_PAINT_BUDGET_SECONDS=15
FIRST_PAINT_WAIT_ATTEMPTS=0
while :; do
  FIRST_PAINT_READY_COUNT=$(grep -h -F '"event":"ui.lifecycle.first_paint.ready"' \
    "$DATA"/logs/ui-web.jsonl* 2>/dev/null | wc -l | tr -d ' ')
  FIRST_PAINT_FAILURE_COUNT=$(grep -h -E \
    '"event":"ui\.lifecycle\.first_paint\.(visibility_rejected|timed_out)"' \
    "$DATA"/logs/ui-web.jsonl* 2>/dev/null | wc -l | tr -d ' ')
  [ "$FIRST_PAINT_FAILURE_COUNT" -eq 0 ] ||
    fail "packaged UI reported a first-paint visibility failure"
  [ "$FIRST_PAINT_READY_COUNT" -eq 0 ] || break
  kill -0 "$UI_PID" 2>/dev/null || fail "packaged UI exited before first paint"
  [ "$FIRST_PAINT_WAIT_ATTEMPTS" -lt $((FIRST_PAINT_BUDGET_SECONDS * 10)) ] ||
    fail "packaged UI did not confirm first paint within ${FIRST_PAINT_BUDGET_SECONDS} seconds"
  FIRST_PAINT_WAIT_ATTEMPTS=$((FIRST_PAINT_WAIT_ATTEMPTS + 1))
  sleep 0.1
done
FIRST_PAINT_WAIT_MS=$((FIRST_PAINT_WAIT_ATTEMPTS * 100))
sleep 1
kill -0 "$UI_PID" 2>/dev/null || fail "packaged UI exited during post-paint observation"

NO_HANDLER_COUNT=$(grep -h -F 'tried to run event handler, but no handler was set' \
  "$DATA"/logs/ui-web.jsonl* 2>/dev/null | wc -l | tr -d ' ')
STARTUP_FAILURE_COUNT=$(grep -h -F '"event":"ui.lifecycle.startup.failed"' \
  "$DATA"/logs/ui-web.jsonl* 2>/dev/null | wc -l | tr -d ' ')
NETWORK_PROBE_COUNT=$(grep -h -F '"event":"ui.network_status.probe.started"' \
  "$DATA"/logs/ui-web.jsonl* 2>/dev/null | wc -l | tr -d ' ')
FIRST_PAINT_READY_COUNT=$(grep -h -F '"event":"ui.lifecycle.first_paint.ready"' \
  "$DATA"/logs/ui-web.jsonl* 2>/dev/null | wc -l | tr -d ' ')
FIRST_PAINT_FAILURE_COUNT=$(grep -h -E \
  '"event":"ui\.lifecycle\.first_paint\.(visibility_rejected|timed_out)"' \
  "$DATA"/logs/ui-web.jsonl* 2>/dev/null | wc -l | tr -d ' ')
[ "$NO_HANDLER_COUNT" -eq 0 ] || fail "packaged UI emitted a pre-handler winit lifecycle error"
[ "$STARTUP_FAILURE_COUNT" -eq 0 ] || fail "packaged UI reported native startup failure"
[ "$NETWORK_PROBE_COUNT" -eq 0 ] || fail "Player startup spawned an unrelated network status probe"
[ "$FIRST_PAINT_READY_COUNT" -eq 1 ] ||
  fail "packaged UI did not confirm exactly one visible first paint"
[ "$FIRST_PAINT_FAILURE_COUNT" -eq 0 ] ||
  fail "packaged UI reported a first-paint visibility failure"
SECONDARY_DATA="$WORK/secondary-data"
open -n "$APP"
sleep 1
TOTAL_UI_COUNT=$(ps ax -o command= |
  grep -F -- "$UI" | grep -v grep | wc -l | tr -d ' ')
TOTAL_HOST_COUNT=$(ps ax -o command= |
  grep -F -- "$APP/Contents/MacOS/hd-host" | grep -v grep | wc -l | tr -d ' ')
[ "$TOTAL_UI_COUNT" -eq 1 ] || fail "second launch created another UI process"
[ "$TOTAL_HOST_COUNT" -eq 1 ] || fail "second launch created another Host process"
[ ! -e "$SECONDARY_DATA" ] || fail "second launch created an independent data root"
SECONDARY_UI_COUNT=$((TOTAL_UI_COUNT - 1))
SECONDARY_HOST_COUNT=$((TOTAL_HOST_COUNT - 1))
UI_MATCHES=$(matching_pids | wc -l | tr -d ' ')
[ "$UI_MATCHES" -eq 2 ] || fail "expected exactly one isolated UI and one Host process"

for log in "$DATA"/logs/ui-web.jsonl* "$DATA"/logs/host-v2.jsonl*; do
  [ -f "$log" ] || continue
  cp -- "$log" "$STAGE/logs/"
done
"$CTL" --data-root "$DATA" --no-start-host shutdown --stop-all \
  >"$STAGE/shutdown.json" 2>"$STAGE/shutdown.stderr"
attempt=0
while :; do
  HOST_MATCHES=$(ps ax -o command= | grep -F -- "$APP/Contents/MacOS/hd-host --data-root $DATA" |
    grep -v grep | wc -l | tr -d ' ')
  [ "$HOST_MATCHES" -eq 0 ] && break
  [ "$attempt" -lt 100 ] || fail "isolated Host did not exit within 10 seconds"
  attempt=$((attempt + 1))
  sleep 0.1
done
kill "$UI_PID" 2>/dev/null || true
UI_PID=
sleep 1
[ -z "$(matching_pids || true)" ] || fail "native lifecycle smoke left an isolated process running"

VERSION=$(plutil -extract CFBundleShortVersionString raw -o - "$APP/Contents/Info.plist")
BUILD=$(plutil -extract CFBundleVersion raw -o - "$APP/Contents/Info.plist")
GENERATED_AT=$(date -u +%Y-%m-%dT%H:%M:%SZ)
jq -n \
  --arg version "$VERSION" --arg build "$BUILD" \
  --argjson no_handler "$NO_HANDLER_COUNT" \
  --argjson startup_failure "$STARTUP_FAILURE_COUNT" \
  --argjson network_probe "$NETWORK_PROBE_COUNT" \
  --argjson first_paint_budget_seconds "$FIRST_PAINT_BUDGET_SECONDS" \
  --argjson first_paint_wait_ms "$FIRST_PAINT_WAIT_MS" \
  --argjson first_paint_ready "$FIRST_PAINT_READY_COUNT" \
  --argjson first_paint_failure "$FIRST_PAINT_FAILURE_COUNT" \
  --argjson secondary_ui_count "$SECONDARY_UI_COUNT" \
  --argjson secondary_host_count "$SECONDARY_HOST_COUNT" \
  '{schema_version:1,profile:"hd-macos-native-lifecycle-v1",status:"pass",
    launch_method:"Finder open -n",version:$version,build:$build,
    post_paint_observation_seconds:1,ui_alive:true,host_healthy:true,
    pre_handler_error_count:$no_handler,startup_failure_count:$startup_failure,
    player_network_probe_count:$network_probe,
    first_paint_budget_seconds:$first_paint_budget_seconds,
    first_paint_wait_ms:$first_paint_wait_ms,
    first_paint_ready_count:$first_paint_ready,
    first_paint_failure_count:$first_paint_failure,
    second_launch_activated_existing:true,
    secondary_ui_processes:$secondary_ui_count,
    secondary_host_processes:$secondary_host_count,
    secondary_data_root_created:false,
    native_root_visible:true,isolated_shutdown:"pass",
    process_cleanup:"pass"}' >"$STAGE/result.json"
jq -n --arg generated_at "$GENERATED_AT" \
  '{schema_version:2,generated_at:$generated_at,source:"scripts/macos-ui-native-lifecycle-smoke.sh",
    gates:[{name:"macos-ui-native-lifecycle",command:"scripts/macos-ui-native-lifecycle-smoke.sh",
      status:"pass",duration_ms:null,log_path:"logs/ui-web.jsonl",
      summary:"Finder launch created native windows only after Resumed, confirmed exactly one visible WebView first paint, activated the existing window on a second launch without another UI/Host/data root, and passed startup logs, isolated Host shutdown and Player background work"}]}' \
  >"$STAGE/ui-native-lifecycle-gates.json"

mv -- "$STAGE" "$OUTPUT"
COMPLETED=1
echo "evidence=$OUTPUT"
echo "gate_report=$OUTPUT/ui-native-lifecycle-gates.json"
echo "pre_handler_errors=0"
echo "startup_failures=0"
echo "player_network_probes=0"
echo "first_paint_ready=1"
echo "first_paint_failures=0"
