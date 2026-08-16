#!/bin/sh
set -eu

usage() {
  cat >&2 <<'EOF'
Usage: macos-host-runtime-active-upgrade-smoke.sh \
  --old-app <previous-HD.app> \
  --target-dir <current-bin-dir> \
  --evidence-dir <new-directory> \
  [--transition upgrade|rollback] \
  [--development-package]
EOF
}

OLD_APP=
TARGET_DIR=
EVIDENCE_DIR=
DEVELOPMENT_PACKAGE=0
TRANSITION=upgrade
while [ "$#" -gt 0 ]; do
  case "$1" in
    --old-app) OLD_APP=$2; shift 2 ;;
    --target-dir) TARGET_DIR=$2; shift 2 ;;
    --evidence-dir) EVIDENCE_DIR=$2; shift 2 ;;
    --transition) TRANSITION=$2; shift 2 ;;
    --development-package) DEVELOPMENT_PACKAGE=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage; exit 2 ;;
  esac
done

case "$TRANSITION" in upgrade|rollback) ;; *) echo "unsupported transition: $TRANSITION" >&2; exit 2 ;; esac

if [ -z "$OLD_APP" ] || [ -z "$TARGET_DIR" ] || [ -z "$EVIDENCE_DIR" ]; then
  usage
  exit 2
fi
case "$OLD_APP:$TARGET_DIR:$EVIDENCE_DIR" in
  /*:/*:/*) ;;
  *) echo "all paths must be absolute" >&2; exit 2 ;;
esac
if [ "$(uname -s)" != Darwin ] || [ "$(uname -m)" != arm64 ]; then
  echo "active runtime upgrade smoke requires Apple Silicon macOS" >&2
  exit 2
fi
if [ ! -d "$OLD_APP" ] || [ -L "$OLD_APP" ]; then
  echo "--old-app must be a real directory, not a symbolic link" >&2
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

OLD_CTL="$OLD_APP/Contents/MacOS/hdctl"
OLD_HOST="$OLD_APP/Contents/MacOS/hd-host"
OLD_WORKER="$OLD_APP/Contents/MacOS/hd-worker"
NEW_CTL="$TARGET_DIR/hdctl"
NEW_HOST="$TARGET_DIR/hd-host"
for binary in "$OLD_CTL" "$OLD_HOST" "$OLD_WORKER" "$NEW_CTL" "$NEW_HOST"; do
  if [ ! -x "$binary" ] || [ -L "$binary" ]; then
    echo "missing or unsafe executable: $binary" >&2
    exit 2
  fi
done
PREVIOUS_SHA256=$(shasum -a 256 "$OLD_HOST" | awk '{print $1}')
CURRENT_SHA256=$(shasum -a 256 "$NEW_HOST" | awk '{print $1}')
if [ "$PREVIOUS_SHA256" = "$CURRENT_SHA256" ]; then
  echo "previous and current Host artifacts are identical" >&2
  exit 2
fi

mkdir -p "$EVIDENCE_DIR/data"
DATA_ROOT="$EVIDENCE_DIR/data"
INSTANCE_ID=$(uuidgen | tr '[:upper:]' '[:lower:]')
OLD_PID=
WORKER_PID=
NEW_PID=

run_old() {
  if [ "$DEVELOPMENT_PACKAGE" -eq 1 ]; then
    HD_MICRODROID_DEV_BYPASS=1 "$OLD_CTL" --data-root "$DATA_ROOT" "$@"
  else
    "$OLD_CTL" --data-root "$DATA_ROOT" "$@"
  fi
}

cleanup() {
  run_old shutdown --stop-all >/dev/null 2>&1 || true
  "$NEW_CTL" --data-root "$DATA_ROOT" shutdown --stop-all >/dev/null 2>&1 || true
  for pid in "$WORKER_PID" "$OLD_PID" "$NEW_PID"; do
    [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
  done
}
trap cleanup EXIT HUP INT TERM
STARTED_AT=$(date +%s)

cat >"$EVIDENCE_DIR/spec.json" <<EOF
{
  "schema_version": 2,
  "id": "$INSTANCE_ID",
  "name": "Runtime Upgrade Active Guest",
  "guest_kind": "microdroid",
  "microdroid": {
    "debug_level": "full",
    "payload": { "kind": "empty" },
    "encrypted_storage_mib": 64
  },
  "cpu_count": 1,
  "memory_mib": 512,
  "display": {
    "width": 1080,
    "height": 1920,
    "dpi": 420,
    "refresh_rate_hz": 60,
    "orientation": "portrait",
    "vsync": "on",
    "show_host_fps": false
  },
  "adb": { "mode": "disabled", "host_port": null, "executable": null },
  "artifacts": null,
  "boot": {
    "kernel_log_level": 4,
    "panic_timeout_seconds": 5,
    "boot_animation": true
  },
  "devices": {
    "bluetooth": false,
    "nfc": false,
    "uwb": false,
    "modem": false,
    "gnss": false,
    "sensors": false,
    "network": false,
    "audio": false,
    "camera": false,
    "power": false
  },
  "restart_policy": "never",
  "labels": { "purpose": "runtime-active-upgrade-smoke" }
}
EOF

run_old health >"$EVIDENCE_DIR/previous-health.json"
run_old create --spec "$EVIDENCE_DIR/spec.json" >"$EVIDENCE_DIR/create.json"
run_old start "$INSTANCE_ID" >"$EVIDENCE_DIR/start.json"
run_old show "$INSTANCE_ID" >"$EVIDENCE_DIR/ready-before.json"
grep -q '"state": "succeeded"' "$EVIDENCE_DIR/start.json"
grep -q '"observed": "ready"' "$EVIDENCE_DIR/ready-before.json"

OLD_PID=$(sed -n 's/^[[:space:]]*"pid": \([0-9]*\).*/\1/p' \
  "$EVIDENCE_DIR/previous-health.json" | head -1)
