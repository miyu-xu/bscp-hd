#!/bin/sh
set -eu

usage() {
  echo "Usage: $0 --old-host <previous-hd-host> --target-dir <current-bin-dir> --evidence-dir <new-directory> [--transition upgrade|rollback]" >&2
}

OLD_HOST=
TARGET_DIR=
EVIDENCE_DIR=
TRANSITION=upgrade
while [ "$#" -gt 0 ]; do
  case "$1" in
    --old-host) OLD_HOST=$2; shift 2 ;;
    --target-dir) TARGET_DIR=$2; shift 2 ;;
    --evidence-dir) EVIDENCE_DIR=$2; shift 2 ;;
    --transition) TRANSITION=$2; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage; exit 2 ;;
  esac
done

case "$TRANSITION" in upgrade|rollback) ;; *) echo "unsupported transition: $TRANSITION" >&2; exit 2 ;; esac

if [ -z "$OLD_HOST" ] || [ -z "$TARGET_DIR" ] || [ -z "$EVIDENCE_DIR" ]; then
  usage
  exit 2
fi
case "$OLD_HOST:$TARGET_DIR:$EVIDENCE_DIR" in
  /*:/*:/*) ;;
  *) echo "all paths must be absolute" >&2; exit 2 ;;
esac
if [ "$(uname -s)" != Darwin ] || [ "$(uname -m)" != arm64 ]; then
  echo "Host runtime upgrade smoke requires Apple Silicon macOS" >&2
  exit 2
fi
if [ ! -d "$TARGET_DIR" ] || [ -L "$TARGET_DIR" ]; then
  echo "--target-dir must be a real directory, not a symbolic link" >&2
  exit 2
fi
if [ -e "$EVIDENCE_DIR" ]; then
  echo "refusing to replace existing evidence: $EVIDENCE_DIR" >&2
  exit 2
fi
for binary in "$OLD_HOST" "$TARGET_DIR/hdctl" "$TARGET_DIR/hd-host"; do
  if [ ! -x "$binary" ] || [ -L "$binary" ]; then
    echo "missing or unsafe executable: $binary" >&2
    exit 2
  fi
done

mkdir -p "$EVIDENCE_DIR"
DATA_ROOT="$EVIDENCE_DIR/data"
mkdir -p "$DATA_ROOT"
DESCRIPTOR="$DATA_ROOT/host-runtime-v2.json"
OLD_PID=
NEW_PID=

process_alive() {
  [ -n "$1" ] && kill -0 "$1" 2>/dev/null
}

shutdown_current() {
  "$TARGET_DIR/hdctl" --data-root "$DATA_ROOT" shutdown --stop-all >/dev/null 2>&1 || true
}

cleanup() {
  shutdown_current
  if process_alive "$OLD_PID"; then
    kill "$OLD_PID" 2>/dev/null || true
  fi
  if process_alive "$NEW_PID"; then
    kill "$NEW_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT HUP INT TERM

nohup "$OLD_HOST" --data-root "$DATA_ROOT" \
  >"$EVIDENCE_DIR/old-host.log" 2>&1 </dev/null &
OLD_PID=$!
READY=false
attempt=0
while [ "$attempt" -lt 200 ]; do
  if [ -f "$DESCRIPTOR" ]; then
    READY=true
    break
  fi
  if ! process_alive "$OLD_PID"; then
    echo "previous Host exited before publishing its runtime descriptor" >&2
    exit 1
  fi
  sleep 0.05
  attempt=$((attempt + 1))
done
if [ "$READY" != true ]; then
  echo "previous Host did not become ready" >&2
  exit 1
fi

descriptor_value() {
  sed -n "s/.*\"$1\"[[:space:]]*:[[:space:]]*\"\\{0,1\\}\\([^\",}]*\\)\"\\{0,1\\}.*/\\1/p" "$2" | head -1
}

OLD_DESCRIPTOR_PID=$(descriptor_value pid "$DESCRIPTOR")
if [ "$OLD_DESCRIPTOR_PID" != "$OLD_PID" ]; then
  echo "previous Host descriptor identity mismatch" >&2
  exit 1
fi
OLD_SHA256=$(shasum -a 256 "$OLD_HOST" | awk '{print $1}')
CURRENT_SHA256=$(shasum -a 256 "$TARGET_DIR/hd-host" | awk '{print $1}')
if [ "$OLD_SHA256" = "$CURRENT_SHA256" ]; then
  echo "previous and current Host artifacts are identical" >&2
  exit 2
