#!/bin/sh
set -eu
umask 077

usage() {
  cat <<'EOF'
Usage: scripts/macos-microdroid-distribution-smoke.sh \
  --archive <HD-macos-arm64.tar.xz> \
  --output <fresh-evidence-directory> \
  --node-root <node-v22.23.1-darwin-arm64> \
  --node-archive <node-v22.23.1-darwin-arm64.tar.gz> \
  --java-home <Temurin-21.0.12+8/Contents/Home> \
  --java-archive <OpenJDK21U-jdk_aarch64_mac_hotspot_21.0.12_8.tar.gz> \
  --android-build-tools <android-sdk/build-tools/36.0.0> \
  [--development-package]

Development archives require --development-package and run with the explicit
HD_MICRODROID_DEV_BYPASS contract. Release archives reject that flag.
EOF
}

ARCHIVE=
OUTPUT=
NODE_ROOT=
NODE_ARCHIVE=
JAVA_HOME_INPUT=
JAVA_ARCHIVE=
ANDROID_BUILD_TOOLS=
DEVELOPMENT_PACKAGE=0

while [ "$#" -gt 0 ]; do
  case "$1" in
    --archive) ARCHIVE=$2; shift 2 ;;
    --output) OUTPUT=$2; shift 2 ;;
    --node-root) NODE_ROOT=$2; shift 2 ;;
    --node-archive) NODE_ARCHIVE=$2; shift 2 ;;
    --java-home) JAVA_HOME_INPUT=$2; shift 2 ;;
    --java-archive) JAVA_ARCHIVE=$2; shift 2 ;;
    --android-build-tools) ANDROID_BUILD_TOOLS=$2; shift 2 ;;
    --development-package) DEVELOPMENT_PACKAGE=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

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

wait_contains() {
  expected=$1
  file=$2
  timeout_seconds=$3
  started=$(date +%s)
  while ! grep -Fq "$expected" "$file" 2>/dev/null; do
    now=$(date +%s)
    [ $((now - started)) -lt "$timeout_seconds" ] ||
      fail "timed out waiting for '$expected' in $file"
    sleep 1
  done
}