WORKER_PID=$(sed -n '/"worker": {/,/}/s/^[[:space:]]*"pid": \([0-9]*\).*/\1/p' \
  "$EVIDENCE_DIR/ready-before.json" | head -1)
[ -n "$OLD_PID" ] && [ -n "$WORKER_PID" ] || {
  echo "previous Host did not publish Host/Worker process identities" >&2
  exit 1
}
kill -0 "$OLD_PID"
kill -0 "$WORKER_PID"

"$NEW_CTL" --data-root "$DATA_ROOT" health >"$EVIDENCE_DIR/deferred-health.json"
DEFERRED_PID=$(sed -n 's/^[[:space:]]*"pid": \([0-9]*\).*/\1/p' \
  "$EVIDENCE_DIR/deferred-health.json" | head -1)
if [ "$DEFERRED_PID" != "$OLD_PID" ]; then
  echo "current client interrupted the active previous Host" >&2
  exit 1
fi
"$NEW_CTL" --data-root "$DATA_ROOT" show "$INSTANCE_ID" \
  >"$EVIDENCE_DIR/ready-after.json"
grep -q '"observed": "ready"' "$EVIDENCE_DIR/ready-after.json"
kill -0 "$OLD_PID"
kill -0 "$WORKER_PID"

run_old shutdown --stop-all >"$EVIDENCE_DIR/previous-shutdown.json"
attempt=0
while [ "$attempt" -lt 100 ] && kill -0 "$OLD_PID" 2>/dev/null; do
  sleep 0.05
  attempt=$((attempt + 1))
done
if kill -0 "$OLD_PID" 2>/dev/null; then
  echo "previous Host leaked after explicit stop-all" >&2
  exit 1
fi

"$NEW_CTL" --data-root "$DATA_ROOT" health >"$EVIDENCE_DIR/current-health.json"
NEW_PID=$(sed -n 's/^[[:space:]]*"pid": \([0-9]*\).*/\1/p' \
  "$EVIDENCE_DIR/current-health.json" | head -1)