fi

STARTED_AT=$(date +%s)
"$TARGET_DIR/hdctl" --data-root "$DATA_ROOT" health >"$EVIDENCE_DIR/current-health.json"
FINISHED_AT=$(date +%s)
ELAPSED_SECONDS=$((FINISHED_AT - STARTED_AT))

NEW_PID=$(descriptor_value pid "$EVIDENCE_DIR/current-health.json")
HEALTH_SHA256=$(descriptor_value executable_sha256 "$EVIDENCE_DIR/current-health.json")
DESCRIPTOR_SHA256=$(descriptor_value executable_sha256 "$DESCRIPTOR")
if [ -z "$NEW_PID" ] || [ "$NEW_PID" = "$OLD_PID" ]; then
  echo "current client did not replace the inactive previous Host" >&2
  exit 1
fi
if [ "$HEALTH_SHA256" != "$CURRENT_SHA256" ] ||
    [ "$DESCRIPTOR_SHA256" != "$CURRENT_SHA256" ]; then
  echo "current Host did not publish the packaged executable identity" >&2
  exit 1
fi
if process_alive "$OLD_PID"; then
  echo "previous Host process leaked after inactive takeover" >&2
  exit 1
fi
if [ "$ELAPSED_SECONDS" -gt 5 ]; then
  echo "inactive runtime takeover exceeded the five-second product budget" >&2
  exit 1
fi

shutdown_current
attempt=0
while [ "$attempt" -lt 100 ] && process_alive "$NEW_PID"; do
  sleep 0.05
  attempt=$((attempt + 1))
done
if process_alive "$NEW_PID"; then
  echo "current Host process leaked after smoke cleanup" >&2
  exit 1
fi

GENERATED_AT=$(date -u '+%Y-%m-%dT%H:%M:%SZ')
printf '%s\n' \
  '{' \
  '  "schema_version": 1,' \
  "  \"generated_at\": \"$GENERATED_AT\"," \
  '  "result": "pass",' \
  "  \"transition\": \"$TRANSITION\"," \
  '  "legacy_descriptor_accepted": true,' \
  '  "inactive_takeover": true,' \
  '  "previous_host_exited": true,' \
  '  "current_identity_verified": true,' \
  '  "process_leak": false,' \
  "  \"previous_pid\": $OLD_PID," \
  "  \"current_pid\": $NEW_PID," \
  "  \"previous_sha256\": \"$OLD_SHA256\"," \
  "  \"current_sha256\": \"$CURRENT_SHA256\"," \
  "  \"elapsed_seconds\": $ELAPSED_SECONDS" \
  '}' >"$EVIDENCE_DIR/result.json"

if [ "$TRANSITION" = rollback ]; then
  GATE_NAME=host-runtime-inactive-rollback
  GATE_FILE=host-runtime-rollback-gate.json
else
  GATE_NAME=host-runtime-inactive-takeover
  GATE_FILE=host-runtime-upgrade-gate.json
fi
printf '%s\n' \
  '{' \
  '  "schema_version": 2,' \
  "  \"generated_at\": \"$GENERATED_AT\"," \
  '  "source": "scripts/macos-host-runtime-upgrade-smoke.sh",' \
  '  "gates": [' \
  '    {' \
  "      \"name\": \"$GATE_NAME\"," \
  '      "command": "macos-host-runtime-upgrade-smoke.sh --old-host <previous> --target-dir <current> [--transition upgrade|rollback]",' \
  '      "status": "pass",' \
  "      \"duration_ms\": $((ELAPSED_SECONDS * 1000))," \
  "      \"log_path\": \"$EVIDENCE_DIR\"," \
  "      \"summary\": \"The $TRANSITION client accepted the runtime descriptor, retired the inactive previous Host, verified the requested Host digest, and left no process leak.\"" \
  '    }' \
  '  ]' \
  '}' >"$EVIDENCE_DIR/$GATE_FILE"

trap - EXIT HUP INT TERM
echo "result=pass"
echo "previous_pid=$OLD_PID"
echo "current_pid=$NEW_PID"
echo "elapsed_seconds=$ELAPSED_SECONDS"
echo "transition=$TRANSITION"
echo "evidence=$EVIDENCE_DIR"