require_abs_file() {
  case "$1" in /*) ;; *) fail "$2 must be absolute" ;; esac
  [ -f "$1" ] && [ ! -L "$1" ] || fail "$2 is not a regular non-symlink file: $1"
}

require_abs_dir() {
  case "$1" in /*) ;; *) fail "$2 must be absolute" ;; esac
  [ -d "$1" ] && [ ! -L "$1" ] || fail "$2 is not a non-symlink directory: $1"
}

require_abs_file "$ARCHIVE" --archive
case "$ARCHIVE" in
  *.tar.xz) ARCHIVE_FORMAT=tar-xz ;;
  *.zip) ARCHIVE_FORMAT=zip ;;
  *) fail "--archive must end in .tar.xz or .zip" ;;
esac
require_abs_dir "$NODE_ROOT" --node-root
require_abs_file "$NODE_ARCHIVE" --node-archive
require_abs_dir "$JAVA_HOME_INPUT" --java-home
require_abs_file "$JAVA_ARCHIVE" --java-archive
require_abs_dir "$ANDROID_BUILD_TOOLS" --android-build-tools
case "$OUTPUT" in /*) ;; *) fail "--output must be absolute" ;; esac
[ ! -e "$OUTPUT" ] || fail "refusing to replace evidence output: $OUTPUT"
FAILURE_OUTPUT="$OUTPUT.failed"
[ ! -e "$FAILURE_OUTPUT" ] || fail "refusing to replace failure evidence: $FAILURE_OUTPUT"
[ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ] ||
  fail "distribution Guest smoke requires an Apple Silicon macOS host"

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)
output_parent=$(dirname -- "$OUTPUT")
mkdir -p "$output_parent"
OUTPUT_STAGE=$(mktemp -d "$output_parent/.hd-distribution-smoke-evidence.XXXXXX")
INSTALL=$(mktemp -d /private/tmp/hd-distribution-install.XXXXXX)
EMPTY_DATA=$(mktemp -d /private/tmp/hd-distribution-empty.XXXXXX)
UPLOADED_DATA=$(mktemp -d /private/tmp/hd-distribution-uploaded.XXXXXX)
DEBUG_NONE_DATA=$(mktemp -d /private/tmp/hd-distribution-debug-none.XXXXXX)
MULTI_DATA=$(mktemp -d /private/tmp/hd-distribution-multi.XXXXXX)
COMPLETED=0

terminate_test_processes() {
  [ -n "$INSTALL" ] || return
  pids=$(pgrep -f "$INSTALL/HD.app" 2>/dev/null || true)
  [ -z "$pids" ] || kill $pids 2>/dev/null || true
  sleep 1
  pids=$(pgrep -f "$INSTALL/HD.app" 2>/dev/null || true)
  [ -z "$pids" ] || kill -KILL $pids 2>/dev/null || true
}

cleanup() {
  status=$?
  if [ -x "$INSTALL/HD.app/Contents/MacOS/hdctl" ]; then
    if [ "$DEVELOPMENT_PACKAGE" -eq 1 ]; then
      HD_MICRODROID_DEV_BYPASS=1 "$INSTALL/HD.app/Contents/MacOS/hdctl" \
        --data-root "$EMPTY_DATA" shutdown --stop-all >/dev/null 2>&1 || true
      HD_MICRODROID_DEV_BYPASS=1 "$INSTALL/HD.app/Contents/MacOS/hdctl" \
        --data-root "$UPLOADED_DATA" shutdown --stop-all >/dev/null 2>&1 || true
      HD_MICRODROID_DEV_BYPASS=1 "$INSTALL/HD.app/Contents/MacOS/hdctl" \
        --data-root "$DEBUG_NONE_DATA" shutdown --stop-all >/dev/null 2>&1 || true
      HD_MICRODROID_DEV_BYPASS=1 "$INSTALL/HD.app/Contents/MacOS/hdctl" \
        --data-root "$MULTI_DATA" shutdown --stop-all >/dev/null 2>&1 || true
    else
      "$INSTALL/HD.app/Contents/MacOS/hdctl" \
        --data-root "$EMPTY_DATA" shutdown --stop-all >/dev/null 2>&1 || true
      "$INSTALL/HD.app/Contents/MacOS/hdctl" \
        --data-root "$UPLOADED_DATA" shutdown --stop-all >/dev/null 2>&1 || true
      "$INSTALL/HD.app/Contents/MacOS/hdctl" \
        --data-root "$DEBUG_NONE_DATA" shutdown --stop-all >/dev/null 2>&1 || true
      "$INSTALL/HD.app/Contents/MacOS/hdctl" \
        --data-root "$MULTI_DATA" shutdown --stop-all >/dev/null 2>&1 || true
    fi
  fi
  terminate_test_processes
  case "$INSTALL" in /private/tmp/hd-distribution-install.*) rm -rf -- "$INSTALL" ;; esac
  case "$EMPTY_DATA" in /private/tmp/hd-distribution-empty.*) rm -rf -- "$EMPTY_DATA" ;; esac
  case "$UPLOADED_DATA" in /private/tmp/hd-distribution-uploaded.*)
    rm -rf -- "$UPLOADED_DATA"
    ;;
  esac
  case "$DEBUG_NONE_DATA" in /private/tmp/hd-distribution-debug-none.*)
    rm -rf -- "$DEBUG_NONE_DATA"
    ;;
  esac
  case "$MULTI_DATA" in /private/tmp/hd-distribution-multi.*)
    rm -rf -- "$MULTI_DATA"
    ;;
  esac
  case "$OUTPUT_STAGE" in
    "$output_parent"/.hd-distribution-smoke-evidence.*)
      if [ "$COMPLETED" -eq 0 ] && [ -d "$OUTPUT_STAGE" ]; then
        printf '%s\n' "$status" > "$OUTPUT_STAGE/exit.code"
        mv "$OUTPUT_STAGE" "$FAILURE_OUTPUT"
        echo "failure_evidence=$FAILURE_OUTPUT" >&2
      else
        rm -rf -- "$OUTPUT_STAGE"
      fi
      ;;
  esac
}
trap cleanup EXIT HUP INT TERM

"$ROOT/scripts/macos-release-distribution.sh" verify \
  --archive "$ARCHIVE" \
  --node-root "$NODE_ROOT" \
  --node-archive "$NODE_ARCHIVE" \
  --java-home "$JAVA_HOME_INPUT" \
  --java-archive "$JAVA_ARCHIVE" \
  --android-build-tools "$ANDROID_BUILD_TOOLS" \
  >"$OUTPUT_STAGE/distribution-verify.log" 2>&1
if [ "$ARCHIVE_FORMAT" = tar-xz ]; then
  (cd "$INSTALL" && COPYFILE_DISABLE=1 bsdtar --safe-writes --no-xattrs -xJf "$ARCHIVE")
else
  ditto -x -k "$ARCHIVE" "$INSTALL"
fi
APP="$INSTALL/HD.app"
CTL="$APP/Contents/MacOS/hdctl"
ADB="$APP/Contents/MacOS/adb"
require_abs_file "$CTL" hdctl
require_abs_file "$ADB" adb
codesign --verify --deep --strict "$APP" \
  >"$OUTPUT_STAGE/codesign.log" 2>&1

PAYLOAD_DIR="$APP/Contents/Resources/products/microdroid/conformance-payload"
PAYLOAD="$PAYLOAD_DIR/payload.apk"
PAYLOAD_MANIFEST="$PAYLOAD_DIR/payload-bundle-v1.plist"
require_abs_file "$PAYLOAD" payload.apk
require_abs_file "$PAYLOAD_MANIFEST" payload-bundle-v1.plist
CHANNEL=$(plutil -extract channel raw -o - "$PAYLOAD_MANIFEST")
case "$CHANNEL:$DEVELOPMENT_PACKAGE" in
  development:1) ;;
  development:0) fail "development archive requires --development-package" ;;
  release:0) ;;
  release:1) fail "release archive rejects --development-package" ;;
  *) fail "unsupported archive channel: $CHANNEL" ;;
esac

run_ctl() {
  data_root=$1
  stdout=$2
  stderr=$3
  shift 3
  if [ "$DEVELOPMENT_PACKAGE" -eq 1 ]; then
    HD_MICRODROID_DEV_BYPASS=1 "$CTL" --data-root "$data_root" "$@" \
      >"$stdout" 2>"$stderr"
  else
    "$CTL" --data-root "$data_root" "$@" >"$stdout" 2>"$stderr"
  fi
}

run_ctl_background() {
  data_root=$1
  stdout=$2
  stderr=$3
  shift 3
  if [ "$DEVELOPMENT_PACKAGE" -eq 1 ]; then
    HD_MICRODROID_DEV_BYPASS=1 "$CTL" --data-root "$data_root" "$@" \
      >"$stdout" 2>"$stderr" &
  else
    "$CTL" --data-root "$data_root" "$@" >"$stdout" 2>"$stderr" &
  fi
  RUN_CTL_BACKGROUND_PID=$!
}

wait_microdroid_adb() {
  data_root=$1
  instance_id=$2
  evidence_dir=$3
  prefix=$4
  started=$(date +%s)
  attempt=0
  while [ "$attempt" -lt 30 ]; do
    run_ctl "$data_root" "$evidence_dir/$prefix-poll-$attempt.json" \
      "$evidence_dir/$prefix-poll-$attempt.stderr" show "$instance_id"
    if [ "$(jq -r '.adb_ready' "$evidence_dir/$prefix-poll-$attempt.json")" = true ]; then
      cp "$evidence_dir/$prefix-poll-$attempt.json" "$evidence_dir/$prefix-ready.json"
      MICRODROID_ADB_SERIAL=$(jq -r '.adb_serial' "$evidence_dir/$prefix-ready.json")
      [ "$MICRODROID_ADB_SERIAL" != null ] && [ -n "$MICRODROID_ADB_SERIAL" ] ||
        fail "$prefix reported ADB Ready without a serial"
      MICRODROID_ADB_READY_SECONDS=$(($(date +%s) - started))
      return
    fi
    sleep 1
    attempt=$((attempt + 1))
  done
  fail "$prefix did not report ADB Ready within 30 seconds"
}

require_microdroid_adb_shell() {
  serial=$1
  stdout=$2
  stderr=$3
  "$ADB" -s "$serial" shell getprop ro.build.version.sdk >"$stdout" 2>"$stderr" &
  adb_pid=$!
  attempt=0
  while kill -0 "$adb_pid" 2>/dev/null && [ "$attempt" -lt 100 ]; do
    sleep 0.1
    attempt=$((attempt + 1))
  done
  if kill -0 "$adb_pid" 2>/dev/null; then
    kill "$adb_pid" 2>/dev/null || true
    wait "$adb_pid" 2>/dev/null || true
    fail "Microdroid ADB shell timed out for $serial"
  fi
  wait "$adb_pid" || fail "Microdroid ADB shell failed for $serial (evidence: $stderr)"
  [ "$(tr -d '\r\n' <"$stdout")" = 35 ] ||
    fail "Microdroid ADB shell returned the wrong SDK for $serial"
}

require_microdroid_graceful_stop() {
  data_root=$1
  instance_id=$2
  console_log=$3
  evidence_dir=$4
  prefix=$5
  run_ctl "$data_root" "$evidence_dir/$prefix-stopped.json" \
    "$evidence_dir/$prefix-stopped.stderr" show "$instance_id"
  [ "$(jq -r '.status.observed' "$evidence_dir/$prefix-stopped.json")" = stopped ] ||
    fail "$prefix did not converge to Stopped after graceful stop"
  [ "$(jq -r '.active_run_id' "$evidence_dir/$prefix-stopped.json")" = null ] ||
    fail "$prefix retained an active run after graceful stop"
  require_contains 'reboot: Power down' "$console_log" \
    "$prefix guest did not record a real power-down path"
  cp "$console_log" "$evidence_dir/$prefix-powerdown-console.log"
  worker_log=$(find "$data_root/logs/workers" -type f \
    -name "$instance_id.jsonl.*" | head -1)
  require_abs_file "$worker_log" "$prefix-worker-log"
  cp "$worker_log" "$evidence_dir/$prefix-worker.jsonl"
  if grep -Eq \
      'worker.stop.adb_power_off.failed|worker.stop.power_button.failed|process.terminate.started' \
      "$evidence_dir/$prefix-worker.jsonl"; then
    fail "$prefix graceful stop used a forced-termination path"
  fi
  if ps -axo command= | \
      grep -E "[c]rosvm.*$instance_id|[v]m run-microdroid.*$instance_id" >/dev/null; then
    fail "$prefix left a VMM or Microdroid launcher running"
  fi
}

mkdir -p "$OUTPUT_STAGE/empty" "$OUTPUT_STAGE/uploaded" \
  "$OUTPUT_STAGE/debug-none" "$OUTPUT_STAGE/multi"
EMPTY_ID=$(uuidgen | tr '[:upper:]' '[:lower:]')
cat > "$OUTPUT_STAGE/empty/spec.json" <<EOF
{
  "schema_version": 2,
  "id": "$EMPTY_ID",
  "name": "Microdroid Installed Empty",
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
  "adb": { "mode": "loopback", "host_port": null, "executable": null },
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
  "labels": { "purpose": "installed-distribution-empty" }
}
EOF
run_ctl "$EMPTY_DATA" "$OUTPUT_STAGE/empty/health.json" \
  "$OUTPUT_STAGE/empty/health.stderr" health
run_ctl "$EMPTY_DATA" "$OUTPUT_STAGE/empty/create.json" \
  "$OUTPUT_STAGE/empty/create.stderr" create --spec "$OUTPUT_STAGE/empty/spec.json"
run_ctl "$EMPTY_DATA" "$OUTPUT_STAGE/empty/capabilities.json" \
  "$OUTPUT_STAGE/empty/capabilities.stderr" capabilities "$EMPTY_ID"
if [ "$DEVELOPMENT_PACKAGE" -eq 1 ]; then
  require_contains '"development_bypass": true' \
    "$OUTPUT_STAGE/empty/capabilities.json" \
    "Empty Microdroid capability did not report the explicit development bypass"
else
  require_contains '"certified": true' "$OUTPUT_STAGE/empty/capabilities.json" \
    "Empty Microdroid capability is not release certified"
  require_contains '"development_bypass": false' \
    "$OUTPUT_STAGE/empty/capabilities.json" \
    "Empty Microdroid release capability unexpectedly used a development bypass"
fi
run_ctl "$EMPTY_DATA" "$OUTPUT_STAGE/empty/start.json" \
  "$OUTPUT_STAGE/empty/start.stderr" start "$EMPTY_ID"
run_ctl "$EMPTY_DATA" "$OUTPUT_STAGE/empty/ready.json" \
  "$OUTPUT_STAGE/empty/ready.stderr" show "$EMPTY_ID"
require_contains '"state": "succeeded"' "$OUTPUT_STAGE/empty/start.json" \
  "Empty Microdroid start operation did not succeed"
require_contains '"observed": "ready"' "$OUTPUT_STAGE/empty/ready.json" \
  "Empty Microdroid did not reach Ready"
wait_microdroid_adb "$EMPTY_DATA" "$EMPTY_ID" "$OUTPUT_STAGE/empty" adb-first
EMPTY_ADB_SERIAL=$MICRODROID_ADB_SERIAL
EMPTY_ADB_READY_SECONDS=$MICRODROID_ADB_READY_SECONDS
require_microdroid_adb_shell "$EMPTY_ADB_SERIAL" \
  "$OUTPUT_STAGE/empty/adb-first-sdk.txt" "$OUTPUT_STAGE/empty/adb-first-sdk.stderr"
EMPTY_RUN_1=$(jq -r '.active_run_id' "$OUTPUT_STAGE/empty/ready.json")
EMPTY_GUEST_1="$EMPTY_DATA/runs/$EMPTY_ID/$EMPTY_RUN_1/microdroid-guest.log"
EMPTY_CONSOLE_1="$EMPTY_DATA/runs/$EMPTY_ID/$EMPTY_RUN_1/microdroid-console.txt"
require_abs_file "$EMPTY_GUEST_1" empty-first-guest-log
require_abs_file "$EMPTY_CONSOLE_1" empty-first-console-log
wait_contains 'Freshly formatting the crypt device' "$EMPTY_GUEST_1" 10
wait_contains 'ext4 filesystem being mounted at /mnt/encryptedstore' "$EMPTY_CONSOLE_1" 10
EMPTY_FS_UUID_1=$(sed -n 's/.*EXT4-fs (dm-3): mounted filesystem \([^ ]*\) r\/w.*/\1/p' \
  "$EMPTY_CONSOLE_1" | tail -1)
[ -n "$EMPTY_FS_UUID_1" ] || fail "first encrypted storage mount has no filesystem UUID"
cp "$EMPTY_GUEST_1" "$OUTPUT_STAGE/empty/first-boot-guest.log"
cp "$EMPTY_CONSOLE_1" "$OUTPUT_STAGE/empty/first-boot-console.log"
run_ctl "$EMPTY_DATA" "$OUTPUT_STAGE/empty/stop-1.json" \
  "$OUTPUT_STAGE/empty/stop-1.stderr" stop "$EMPTY_ID"
require_microdroid_graceful_stop "$EMPTY_DATA" "$EMPTY_ID" "$EMPTY_CONSOLE_1" \
  "$OUTPUT_STAGE/empty" first-boot
EMPTY_STORAGE="$EMPTY_DATA/instances/$EMPTY_ID/microdroid/storage.img"
require_abs_file "$EMPTY_STORAGE" empty-encrypted-storage
EMPTY_STORAGE_STAT_1=$(stat -f '%i %B %z' "$EMPTY_STORAGE")
EMPTY_STORAGE_SHA_1=$(shasum -a 256 "$EMPTY_STORAGE" | awk '{print $1}')
[ "$(echo "$EMPTY_STORAGE_STAT_1" | awk '{print $3}')" = 67108864 ] ||
  fail "encrypted storage is not exactly 64 MiB"
run_ctl "$EMPTY_DATA" "$OUTPUT_STAGE/empty/start-2.json" \
  "$OUTPUT_STAGE/empty/start-2.stderr" start "$EMPTY_ID"
run_ctl "$EMPTY_DATA" "$OUTPUT_STAGE/empty/ready-2.json" \
  "$OUTPUT_STAGE/empty/ready-2.stderr" show "$EMPTY_ID"
require_contains '"observed": "ready"' "$OUTPUT_STAGE/empty/ready-2.json" \
  "Empty Microdroid did not return to Ready with existing encrypted storage"
wait_microdroid_adb "$EMPTY_DATA" "$EMPTY_ID" "$OUTPUT_STAGE/empty" adb-restart
EMPTY_RESTART_ADB_SERIAL=$MICRODROID_ADB_SERIAL
require_microdroid_adb_shell "$EMPTY_RESTART_ADB_SERIAL" \
  "$OUTPUT_STAGE/empty/adb-restart-sdk.txt" "$OUTPUT_STAGE/empty/adb-restart-sdk.stderr"
EMPTY_RUN_2=$(jq -r '.active_run_id' "$OUTPUT_STAGE/empty/ready-2.json")
[ "$EMPTY_RUN_1" != "$EMPTY_RUN_2" ] || fail "encrypted storage restart reused a run id"
EMPTY_GUEST_2="$EMPTY_DATA/runs/$EMPTY_ID/$EMPTY_RUN_2/microdroid-guest.log"
EMPTY_CONSOLE_2="$EMPTY_DATA/runs/$EMPTY_ID/$EMPTY_RUN_2/microdroid-console.txt"
require_abs_file "$EMPTY_GUEST_2" empty-restart-guest-log
require_abs_file "$EMPTY_CONSOLE_2" empty-restart-console-log
wait_contains 'Starting encryptedstore binary' "$EMPTY_GUEST_2" 10
wait_contains 'ext4 filesystem being mounted at /mnt/encryptedstore' "$EMPTY_CONSOLE_2" 10
if grep -Fq 'Freshly formatting the crypt device' "$EMPTY_GUEST_2"; then
  fail "existing encrypted storage was formatted again after restart"
fi
EMPTY_FS_UUID_2=$(sed -n 's/.*EXT4-fs (dm-3): mounted filesystem \([^ ]*\) r\/w.*/\1/p' \
  "$EMPTY_CONSOLE_2" | tail -1)
[ "$EMPTY_FS_UUID_1" = "$EMPTY_FS_UUID_2" ] ||
  fail "encrypted storage filesystem identity changed across restart"
cp "$EMPTY_GUEST_2" "$OUTPUT_STAGE/empty/restart-guest.log"
cp "$EMPTY_CONSOLE_2" "$OUTPUT_STAGE/empty/restart-console.log"
run_ctl "$EMPTY_DATA" "$OUTPUT_STAGE/empty/stop-2.json" \
  "$OUTPUT_STAGE/empty/stop-2.stderr" stop "$EMPTY_ID"
require_microdroid_graceful_stop "$EMPTY_DATA" "$EMPTY_ID" "$EMPTY_CONSOLE_2" \
  "$OUTPUT_STAGE/empty" restart
EMPTY_STORAGE_STAT_2=$(stat -f '%i %B %z' "$EMPTY_STORAGE")
EMPTY_STORAGE_SHA_2=$(shasum -a 256 "$EMPTY_STORAGE" | awk '{print $1}')
[ "$EMPTY_STORAGE_STAT_1" = "$EMPTY_STORAGE_STAT_2" ] ||
  fail "encrypted storage file identity or size changed across restart"
[ "$(file -b "$EMPTY_STORAGE")" = data ] ||
  fail "encrypted storage unexpectedly exposes a recognizable host filesystem"
run_ctl "$EMPTY_DATA" "$OUTPUT_STAGE/empty/delete.json" \
  "$OUTPUT_STAGE/empty/delete.stderr" delete "$EMPTY_ID"
run_ctl "$EMPTY_DATA" "$OUTPUT_STAGE/empty/shutdown.json" \
  "$OUTPUT_STAGE/empty/shutdown.stderr" shutdown

run_ctl "$UPLOADED_DATA" "$OUTPUT_STAGE/uploaded/upload.json" \
  "$OUTPUT_STAGE/uploaded/upload.stderr" upload --microdroid-payload "$PAYLOAD"
UPLOAD_ID=$(sed -n 's/^[[:space:]]*"id": "\([^"]*\)",*/\1/p' \
  "$OUTPUT_STAGE/uploaded/upload.json" | head -1)
UPLOAD_SHA=$(sed -n 's/^[[:space:]]*"sha256": "\([0-9a-f]*\)",*/\1/p' \
  "$OUTPUT_STAGE/uploaded/upload.json" | head -1)
[ -n "$UPLOAD_ID" ] && [ "${#UPLOAD_SHA}" -eq 64 ] ||
  fail "uploaded Payload response has no valid id/digest"
UPLOADED_ID=$(uuidgen | tr '[:upper:]' '[:lower:]')
cat > "$OUTPUT_STAGE/uploaded/spec.json" <<EOF
{
  "schema_version": 2,
  "id": "$UPLOADED_ID",
  "name": "Microdroid Installed Uploaded",
  "guest_kind": "microdroid",
  "microdroid": {
    "debug_level": "full",
    "payload": {
      "kind": "uploaded",
      "upload_id": "$UPLOAD_ID",
      "sha256": "$UPLOAD_SHA",
      "config_path": "assets/vm_config.json"
    },
    "encrypted_storage_mib": null
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
  "adb": { "mode": "loopback", "host_port": null, "executable": null },
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
  "labels": { "purpose": "installed-distribution-uploaded" }
}
EOF
run_ctl "$UPLOADED_DATA" "$OUTPUT_STAGE/uploaded/create.json" \
  "$OUTPUT_STAGE/uploaded/create.stderr" create --spec "$OUTPUT_STAGE/uploaded/spec.json"
run_ctl "$UPLOADED_DATA" "$OUTPUT_STAGE/uploaded/capabilities.json" \
  "$OUTPUT_STAGE/uploaded/capabilities.stderr" capabilities "$UPLOADED_ID"
if [ "$DEVELOPMENT_PACKAGE" -eq 1 ]; then
  require_contains '"development_bypass": true' \
    "$OUTPUT_STAGE/uploaded/capabilities.json" \
    "Uploaded Microdroid capability did not report the explicit development bypass"
else
  require_contains '"certified": true' "$OUTPUT_STAGE/uploaded/capabilities.json" \
    "Uploaded Microdroid capability is not release certified"
  require_contains '"development_bypass": false' \
    "$OUTPUT_STAGE/uploaded/capabilities.json" \
    "Uploaded Microdroid release capability unexpectedly used a development bypass"
fi
run_ctl "$UPLOADED_DATA" "$OUTPUT_STAGE/uploaded/start.json" \
  "$OUTPUT_STAGE/uploaded/start.stderr" start "$UPLOADED_ID"
run_ctl "$UPLOADED_DATA" "$OUTPUT_STAGE/uploaded/ready.json" \
  "$OUTPUT_STAGE/uploaded/ready.stderr" show "$UPLOADED_ID"
require_contains '"state": "succeeded"' "$OUTPUT_STAGE/uploaded/start.json" \
  "Uploaded Microdroid start operation did not succeed"
require_contains '"observed": "ready"' "$OUTPUT_STAGE/uploaded/ready.json" \
  "Uploaded Microdroid did not reach Ready"
wait_microdroid_adb "$UPLOADED_DATA" "$UPLOADED_ID" "$OUTPUT_STAGE/uploaded" adb
UPLOADED_ADB_SERIAL=$MICRODROID_ADB_SERIAL
UPLOADED_ADB_READY_SECONDS=$MICRODROID_ADB_READY_SECONDS
require_microdroid_adb_shell "$UPLOADED_ADB_SERIAL" \
  "$OUTPUT_STAGE/uploaded/adb-sdk.txt" "$OUTPUT_STAGE/uploaded/adb-sdk.stderr"
run_ctl "$UPLOADED_DATA" "$OUTPUT_STAGE/uploaded/stop.json" \
  "$OUTPUT_STAGE/uploaded/stop.stderr" stop "$UPLOADED_ID"
UPLOADED_RUN=$(jq -r '.active_run_id' "$OUTPUT_STAGE/uploaded/ready.json")
UPLOADED_CONSOLE="$UPLOADED_DATA/runs/$UPLOADED_ID/$UPLOADED_RUN/microdroid-console.txt"
require_abs_file "$UPLOADED_CONSOLE" uploaded-console-log
cp "$UPLOADED_CONSOLE" "$OUTPUT_STAGE/uploaded/console.log"
require_microdroid_graceful_stop "$UPLOADED_DATA" "$UPLOADED_ID" "$UPLOADED_CONSOLE" \
  "$OUTPUT_STAGE/uploaded" uploaded
run_ctl "$UPLOADED_DATA" "$OUTPUT_STAGE/uploaded/delete.json" \
  "$OUTPUT_STAGE/uploaded/delete.stderr" delete "$UPLOADED_ID"
run_ctl "$UPLOADED_DATA" "$OUTPUT_STAGE/uploaded/shutdown.json" \
  "$OUTPUT_STAGE/uploaded/shutdown.stderr" shutdown

# AOSP exposes both Full and None debug levels. None must remain a first-class bootable mode,
# must not advertise ADB, and must not inherit the Full debug policy from another instance.
DEBUG_NONE_ID=$(uuidgen | tr '[:upper:]' '[:lower:]')
jq --arg id "$DEBUG_NONE_ID" \
  '.id = $id | .name = "Microdroid Installed Debug None" |
   .microdroid.debug_level = "none" | .microdroid.encrypted_storage_mib = null |
   .adb = {"mode":"disabled","host_port":null,"executable":null} |
   .labels.purpose = "installed-distribution-debug-none"' \
  "$OUTPUT_STAGE/empty/spec.json" >"$OUTPUT_STAGE/debug-none/spec.json"
run_ctl "$DEBUG_NONE_DATA" "$OUTPUT_STAGE/debug-none/create.json" \
  "$OUTPUT_STAGE/debug-none/create.stderr" create --spec "$OUTPUT_STAGE/debug-none/spec.json"
DEBUG_NONE_START_EPOCH=$(date +%s)
run_ctl "$DEBUG_NONE_DATA" "$OUTPUT_STAGE/debug-none/start.json" \
  "$OUTPUT_STAGE/debug-none/start.stderr" start "$DEBUG_NONE_ID"
DEBUG_NONE_READY_SECONDS=$(($(date +%s) - DEBUG_NONE_START_EPOCH))
[ "$DEBUG_NONE_READY_SECONDS" -lt 10 ] ||
  fail "debug=none Microdroid Payload Ready took ${DEBUG_NONE_READY_SECONDS}s"
run_ctl "$DEBUG_NONE_DATA" "$OUTPUT_STAGE/debug-none/ready.json" \
  "$OUTPUT_STAGE/debug-none/ready.stderr" show "$DEBUG_NONE_ID"
require_contains '"state": "succeeded"' "$OUTPUT_STAGE/debug-none/start.json" \
  "debug=none Microdroid start operation did not succeed"
require_contains '"observed": "ready"' "$OUTPUT_STAGE/debug-none/ready.json" \
  "debug=none Microdroid did not reach Ready"
[ "$(jq -r '.adb_ready' "$OUTPUT_STAGE/debug-none/ready.json")" = false ] ||
  fail "debug=none Microdroid incorrectly reported ADB Ready"
[ "$(jq -r '.adb_serial' "$OUTPUT_STAGE/debug-none/ready.json")" = null ] ||
  fail "debug=none Microdroid unexpectedly published an ADB serial"
DEBUG_NONE_RUN=$(jq -r '.active_run_id' "$OUTPUT_STAGE/debug-none/ready.json")
DEBUG_NONE_WORKER=$(jq -r '.worker.pid' "$OUTPUT_STAGE/debug-none/ready.json")
DEBUG_NONE_MANIFEST="$DEBUG_NONE_DATA/runs/$DEBUG_NONE_ID/$DEBUG_NONE_RUN/manifest.json"
DEBUG_NONE_GUEST="$DEBUG_NONE_DATA/runs/$DEBUG_NONE_ID/$DEBUG_NONE_RUN/microdroid-guest.log"
DEBUG_NONE_CONSOLE="$DEBUG_NONE_DATA/runs/$DEBUG_NONE_ID/$DEBUG_NONE_RUN/microdroid-console.txt"
require_abs_file "$DEBUG_NONE_MANIFEST" debug-none-manifest
require_abs_file "$DEBUG_NONE_GUEST" debug-none-guest-log
require_abs_file "$DEBUG_NONE_CONSOLE" debug-none-console-log
jq -e '.launch.arguments as $a | ($a | index("--debug")) as $i |
  $i != null and $a[$i + 1] == "none"' "$DEBUG_NONE_MANIFEST" >/dev/null ||
  fail "debug=none Microdroid launch did not preserve --debug none"
if grep -Fq 'init_debug_policy.adbd.enabled=1' "$DEBUG_NONE_CONSOLE"; then
  fail "debug=none Microdroid unexpectedly enabled the adbd debug policy"
fi
cp "$DEBUG_NONE_MANIFEST" "$OUTPUT_STAGE/debug-none/manifest.json"
cp "$DEBUG_NONE_GUEST" "$OUTPUT_STAGE/debug-none/guest.log"
cp "$DEBUG_NONE_CONSOLE" "$OUTPUT_STAGE/debug-none/console.log"
run_ctl "$DEBUG_NONE_DATA" "$OUTPUT_STAGE/debug-none/shutdown-stop-all.json" \
  "$OUTPUT_STAGE/debug-none/shutdown-stop-all.stderr" shutdown --stop-all
attempt=0
while kill -0 "$DEBUG_NONE_WORKER" 2>/dev/null && [ "$attempt" -lt 50 ]; do
  sleep 0.1
  attempt=$((attempt + 1))
done
kill -0 "$DEBUG_NONE_WORKER" 2>/dev/null &&
  fail "debug=none Worker survived shutdown --stop-all"

# The release contract requires real multi-instance isolation inside one Host, not two
# sequentially successful private data roots. Use loopback ADB leases for both guests so the
# allocator and the bundled Full-debug adbd path must stay independent under concurrency.
run_ctl "$MULTI_DATA" "$OUTPUT_STAGE/multi/health.json" \
  "$OUTPUT_STAGE/multi/health.stderr" health
run_ctl "$MULTI_DATA" "$OUTPUT_STAGE/multi/upload.json" \
  "$OUTPUT_STAGE/multi/upload.stderr" upload --microdroid-payload "$PAYLOAD"
MULTI_UPLOAD_ID=$(sed -n 's/^[[:space:]]*"id": "\([^"]*\)",*/\1/p' \
  "$OUTPUT_STAGE/multi/upload.json" | head -1)
MULTI_UPLOAD_SHA=$(sed -n 's/^[[:space:]]*"sha256": "\([0-9a-f]*\)",*/\1/p' \
  "$OUTPUT_STAGE/multi/upload.json" | head -1)
[ -n "$MULTI_UPLOAD_ID" ] && [ "${#MULTI_UPLOAD_SHA}" -eq 64 ] ||
  fail "multi-instance Payload response has no valid id/digest"
MULTI_EMPTY_ID=$(uuidgen | tr '[:upper:]' '[:lower:]')
MULTI_UPLOADED_ID=$(uuidgen | tr '[:upper:]' '[:lower:]')
jq --arg id "$MULTI_EMPTY_ID" \
  '.id = $id | .name = "Microdroid Concurrent Empty" |
   .adb = {"mode":"loopback","host_port":null,"executable":null} |
   .labels.purpose = "installed-distribution-multi-empty"' \
  "$OUTPUT_STAGE/empty/spec.json" >"$OUTPUT_STAGE/multi/empty-spec.json"
jq --arg id "$MULTI_UPLOADED_ID" --arg upload_id "$MULTI_UPLOAD_ID" \
  --arg sha256 "$MULTI_UPLOAD_SHA" \
  '.id = $id | .name = "Microdroid Concurrent Uploaded" |
   .microdroid.payload.upload_id = $upload_id |
   .microdroid.payload.sha256 = $sha256 |
   .adb = {"mode":"loopback","host_port":null,"executable":null} |
   .labels.purpose = "installed-distribution-multi-uploaded"' \
  "$OUTPUT_STAGE/uploaded/spec.json" >"$OUTPUT_STAGE/multi/uploaded-spec.json"
run_ctl "$MULTI_DATA" "$OUTPUT_STAGE/multi/create-empty.json" \
  "$OUTPUT_STAGE/multi/create-empty.stderr" create --spec "$OUTPUT_STAGE/multi/empty-spec.json"
run_ctl "$MULTI_DATA" "$OUTPUT_STAGE/multi/create-uploaded.json" \
  "$OUTPUT_STAGE/multi/create-uploaded.stderr" create --spec "$OUTPUT_STAGE/multi/uploaded-spec.json"

MULTI_START_EPOCH=$(date +%s)
run_ctl_background "$MULTI_DATA" "$OUTPUT_STAGE/multi/start-empty.json" \
  "$OUTPUT_STAGE/multi/start-empty.stderr" start "$MULTI_EMPTY_ID"
MULTI_EMPTY_START_PID=$RUN_CTL_BACKGROUND_PID
run_ctl_background "$MULTI_DATA" "$OUTPUT_STAGE/multi/start-uploaded.json" \
  "$OUTPUT_STAGE/multi/start-uploaded.stderr" start "$MULTI_UPLOADED_ID"
MULTI_UPLOADED_START_PID=$RUN_CTL_BACKGROUND_PID
wait "$MULTI_EMPTY_START_PID" || fail "concurrent Empty Microdroid start failed"
wait "$MULTI_UPLOADED_START_PID" || fail "concurrent uploaded Microdroid start failed"
MULTI_READY_SECONDS=$(($(date +%s) - MULTI_START_EPOCH))
[ "$MULTI_READY_SECONDS" -lt 10 ] ||
  fail "concurrent Microdroid Payload Ready took ${MULTI_READY_SECONDS}s; ADB probing blocked startup"
run_ctl "$MULTI_DATA" "$OUTPUT_STAGE/multi/ready-empty.json" \
  "$OUTPUT_STAGE/multi/ready-empty.stderr" show "$MULTI_EMPTY_ID"
run_ctl "$MULTI_DATA" "$OUTPUT_STAGE/multi/ready-uploaded.json" \
  "$OUTPUT_STAGE/multi/ready-uploaded.stderr" show "$MULTI_UPLOADED_ID"
require_contains '"state": "succeeded"' "$OUTPUT_STAGE/multi/start-empty.json" \
  "concurrent Empty Microdroid start operation did not succeed"
require_contains '"state": "succeeded"' "$OUTPUT_STAGE/multi/start-uploaded.json" \
  "concurrent uploaded Microdroid start operation did not succeed"
require_contains '"observed": "ready"' "$OUTPUT_STAGE/multi/ready-empty.json" \
  "concurrent Empty Microdroid did not reach Ready"
require_contains '"observed": "ready"' "$OUTPUT_STAGE/multi/ready-uploaded.json" \
  "concurrent uploaded Microdroid did not reach Ready"
wait_microdroid_adb "$MULTI_DATA" "$MULTI_EMPTY_ID" "$OUTPUT_STAGE/multi" adb-empty
MULTI_EMPTY_ADB_READY_SECONDS=$MICRODROID_ADB_READY_SECONDS
MULTI_EMPTY_ADB_LIVE=$MICRODROID_ADB_SERIAL
wait_microdroid_adb "$MULTI_DATA" "$MULTI_UPLOADED_ID" "$OUTPUT_STAGE/multi" adb-uploaded
MULTI_UPLOADED_ADB_READY_SECONDS=$MICRODROID_ADB_READY_SECONDS
MULTI_UPLOADED_ADB_LIVE=$MICRODROID_ADB_SERIAL
[ "$MULTI_EMPTY_ADB_LIVE" != "$MULTI_UPLOADED_ADB_LIVE" ] ||
  fail "concurrent guests published one ADB serial"
require_microdroid_adb_shell "$MULTI_EMPTY_ADB_LIVE" \
  "$OUTPUT_STAGE/multi/empty-adb-sdk.txt" "$OUTPUT_STAGE/multi/empty-adb-sdk.stderr"
require_microdroid_adb_shell "$MULTI_UPLOADED_ADB_LIVE" \
  "$OUTPUT_STAGE/multi/uploaded-adb-sdk.txt" "$OUTPUT_STAGE/multi/uploaded-adb-sdk.stderr"

MULTI_EMPTY_RUN=$(jq -r '.active_run_id' "$OUTPUT_STAGE/multi/ready-empty.json")
MULTI_UPLOADED_RUN=$(jq -r '.active_run_id' "$OUTPUT_STAGE/multi/ready-uploaded.json")
MULTI_EMPTY_GUEST="$MULTI_DATA/runs/$MULTI_EMPTY_ID/$MULTI_EMPTY_RUN/microdroid-guest.log"
MULTI_EMPTY_CONSOLE="$MULTI_DATA/runs/$MULTI_EMPTY_ID/$MULTI_EMPTY_RUN/microdroid-console.txt"
MULTI_UPLOADED_CONSOLE="$MULTI_DATA/runs/$MULTI_UPLOADED_ID/$MULTI_UPLOADED_RUN/microdroid-console.txt"
require_abs_file "$MULTI_EMPTY_GUEST" concurrent-empty-guest-log
require_abs_file "$MULTI_EMPTY_CONSOLE" concurrent-empty-console-log
require_abs_file "$MULTI_UPLOADED_CONSOLE" concurrent-uploaded-console-log
wait_contains 'Freshly formatting the crypt device' "$MULTI_EMPTY_GUEST" 10
wait_contains 'ext4 filesystem being mounted at /mnt/encryptedstore' "$MULTI_EMPTY_CONSOLE" 10
MULTI_EMPTY_FS_UUID=$(sed -n 's/.*EXT4-fs (dm-3): mounted filesystem \([^ ]*\) r\/w.*/\1/p' \
  "$MULTI_EMPTY_CONSOLE" | tail -1)
[ -n "$MULTI_EMPTY_FS_UUID" ] && [ "$MULTI_EMPTY_FS_UUID" != "$EMPTY_FS_UUID_1" ] ||
  fail "independent Microdroid instances reused one encrypted filesystem identity"
cp "$MULTI_EMPTY_GUEST" "$OUTPUT_STAGE/multi/empty-guest.log"
cp "$MULTI_EMPTY_CONSOLE" "$OUTPUT_STAGE/multi/empty-console.log"
MULTI_EMPTY_WORKER=$(jq -r '.worker.pid' "$OUTPUT_STAGE/multi/ready-empty.json")
MULTI_UPLOADED_WORKER=$(jq -r '.worker.pid' "$OUTPUT_STAGE/multi/ready-uploaded.json")
[ "$MULTI_EMPTY_RUN" != "$MULTI_UPLOADED_RUN" ] || fail "concurrent runs reused one run id"
[ "$MULTI_EMPTY_WORKER" != "$MULTI_UPLOADED_WORKER" ] || fail "concurrent instances reused one Worker"
kill -0 "$MULTI_EMPTY_WORKER" 2>/dev/null || fail "concurrent Empty Worker is not alive"
kill -0 "$MULTI_UPLOADED_WORKER" 2>/dev/null || fail "concurrent uploaded Worker is not alive"
MULTI_EMPTY_MANIFEST="$MULTI_DATA/runs/$MULTI_EMPTY_ID/$MULTI_EMPTY_RUN/manifest.json"
MULTI_UPLOADED_MANIFEST="$MULTI_DATA/runs/$MULTI_UPLOADED_ID/$MULTI_UPLOADED_RUN/manifest.json"
require_abs_file "$MULTI_EMPTY_MANIFEST" concurrent-empty-manifest
require_abs_file "$MULTI_UPLOADED_MANIFEST" concurrent-uploaded-manifest
MULTI_EMPTY_CID=$(jq -r '.launch.guest_cid' "$MULTI_EMPTY_MANIFEST")
MULTI_UPLOADED_CID=$(jq -r '.launch.guest_cid' "$MULTI_UPLOADED_MANIFEST")
MULTI_EMPTY_ADB=$(jq -r '.launch.adb_serial' "$MULTI_EMPTY_MANIFEST")
MULTI_UPLOADED_ADB=$(jq -r '.launch.adb_serial' "$MULTI_UPLOADED_MANIFEST")
MULTI_EMPTY_WORKDIR=$(jq -r '.launch.working_directory' "$MULTI_EMPTY_MANIFEST")
MULTI_UPLOADED_WORKDIR=$(jq -r '.launch.working_directory' "$MULTI_UPLOADED_MANIFEST")
MULTI_EMPTY_LOG=$(jq -r '.launch.arguments as $a | ($a | index("--log")) as $i | $a[$i + 1]' \
  "$MULTI_EMPTY_MANIFEST")
MULTI_UPLOADED_LOG=$(jq -r '.launch.arguments as $a | ($a | index("--log")) as $i | $a[$i + 1]' \
  "$MULTI_UPLOADED_MANIFEST")
[ "$MULTI_EMPTY_CID" != "$MULTI_UPLOADED_CID" ] || fail "concurrent guests reused one CID"
[ "$MULTI_EMPTY_ADB" != "$MULTI_UPLOADED_ADB" ] || fail "concurrent guests reused one ADB lease"
[ "$MULTI_EMPTY_WORKDIR" != "$MULTI_UPLOADED_WORKDIR" ] || fail "concurrent guests reused one work directory"
[ "$MULTI_EMPTY_LOG" != "$MULTI_UPLOADED_LOG" ] || fail "concurrent guests reused one log path"
case "$MULTI_EMPTY_WORKDIR" in "$MULTI_DATA"/*) ;; *) fail "Empty work directory escaped data root" ;; esac
case "$MULTI_UPLOADED_WORKDIR" in "$MULTI_DATA"/*) ;; *) fail "uploaded work directory escaped data root" ;; esac
require_contains '"run-microdroid"' "$MULTI_EMPTY_MANIFEST" \
  "concurrent Empty guest did not keep its EmptyPayload launch"
require_contains '"run-app"' "$MULTI_UPLOADED_MANIFEST" \
  "concurrent uploaded guest did not keep its uploaded Payload launch"
printf 'instance\tguest_cid\tadb_serial\tworker_pid\trun_id\twork_dir\tguest_log\n' \
  >"$OUTPUT_STAGE/multi/resources.tsv"
printf 'empty\t%s\t%s\t%s\t%s\t%s\t%s\n' "$MULTI_EMPTY_CID" "$MULTI_EMPTY_ADB" \
  "$MULTI_EMPTY_WORKER" "$MULTI_EMPTY_RUN" "$MULTI_EMPTY_WORKDIR" "$MULTI_EMPTY_LOG" \
  >>"$OUTPUT_STAGE/multi/resources.tsv"
printf 'uploaded\t%s\t%s\t%s\t%s\t%s\t%s\n' "$MULTI_UPLOADED_CID" \
  "$MULTI_UPLOADED_ADB" "$MULTI_UPLOADED_WORKER" "$MULTI_UPLOADED_RUN" \
  "$MULTI_UPLOADED_WORKDIR" "$MULTI_UPLOADED_LOG" >>"$OUTPUT_STAGE/multi/resources.tsv"

run_ctl "$MULTI_DATA" "$OUTPUT_STAGE/multi/stop-empty.json" \
  "$OUTPUT_STAGE/multi/stop-empty.stderr" stop "$MULTI_EMPTY_ID"
require_microdroid_graceful_stop "$MULTI_DATA" "$MULTI_EMPTY_ID" "$MULTI_EMPTY_CONSOLE" \
  "$OUTPUT_STAGE/multi" empty
MULTI_EMPTY_STORAGE="$MULTI_DATA/instances/$MULTI_EMPTY_ID/microdroid/storage.img"
require_abs_file "$MULTI_EMPTY_STORAGE" concurrent-empty-encrypted-storage
MULTI_EMPTY_STORAGE_SHA=$(shasum -a 256 "$MULTI_EMPTY_STORAGE" | awk '{print $1}')
[ "$MULTI_EMPTY_STORAGE_SHA" != "$EMPTY_STORAGE_SHA_2" ] ||
  fail "independent Microdroid instances produced identical encrypted storage bytes"
run_ctl "$MULTI_DATA" "$OUTPUT_STAGE/multi/uploaded-after-empty-stop.json" \
  "$OUTPUT_STAGE/multi/uploaded-after-empty-stop.stderr" show "$MULTI_UPLOADED_ID"
require_contains '"observed": "ready"' "$OUTPUT_STAGE/multi/uploaded-after-empty-stop.json" \
  "stopping concurrent Empty guest disrupted uploaded guest"
[ "$(jq -r '.worker.pid' "$OUTPUT_STAGE/multi/uploaded-after-empty-stop.json")" = \
  "$MULTI_UPLOADED_WORKER" ] || fail "uploaded Worker changed after stopping peer"
kill -0 "$MULTI_UPLOADED_WORKER" 2>/dev/null || fail "uploaded Worker exited with peer"
run_ctl "$MULTI_DATA" "$OUTPUT_STAGE/multi/stop-uploaded.json" \
  "$OUTPUT_STAGE/multi/stop-uploaded.stderr" stop "$MULTI_UPLOADED_ID"
require_microdroid_graceful_stop "$MULTI_DATA" "$MULTI_UPLOADED_ID" \
  "$MULTI_UPLOADED_CONSOLE" "$OUTPUT_STAGE/multi" uploaded
run_ctl "$MULTI_DATA" "$OUTPUT_STAGE/multi/delete-empty.json" \
  "$OUTPUT_STAGE/multi/delete-empty.stderr" delete "$MULTI_EMPTY_ID"
run_ctl "$MULTI_DATA" "$OUTPUT_STAGE/multi/delete-uploaded.json" \
  "$OUTPUT_STAGE/multi/delete-uploaded.stderr" delete "$MULTI_UPLOADED_ID"
run_ctl "$MULTI_DATA" "$OUTPUT_STAGE/multi/shutdown.json" \
  "$OUTPUT_STAGE/multi/shutdown.stderr" shutdown

sleep 1
if pgrep -fl "$INSTALL/HD.app" >"$OUTPUT_STAGE/process-leaks.txt"; then
  cat "$OUTPUT_STAGE/process-leaks.txt" >&2
  fail "installed distribution left an HD process running"
fi

VERSION=$(plutil -extract CFBundleShortVersionString raw -o - "$APP/Contents/Info.plist")
BUILD=$(plutil -extract CFBundleVersion raw -o - "$APP/Contents/Info.plist")
ARCHIVE_SHA256=$(shasum -a 256 "$ARCHIVE" | awk '{print $1}')
PAYLOAD_SHA256=$(plutil -extract sha256 raw -o - "$PAYLOAD_MANIFEST")
GUEST_DIGEST=$(sed -n 's/.*"guest_digest": "\([0-9a-f]*\)".*/\1/p' \
  "$APP/Contents/Resources/products/microdroid/runtime-identity-v2.json")
HOST_DIGEST=$(sed -n 's/.*"host_digest": "\([0-9a-f]*\)".*/\1/p' \
  "$APP/Contents/Resources/products/microdroid/runtime-identity-v2.json")
cat > "$OUTPUT_STAGE/result.json" <<EOF
{
  "schema_version": 1,
  "profile": "hd-macos-arm64-installed-microdroid-v3",
  "archive_sha256": "$ARCHIVE_SHA256",
  "version": "$VERSION",
  "build": "$BUILD",
  "channel": "$CHANNEL",
  "development_bypass": $([ "$DEVELOPMENT_PACKAGE" -eq 1 ] && echo true || echo false),
  "empty_instance_id": "$EMPTY_ID",
  "empty_observed": "ready",
  "empty_adb_serial": "$EMPTY_ADB_SERIAL",
  "empty_adb_ready_seconds": $EMPTY_ADB_READY_SECONDS,
  "empty_adb_shell_sdk": 35,
  "uploaded_instance_id": "$UPLOADED_ID",
  "uploaded_observed": "ready",
  "uploaded_adb_serial": "$UPLOADED_ADB_SERIAL",
  "uploaded_adb_ready_seconds": $UPLOADED_ADB_READY_SECONDS,
  "uploaded_adb_shell_sdk": 35,
  "multi_instance": {
    "empty_instance_id": "$MULTI_EMPTY_ID",
    "uploaded_instance_id": "$MULTI_UPLOADED_ID",
    "guest_cids_unique": true,
    "adb_leases_unique": true,
    "workers_unique": true,
    "run_directories_unique": true,
    "payloads_isolated": true,
    "logs_isolated": true,
    "peer_stop_isolated": true
  },
  "deferred_adb": {
    "payload_ready_seconds": $MULTI_READY_SECONDS,
    "ready_budget_seconds": 10,
    "empty_adb_ready": true,
    "uploaded_adb_ready": true,
    "empty_adb_ready_seconds": $MULTI_EMPTY_ADB_READY_SECONDS,
    "uploaded_adb_ready_seconds": $MULTI_UPLOADED_ADB_READY_SECONDS,
    "adb_shell_sdk": 35,
    "startup_not_blocked_by_adb_probe": true
  },
  "debug_none": {
    "instance_id": "$DEBUG_NONE_ID",
    "run_id": "$DEBUG_NONE_RUN",
    "payload_ready_seconds": $DEBUG_NONE_READY_SECONDS,
    "launch_argument": "none",
    "adb_serial": null,
    "adb_ready": false,
    "adbd_debug_policy_started": false,
    "process_cleanup": "pass"
  },
  "graceful_stop": {
    "empty_first_boot": "guest_power_down",
    "empty_restart": "guest_power_down",
    "uploaded": "guest_power_down",
    "concurrent_empty": "guest_power_down",
    "concurrent_uploaded": "guest_power_down",
    "forced_termination_events": 0
  },
  "encrypted_storage": {
    "size_bytes": 67108864,
    "first_filesystem_uuid": "$EMPTY_FS_UUID_1",
    "restart_filesystem_uuid": "$EMPTY_FS_UUID_2",
    "peer_filesystem_uuid": "$MULTI_EMPTY_FS_UUID",
    "persistent_file_identity": true,
    "restart_reused_filesystem": true,
    "peer_identity_isolated": true,
    "first_boot_sha256": "$EMPTY_STORAGE_SHA_1",
    "restart_sha256": "$EMPTY_STORAGE_SHA_2",
    "peer_sha256": "$MULTI_EMPTY_STORAGE_SHA",
    "host_file_type": "data"
  },
  "payload_sha256": "$PAYLOAD_SHA256",
  "microdroid_guest_digest": "$GUEST_DIGEST",
  "microdroid_host_digest": "$HOST_DIGEST",
  "active_microdroid_stop_all": "pass",
  "process_cleanup": "pass"
}
EOF
generated_at=$(date -u '+%Y-%m-%dT%H:%M:%SZ')
cat > "$OUTPUT_STAGE/installed-guest-gates.json" <<EOF
{
  "schema_version": 2,
  "generated_at": "$generated_at",
  "source": "scripts/macos-microdroid-distribution-smoke.sh",
  "gates": [
    {
      "name": "macos-release-installed-guest",
      "command": "macos-microdroid-distribution-smoke.sh --archive <candidate>",
      "status": "pass",
      "duration_ms": null,
      "log_path": null,
      "summary": "独立解包 HD ${VERSION} build ${BUILD}；先复验归档、安全路径、稀疏 Android aggregate、应用、闭包、Payload 与 provenance，再以 ${CHANNEL} 契约运行 Full/None debug、Empty 与 uploaded v3 Payload 到 Ready；Full 下两种 Payload 均通过独立回环端口完成 ADB shell，并在停止时记录真实 guest Power down 且无强制终止；None 精确保留 --debug none 且不发布 ADB；加密存储首次格式化、重启复用同一文件系统、跨实例身份与密文隔离均已回读；delete/shutdown 后无遗留进程。"
    },
    {
      "name": "macos-microdroid-graceful-stop",
      "command": "hdctl stop <full-debug-microdroid-instance>",
      "status": "pass",
      "duration_ms": null,
      "log_path": "$OUTPUT/empty/restart-worker.jsonl",
      "summary": "Full-debug Empty、重启后的 Empty、uploaded 以及并发的两个实例均通过 AOSP debuggable-adbd 权限链路完成 guest Power down；状态收敛到 Stopped，活动 run 清空，未出现 ADB power-off failure、power-button failure 或 forced process termination。"
    },
    {
      "name": "host-shutdown-stop-all",
      "command": "hdctl shutdown --stop-all with an active Microdroid instance and disabled ADB",
      "status": "pass",
      "duration_ms": null,
      "log_path": "$OUTPUT/debug-none/shutdown-stop-all.stderr",
      "summary": "活动的 debug=None Microdroid 没有 ADB 端点时，Host 通过 shutdown --stop-all 回收 Worker 与 Host；客户端成功返回且测试命名空间无进程泄漏。"
    },
    {
      "name": "macos-microdroid-multi-instance",
      "command": "macos-microdroid-distribution-smoke.sh --archive <candidate>",
      "status": "pass",
      "duration_ms": null,
      "log_path": "$OUTPUT/multi/resources.tsv",
      "summary": "同一 Host 并发启动 Empty 与 uploaded Microdroid 到 Ready；两个实例的 Guest CID、loopback ADB 租约、ADB shell、Worker、run/work 目录、Payload 和日志路径均独立。先正常关机 Empty 后 uploaded 保持相同 Worker 与 Ready，随后正常关机 uploaded、分别删除并关闭 Host，证明实例生命周期互不影响。"
    }
  ]
}
EOF

mv "$OUTPUT_STAGE" "$OUTPUT"
COMPLETED=1
trap - EXIT HUP INT TERM
case "$INSTALL" in /private/tmp/hd-distribution-install.*) rm -rf -- "$INSTALL" ;; esac
case "$EMPTY_DATA" in /private/tmp/hd-distribution-empty.*) rm -rf -- "$EMPTY_DATA" ;; esac
case "$UPLOADED_DATA" in /private/tmp/hd-distribution-uploaded.*)
  rm -rf -- "$UPLOADED_DATA"
  ;;
esac
case "$DEBUG_NONE_DATA" in /private/tmp/hd-distribution-debug-none.*)
  rm -rf -- "$DEBUG_NONE_DATA"
  ;;
esac
case "$MULTI_DATA" in /private/tmp/hd-distribution-multi.*)
  rm -rf -- "$MULTI_DATA"
  ;;
esac
echo "evidence=$OUTPUT"
echo "gate_report=$OUTPUT/installed-guest-gates.json"
echo "archive_sha256=$ARCHIVE_SHA256"
echo "empty=ready"
echo "uploaded=ready"
echo "process_cleanup=pass"