HEALTH_SHA256=$(sed -n \
  's/^[[:space:]]*"executable_sha256": "\([0-9a-f]*\)".*/\1/p' \
  "$EVIDENCE_DIR/current-health.json" | head -1)
if [ -z "$NEW_PID" ] || [ "$NEW_PID" = "$OLD_PID" ] ||
    [ "$HEALTH_SHA256" != "$CURRENT_SHA256" ]; then
  echo "runtime upgrade did not complete after the active Guest stopped" >&2
  exit 1
fi

"$NEW_CTL" --data-root "$DATA_ROOT" shutdown --stop-all >/dev/null
attempt=0
while [ "$attempt" -lt 100 ] && kill -0 "$NEW_PID" 2>/dev/null; do
  sleep 0.05
  attempt=$((attempt + 1))
done
if kill -0 "$NEW_PID" 2>/dev/null; then
  echo "current Host leaked after smoke cleanup" >&2
  exit 1
fi

GENERATED_AT=$(date -u '+%Y-%m-%dT%H:%M:%SZ')
FINISHED_AT=$(date +%s)
ELAPSED_SECONDS=$((FINISHED_AT - STARTED_AT))
if [ "$TRANSITION" = upgrade ]; then
  LEGACY_UPGRADE_DEFERRED=true
  GATE_NAME=host-runtime-active-deferral
  GATE_FILE=host-runtime-active-upgrade-gate.json
else
  LEGACY_UPGRADE_DEFERRED=false
  GATE_NAME=host-runtime-active-rollback
  GATE_FILE=host-runtime-active-rollback-gate.json
fi
printf '%s\n' \
  '{' \
  '  "schema_version": 1,' \
  "  \"generated_at\": \"$GENERATED_AT\"," \
  '  "result": "pass",' \
  "  \"transition\": \"$TRANSITION\"," \
  '  "guest_kind": "microdroid",' \
  '  "active_runtime_transition_deferred": true,' \
  "  \"active_runtime_upgrade_deferred\": $LEGACY_UPGRADE_DEFERRED," \
  '  "active_host_preserved": true,' \
  '  "active_worker_preserved": true,' \
  '  "guest_ready_before": true,' \
  '  "guest_ready_after": true,' \
  '  "upgrade_completed_after_stop": true,' \
  '  "current_identity_verified": true,' \
  '  "process_leak": false,' \
  "  \"previous_host_pid\": $OLD_PID," \
  "  \"previous_worker_pid\": $WORKER_PID," \
  "  \"current_host_pid\": $NEW_PID," \
  "  \"previous_sha256\": \"$PREVIOUS_SHA256\"," \
  "  \"current_sha256\": \"$CURRENT_SHA256\"" \
  '}' >"$EVIDENCE_DIR/result.json"

printf '%s\n' \
  '{' \
  '  "schema_version": 2,' \
  "  \"generated_at\": \"$GENERATED_AT\"," \
  '  "source": "scripts/macos-host-runtime-active-upgrade-smoke.sh",' \
  '  "gates": [' \
  '    {' \
  "      \"name\": \"$GATE_NAME\"," \
  '      "command": "macos-host-runtime-active-upgrade-smoke.sh --old-app <previous> --target-dir <current> [--transition upgrade|rollback]",' \
  '      "status": "pass",' \
  "      \"duration_ms\": $((ELAPSED_SECONDS * 1000))," \
  "      \"log_path\": \"$EVIDENCE_DIR\"," \
  "      \"summary\": \"Distinct previous/requested Host digests were enforced; the $TRANSITION client preserved the previous Host, Worker, and Ready Microdroid Guest while active, then completed the digest-bound runtime transition after explicit stop-all.\"" \
  '    }' \
  '  ]' \
  '}' >"$EVIDENCE_DIR/$GATE_FILE"

trap - EXIT HUP INT TERM
echo "result=pass"
echo "previous_host_pid=$OLD_PID"
echo "previous_worker_pid=$WORKER_PID"
echo "current_host_pid=$NEW_PID"
echo "transition=$TRANSITION"
echo "evidence=$EVIDENCE_DIR"
