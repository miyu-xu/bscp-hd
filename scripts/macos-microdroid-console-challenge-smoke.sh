#!/bin/sh
set -eu
umask 077

usage() {
  cat <<'EOF'
Usage: scripts/macos-microdroid-console-challenge-smoke.sh \
  --app <HD.app> \
  --output <fresh-evidence-directory> \
  --payload <v3-signed-trusted-console-challenge-payload.apk> \
  [--development-package]

The trusted Full-debug Payload must read one HD_CONSOLE_CHALLENGE_V1 frame
from its console and write the exact HD_CONSOLE_RESPONSE_V1 frame back. This
runner owns a random data root and never terminates processes outside it.
EOF
}

APP=
OUTPUT=
PAYLOAD=
DEVELOPMENT_PACKAGE=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    --app) APP=$2; shift 2 ;;
    --output) OUTPUT=$2; shift 2 ;;
    --payload) PAYLOAD=$2; shift 2 ;;
    --development-package) DEVELOPMENT_PACKAGE=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

fail() {
  echo "$*" >&2
  exit 1
}

require_abs_file() {
  case "$1" in /*) ;; *) fail "$2 must be absolute" ;; esac
  [ -f "$1" ] && [ ! -L "$1" ] || fail "$2 is not a regular non-symlink file: $1"
}

require_abs_dir() {
  case "$1" in /*) ;; *) fail "$2 must be absolute" ;; esac
  [ -d "$1" ] && [ ! -L "$1" ] || fail "$2 is not a non-symlink directory: $1"
}

for command in codesign jq lsof plutil shasum unzip uuidgen xxd awk grep ps stat; do
  command -v "$command" >/dev/null 2>&1 || fail "required tool is missing: $command"
done
[ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ] ||
  fail "Microdroid console challenge Guest smoke requires Apple Silicon macOS"
require_abs_dir "$APP" --app
require_abs_file "$PAYLOAD" --payload
case "$OUTPUT" in /*) ;; *) fail "--output must be absolute" ;; esac
[ ! -e "$OUTPUT" ] || fail "refusing to replace evidence output: $OUTPUT"
FAILURE_OUTPUT="$OUTPUT.failed"
[ ! -e "$FAILURE_OUTPUT" ] || fail "refusing to replace failure evidence: $FAILURE_OUTPUT"

CTL="$APP/Contents/MacOS/hdctl"
PAYLOAD_MANIFEST="$APP/Contents/Resources/products/microdroid/conformance-payload/payload-bundle-v1.plist"
require_abs_file "$CTL" hdctl
require_abs_file "$PAYLOAD_MANIFEST" payload-bundle-v1.plist
codesign --verify --deep --strict "$APP" || fail "HD.app code signature verification failed"
CHANNEL=$(plutil -extract channel raw -o - "$PAYLOAD_MANIFEST")
case "$CHANNEL:$DEVELOPMENT_PACKAGE" in
  development:1|release:0) ;;
  development:0) fail "development HD.app requires --development-package" ;;
  release:1) fail "release HD.app rejects --development-package" ;;
  *) fail "unsupported HD.app channel: $CHANNEL" ;;
esac
unzip -p "$PAYLOAD" assets/vm_config.json >/dev/null ||
  fail "trusted Payload has no assets/vm_config.json"

output_parent=$(dirname -- "$OUTPUT")
mkdir -p "$output_parent"
OUTPUT_STAGE=$(mktemp -d "$output_parent/.hd-microdroid-console-evidence.XXXXXX")
DATA_ROOT=$(mktemp -d /private/tmp/hd-microdroid-console-data.XXXXXX)
COMPLETED=0

own_process_ids() {
  HD_PROCESS_ROOT="$DATA_ROOT" \
    awk 'index($0, ENVIRON["HD_PROCESS_ROOT"]) &&
         $2 ~ /(^|\/)(hd-host|hd-worker|crosvm|vm|virtmgr)$/ { print $1 }' <<EOF
$(ps -axo pid=,comm=,command=)
EOF
}

run_ctl() {
  stdout=$1
  stderr=$2
  shift 2
  if [ "$DEVELOPMENT_PACKAGE" -eq 1 ]; then
    HD_MICRODROID_DEV_BYPASS=1 "$CTL" --data-root "$DATA_ROOT" "$@" \
      >"$stdout" 2>"$stderr"
  else
    "$CTL" --data-root "$DATA_ROOT" "$@" >"$stdout" 2>"$stderr"
  fi
}

cleanup() {
  exit_code=$?
  if [ -x "$CTL" ] && [ -d "$DATA_ROOT" ]; then
    if [ "$DEVELOPMENT_PACKAGE" -eq 1 ]; then
      HD_MICRODROID_DEV_BYPASS=1 "$CTL" --data-root "$DATA_ROOT" \
        shutdown --stop-all >/dev/null 2>&1 || true
    else
      "$CTL" --data-root "$DATA_ROOT" shutdown --stop-all >/dev/null 2>&1 || true
    fi
  fi
  pids=$(own_process_ids 2>/dev/null || true)
  [ -z "$pids" ] || kill $pids 2>/dev/null || true
  sleep 1
  pids=$(own_process_ids 2>/dev/null || true)
  [ -z "$pids" ] || kill -KILL $pids 2>/dev/null || true
  case "$DATA_ROOT" in /private/tmp/hd-microdroid-console-data.*) rm -rf -- "$DATA_ROOT" ;; esac
  case "$OUTPUT_STAGE" in
    "$output_parent"/.hd-microdroid-console-evidence.*)
      if [ "$COMPLETED" -eq 0 ] && [ -d "$OUTPUT_STAGE" ]; then
        printf '%s\n' "$exit_code" >"$OUTPUT_STAGE/exit.code"
        mv "$OUTPUT_STAGE" "$FAILURE_OUTPUT"
        echo "failure_evidence=$FAILURE_OUTPUT" >&2
      else
        rm -rf -- "$OUTPUT_STAGE"
      fi
      ;;
  esac
}
trap cleanup EXIT HUP INT TERM

expect_ctl_failure() {
  expected=$1
  stdout=$2
  stderr=$3
  shift 3
  if run_ctl "$stdout" "$stderr" "$@"; then
    fail "command unexpectedly succeeded: $*"
  fi
  if ! grep -Fq "$expected" "$stderr" && ! grep -Fq "$expected" "$stdout"; then
    fail "command failure omitted '$expected' (evidence: $stderr)"
  fi
}

wait_for_state() {
  instance_id=$1
  expected=$2
  prefix=$3
  attempt=0
  while [ "$attempt" -lt 60 ]; do
    run_ctl "$OUTPUT_STAGE/$prefix-$attempt.json" "$OUTPUT_STAGE/$prefix-$attempt.stderr" \
      show "$instance_id"
    if [ "$(jq -r '.status.observed' "$OUTPUT_STAGE/$prefix-$attempt.json")" = "$expected" ]; then
      cp "$OUTPUT_STAGE/$prefix-$attempt.json" "$OUTPUT_STAGE/$prefix.json"
      return
    fi
    sleep 1
    attempt=$((attempt + 1))
  done
  fail "instance did not reach $expected within 60 seconds"
}

run_ctl "$OUTPUT_STAGE/health.json" "$OUTPUT_STAGE/health.stderr" health
run_ctl "$OUTPUT_STAGE/upload.json" "$OUTPUT_STAGE/upload.stderr" \
  upload --microdroid-payload "$PAYLOAD"
UPLOAD_ID=$(jq -er '.id' "$OUTPUT_STAGE/upload.json")
UPLOAD_SHA=$(jq -er '.sha256 | select(test("^[0-9a-f]{64}$"))' "$OUTPUT_STAGE/upload.json")
INSTANCE_ID=$(uuidgen | tr '[:upper:]' '[:lower:]')
cat >"$OUTPUT_STAGE/spec.json" <<EOF
{
  "schema_version": 2,
  "id": "$INSTANCE_ID",
  "name": "Microdroid Console Challenge Acceptance",
  "guest_kind": "microdroid",
  "microdroid": {
    "debug_level": "full",
    "cpu_topology": "one_cpu",
    "payload": {
      "kind": "uploaded",
      "upload_id": "$UPLOAD_ID",
      "sha256": "$UPLOAD_SHA",
      "config_path": "assets/vm_config.json"
    },
    "payload_extra_apk_count": null,
    "extra_apks": [],
    "encrypted_storage_mib": null
  },
  "cpu_count": 1,
  "memory_mib": 512,
  "display": { "width": 1080, "height": 1920, "dpi": 420, "refresh_rate_hz": 60, "orientation": "portrait", "vsync": "on", "show_host_fps": false },
  "adb": { "mode": "loopback", "host_port": null, "executable": null },
  "artifacts": null,
  "boot": { "kernel_log_level": 4, "panic_timeout_seconds": 5, "boot_animation": true },
  "devices": { "bluetooth": false, "nfc": false, "uwb": false, "modem": false, "gnss": false, "sensors": false, "network": false, "audio": false, "camera": false, "power": false },
  "restart_policy": "never",
  "labels": { "purpose": "microdroid-console-challenge-acceptance" }
}
EOF

run_ctl "$OUTPUT_STAGE/create.json" "$OUTPUT_STAGE/create.stderr" create --spec "$OUTPUT_STAGE/spec.json"
run_ctl "$OUTPUT_STAGE/capabilities.json" "$OUTPUT_STAGE/capabilities.stderr" capabilities "$INSTANCE_ID"
jq -e '.probes[] | select(.id == "microdroid.profile" and .status == "supported" and .properties.console_challenge == "typed-nonce-v1")' \
  "$OUTPUT_STAGE/capabilities.json" >/dev/null || fail "typed console challenge capability was not advertised"
run_ctl "$OUTPUT_STAGE/start.json" "$OUTPUT_STAGE/start.stderr" start "$INSTANCE_ID"
wait_for_state "$INSTANCE_ID" ready ready
RUN_ID=$(jq -er '.active_run_id' "$OUTPUT_STAGE/ready.json")
RUN_DIR="$DATA_ROOT/runs/$INSTANCE_ID/$RUN_ID"
MANIFEST="$RUN_DIR/manifest.json"
FIFO="$RUN_DIR/microdroid-console-in.fifo"
AUDIT="$RUN_DIR/microdroid-console-challenge.json"
CONSOLE="$RUN_DIR/microdroid-console.txt"
require_abs_file "$MANIFEST" manifest
[ -p "$FIFO" ] || fail "Ready run has no console input FIFO"
[ "$(stat -f '%Lp' "$FIFO")" = 600 ] || fail "console input FIFO is not 0600"
jq -e --arg fifo "$FIFO" '
  .launch.arguments as $args |
  any(range(0; ($args | length) - 1); $args[.] == "--console-in" and $args[. + 1] == $fifo)
' "$MANIFEST" >/dev/null || fail "run manifest did not bind the owned console FIFO"

NIL_ID=00000000-0000-0000-0000-000000000000
expect_ctl_failure action_invalid "$OUTPUT_STAGE/nil.json" "$OUTPUT_STAGE/nil.stderr" \
  action "$INSTANCE_ID" microdroid-console-challenge --challenge-id "$NIL_ID" --confirm
UNCONFIRMED_ID=$(uuidgen | tr '[:upper:]' '[:lower:]')
expect_ctl_failure action_invalid "$OUTPUT_STAGE/unconfirmed.json" "$OUTPUT_STAGE/unconfirmed.stderr" \
  action "$INSTANCE_ID" microdroid-console-challenge --challenge-id "$UNCONFIRMED_ID"
CHALLENGE_ID=$(uuidgen | tr '[:upper:]' '[:lower:]')
run_ctl "$OUTPUT_STAGE/challenge.json" "$OUTPUT_STAGE/challenge.stderr" \
  action "$INSTANCE_ID" microdroid-console-challenge \
  --challenge-id "$CHALLENGE_ID" --confirm
require_abs_file "$AUDIT" challenge-audit
require_abs_file "$CONSOLE" console
jq -e --arg id "$CHALLENGE_ID" '
  .schema_version == 2 and .challenge_id == $id and
  .response_verified == true and .request_size_bytes <= 160 and
  (.nonce_sha256 | test("^[0-9a-f]{64}$")) and .error_code == null
' "$AUDIT" >/dev/null || fail "challenge audit did not prove the exact Guest response"
RESPONSE_LINE=$(grep -E "^HD_CONSOLE_RESPONSE_V1 $CHALLENGE_ID [0-9a-f]{64}$" "$CONSOLE" | tail -1)
[ -n "$RESPONSE_LINE" ] || fail "Guest console omitted the exact challenge response"
NONCE_HEX=$(printf '%s\n' "$RESPONSE_LINE" | awk '{print $3}')
NONCE_SHA=$(printf '%s' "$NONCE_HEX" | xxd -r -p | shasum -a 256 | awk '{print $1}')
[ "$NONCE_SHA" = "$(jq -r '.nonce_sha256' "$AUDIT")" ] ||
  fail "Guest response nonce does not match the Host audit"
expect_ctl_failure microdroid_console_challenge_already_sent \
  "$OUTPUT_STAGE/second.json" "$OUTPUT_STAGE/second.stderr" \
  action "$INSTANCE_ID" microdroid-console-challenge \
  --challenge-id "$(uuidgen | tr '[:upper:]' '[:lower:]')" --confirm

run_ctl "$OUTPUT_STAGE/stop.json" "$OUTPUT_STAGE/stop.stderr" \
  stop "$INSTANCE_ID" --force --graceful-timeout-ms 5000
wait_for_state "$INSTANCE_ID" stopped stopped
jq -e '.active_run_id == null' "$OUTPUT_STAGE/stopped.json" >/dev/null ||
  fail "stopped instance retained an active run"
[ ! -e "$FIFO" ] || fail "stopped run retained the console FIFO"
if lsof +L1 -Fn 2>/dev/null | grep -F "$FIFO" >/dev/null; then
  fail "stopped run retained an open descriptor for the deleted console FIFO"
fi
require_abs_file "$RUN_DIR/result.json" result
run_ctl "$OUTPUT_STAGE/delete.json" "$OUTPUT_STAGE/delete.stderr" delete "$INSTANCE_ID"
run_ctl "$OUTPUT_STAGE/shutdown.json" "$OUTPUT_STAGE/shutdown.stderr" shutdown
sleep 1
[ -z "$(own_process_ids)" ] || fail "console challenge smoke left a process bound to its data root"

VERSION=$(plutil -extract CFBundleShortVersionString raw -o - "$APP/Contents/Info.plist")
BUILD=$(plutil -extract CFBundleVersion raw -o - "$APP/Contents/Info.plist")
cat >"$OUTPUT_STAGE/result.json" <<EOF
{
  "schema_version": 1,
  "gate": "macos-microdroid-console-challenge-real-guest",
  "status": "pass",
  "version": "$VERSION",
  "build": "$BUILD",
  "channel": "$CHANNEL",
  "development_bypass": $([ "$DEVELOPMENT_PACKAGE" -eq 1 ] && echo true || echo false),
  "instance_id": "$INSTANCE_ID",
  "run_id": "$RUN_ID",
  "payload_sha256": "$UPLOAD_SHA",
  "challenge_id": "$CHALLENGE_ID",
  "nonce_sha256": "$NONCE_SHA",
  "request_size_bytes": $(jq -r '.request_size_bytes' "$AUDIT"),
  "guest_response_verified": true,
  "nil_id_rejected": true,
  "explicit_confirmation_enforced": true,
  "one_shot_enforced": true,
  "owner_only_fifo": true,
  "fifo_cleanup": true,
  "deleted_fifo_fd_cleanup": true,
  "process_cleanup": "pass"
}
EOF
chmod 0600 "$OUTPUT_STAGE"/*
mv "$OUTPUT_STAGE" "$OUTPUT"
COMPLETED=1
trap - EXIT HUP INT TERM
case "$DATA_ROOT" in /private/tmp/hd-microdroid-console-data.*) rm -rf -- "$DATA_ROOT" ;; esac
echo "evidence=$OUTPUT"
echo "challenge_id=$CHALLENGE_ID"
echo "nonce_sha256=$NONCE_SHA"
