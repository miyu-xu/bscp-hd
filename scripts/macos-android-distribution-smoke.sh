#!/bin/sh
set -eu

usage() {
  cat >&2 <<'EOF'
Usage: scripts/macos-android-distribution-smoke.sh \
  --archive <HD-macos-arm64.tar.xz> \
  --output <fresh-evidence-directory> \
  --node-root <node-v22.23.1-darwin-arm64> \
  --node-archive <node-v22.23.1-darwin-arm64.tar.gz> \
  --java-home <Temurin-21.0.12+8/Contents/Home> \
  --java-archive <OpenJDK21U-jdk_aarch64_mac_hotspot_21.0.12_8.tar.gz> \
  --android-build-tools <android-sdk/build-tools/36.0.0> \
  --location-probe-apk <hd-location-probe.apk> \
  --zstd <absolute-zstd-cli> \
  --development-package

The runner verifies and extracts a self-contained sparse distribution, launches
the installed desktop shell without external runtime overrides, and requires two
Android 15 boots with ADB, network, persistent userdata and monotonic frames.
EOF
}

ARCHIVE=
OUTPUT=
NODE_ROOT=
NODE_ARCHIVE=
JAVA_HOME_INPUT=
JAVA_ARCHIVE=
ANDROID_BUILD_TOOLS=
LOCATION_PROBE_APK=
ZSTD=
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
    --location-probe-apk) LOCATION_PROBE_APK=$2; shift 2 ;;
    --zstd) ZSTD=$2; shift 2 ;;
    --development-package) DEVELOPMENT_PACKAGE=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage; exit 2 ;;
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

json_number() {
  sed -n "s/^[[:space:]]*\"$1\": \([0-9][0-9]*\),*$/\1/p" "$2" | head -1
}

json_string() {
  sed -n "s/^[[:space:]]*\"$1\": \"\([^\"]*\)\",*$/\1/p" "$2" | head -1
}

require_contains() {
  grep -Fq "$1" "$2" || fail "$3 (evidence: $2)"
}

ui_node_center() {
  xml=$1
  match_kind=$2
  match_value=$3
  /usr/bin/python3 - "$xml" "$match_kind" "$match_value" <<'PY'
import re
import sys
import xml.etree.ElementTree as ET

root = ET.parse(sys.argv[1]).getroot()
kind = sys.argv[2]
value = sys.argv[3]
for node in root.iter("node"):
    if node.attrib.get(kind) != value:
        continue
    match = re.fullmatch(r"\[(\d+),(\d+)\]\[(\d+),(\d+)\]", node.attrib.get("bounds", ""))
    if match:
        left, top, right, bottom = map(int, match.groups())
        print((left + right) // 2, (top + bottom) // 2)
        raise SystemExit(0)
raise SystemExit(1)
PY
}

dump_android_ui() {
  destination=$1
  "$ADB" -s "$ADB_SERIAL" shell uiautomator dump /sdcard/hd-window.xml >/dev/null 2>&1
  "$ADB" -s "$ADB_SERIAL" shell cat /sdcard/hd-window.xml > "$destination"
}

wait_android_network() {
  serial=$1
  evidence=$2
  label=$3
  attempts=0
  error_evidence="$evidence.stderr"
  while [ "$attempts" -lt 30 ]; do
    if "$ADB" -s "$serial" shell ping -c 1 -W 1 8.8.8.8 \
        > "$evidence" 2> "$error_evidence"; then
      rm -f -- "$error_evidence"
      return 0
    fi
    sleep 1
    attempts=$((attempts + 1))
  done
  fail "$label Android network did not become reachable within 30 bounded probes"
}

wait_android_ready() {
  instance_id=$1
  evidence=$2
  label=$3
  attempt=0
  while [ "$attempt" -lt 600 ]; do
    "$CTL" --data-root "$DATA_ROOT" --no-start-host show "$instance_id" > "$evidence"
    if grep -Fq '"observed": "ready"' "$evidence" &&
        grep -Fq '"adb_ready": true' "$evidence"; then
      return 0
    fi
    kill -0 "$UI_PID" 2>/dev/null ||
      fail "installed UI exited while waiting for $label Android ADB readiness"
    sleep 0.5
    attempt=$((attempt + 1))
  done
  fail "$label Android boot did not reach authenticated ADB readiness within five minutes (evidence: $evidence)"
}

verify_android_uwb_fira() {
  uwb_fira_serial=$1
  uwb_fira_prefix=$2
  uwb_fira_session_id=77
  uwb_fira_start="$OUTPUT_STAGE/${uwb_fira_prefix}-uwb-fira-start.txt"
  uwb_fira_reports="$OUTPUT_STAGE/${uwb_fira_prefix}-uwb-fira-reports.txt"
  uwb_fira_stop="$OUTPUT_STAGE/${uwb_fira_prefix}-uwb-fira-stop.txt"

  "$ADB" -s "$uwb_fira_serial" shell cmd uwb stop-all-ranging-sessions \
    > "$OUTPUT_STAGE/${uwb_fira_prefix}-uwb-fira-pre-stop.txt" 2>&1 || true
  uwb_fira_start_ok=0
  if "$ADB" -s "$uwb_fira_serial" shell cmd uwb \
      start-fira-ranging-session -i "$uwb_fira_session_id" \
      -R enabled -e none -f tof > "$uwb_fira_start" 2>&1; then
    if grep -Fq 'Ranging session opened with params:' "$uwb_fira_start" && \
        grep -Fq "Ranging session started for sessionId: $uwb_fira_session_id" \
          "$uwb_fira_start"; then
      uwb_fira_start_ok=1
    fi
  fi

  uwb_fira_report_ok=0
  uwb_fira_attempt=0
  : > "$uwb_fira_reports"
  while [ "$uwb_fira_start_ok" -eq 1 ] && [ "$uwb_fira_attempt" -lt 150 ]; do
    "$ADB" -s "$uwb_fira_serial" shell cmd uwb \
      get-ranging-session-reports "$uwb_fira_session_id" \
      > "$uwb_fira_reports" 2>&1 || true
    if grep -Fq 'RangingReport[' "$uwb_fira_reports" && \
        grep -Fq 'distance measurement: DistanceMeasurement[meters: 3.21' \
          "$uwb_fira_reports" && \
        grep -Fq 'status: 0' "$uwb_fira_reports"; then
      uwb_fira_report_ok=1
      break
    fi
    sleep 0.2
    uwb_fira_attempt=$((uwb_fira_attempt + 1))
  done

  "$ADB" -s "$uwb_fira_serial" shell cmd uwb \
    stop-ranging-session "$uwb_fira_session_id" > "$uwb_fira_stop" 2>&1 || true
  uwb_fira_stop_ok=0
  if grep -Fq 'Ranging session stopped' "$uwb_fira_stop" && \
      grep -Fq 'Ranging session closed' "$uwb_fira_stop"; then
    uwb_fira_stop_ok=1
  fi

  [ "$uwb_fira_start_ok" -eq 1 ] || \
    fail "Android UWB shell did not open and start FiRa session 77"
  [ "$uwb_fira_report_ok" -eq 1 ] || \
    fail "Android framework did not publish the controlled 321 cm FiRa report"
  [ "$uwb_fira_stop_ok" -eq 1 ] || \
    fail "Android UWB shell did not stop and close FiRa session 77"
  printf 'session_id\trequested_distance_cm\tobserved_distance_m\tmeasurement_status\treport_attempts\n' \
    > "$OUTPUT_STAGE/${uwb_fira_prefix}-uwb-fira.tsv"
  printf '77\t321\t3.21\t0\t%s\n' "$((uwb_fira_attempt + 1))" \
    >> "$OUTPUT_STAGE/${uwb_fira_prefix}-uwb-fira.tsv"
}

seed_run_retention_fixtures() {
  runs_root="$DATA_ROOT/runs/$INSTANCE_ID"
  mkdir -p "$runs_root"
  index=1
  while [ "$index" -le 23 ]; do
    suffix=$(printf '%012d' "$index")
    run_id="00000000-0000-4000-8000-$suffix"
    run_dir="$runs_root/$run_id"
    mkdir -p "$run_dir"
    cat > "$run_dir/result.json" <<EOF
{
  "schema_version": 2,
  "run_id": "$run_id",
  "instance_id": "$INSTANCE_ID",
  "started_at": "2020-01-01T00:00:00Z",
  "finished_at": "2020-01-01T00:00:01Z",
  "final_state": "stopped",
  "exit_code": 0,
  "error_code": null,
  "reason": null
}
EOF
    printf 'installed retention fixture %s\n' "$index" > "$run_dir/logcat-hvc2.txt"
    timestamp=$(printf '2020010100%02d' "$index")
    touch -t "$timestamp" "$run_dir/result.json" "$run_dir/logcat-hvc2.txt"
    touch -t "$timestamp" "$run_dir"
    index=$((index + 1))
  done
  RETENTION_PRUNED_ID=00000000-0000-4000-8000-000000000001
  RETENTION_SURVIVOR_ID=00000000-0000-4000-8000-000000000023
  RETENTION_MARKER=HD-INSTALLED-FINISHED-RUN-TAIL
  RETENTION_LEGACY_INITRD="$runs_root/$RETENTION_SURVIVOR_ID/initrd-android-hd.img"
  survivor_log="$runs_root/$RETENTION_SURVIVOR_ID/logcat-hvc2.txt"
  truncate -s 67108865 "$survivor_log"
  marker_bytes=${#RETENTION_MARKER}
  marker_offset=$((67108865 - marker_bytes))
  printf '%s' "$RETENTION_MARKER" | \
    dd of="$survivor_log" bs=1 seek="$marker_offset" conv=notrunc 2>/dev/null
  touch -t 202001010023 "$survivor_log"
  printf 'legacy reproducible patched initrd\n' > "$RETENTION_LEGACY_INITRD"
  touch -t 202001010023 "$RETENTION_LEGACY_INITRD"
  touch -t 202001010023 "$runs_root/$RETENTION_SURVIVOR_ID"
  printf 'fixture_count\tpruned_id\tsurvivor_id\toversized_log_bytes\tlegacy_ephemeral\n' \
    > "$OUTPUT_STAGE/run-retention-seed.tsv"
  printf '23\t%s\t%s\t67108865\tinitrd-android-hd.img\n' \
    "$RETENTION_PRUNED_ID" "$RETENTION_SURVIVOR_ID" \
    >> "$OUTPUT_STAGE/run-retention-seed.tsv"
}

verify_run_retention_fixtures() {
  runs_root="$DATA_ROOT/runs/$INSTANCE_ID"
  finished_count=$(find "$runs_root" -mindepth 2 -maxdepth 2 -type f \
    -name result.json | wc -l | tr -d ' ')
  [ "$finished_count" -eq 20 ] ||
    fail "installed Worker retained $finished_count finalized runs before second stop, expected 20"
  [ ! -e "$runs_root/$RETENTION_PRUNED_ID" ] ||
    fail "installed Worker did not prune the oldest finalized run"
  survivor="$runs_root/$RETENTION_SURVIVOR_ID"
  [ -d "$survivor" ] && [ ! -L "$survivor" ] ||
    fail "installed Worker pruned the recent retention fixture"
  survivor_log="$survivor/logcat-hvc2.txt"
  previous_log="$survivor/logcat-hvc2.txt.previous"
  [ "$(stat -f '%z' "$survivor_log")" -eq 0 ] ||
    fail "installed Worker did not truncate the finalized oversized log"
  [ "$(stat -f '%z' "$previous_log")" -eq 16777216 ] ||
    fail "installed Worker did not retain the bounded 16 MiB historical log tail"
  [ "$(stat -f '%Lp' "$previous_log")" = 600 ] ||
    fail "installed Worker historical log tail is not owner-only"
  marker_bytes=${#RETENTION_MARKER}
  [ "$(tail -c "$marker_bytes" "$previous_log")" = "$RETENTION_MARKER" ] ||
    fail "installed Worker historical log tail did not preserve the newest bytes"
  [ ! -e "$RETENTION_LEGACY_INITRD" ] ||
    fail "installed Worker retained a legacy reproducible patched initrd"
  printf 'finished_count\toldest_pruned\tsurvivor_compacted\tretained_tail_bytes\tlegacy_ephemeral_removed\tactive_run_protected\n' \
    > "$OUTPUT_STAGE/run-retention.tsv"
  printf '20\tpass\tpass\t16777216\tpass\tpass\n' \
    >> "$OUTPUT_STAGE/run-retention.tsv"
}

require_abs_file "$ARCHIVE" --archive
case "$ARCHIVE" in
  *.tar.xz) ;;
  *) fail "self-contained Android distribution must use sparse-preserving .tar.xz" ;;
esac
require_abs_dir "$NODE_ROOT" --node-root
require_abs_file "$NODE_ARCHIVE" --node-archive
require_abs_dir "$JAVA_HOME_INPUT" --java-home
require_abs_file "$JAVA_ARCHIVE" --java-archive
require_abs_dir "$ANDROID_BUILD_TOOLS" --android-build-tools
require_abs_file "$LOCATION_PROBE_APK" --location-probe-apk
require_abs_file "$ZSTD" --zstd
[ -x "$ZSTD" ] || fail "--zstd is not executable: $ZSTD"
[ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ] ||
  fail "installed Android smoke requires an Apple Silicon macOS host"
file "$ZSTD" | grep -q 'arm64' || fail "--zstd is not an arm64 executable"
ZSTD_VERSION=$("$ZSTD" --version | head -1)
ZSTD_SHA256=$(shasum -a 256 "$ZSTD" | awk '{print $1}')
ZSTD_BIN_DIR=$(dirname -- "$ZSTD")
case "$OUTPUT" in /*) ;; *) fail "--output must be absolute" ;; esac
[ "$DEVELOPMENT_PACKAGE" -eq 1 ] || fail "current direct Android profile requires --development-package"
[ ! -e "$OUTPUT" ] || fail "refusing to replace evidence output: $OUTPUT"
FAILURE_OUTPUT="$OUTPUT.failed"
[ ! -e "$FAILURE_OUTPUT" ] || fail "refusing to replace failure evidence: $FAILURE_OUTPUT"

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)
output_parent=$(dirname -- "$OUTPUT")
mkdir -p "$output_parent"
OUTPUT_STAGE=$(mktemp -d "$output_parent/.hd-android-distribution-evidence.XXXXXX")
INSTALL=$(mktemp -d /private/tmp/hd-android-distribution-install.XXXXXX)
DATA_ROOT=$(mktemp -d /private/tmp/hd-android-distribution-data.XXXXXX)
UI_PID=
INSTANCE_ID=
SCREEN_RECORDING_PATH=
COMPLETED=0
LOCATION_PROBE_INSTALLED=0
unset ADB_SERVER_SOCKET ANDROID_ADB_SERVER_ADDRESS
ANDROID_ADB_SERVER_PORT=$((20000 + ($$ % 20000)))
export ANDROID_ADB_SERVER_PORT

terminate_test_processes() {
  [ -n "$INSTALL" ] || return
  pids=$(pgrep -f "$INSTALL/HD.app" 2>/dev/null || true)
  [ -z "$pids" ] || kill $pids 2>/dev/null || true
  sleep 1
  pids=$(pgrep -f "$INSTALL/HD.app" 2>/dev/null || true)
  [ -z "$pids" ] || kill -KILL $pids 2>/dev/null || true
}

remove_test_screen_recording() {
  [ -n "$SCREEN_RECORDING_PATH" ] || return 0
  case "$SCREEN_RECORDING_PATH" in
    "$HOME/Movies/HD/$INSTANCE_ID-"*.mp4) ;;
    *) return 0 ;;
  esac
  if [ -f "$SCREEN_RECORDING_PATH" ] && [ ! -L "$SCREEN_RECORDING_PATH" ]; then
    rm -f -- "$SCREEN_RECORDING_PATH"
  fi
}

cleanup() {
  status=$?
  remove_test_screen_recording
  if [ "$LOCATION_PROBE_INSTALLED" -eq 1 ] && [ -n "${ADB:-}" ] && \
      [ -n "${ADB_SERIAL:-}" ] && [ -x "$ADB" ]; then
    "$ADB" -s "$ADB_SERIAL" shell am force-stop com.hd.locationprobe \
      >/dev/null 2>&1 || true
    "$ADB" -s "$ADB_SERIAL" uninstall com.hd.locationprobe \
      >/dev/null 2>&1 || true
    LOCATION_PROBE_INSTALLED=0
  fi
  if [ "$status" -ne 0 ] && [ -d "$DATA_ROOT" ]; then
    runtime_evidence="$OUTPUT_STAGE/runtime-failure"
    mkdir -p "$runtime_evidence"
    for relative in host-runtime-v2.json openapi-v2.json trusted-keys-v2.json; do
      source_path="$DATA_ROOT/$relative"
      if [ -f "$source_path" ] && [ ! -L "$source_path" ]; then
        cp -p "$source_path" "$runtime_evidence/$relative" 2>/dev/null || true
      fi
    done
    if [ -d "$DATA_ROOT/logs" ] && [ ! -L "$DATA_ROOT/logs" ]; then
      cp -Rp "$DATA_ROOT/logs" "$runtime_evidence/" 2>/dev/null || true
    fi
    if [ -n "$INSTANCE_ID" ]; then
      for relative in "runs/$INSTANCE_ID" "workers/$INSTANCE_ID"; do
        source_path="$DATA_ROOT/$relative"
        if [ -d "$source_path" ] && [ ! -L "$source_path" ]; then
          destination="$runtime_evidence/$(dirname -- "$relative")"
          mkdir -p "$destination"
          cp -Rp "$source_path" "$destination/" 2>/dev/null || true
        fi
      done
    fi
    find "$runtime_evidence" -type f -exec stat -f '%z %N' {} \; \
      > "$runtime_evidence/files.txt" 2>/dev/null || true
  fi
  if [ -x "$INSTALL/HD.app/Contents/MacOS/hdctl" ]; then
    "$INSTALL/HD.app/Contents/MacOS/hdctl" --data-root "$DATA_ROOT" \
      --no-start-host shutdown --stop-all >/dev/null 2>&1 || true
  fi
  if [ -x "$INSTALL/HD.app/Contents/MacOS/adb" ]; then
    "$INSTALL/HD.app/Contents/MacOS/adb" kill-server >/dev/null 2>&1 || true
  fi
  [ -z "$UI_PID" ] || kill "$UI_PID" 2>/dev/null || true
  terminate_test_processes
  case "$INSTALL" in /private/tmp/hd-android-distribution-install.*) rm -rf -- "$INSTALL" ;; esac
  case "$DATA_ROOT" in /private/tmp/hd-android-distribution-data.*) rm -rf -- "$DATA_ROOT" ;; esac
  case "$OUTPUT_STAGE" in
    "$output_parent"/.hd-android-distribution-evidence.*)
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
  > "$OUTPUT_STAGE/distribution-verify.log" 2>&1
(cd "$INSTALL" && COPYFILE_DISABLE=1 bsdtar --safe-writes --no-xattrs -xJf "$ARCHIVE")

APP="$INSTALL/HD.app"
UI="$APP/Contents/MacOS/HD"
CTL="$APP/Contents/MacOS/hdctl"
ADB="$APP/Contents/MacOS/adb"
for binary in "$UI" "$CTL" "$ADB"; do
  require_abs_file "$binary" installed-binary
  [ -x "$binary" ] || fail "installed binary is not executable: $binary"
done
SIGNED_ARTIFACT_STORE="$APP/Contents/Resources/products/android/artifact-store-v2"
DIRECT_DEVELOPMENT_MARKER="$APP/Contents/Resources/products/android/development-direct-v1.plist"
ARTIFACT_DISTRIBUTION=
ARTIFACT_STORE=
GUEST_DIGEST=
HOST_DIGEST=
ANDROID_AGGREGATE_SHA256=
ARTIFACT_DEVELOPMENT_BYPASS=
if [ -d "$SIGNED_ARTIFACT_STORE" ] && [ ! -L "$SIGNED_ARTIFACT_STORE" ]; then
  [ ! -e "$DIRECT_DEVELOPMENT_MARKER" ] ||
    fail "installed app must not mix signed and direct development Android distributions"
  EMBEDDED_TRUST="$SIGNED_ARTIFACT_STORE/trusted-keys-v2.json"
  require_abs_file "$EMBEDDED_TRUST" embedded-development-trust-store
  "$CTL" verify-android-artifact-store \
    --store-root "$SIGNED_ARTIFACT_STORE" \
    --trust-store "$EMBEDDED_TRUST" \
    --channel development \
    > "$OUTPUT_STAGE/signed-android-verification.json"
  require_contains '"exact_closure": true' \
    "$OUTPUT_STAGE/signed-android-verification.json" \
    "installed signed Android store did not have an exact closure"
  require_contains '"signature_verified": true' \
    "$OUTPUT_STAGE/signed-android-verification.json" \
    "installed signed Android store did not verify its signatures"
  ARTIFACT_DISTRIBUTION=signed-artifact-store-v2
  ARTIFACT_STORE=$SIGNED_ARTIFACT_STORE
  GUEST_DIGEST=$(json_string guest_bundle_digest "$OUTPUT_STAGE/signed-android-verification.json")
  HOST_DIGEST=$(json_string host_bundle_digest "$OUTPUT_STAGE/signed-android-verification.json")
  ANDROID_AGGREGATE_SHA256=$(json_string rootfs_sha256 "$OUTPUT_STAGE/signed-android-verification.json")
  ARTIFACT_DEVELOPMENT_BYPASS=true
else
  require_abs_file "$DIRECT_DEVELOPMENT_MARKER" android-development-marker
  ARTIFACT_DISTRIBUTION=direct-development-v1
  ARTIFACT_DEVELOPMENT_BYPASS=true
fi
codesign --verify --deep --strict "$APP" > "$OUTPUT_STAGE/codesign.log" 2>&1

mkdir -p "$OUTPUT_STAGE/logs"
"$UI" --data-root "$DATA_ROOT" \
  > "$OUTPUT_STAGE/logs/ui.stdout" 2> "$OUTPUT_STAGE/logs/ui.stderr" &
UI_PID=$!
printf '%s\n' "$UI_PID" > "$OUTPUT_STAGE/ui.pid"
ready=0
attempt=0
while [ "$attempt" -lt 300 ]; do
  if "$CTL" --data-root "$DATA_ROOT" --no-start-host health \
      > "$OUTPUT_STAGE/health.json" 2> "$OUTPUT_STAGE/health.stderr"; then
    ready=1
    break
  fi
  kill -0 "$UI_PID" 2>/dev/null || fail "installed UI exited before Host became healthy"
  sleep 0.1
  attempt=$((attempt + 1))
done
[ "$ready" -eq 1 ] || fail "installed Host did not become healthy within 30 seconds"

if [ "$ARTIFACT_DISTRIBUTION" = direct-development-v1 ]; then
  ARTIFACT_STORE="$DATA_ROOT/cache/direct-dev-artifacts-v2"
  for manifest in "$ARTIFACT_STORE"/bundles/*/manifest-v2.json; do
    require_abs_file "$manifest" direct-development-manifest
    kind=$(json_string kind "$manifest")
    digest=$(json_string digest "$manifest")
    [ "${#digest}" -eq 64 ] || fail "direct development manifest has an invalid digest: $manifest"
    case "$kind" in
      guest)
        [ -z "$GUEST_DIGEST" ] || fail "direct development store contains multiple Guest bundles"
        GUEST_DIGEST=$digest
        ;;
      host_tools)
        [ -z "$HOST_DIGEST" ] || fail "direct development store contains multiple Host bundles"
        HOST_DIGEST=$digest
        ;;
      *) fail "direct development store contains an unexpected bundle kind: $kind" ;;
    esac
  done
  ANDROID_MANIFEST="$APP/Contents/Resources/products/android/vsoc_arm64_only/direct-linux/runtime-files-v1.sha256"
  ANDROID_AGGREGATE_SHA256=$(sed -n \
    's/^\([0-9a-f][0-9a-f]*\)  aggregate_android[.]img$/\1/p' "$ANDROID_MANIFEST")
fi
[ -n "$GUEST_DIGEST" ] && [ -n "$HOST_DIGEST" ] ||
  fail "installed UI did not expose both Android artifact bundles"
[ "${#ANDROID_AGGREGATE_SHA256}" -eq 64 ] ||
  fail "installed Android distribution did not expose a valid aggregate image digest"

INSTANCE_ID=$(uuidgen | tr '[:upper:]' '[:lower:]')
MARKER="installed-v36-$(date +%s)"
cat > "$OUTPUT_STAGE/spec.json" <<EOF
{
  "schema_version": 2,
  "id": "$INSTANCE_ID",
  "name": "Installed Android 15 Distribution",
  "guest_kind": "android",
  "microdroid": null,
  "cpu_count": 4,
  "memory_mib": 4096,
  "display": {
    "width": 1080,
    "height": 1920,
    "dpi": 420,
    "refresh_rate_hz": 60,
    "orientation": "portrait",
    "vsync": "on",
    "show_host_fps": true
  },
  "adb": { "mode": "loopback", "host_port": null, "executable": null },
  "artifacts": {
    "store_root": "$ARTIFACT_STORE",
    "guest_bundle_digest": "$GUEST_DIGEST",
    "host_bundle_digest": "$HOST_DIGEST"
  },
  "boot": {
    "kernel_log_level": 4,
    "panic_timeout_seconds": 5,
    "boot_animation": true
  },
  "devices": {
    "bluetooth": true,
    "nfc": true,
    "uwb": true,
    "modem": true,
    "gnss": true,
    "sensors": true,
    "network": true,
    "audio": true,
    "camera": true,
    "power": true
  },
  "restart_policy": "never",
  "labels": {
    "artifact_source": "installed-distribution",
    "artifact_distribution": "$ARTIFACT_DISTRIBUTION",
    "data_profile": "development-unencrypted"
  }
}
EOF

"$CTL" --data-root "$DATA_ROOT" --no-start-host create \
  --spec "$OUTPUT_STAGE/spec.json" > "$OUTPUT_STAGE/create.json"
"$CTL" --data-root "$DATA_ROOT" --no-start-host capabilities "$INSTANCE_ID" \
  > "$OUTPUT_STAGE/capabilities.json"
awk '
  /"id": "host.resources"/ { capture = 1 }
  capture { print }
  capture && /"properties": \{/ { properties = 1 }
  capture && properties && /^      }/ { exit }
' "$OUTPUT_STAGE/capabilities.json" > "$OUTPUT_STAGE/host-resource-capability.json"
if grep -Fq '"status": "blocked"' "$OUTPUT_STAGE/host-resource-capability.json"; then
  fail "installed Android host resource admission blocked before artifact verification; free the reported CPU, memory or disk capacity and rerun (evidence: $OUTPUT_STAGE/host-resource-capability.json)"
fi
require_contains '"status": "supported"' \
  "$OUTPUT_STAGE/host-resource-capability.json" \
  "installed Android host resource probe was neither supported nor explicitly blocked"
require_contains "\"development_bypass\": $ARTIFACT_DEVELOPMENT_BYPASS" \
  "$OUTPUT_STAGE/capabilities.json" \
  "installed Android capability reported the wrong signature-bypass state"
require_contains '"id": "display.zero_copy"' "$OUTPUT_STAGE/capabilities.json" \
  "installed Android capability omitted the macOS zero-copy display probe"
require_contains '"disk_requirement_mode": "new_instance_storage"' \
  "$OUTPUT_STAGE/capabilities.json" \
  "new Android storage did not reserve the full provisioning capacity"
require_contains '"required_disk_bytes": "10737418240"' \
  "$OUTPUT_STAGE/capabilities.json" \
  "new Android storage did not require the configured 10 GiB capacity"
awk '
  /"id": "nfc"/ { capture = 1 }
  capture { print }
  capture && /"detail":/ { exit }
' "$OUTPUT_STAGE/capabilities.json" > "$OUTPUT_STAGE/nfc-capability.json"
require_contains '"backend": "official_component"' \
  "$OUTPUT_STAGE/nfc-capability.json" \
  "installed macOS NFC capability did not select the formal Casimir component"
require_contains '"runtime_control"' \
  "$OUTPUT_STAGE/nfc-capability.json" \
  "installed macOS NFC capability did not expose runtime control"
require_contains '"controllable": true' \
  "$OUTPUT_STAGE/nfc-capability.json" \
  "installed macOS NFC capability was not controllable"
awk '
  /"id": "bluetooth"/ { capture = 1 }
  capture { print }
  capture && /"detail":/ { exit }
' "$OUTPUT_STAGE/capabilities.json" > "$OUTPUT_STAGE/bluetooth-capability.json"
require_contains '"backend": "official_component"' \
  "$OUTPUT_STAGE/bluetooth-capability.json" \
  "installed macOS Bluetooth capability did not select the formal RootCanal component"
require_contains '"runtime_control"' \
  "$OUTPUT_STAGE/bluetooth-capability.json" \
  "installed macOS Bluetooth capability did not expose runtime control"
require_contains '"controllable": true' \
  "$OUTPUT_STAGE/bluetooth-capability.json" \
  "installed macOS Bluetooth capability was not controllable"
awk '
  /"id": "uwb"/ { capture = 1 }
  capture { print }
  capture && /"detail":/ { exit }
' "$OUTPUT_STAGE/capabilities.json" > "$OUTPUT_STAGE/uwb-capability.json"
require_contains '"backend": "official_component"' \
  "$OUTPUT_STAGE/uwb-capability.json" \
  "installed macOS UWB capability did not select the formal UCI component"
require_contains '"fira_v2"' \
  "$OUTPUT_STAGE/uwb-capability.json" \
  "installed macOS UWB capability did not expose the FiRa v2 boundary"
awk '
  /"id": "modem"/ { capture = 1 }
  capture { print }
  capture && /"detail":/ { exit }
' "$OUTPUT_STAGE/capabilities.json" > "$OUTPUT_STAGE/modem-capability.json"
require_contains '"backend": "official_component"' \
  "$OUTPUT_STAGE/modem-capability.json" \
  "installed macOS modem capability did not select the formal component"
require_contains '"guest_vsock"' \
  "$OUTPUT_STAGE/modem-capability.json" \
  "installed macOS modem capability did not expose the Guest vsock boundary"

"$CTL" --data-root "$DATA_ROOT" --no-start-host start "$INSTANCE_ID" \
  > "$OUTPUT_STAGE/first-start.json"
require_contains '"state": "succeeded"' "$OUTPUT_STAGE/first-start.json" \
  "first Android start operation did not succeed"
wait_android_ready "$INSTANCE_ID" "$OUTPUT_STAGE/first-ready.json" first
require_contains '"observed": "ready"' "$OUTPUT_STAGE/first-ready.json" \
  "first Android boot did not reach Ready"
require_contains '"adb_ready": true' "$OUTPUT_STAGE/first-ready.json" \
  "first Android boot reached Ready without authenticated ADB"
ADB_SERIAL=$(json_string adb_serial "$OUTPUT_STAGE/first-ready.json")
[ -n "$ADB_SERIAL" ] || fail "first Android boot did not publish an ADB serial"
FIRST_GENERATION=$(json_number frame_generation "$OUTPUT_STAGE/first-ready.json")
[ -n "$FIRST_GENERATION" ] && [ "$FIRST_GENERATION" -gt 0 ] ||
  fail "first Android boot did not publish a frame generation"
"$ADB" -s "$ADB_SERIAL" shell getprop sys.boot_completed \
  > "$OUTPUT_STAGE/first-sys-boot-completed.txt"
require_contains 1 "$OUTPUT_STAGE/first-sys-boot-completed.txt" \
  "first Android boot did not report sys.boot_completed=1"
"$ADB" -s "$ADB_SERIAL" shell getprop init.svc.bootanim \
  > "$OUTPUT_STAGE/first-bootanim.txt"
require_contains stopped "$OUTPUT_STAGE/first-bootanim.txt" \
  "first Android boot animation did not stop"
"$ADB" -s "$ADB_SERIAL" shell \
  "echo '$MARKER' > /data/local/tmp/hd-installed-marker"
wait_android_network "$ADB_SERIAL" "$OUTPUT_STAGE/first-network.txt" first
"$ADB" -s "$ADB_SERIAL" shell ip -o link show \
  > "$OUTPUT_STAGE/first-network-links.txt"
"$ADB" -s "$ADB_SERIAL" shell getprop ro.vendor.virtwifi.port \
  > "$OUTPUT_STAGE/first-virtwifi-port.txt"
"$ADB" -s "$ADB_SERIAL" shell dumpsys connectivity \
  > "$OUTPUT_STAGE/first-connectivity.txt"

ACTIVE_RUN_ID=$(json_string active_run_id "$OUTPUT_STAGE/first-ready.json")
[ -n "$ACTIVE_RUN_ID" ] || fail "first Android boot did not publish an active run id"
BUGREPORT_STARTED=$(date +%s)
"$CTL" --data-root "$DATA_ROOT" --no-start-host bugreport "$INSTANCE_ID" \
  > "$OUTPUT_STAGE/android-bugreport-record.json"
BUGREPORT_FINISHED=$(date +%s)
BUGREPORT_DURATION_MS=$(((BUGREPORT_FINISHED - BUGREPORT_STARTED) * 1000))
BUGREPORT_ID=$(json_string id "$OUTPUT_STAGE/android-bugreport-record.json")
BUGREPORT_INSTANCE_ID=$(json_string instance_id "$OUTPUT_STAGE/android-bugreport-record.json")
BUGREPORT_RUN_ID=$(json_string run_id "$OUTPUT_STAGE/android-bugreport-record.json")
BUGREPORT_PATH=$(json_string path "$OUTPUT_STAGE/android-bugreport-record.json")
BUGREPORT_SIZE=$(json_number size_bytes "$OUTPUT_STAGE/android-bugreport-record.json")
BUGREPORT_SHA256=$(json_string sha256 "$OUTPUT_STAGE/android-bugreport-record.json")
[ -n "$BUGREPORT_ID" ] && [ "$BUGREPORT_INSTANCE_ID" = "$INSTANCE_ID" ] && \
  [ "$BUGREPORT_RUN_ID" = "$ACTIVE_RUN_ID" ] || \
  fail "Android bugreport record was not bound to the current instance and run"
require_contains '"contains_sensitive_data": true' \
  "$OUTPUT_STAGE/android-bugreport-record.json" \
  "Android bugreport record did not disclose its sensitive-data boundary"
case "$BUGREPORT_PATH" in
  "$DATA_ROOT/diagnostics/android-bugreport-$INSTANCE_ID-"*.zip) ;;
  *) fail "Android bugreport escaped the managed diagnostics directory: $BUGREPORT_PATH" ;;
esac
require_abs_file "$BUGREPORT_PATH" android-bugreport
[ "$(stat -f '%Lp' "$BUGREPORT_PATH")" = 600 ] || \
  fail "Android bugreport is not owner-only"
[ -n "$BUGREPORT_SIZE" ] && [ "$BUGREPORT_SIZE" -ge 22 ] && \
  [ "$BUGREPORT_SIZE" -le 268435456 ] && \
  [ "$BUGREPORT_SIZE" -eq "$(stat -f '%z' "$BUGREPORT_PATH")" ] || \
  fail "Android bugreport size is outside the product boundary or does not match the file"
[ "$(shasum -a 256 "$BUGREPORT_PATH" | awk '{print $1}')" = "$BUGREPORT_SHA256" ] || \
  fail "Android bugreport SHA-256 did not round-trip"
/usr/bin/bsdtar -tf "$BUGREPORT_PATH" > "$OUTPUT_STAGE/android-bugreport-members.txt"
grep -Eq '(^|/)bugreport-.*\.txt$' "$OUTPUT_STAGE/android-bugreport-members.txt" || \
  fail "Android bugreport ZIP does not contain the main dumpstate text"
cp -p -- "$BUGREPORT_PATH" "$OUTPUT_STAGE/android-bugreport.zip"
chmod 600 "$OUTPUT_STAGE/android-bugreport.zip"
cmp -s "$BUGREPORT_PATH" "$OUTPUT_STAGE/android-bugreport.zip" || \
  fail "preserved Android bugreport differs from the verified source"
"$CTL" --data-root "$DATA_ROOT" --no-start-host show "$INSTANCE_ID" \
  > "$OUTPUT_STAGE/post-bugreport-ready.json"
require_contains '"observed": "ready"' "$OUTPUT_STAGE/post-bugreport-ready.json" \
  "Android instance left Ready after bugreport"
require_contains '"adb_ready": true' "$OUTPUT_STAGE/post-bugreport-ready.json" \
  "ADB did not remain Ready after bugreport"

# Establish a permanent rotation baseline before the rest of the installed control gate.
for rotation_case in landscape:1 portrait:0; do
  orientation=${rotation_case%:*}
  expected_rotation=${rotation_case#*:}
  "$CTL" --data-root "$DATA_ROOT" --no-start-host action "$INSTANCE_ID" \
    rotate "$orientation" \
    > "$OUTPUT_STAGE/pre-recording-rotate-$orientation-$expected_rotation.json"
  "$ADB" -s "$ADB_SERIAL" shell dumpsys window \
    > "$OUTPUT_STAGE/pre-recording-rotate-$orientation-$expected_rotation-window.txt"
  grep -Eq "mRotation=$expected_rotation([[:space:]]|$)" \
    "$OUTPUT_STAGE/pre-recording-rotate-$orientation-$expected_rotation-window.txt" ||
    fail "Android WindowManager did not apply pre-recording $orientation rotation $expected_rotation"
done

# Guest screenrecord remains disabled because its virtual-display teardown can lock later Android
# display transitions on this profile. The macOS product path instead uses gfxstream's dedicated
# recording readback callback and an AVFoundation MP4 writer. Readback is allowed only while the
# user is actively recording; the normal display path remains zero-copy.
"$CTL" --data-root "$DATA_ROOT" --no-start-host screen-record-start \
  "$INSTANCE_ID" --max-duration-seconds 10 \
  > "$OUTPUT_STAGE/screen-recording-start.json"
require_contains '"max_duration_seconds": 10' \
  "$OUTPUT_STAGE/screen-recording-start.json" \
  "macOS host screen recording did not publish its bounded status"
for key in recent home back; do
  "$CTL" --data-root "$DATA_ROOT" --no-start-host action "$INSTANCE_ID" key "$key" \
    > "$OUTPUT_STAGE/screen-recording-key-$key.json"
  sleep 0.5
done
sleep 2
"$CTL" --data-root "$DATA_ROOT" --no-start-host screen-record-stop "$INSTANCE_ID" \
  > "$OUTPUT_STAGE/screen-recording-stop.json"
SCREEN_RECORDING_PATH=$(json_string path "$OUTPUT_STAGE/screen-recording-stop.json")
SCREEN_RECORDING_SIZE=$(json_number size_bytes "$OUTPUT_STAGE/screen-recording-stop.json")
SCREEN_RECORDING_WALL_DURATION_MILLIS=$(json_number duration_millis \
  "$OUTPUT_STAGE/screen-recording-stop.json")
SCREEN_RECORDING_SHA256=$(json_string sha256 "$OUTPUT_STAGE/screen-recording-stop.json")
case "$SCREEN_RECORDING_PATH" in
  "$HOME/Movies/HD/$INSTANCE_ID-"*.mp4) ;;
  *) fail "macOS host screen recording escaped the per-user HD recording directory" ;;
esac
require_abs_file "$SCREEN_RECORDING_PATH" "macOS host screen recording"
[ -n "$SCREEN_RECORDING_SIZE" ] && [ "$SCREEN_RECORDING_SIZE" -gt 1024 ] ||
  fail "macOS host screen recording did not produce a non-empty MP4"
SCREEN_RECORDING_ACTUAL_SHA256=$(shasum -a 256 "$SCREEN_RECORDING_PATH" | awk '{print $1}')
[ "$SCREEN_RECORDING_ACTUAL_SHA256" = "$SCREEN_RECORDING_SHA256" ] ||
  fail "macOS host screen recording SHA-256 did not match the Worker result"
SCREEN_RECORDING_METRICS=$(
  /usr/bin/python3 - "$SCREEN_RECORDING_PATH" <<'PY'
import pathlib
import struct
import sys

path = pathlib.Path(sys.argv[1])
required = {b"ftyp", b"mdat", b"moov"}
seen = set()
sample_counts = []
movie_durations_millis = []
container_kinds = {b"moov", b"trak", b"mdia", b"minf", b"stbl"}


def walk_boxes(stream, start, end):
    position = start
    while position < end:
        stream.seek(position)
        header = stream.read(8)
        if len(header) != 8:
            raise SystemExit("truncated MP4 box header")
        size, kind = struct.unpack(">I4s", header)
        header_size = 8
        if size == 1:
            extended = stream.read(8)
            if len(extended) != 8:
                raise SystemExit("truncated extended MP4 box header")
            size = struct.unpack(">Q", extended)[0]
            header_size = 16
        elif size == 0:
            size = end - position
        if size < header_size or position + size > end:
            raise SystemExit("invalid MP4 box size")
        payload_start = position + header_size
        payload_end = position + size
        if start == 0:
            seen.add(kind)
        if kind == b"stsz":
            stream.seek(payload_start)
            payload = stream.read(12)
            if len(payload) != 12:
                raise SystemExit("truncated MP4 sample-size box")
            sample_counts.append(struct.unpack(">I", payload[8:12])[0])
        if kind == b"mvhd":
            stream.seek(payload_start)
            payload = stream.read(min(32, payload_end - payload_start))
            if not payload:
                raise SystemExit("truncated MP4 movie header")
            version = payload[0]
            if version == 0 and len(payload) >= 20:
                timescale = struct.unpack(">I", payload[12:16])[0]
                duration = struct.unpack(">I", payload[16:20])[0]
            elif version == 1 and len(payload) >= 32:
                timescale = struct.unpack(">I", payload[20:24])[0]
                duration = struct.unpack(">Q", payload[24:32])[0]
            else:
                raise SystemExit("unsupported or truncated MP4 movie header")
            if timescale == 0:
                raise SystemExit("MP4 movie header has a zero timescale")
            movie_durations_millis.append(round(duration * 1000 / timescale))
        if kind in container_kinds:
            walk_boxes(stream, payload_start, payload_end)
        position = payload_end


with path.open("rb") as stream:
    walk_boxes(stream, 0, path.stat().st_size)
missing = required - seen
if missing:
    raise SystemExit(f"missing MP4 boxes: {sorted(missing)!r}")
sample_count = max(sample_counts, default=0)
if sample_count < 2:
    raise SystemExit(f"MP4 contains too few video samples: {sample_count}")
media_duration_millis = max(movie_durations_millis, default=0)
if media_duration_millis <= 0:
    raise SystemExit("MP4 has no positive movie duration")
print(sample_count, media_duration_millis)
PY
)
SCREEN_RECORDING_SAMPLE_COUNT=${SCREEN_RECORDING_METRICS%% *}
SCREEN_RECORDING_MEDIA_DURATION_MILLIS=${SCREEN_RECORDING_METRICS#* }
[ -n "$SCREEN_RECORDING_WALL_DURATION_MILLIS" ] &&
  [ "$SCREEN_RECORDING_WALL_DURATION_MILLIS" -gt 0 ] ||
  fail "macOS host screen recording did not publish a positive wall duration"
cp -p -- "$SCREEN_RECORDING_PATH" "$OUTPUT_STAGE/screen-recording.mp4"
chmod 600 "$OUTPUT_STAGE/screen-recording.mp4"
cmp -s "$SCREEN_RECORDING_PATH" "$OUTPUT_STAGE/screen-recording.mp4" ||
  fail "macOS host screen recording evidence copy did not match the original"
printf 'platform\tguest_screenrecord\thost_backend\treadback_scope\tsize_bytes\tsample_count\twall_duration_millis\tmedia_duration_millis\tevidence\trelease_state\n' \
  > "$OUTPUT_STAGE/screen-recording-capability.tsv"
printf 'macos\tdisabled\tgfxstream-avfoundation-hardware-h264\trecording-only\t%s\t%s\t%s\t%s\tscreen-recording.mp4\tmeasured\n' \
  "$SCREEN_RECORDING_SIZE" "$SCREEN_RECORDING_SAMPLE_COUNT" \
  "$SCREEN_RECORDING_WALL_DURATION_MILLIS" "$SCREEN_RECORDING_MEDIA_DURATION_MILLIS" \
  >> "$OUTPUT_STAGE/screen-recording-capability.tsv"
[ $((SCREEN_RECORDING_MEDIA_DURATION_MILLIS * 100)) -ge \
    $((SCREEN_RECORDING_WALL_DURATION_MILLIS * 75)) ] ||
  fail "macOS host screen recording compressed static wall time"
[ "$SCREEN_RECORDING_MEDIA_DURATION_MILLIS" -le \
    $((SCREEN_RECORDING_WALL_DURATION_MILLIS + 1000)) ] ||
  fail "macOS host screen recording media duration exceeded its wall duration"
printf 'platform\tguest_screenrecord\thost_backend\treadback_scope\tsize_bytes\tsample_count\twall_duration_millis\tmedia_duration_millis\tevidence\trelease_state\n' \
  > "$OUTPUT_STAGE/screen-recording-capability.tsv"
printf 'macos\tdisabled\tgfxstream-avfoundation-hardware-h264\trecording-only\t%s\t%s\t%s\t%s\tscreen-recording.mp4\tpass\n' \
  "$SCREEN_RECORDING_SIZE" "$SCREEN_RECORDING_SAMPLE_COUNT" \
  "$SCREEN_RECORDING_WALL_DURATION_MILLIS" "$SCREEN_RECORDING_MEDIA_DURATION_MILLIS" \
  >> "$OUTPUT_STAGE/screen-recording-capability.tsv"

# AOSP Android 15 control-panel parity. Every typed action below must cross the
# installed Host/Worker boundary. Rotation, battery and network have an independent
# Android framework/kernel readback; location and sensors additionally prove that
# the real Cuttlefish HAL opened and used their fixed virtio-console channels.
: > "$OUTPUT_STAGE/android-aosp-controls.tsv"
printf 'control\trequested\tguest_readback\n' \
  >> "$OUTPUT_STAGE/android-aosp-controls.tsv"
for rotation_case in portrait:0 landscape:1 reverse-portrait:2 reverse-landscape:3 portrait:0; do
  orientation=${rotation_case%:*}
  expected_rotation=${rotation_case#*:}
  "$CTL" --data-root "$DATA_ROOT" --no-start-host action "$INSTANCE_ID" \
    rotate "$orientation" \
    > "$OUTPUT_STAGE/rotate-$orientation-$expected_rotation.json"
  "$ADB" -s "$ADB_SERIAL" shell dumpsys window \
    > "$OUTPUT_STAGE/rotate-$orientation-$expected_rotation-window.txt"
  grep -Eq "mRotation=$expected_rotation([[:space:]]|$)" \
    "$OUTPUT_STAGE/rotate-$orientation-$expected_rotation-window.txt" ||
    fail "Android WindowManager did not apply $orientation rotation $expected_rotation"
  printf 'rotation\t%s\tmRotation=%s\n' "$orientation" "$expected_rotation" \
    >> "$OUTPUT_STAGE/android-aosp-controls.tsv"
done

for key in home recent back volume-up volume-down; do
  "$CTL" --data-root "$DATA_ROOT" --no-start-host action "$INSTANCE_ID" key "$key" \
    > "$OUTPUT_STAGE/key-$key.json"
done
"$ADB" -s "$ADB_SERIAL" shell dumpsys window \
  > "$OUTPUT_STAGE/key-navigation-window.txt"
"$CTL" --data-root "$DATA_ROOT" --no-start-host action "$INSTANCE_ID" key power \
  > "$OUTPUT_STAGE/key-power-off.json"
sleep 0.5
"$CTL" --data-root "$DATA_ROOT" --no-start-host action "$INSTANCE_ID" key power \
  > "$OUTPUT_STAGE/key-power-on.json"
sleep 0.5
"$ADB" -s "$ADB_SERIAL" shell dumpsys power \
  > "$OUTPUT_STAGE/key-power-readback.txt"
grep -Eq 'mWakefulness=Awake|Wakefulness: Awake' \
  "$OUTPUT_STAGE/key-power-readback.txt" ||
  fail "Android power button round trip did not restore the display to Awake"
printf 'navigation_keys\thome,recent,back,volume-up,volume-down,power\tframework-and-power-readback\n' \
  >> "$OUTPUT_STAGE/android-aosp-controls.tsv"

"$CTL" --data-root "$DATA_ROOT" --no-start-host action "$INSTANCE_ID" \
  battery 73 --charging --temperature-deci-celsius 321 \
  > "$OUTPUT_STAGE/battery-action.json"
"$ADB" -s "$ADB_SERIAL" shell dumpsys battery \
  > "$OUTPUT_STAGE/battery-readback.txt"
require_contains 'AC powered: true' "$OUTPUT_STAGE/battery-readback.txt" \
  "Android battery charging state did not match the typed action"
require_contains 'level: 73' "$OUTPUT_STAGE/battery-readback.txt" \
  "Android battery level did not match the typed action"
require_contains 'temperature: 321' "$OUTPUT_STAGE/battery-readback.txt" \
  "Android battery temperature did not match the typed action"
printf 'battery\tlevel=73,charging=true,temperature=321\tframework-match\n' \
  >> "$OUTPUT_STAGE/android-aosp-controls.tsv"

"$CTL" --data-root "$DATA_ROOT" --no-start-host action "$INSTANCE_ID" \
  network 25 10 --bandwidth-kbps 50000 \
  > "$OUTPUT_STAGE/network-action.json"
NETWORK_INTERFACE=
for candidate in eth2 eth1 eth0; do
  "$ADB" -s "$ADB_SERIAL" shell su 0 tc qdisc show dev "$candidate" \
    > "$OUTPUT_STAGE/network-qdisc-$candidate.txt" 2>&1 || true
  if grep -Fq 'qdisc netem' "$OUTPUT_STAGE/network-qdisc-$candidate.txt"; then
    NETWORK_INTERFACE=$candidate
    cp "$OUTPUT_STAGE/network-qdisc-$candidate.txt" \
      "$OUTPUT_STAGE/network-qdisc-readback.txt"
    break
  fi
done
[ -n "$NETWORK_INTERFACE" ] ||
  fail "Android eth0/eth1/eth2 did not expose the netem qdisc verified by the runtime action"
require_contains 'qdisc netem' "$OUTPUT_STAGE/network-qdisc-readback.txt" \
  "Android data interface did not install the requested netem qdisc"
require_contains 'delay 25.0ms' "$OUTPUT_STAGE/network-qdisc-readback.txt" \
  "Android data interface did not apply the requested latency"
require_contains 'loss 0.1%' "$OUTPUT_STAGE/network-qdisc-readback.txt" \
  "Android data interface did not apply the requested packet loss"
printf 'network\tlatency=25ms,loss=0.1%%,bandwidth=50000kbps\tkernel-netem-match\n' \
  >> "$OUTPUT_STAGE/android-aosp-controls.tsv"
"$CTL" --data-root "$DATA_ROOT" --no-start-host action "$INSTANCE_ID" \
  network 0 0 > "$OUTPUT_STAGE/network-reset.json"
wait_android_network "$ADB_SERIAL" "$OUTPUT_STAGE/network-reset-reachable.txt" reset

DEVICE_SIM_LAUNCH=$(find "$DATA_ROOT/runs/$INSTANCE_ID" -type f \
  -name 'hd-device-sim-launch-v2.json' -print -quit)
[ -n "$DEVICE_SIM_LAUNCH" ] || fail "Worker did not create an hd-device-sim launch contract"
GUEST_LOCATION_OUTPUT=$(awk '
  /"location": \{/ { capture = 1; next }
  capture && /"guest_output":/ {
    value = $0
    sub(/^[^:]*:[[:space:]]*"/, "", value)
    sub(/",?[[:space:]]*$/, "", value)
    print value
    exit
  }
' "$DEVICE_SIM_LAUNCH")
require_abs_file "$GUEST_LOCATION_OUTPUT" fixed-location-guest-output
LOCATION_DELIVERY_EVIDENCE=$(dirname -- "$DEVICE_SIM_LAUNCH")/location-delivery-v1.json
require_abs_file "$LOCATION_DELIVERY_EVIDENCE" fixed-location-delivery-evidence
"$ADB" -s "$ADB_SERIAL" install -r -t "$LOCATION_PROBE_APK" \
  > "$OUTPUT_STAGE/location-probe-install.txt" 2>&1
require_contains 'Success' "$OUTPUT_STAGE/location-probe-install.txt" \
  "location framework probe APK installation failed"
LOCATION_PROBE_INSTALLED=1
"$ADB" -s "$ADB_SERIAL" shell pm grant com.hd.locationprobe \
  android.permission.ACCESS_COARSE_LOCATION
"$ADB" -s "$ADB_SERIAL" shell pm grant com.hd.locationprobe \
  android.permission.ACCESS_FINE_LOCATION
"$ADB" -s "$ADB_SERIAL" shell cmd location set-location-enabled true
"$ADB" -s "$ADB_SERIAL" shell am force-stop com.hd.locationprobe
"$ADB" -s "$ADB_SERIAL" shell run-as com.hd.locationprobe \
  rm -f files/location.txt
"$ADB" -s "$ADB_SERIAL" shell input keyevent KEYCODE_WAKEUP
"$ADB" -s "$ADB_SERIAL" shell wm dismiss-keyguard
"$ADB" -s "$ADB_SERIAL" shell am start -W \
  -n com.hd.locationprobe/.LocationProbeActivity \
  --es expected_latitude 37.4219999 \
  --es expected_longitude -122.0840575 \
  --es expected_altitude 123.456 \
  --es expected_accuracy 7.890 \
  > "$OUTPUT_STAGE/location-probe-start.txt"
"$CTL" --data-root "$DATA_ROOT" --no-start-host action "$INSTANCE_ID" \
  location --altitude-mm 123456 --accuracy-mm 7890 -- 374219999 -1220840575 \
  > "$OUTPUT_STAGE/location-action.json"
location_probe_matched=0
location_probe_attempt=0
while [ "$location_probe_attempt" -lt 80 ]; do
  "$ADB" -s "$ADB_SERIAL" shell run-as com.hd.locationprobe \
    cat files/location.txt > "$OUTPUT_STAGE/location-framework-callback.txt" 2>/dev/null || true
  if grep -Fq 'status=match' "$OUTPUT_STAGE/location-framework-callback.txt" && \
      grep -Fq 'provider=gps' "$OUTPUT_STAGE/location-framework-callback.txt" && \
      grep -Fq 'latitude=37.4219999' "$OUTPUT_STAGE/location-framework-callback.txt" && \
      grep -Fq 'longitude=-122.0840575' "$OUTPUT_STAGE/location-framework-callback.txt" && \
      grep -Fq 'altitude=123.456' "$OUTPUT_STAGE/location-framework-callback.txt" && \
      grep -Fq 'accuracy=7.890' "$OUTPUT_STAGE/location-framework-callback.txt" && \
      grep -Fq 'has_altitude=true' "$OUTPUT_STAGE/location-framework-callback.txt" && \
      grep -Fq 'has_accuracy=true' "$OUTPUT_STAGE/location-framework-callback.txt"; then
    location_probe_matched=1
    break
  fi
  sleep 0.25
  location_probe_attempt=$((location_probe_attempt + 1))
done
[ "$location_probe_matched" -eq 1 ] || {
  "$ADB" -s "$ADB_SERIAL" shell dumpsys location \
    > "$OUTPUT_STAGE/location-framework-dumpsys.txt" 2>&1 || true
  "$ADB" -s "$ADB_SERIAL" shell logcat -d -v threadtime \
    -s HDLocationProbe:V GnssLocationProvider:V GnssManager:V '*:S' \
    > "$OUTPUT_STAGE/location-probe-logcat.txt" 2>&1 || true
  fail "real GPS LocationManager callback did not preserve all controlled fields"
}
grep -aFq 'CMD_GET_LOCATION' "$GUEST_LOCATION_OUTPUT" ||
  fail "Android fixed-location HAL did not poll its Guest serial channel"
LOCATION_FIXED_DELIVERIES=$(json_number delivered_sequence "$LOCATION_DELIVERY_EVIDENCE")
[ -n "$LOCATION_FIXED_DELIVERIES" ] && [ "$LOCATION_FIXED_DELIVERIES" -ge 2 ] ||
  fail "fixed-location component did not deliver both the initial and controlled Guest samples"
"$ADB" -s "$ADB_SERIAL" shell dumpsys location \
  > "$OUTPUT_STAGE/location-framework-readback.txt"
cp "$LOCATION_DELIVERY_EVIDENCE" "$OUTPUT_STAGE/location-delivery-v1.json"
stat -f '%z' "$GUEST_LOCATION_OUTPUT" \
  > "$OUTPUT_STAGE/location-guest-output-bytes.txt"
LOCATION_PROBE_SHA256=$(shasum -a 256 "$LOCATION_PROBE_APK" | awk '{print $1}')
printf '%s\n' "$LOCATION_PROBE_SHA256" > "$OUTPUT_STAGE/location-probe-apk-sha256.txt"
"$ADB" -s "$ADB_SERIAL" shell am force-stop com.hd.locationprobe
"$ADB" -s "$ADB_SERIAL" uninstall com.hd.locationprobe \
  > "$OUTPUT_STAGE/location-probe-uninstall.txt"
require_contains 'Success' "$OUTPUT_STAGE/location-probe-uninstall.txt" \
  "location framework probe APK uninstall failed"
LOCATION_PROBE_INSTALLED=0
printf 'location\tlat=37.4219999,lon=-122.0840575,alt=123.456,accuracy=7.890\tLocationManager-GPS-exact-match,mock-provider=false\n' \
  >> "$OUTPUT_STAGE/android-aosp-controls.tsv"

cat > "$OUTPUT_STAGE/location-route.kml" <<'EOF'
<kml xmlns="http://www.opengis.net/kml/2.2"><Document><Placemark><LineString><coordinates>-122.0840577,37.4219999,5 -122.0839000,37.4221000,6 -122.0837000,37.4223000,7</coordinates></LineString></Placemark></Document></kml>
EOF
"$ADB" -s "$ADB_SERIAL" install -r -t "$LOCATION_PROBE_APK" \
  > "$OUTPUT_STAGE/location-route-probe-install.txt" 2>&1
require_contains 'Success' "$OUTPUT_STAGE/location-route-probe-install.txt" \
  "location route framework probe APK installation failed"
LOCATION_PROBE_INSTALLED=1
"$ADB" -s "$ADB_SERIAL" shell pm grant com.hd.locationprobe \
  android.permission.ACCESS_COARSE_LOCATION
"$ADB" -s "$ADB_SERIAL" shell pm grant com.hd.locationprobe \
  android.permission.ACCESS_FINE_LOCATION
"$ADB" -s "$ADB_SERIAL" shell am force-stop com.hd.locationprobe
"$ADB" -s "$ADB_SERIAL" shell run-as com.hd.locationprobe \
  rm -f files/location.txt
"$ADB" -s "$ADB_SERIAL" shell input keyevent KEYCODE_WAKEUP
"$ADB" -s "$ADB_SERIAL" shell wm dismiss-keyguard
"$ADB" -s "$ADB_SERIAL" shell am start -W \
  -n com.hd.locationprobe/.LocationProbeActivity \
  --es expected_latitude 37.4223000 \
  --es expected_longitude -122.0837000 \
  --es expected_altitude 7.000 \
  --es expected_accuracy 5.000 \
  > "$OUTPUT_STAGE/location-route-probe-start.txt"
LOCATION_ROUTE_DELIVERIES_BEFORE=$(json_number delivered_sequence "$LOCATION_DELIVERY_EVIDENCE")
[ -n "$LOCATION_ROUTE_DELIVERIES_BEFORE" ] ||
  fail "fixed-location bridge did not publish its initial delivery sequence"
"$CTL" --data-root "$DATA_ROOT" --no-start-host action "$INSTANCE_ID" \
  route-start "$OUTPUT_STAGE/location-route.kml" --interval-ms 500 --repeat \
  > "$OUTPUT_STAGE/location-route-start.json"
sleep 0.7
"$CTL" --data-root "$DATA_ROOT" --no-start-host show "$INSTANCE_ID" \
  > "$OUTPUT_STAGE/location-route-playing.json"
"$CTL" --data-root "$DATA_ROOT" --no-start-host action "$INSTANCE_ID" route-pause \
  > "$OUTPUT_STAGE/location-route-pause.json"
"$CTL" --data-root "$DATA_ROOT" --no-start-host show "$INSTANCE_ID" \
  > "$OUTPUT_STAGE/location-route-paused.json"
sleep 0.7
"$CTL" --data-root "$DATA_ROOT" --no-start-host show "$INSTANCE_ID" \
  > "$OUTPUT_STAGE/location-route-paused-stable.json"
"$CTL" --data-root "$DATA_ROOT" --no-start-host action "$INSTANCE_ID" route-resume \
  > "$OUTPUT_STAGE/location-route-resume.json"
: > "$OUTPUT_STAGE/location-route-guest-poll.tsv"
printf 'sample\tdelivered_sequence\n' \
  >> "$OUTPUT_STAGE/location-route-guest-poll.tsv"
route_poll_sample=0
location_route_probe_matched=0
while [ "$route_poll_sample" -lt 20 ]; do
  route_poll_sample=$((route_poll_sample + 1))
  route_poll_deliveries=$(json_number delivered_sequence "$LOCATION_DELIVERY_EVIDENCE")
  "$ADB" -s "$ADB_SERIAL" shell run-as com.hd.locationprobe \
    cat files/location.txt > "$OUTPUT_STAGE/location-route-framework-callback.txt" \
    2>/dev/null || true
  if grep -Fq 'status=match' "$OUTPUT_STAGE/location-route-framework-callback.txt" && \
      grep -Fq 'provider=gps' "$OUTPUT_STAGE/location-route-framework-callback.txt" && \
      grep -Fq 'latitude=37.4223000' "$OUTPUT_STAGE/location-route-framework-callback.txt" && \
      grep -Fq 'longitude=-122.0837000' "$OUTPUT_STAGE/location-route-framework-callback.txt" && \
      grep -Fq 'altitude=7.000' "$OUTPUT_STAGE/location-route-framework-callback.txt" && \
      grep -Fq 'accuracy=5.000' "$OUTPUT_STAGE/location-route-framework-callback.txt" && \
      grep -Fq 'has_altitude=true' "$OUTPUT_STAGE/location-route-framework-callback.txt" && \
      grep -Fq 'has_accuracy=true' "$OUTPUT_STAGE/location-route-framework-callback.txt"; then
    location_route_probe_matched=1
  fi
  printf '%s\t%s\n' "$route_poll_sample" "${route_poll_deliveries:-missing}" \
    >> "$OUTPUT_STAGE/location-route-guest-poll.tsv"
  [ -n "$route_poll_deliveries" ] &&
    [ "$route_poll_deliveries" -gt "$LOCATION_ROUTE_DELIVERIES_BEFORE" ] &&
    [ "$location_route_probe_matched" -eq 1 ] && break
  sleep 0.5
done
"$CTL" --data-root "$DATA_ROOT" --no-start-host action "$INSTANCE_ID" route-stop \
  > "$OUTPUT_STAGE/location-route-stop.json"
"$CTL" --data-root "$DATA_ROOT" --no-start-host show "$INSTANCE_ID" \
  > "$OUTPUT_STAGE/location-route-stopped.json"
LOCATION_ROUTE_DELIVERIES_AFTER=$(json_number delivered_sequence "$LOCATION_DELIVERY_EVIDENCE")
/usr/bin/python3 - \
  "$OUTPUT_STAGE/location-route-playing.json" \
  "$OUTPUT_STAGE/location-route-paused.json" \
  "$OUTPUT_STAGE/location-route-paused-stable.json" \
  "$OUTPUT_STAGE/location-route-stopped.json" \
  "$LOCATION_ROUTE_DELIVERIES_BEFORE" "$LOCATION_ROUTE_DELIVERIES_AFTER" <<'PY'
import json
import sys

playing, paused, paused_stable, stopped = [json.load(open(path, encoding="utf-8")) for path in sys.argv[1:5]]
before, after = map(int, sys.argv[5:7])
assert playing["location_route"]["state"] == "playing"
assert playing["location_route"]["point_count"] == 3
assert paused["location_route"]["state"] == "paused"
assert paused_stable["location_route"]["state"] == "paused"
assert paused["location_route"]["current_point"] == paused_stable["location_route"]["current_point"]
assert stopped["location_route"] is None
assert stopped["last_location_route"]["reason"] == "stopped"
assert stopped["last_location_route"]["applied_points"] >= 2
assert after > before
PY
[ "$location_route_probe_matched" -eq 1 ] || {
  "$ADB" -s "$ADB_SERIAL" shell dumpsys location \
    > "$OUTPUT_STAGE/location-route-framework-dumpsys.txt" 2>&1 || true
  "$ADB" -s "$ADB_SERIAL" shell logcat -d -v threadtime \
    -s HDLocationProbe:V GnssLocationProvider:V GnssManager:V '*:S' \
    > "$OUTPUT_STAGE/location-route-probe-logcat.txt" 2>&1 || true
  fail "real GPS LocationManager callback did not observe the controlled route point"
}
"$ADB" -s "$ADB_SERIAL" shell am force-stop com.hd.locationprobe
"$ADB" -s "$ADB_SERIAL" uninstall com.hd.locationprobe \
  > "$OUTPUT_STAGE/location-route-probe-uninstall.txt"
require_contains 'Success' "$OUTPUT_STAGE/location-route-probe-uninstall.txt" \
  "location route framework probe APK uninstall failed"
LOCATION_PROBE_INSTALLED=0
printf 'location_route\tGPX/KML,start,pause,resume,stop,repeat\tLocationManager-GPS-route-point-exact-match,mock-provider=false\n' \
  >> "$OUTPUT_STAGE/android-aosp-controls.tsv"

for sensor_case in \
  'accelerometer 0 0 9806650' \
  'gyroscope 1000 2000 3000' \
  'magnetometer 25000000 1000000 40000000' \
  'light 42000000' \
  'proximity 5000000'; do
  # Intentional fixed word splitting: each case is a closed typed CLI action.
  set -- $sensor_case
  sensor=$1
  shift
  "$CTL" --data-root "$DATA_ROOT" --no-start-host action "$INSTANCE_ID" \
    sensor "$sensor" "$@" --duration-ms 500 \
    > "$OUTPUT_STAGE/sensor-$sensor.json"
done
GUEST_SENSOR_OUTPUT=$(awk '
  /"sensors": \{/ { capture = 1; next }
  capture && /"guest_output":/ {
    value = $0
    sub(/^[^:]*:[[:space:]]*"/, "", value)
    sub(/",?[[:space:]]*$/, "", value)
    print value
    exit
  }
' "$DEVICE_SIM_LAUNCH")
require_abs_file "$GUEST_SENSOR_OUTPUT" sensor-guest-output
GUEST_SENSOR_BYTES=$(stat -f '%z' "$GUEST_SENSOR_OUTPUT")
printf '%s\n' "$GUEST_SENSOR_BYTES" > "$OUTPUT_STAGE/sensor-guest-output-bytes.txt"
"$ADB" -s "$ADB_SERIAL" shell dumpsys sensorservice \
  > "$OUTPUT_STAGE/sensor-framework-readback.txt"
"$CTL" --data-root "$DATA_ROOT" --no-start-host capabilities "$INSTANCE_ID" \
  > "$OUTPUT_STAGE/sensor-runtime-capabilities.json"
awk '
  /"id": "sensors"/ { capture = 1 }
  capture { print }
  capture && /"detail":/ { exit }
' "$OUTPUT_STAGE/sensor-runtime-capabilities.json" \
  > "$OUTPUT_STAGE/sensor-runtime-capability.json"
require_contains '"running": true' "$OUTPUT_STAGE/sensor-runtime-capability.json" \
  "Android SensorService did not report the five-sensor HAL running"
require_contains '"verified": true' "$OUTPUT_STAGE/sensor-runtime-capability.json" \
  "Android SensorService did not verify the five required sensors and recent events"
printf 'sensors\taccelerometer,gyroscope,magnetometer,light,proximity\tframework-live,serial-bytes=%s,lazy-until-app-subscription\n' \
  "$GUEST_SENSOR_BYTES" \
  >> "$OUTPUT_STAGE/android-aosp-controls.tsv"

bluetooth_ready=0
attempt=0
: > "$OUTPUT_STAGE/bluetooth-lifecycle.tsv"
printf 'timestamp\thal_pids\tbinder\tenabled\tstate\trootcanal_pids\n' \
  >> "$OUTPUT_STAGE/bluetooth-lifecycle.tsv"
while [ "$attempt" -lt 150 ]; do
  lifecycle_timestamp=$(date -u '+%Y-%m-%dT%H:%M:%S.%NZ')
  bluetooth_hal_pids=$("$ADB" -s "$ADB_SERIAL" shell pidof \
    android.hardware.bluetooth-service.default 2>/dev/null | tr -d '\r' | \
    tr '\n ' ',,' | sed 's/,,*/,/g; s/,$//' || true)
  "$ADB" -s "$ADB_SERIAL" shell service check bluetooth_manager \
    > "$OUTPUT_STAGE/bluetooth-framework-service.txt" 2>&1 || true
  bluetooth_binder=missing
  grep -Fq found "$OUTPUT_STAGE/bluetooth-framework-service.txt" && bluetooth_binder=found
  "$ADB" -s "$ADB_SERIAL" shell dumpsys bluetooth_manager \
    > "$OUTPUT_STAGE/bluetooth-dumpsys.txt" 2>&1 || true
  bluetooth_enabled=false
  grep -Fq 'enabled: true' "$OUTPUT_STAGE/bluetooth-dumpsys.txt" && bluetooth_enabled=true
  bluetooth_state=unknown
  grep -Eq 'state: ON|state ON|STATE_ON' "$OUTPUT_STAGE/bluetooth-dumpsys.txt" && bluetooth_state=on
  rootcanal_pids=$(pgrep -f '/hd-rootcanal-adapter --serve-v2 --launch' \
    2>/dev/null | tr '\n' ',' | sed 's/,$//' || true)
  printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$lifecycle_timestamp" \
    "$bluetooth_hal_pids" "$bluetooth_binder" "$bluetooth_enabled" \
    "$bluetooth_state" "$rootcanal_pids" >> "$OUTPUT_STAGE/bluetooth-lifecycle.tsv"
  if [ -n "$bluetooth_hal_pids" ] && [ "$bluetooth_binder" = found ] && \
      [ "$bluetooth_enabled" = true ] && [ "$bluetooth_state" = on ] && \
      [ -n "$rootcanal_pids" ]; then
    bluetooth_ready=1
    break
  fi
  sleep 0.2
  attempt=$((attempt + 1))
done
[ "$bluetooth_ready" -eq 1 ] || \
  fail "Android Bluetooth HAL, framework state or RootCanal component was unavailable"
ROOTCANAL_LAUNCH=$(find "$DATA_ROOT/runs/$INSTANCE_ID" -type f \
  -name 'rootcanal-adapter-launch-v2.json' -print -quit)
[ -n "$ROOTCANAL_LAUNCH" ] || fail "Worker did not create a RootCanal launch contract"
GUEST_BLUETOOTH_OUTPUT=$(json_string guest_output "$ROOTCANAL_LAUNCH")
require_abs_file "$GUEST_BLUETOOTH_OUTPUT" rootcanal-guest-bluetooth-output
GUEST_BLUETOOTH_BYTES=$(stat -f '%z' "$GUEST_BLUETOOTH_OUTPUT")
[ "$GUEST_BLUETOOTH_BYTES" -gt 0 ] || \
  fail "Android Bluetooth HAL did not emit H4 traffic to RootCanal"
printf '%s\n' "$GUEST_BLUETOOTH_BYTES" \
  > "$OUTPUT_STAGE/bluetooth-guest-output-bytes.txt"
pgrep -f '/hd-rootcanal-adapter --serve-v2 --launch' \
  > "$OUTPUT_STAGE/rootcanal-pids.txt"
[ "$(wc -l < "$OUTPUT_STAGE/rootcanal-pids.txt" | tr -d ' ')" -eq 1 ] || \
  fail "installed Android run did not own exactly one RootCanal adapter"
BLUETOOTH_PEER_ID=11111111-2222-4333-8444-555555555555
BLUETOOTH_HID_ID=12121212-3434-4567-8899-aabbccddeeff
BLUETOOTH_HID_NAME=HD-Smoke-HID
"$CTL" --data-root "$DATA_ROOT" --no-start-host action "$INSTANCE_ID" \
  bluetooth-hid-keyboard "$BLUETOOTH_HID_ID" "$BLUETOOTH_HID_NAME" \
  > "$OUTPUT_STAGE/bluetooth-hid-create.json"
"$ADB" -s "$ADB_SERIAL" shell input keyevent WAKEUP >/dev/null
"$ADB" -s "$ADB_SERIAL" shell wm dismiss-keyguard >/dev/null 2>&1 || true
"$ADB" -s "$ADB_SERIAL" shell am start -a android.settings.BLUETOOTH_SETTINGS \
  > "$OUTPUT_STAGE/bluetooth-hid-settings-start.txt"
pair_page_opened=0
attempt=0
while [ "$attempt" -lt 20 ]; do
  dump_android_ui "$OUTPUT_STAGE/bluetooth-hid-connected-devices.xml"
  if center=$(ui_node_center "$OUTPUT_STAGE/bluetooth-hid-connected-devices.xml" text \
      "Pair new device" 2>/dev/null); then
    center_x=${center%% *}
    center_y=${center#* }
    "$ADB" -s "$ADB_SERIAL" shell input tap "$center_x" "$center_y"
    pair_page_opened=1
    break
  fi
  sleep 0.25
  attempt=$((attempt + 1))
done
[ "$pair_page_opened" -eq 1 ] || \
  fail "Android Bluetooth settings did not expose the Pair new device action"
hid_device_tapped=0
attempt=0
while [ "$attempt" -lt 60 ]; do
  dump_android_ui "$OUTPUT_STAGE/bluetooth-hid-discovery.xml"
  if center=$(ui_node_center "$OUTPUT_STAGE/bluetooth-hid-discovery.xml" text \
      "$BLUETOOTH_HID_NAME" 2>/dev/null); then
    center_x=${center%% *}
    center_y=${center#* }
    "$ADB" -s "$ADB_SERIAL" shell input tap "$center_x" "$center_y"
    hid_device_tapped=1
    break
  fi
  sleep 0.5
  attempt=$((attempt + 1))
done
[ "$hid_device_tapped" -eq 1 ] || \
  fail "Android Bluetooth settings did not discover the HOGP keyboard"

# Android 15 may auto-accept NoInputNoOutput LE pairing or show a standard consent button.
attempt=0
while [ "$attempt" -lt 20 ]; do
  dump_android_ui "$OUTPUT_STAGE/bluetooth-hid-pairing.xml"
  if center=$(ui_node_center "$OUTPUT_STAGE/bluetooth-hid-pairing.xml" resource-id \
      android:id/button1 2>/dev/null); then
    center_x=${center%% *}
    center_y=${center#* }
    "$ADB" -s "$ADB_SERIAL" shell input tap "$center_x" "$center_y"
    break
  fi
  "$ADB" -s "$ADB_SERIAL" shell dumpsys bluetooth_manager \
    > "$OUTPUT_STAGE/bluetooth-hid-bond.txt" 2>&1 || true
  grep -Fq "$BLUETOOTH_HID_NAME" "$OUTPUT_STAGE/bluetooth-hid-bond.txt" && break
  sleep 0.25
  attempt=$((attempt + 1))
done

hid_ready=0
attempt=0
while [ "$attempt" -lt 120 ]; do
  "$ADB" -s "$ADB_SERIAL" shell dumpsys bluetooth_manager \
    > "$OUTPUT_STAGE/bluetooth-hid-bond.txt" 2>&1 || true
  if grep -Fq "$BLUETOOTH_HID_NAME" "$OUTPUT_STAGE/bluetooth-hid-bond.txt" && \
      "$CTL" --data-root "$DATA_ROOT" --no-start-host action "$INSTANCE_ID" \
        bluetooth-hid-keyboard-report "$BLUETOOTH_HID_ID" \
        > "$OUTPUT_STAGE/bluetooth-hid-ready.json" 2> "$OUTPUT_STAGE/bluetooth-hid-ready.stderr"; then
    hid_ready=1
    break
  fi
  sleep 0.25
  attempt=$((attempt + 1))
done
[ "$hid_ready" -eq 1 ] || \
  fail "Android did not complete encrypted HOGP discovery and input notification subscription"

"$ADB" -s "$ADB_SERIAL" shell getevent -lt \
  > "$OUTPUT_STAGE/bluetooth-hid-getevent.txt" \
  2> "$OUTPUT_STAGE/bluetooth-hid-getevent.stderr" &
HID_GETEVENT_PID=$!
sleep 0.5
"$CTL" --data-root "$DATA_ROOT" --no-start-host action "$INSTANCE_ID" \
  bluetooth-hid-keyboard-report "$BLUETOOTH_HID_ID" --keys 4 \
  > "$OUTPUT_STAGE/bluetooth-hid-key-a-down.json"
"$CTL" --data-root "$DATA_ROOT" --no-start-host action "$INSTANCE_ID" \
  bluetooth-hid-keyboard-report "$BLUETOOTH_HID_ID" \
  > "$OUTPUT_STAGE/bluetooth-hid-key-a-up.json"
sleep 0.75
kill "$HID_GETEVENT_PID" 2>/dev/null || true
wait "$HID_GETEVENT_PID" 2>/dev/null || true
require_contains KEY_A "$OUTPUT_STAGE/bluetooth-hid-getevent.txt" \
  "Android input subsystem did not receive KEY_A from the HOGP keyboard"
require_contains DOWN "$OUTPUT_STAGE/bluetooth-hid-getevent.txt" \
  "Android input subsystem did not receive the HOGP key-down event"
require_contains UP "$OUTPUT_STAGE/bluetooth-hid-getevent.txt" \
  "Android input subsystem did not receive the HOGP key-up event"
"$CTL" --data-root "$DATA_ROOT" --no-start-host show "$INSTANCE_ID" \
  > "$OUTPUT_STAGE/bluetooth-hid-instance-state.json"
require_contains '"keyboard_reports_sent": 3' \
  "$OUTPUT_STAGE/bluetooth-hid-instance-state.json" \
  "HOGP keyboard state did not count readiness, key-down and key-up reports"

# Capture a bounded real-Guest HCI trace while Android itself drives the controller through a
# disable/enable cycle. The Host action is intentionally left in the background so ADB, rather
# than another simulated peer action, supplies independently observable Guest H4 traffic.
"$CTL" --data-root "$DATA_ROOT" --no-start-host action "$INSTANCE_ID" \
  bluetooth-hci-capture --duration-ms 6000 \
  > "$OUTPUT_STAGE/bluetooth-hci-capture-action.json" &
HCI_CAPTURE_PID=$!
sleep 0.5
"$ADB" -s "$ADB_SERIAL" shell cmd bluetooth_manager disable \
  > "$OUTPUT_STAGE/bluetooth-hci-disable.txt"
sleep 0.75
"$ADB" -s "$ADB_SERIAL" shell cmd bluetooth_manager enable \
  > "$OUTPUT_STAGE/bluetooth-hci-enable.txt"
wait "$HCI_CAPTURE_PID"
"$CTL" --data-root "$DATA_ROOT" --no-start-host show "$INSTANCE_ID" \
  > "$OUTPUT_STAGE/bluetooth-hci-instance-state.json"
HCI_CAPTURE_ID=$(json_string capture_id "$OUTPUT_STAGE/bluetooth-hci-instance-state.json")
HCI_CAPTURE_FILE=$(json_string file_name "$OUTPUT_STAGE/bluetooth-hci-instance-state.json")
[ -n "$HCI_CAPTURE_ID" ] && [ -n "$HCI_CAPTURE_FILE" ] || \
  fail "bounded Bluetooth HCI capture did not persist its typed instance record"
[ "$HCI_CAPTURE_FILE" = "rootcanal-hci-$HCI_CAPTURE_ID.btsnoop" ] || \
  fail "Bluetooth HCI capture file name is not bound to its generated UUID"
ROOTCANAL_COMPONENT_DIR=$(dirname -- "$ROOTCANAL_LAUNCH")
HCI_CAPTURE_PATH="$ROOTCANAL_COMPONENT_DIR/$HCI_CAPTURE_FILE"
HCI_CAPTURE_METADATA="$ROOTCANAL_COMPONENT_DIR/rootcanal-hci-$HCI_CAPTURE_ID.json"
require_abs_file "$HCI_CAPTURE_PATH" bluetooth-hci-capture
require_abs_file "$HCI_CAPTURE_METADATA" bluetooth-hci-capture-metadata
[ "$(stat -f '%Lp' "$HCI_CAPTURE_PATH")" = 600 ] || \
  fail "Bluetooth HCI capture is not owner-only"
[ "$(stat -f '%Lp' "$HCI_CAPTURE_METADATA")" = 600 ] || \
  fail "Bluetooth HCI capture metadata is not owner-only"
HCI_CAPTURE_SIZE=$(stat -f '%z' "$HCI_CAPTURE_PATH")
[ "$HCI_CAPTURE_SIZE" -ge 16 ] && [ "$HCI_CAPTURE_SIZE" -le 4194304 ] || \
  fail "Bluetooth HCI capture exceeded its 4 MiB product bound"
HCI_CAPTURE_HEADER=$(od -An -tx1 -N16 "$HCI_CAPTURE_PATH" | tr -d ' \n')
[ "$HCI_CAPTURE_HEADER" = 6274736e6f6f700000000001000003ea ] || \
  fail "Bluetooth HCI capture is not standard btsnoop HCI UART"
/usr/bin/python3 - \
  "$OUTPUT_STAGE/bluetooth-hci-instance-state.json" \
  "$HCI_CAPTURE_METADATA" "$HCI_CAPTURE_SIZE" \
  > "$OUTPUT_STAGE/bluetooth-hci-capture.tsv" <<'PY'
import json
import sys
import uuid

state = json.load(open(sys.argv[1], encoding="utf-8"))
sidecar = json.load(open(sys.argv[2], encoding="utf-8"))
actual_size = int(sys.argv[3])
record = state.get("last_bluetooth_hci_capture")
assert isinstance(record, dict)
assert record == sidecar
uuid.UUID(record["capture_id"])
assert record["requested_duration_ms"] == 6000
assert record["packets_captured"] > 0
assert record["packets_dropped"] >= 0
assert record["output_size_bytes"] == actual_size
assert 16 <= actual_size <= 4 * 1024 * 1024
assert isinstance(record["truncated"], bool)
print("capture_id\trequested_duration_ms\tpackets_captured\tpackets_dropped\toutput_size_bytes\ttruncated")
print(
    record["capture_id"],
    record["requested_duration_ms"],
    record["packets_captured"],
    record["packets_dropped"],
    record["output_size_bytes"],
    str(record["truncated"]).lower(),
    sep="\t",
)
PY
HCI_PACKETS_CAPTURED=$(sed -n '2p' "$OUTPUT_STAGE/bluetooth-hci-capture.tsv" | cut -f 3)
cp -p "$HCI_CAPTURE_PATH" "$OUTPUT_STAGE/bluetooth-hci-capture.btsnoop"
cp -p "$HCI_CAPTURE_METADATA" "$OUTPUT_STAGE/bluetooth-hci-capture.json"

# Prove the product delivery path too: diagnostics must package both files under this exact run,
# and the archive manifest/API hashes must bind to the source bytes.
"$CTL" --data-root "$DATA_ROOT" --no-start-host diagnostics \
  --instance-id "$INSTANCE_ID" > "$OUTPUT_STAGE/bluetooth-hci-diagnostics.json"
HCI_DIAGNOSTIC_PATH=$(json_string path "$OUTPUT_STAGE/bluetooth-hci-diagnostics.json")
HCI_DIAGNOSTIC_SHA=$(json_string archive_sha256 "$OUTPUT_STAGE/bluetooth-hci-diagnostics.json")
HCI_DIAGNOSTIC_MANIFEST_SHA=$(json_string manifest_sha256 \
  "$OUTPUT_STAGE/bluetooth-hci-diagnostics.json")
require_abs_file "$HCI_DIAGNOSTIC_PATH" bluetooth-hci-diagnostic-bundle
[ "$(stat -f '%Lp' "$HCI_DIAGNOSTIC_PATH")" = 600 ] || \
  fail "Bluetooth HCI diagnostic bundle is not owner-only"
[ "$(shasum -a 256 "$HCI_DIAGNOSTIC_PATH" | awk '{print $1}')" = "$HCI_DIAGNOSTIC_SHA" ] || \
  fail "Bluetooth HCI diagnostic archive hash did not match its API record"
ACTIVE_RUN_ID=$(json_string active_run_id "$OUTPUT_STAGE/bluetooth-hci-instance-state.json")
HCI_DIAGNOSTIC_MEMBER="run/$ACTIVE_RUN_ID/components/$HCI_CAPTURE_FILE"
HCI_DIAGNOSTIC_METADATA_MEMBER="run/$ACTIVE_RUN_ID/components/rootcanal-hci-$HCI_CAPTURE_ID.json"
PATH="$ZSTD_BIN_DIR:/usr/bin:/bin" /usr/bin/bsdtar -tf "$HCI_DIAGNOSTIC_PATH" \
  > "$OUTPUT_STAGE/bluetooth-hci-diagnostics-list.txt"
require_contains "$HCI_DIAGNOSTIC_MEMBER" \
  "$OUTPUT_STAGE/bluetooth-hci-diagnostics-list.txt" \
  "diagnostics omitted the bounded Bluetooth HCI capture"
require_contains "$HCI_DIAGNOSTIC_METADATA_MEMBER" \
  "$OUTPUT_STAGE/bluetooth-hci-diagnostics-list.txt" \
  "diagnostics omitted the Bluetooth HCI capture metadata"
HCI_CAPTURE_SHA=$(shasum -a 256 "$HCI_CAPTURE_PATH" | awk '{print $1}')
[ "$(PATH="$ZSTD_BIN_DIR:/usr/bin:/bin" /usr/bin/bsdtar -xOf \
    "$HCI_DIAGNOSTIC_PATH" "$HCI_DIAGNOSTIC_MEMBER" | shasum -a 256 | awk '{print $1}')" = "$HCI_CAPTURE_SHA" ] || \
  fail "diagnostics changed the Bluetooth HCI capture bytes"
PATH="$ZSTD_BIN_DIR:/usr/bin:/bin" /usr/bin/bsdtar -xOf \
  "$HCI_DIAGNOSTIC_PATH" "$HCI_DIAGNOSTIC_METADATA_MEMBER" \
  > "$OUTPUT_STAGE/bluetooth-hci-diagnostic-sidecar.json"
/usr/bin/python3 - "$HCI_CAPTURE_METADATA" \
  "$OUTPUT_STAGE/bluetooth-hci-diagnostic-sidecar.json" <<'PY'
import json
import sys

assert json.load(open(sys.argv[1], encoding="utf-8")) == json.load(
    open(sys.argv[2], encoding="utf-8")
)
PY
HCI_DIAGNOSTIC_METADATA_SHA=$(shasum -a 256 \
  "$OUTPUT_STAGE/bluetooth-hci-diagnostic-sidecar.json" | awk '{print $1}')
PATH="$ZSTD_BIN_DIR:/usr/bin:/bin" /usr/bin/bsdtar -xOf \
  "$HCI_DIAGNOSTIC_PATH" diagnostic-manifest-v2.json \
  > "$OUTPUT_STAGE/bluetooth-hci-diagnostic-manifest.json"
[ "$(shasum -a 256 "$OUTPUT_STAGE/bluetooth-hci-diagnostic-manifest.json" | awk '{print $1}')" = "$HCI_DIAGNOSTIC_MANIFEST_SHA" ] || \
  fail "Bluetooth HCI diagnostic manifest hash did not match its API record"
/usr/bin/python3 - "$OUTPUT_STAGE/bluetooth-hci-diagnostic-manifest.json" \
  "$HCI_DIAGNOSTIC_MEMBER" "$HCI_CAPTURE_SHA" "$HCI_CAPTURE_SIZE" \
  "$HCI_DIAGNOSTIC_METADATA_MEMBER" "$HCI_DIAGNOSTIC_METADATA_SHA" <<'PY'
import json
import sys

manifest = json.load(open(sys.argv[1], encoding="utf-8"))
files = {item["relative_path"]: item for item in manifest["files"]}
capture = files[sys.argv[2]]
assert capture["sha256"] == sys.argv[3]
assert capture["size_bytes"] == int(sys.argv[4])
assert not capture["truncated"]
metadata = files[sys.argv[5]]
assert metadata["sha256"] == sys.argv[6]
assert not metadata["truncated"]
PY

bluetooth_reenabled=0
attempt=0
while [ "$attempt" -lt 100 ]; do
  "$ADB" -s "$ADB_SERIAL" shell dumpsys bluetooth_manager \
    > "$OUTPUT_STAGE/bluetooth-hci-post-enable-dumpsys.txt" 2>&1 || true
  if grep -Fq 'enabled: true' "$OUTPUT_STAGE/bluetooth-hci-post-enable-dumpsys.txt" && \
      grep -Eq 'state: ON|state ON|STATE_ON' \
        "$OUTPUT_STAGE/bluetooth-hci-post-enable-dumpsys.txt"; then
    bluetooth_reenabled=1
    break
  fi
  sleep 0.2
  attempt=$((attempt + 1))
done
[ "$bluetooth_reenabled" -eq 1 ] || \
  fail "Android Bluetooth framework did not recover after real-Guest HCI capture traffic"
"$CTL" --data-root "$DATA_ROOT" --no-start-host action "$INSTANCE_ID" \
  bluetooth-remove "$BLUETOOTH_HID_ID" \
  > "$OUTPUT_STAGE/bluetooth-hid-remove.json"
"$CTL" --data-root "$DATA_ROOT" --no-start-host action "$INSTANCE_ID" \
  bluetooth-create "$BLUETOOTH_PEER_ID" HD-Smoke-Peer \
  > "$OUTPUT_STAGE/bluetooth-create.json"
"$CTL" --data-root "$DATA_ROOT" --no-start-host action "$INSTANCE_ID" \
  bluetooth-advertise "$BLUETOOTH_PEER_ID" true \
  > "$OUTPUT_STAGE/bluetooth-advertise-on.json"
sleep 1
"$CTL" --data-root "$DATA_ROOT" --no-start-host action "$INSTANCE_ID" \
  bluetooth-advertise "$BLUETOOTH_PEER_ID" false \
  > "$OUTPUT_STAGE/bluetooth-advertise-off.json"
"$CTL" --data-root "$DATA_ROOT" --no-start-host action "$INSTANCE_ID" \
  bluetooth-remove "$BLUETOOTH_PEER_ID" \
  > "$OUTPUT_STAGE/bluetooth-remove.json"
"$ADB" -s "$ADB_SERIAL" shell dumpsys bluetooth_manager \
  > "$OUTPUT_STAGE/bluetooth-post-actions-dumpsys.txt"
require_contains 'enabled: true' "$OUTPUT_STAGE/bluetooth-post-actions-dumpsys.txt" \
  "Android Bluetooth framework was disabled after the peer action sequence"
"$ADB" -s "$ADB_SERIAL" shell pidof android.hardware.bluetooth-service.default \
  > "$OUTPUT_STAGE/bluetooth-post-actions-hal-pids.txt"
[ -s "$OUTPUT_STAGE/bluetooth-post-actions-hal-pids.txt" ] || \
  fail "Android Bluetooth HAL stopped after the peer action sequence"
modem_ready=0
attempt=0
: > "$OUTPUT_STAGE/modem-lifecycle.tsv"
printf 'timestamp\tril_state\tril_pids\tradio_service\toperator_numeric\toperator_alpha\tadapter_pids\n' \
  >> "$OUTPUT_STAGE/modem-lifecycle.tsv"
while [ "$attempt" -lt 200 ]; do
  lifecycle_timestamp=$(date -u '+%Y-%m-%dT%H:%M:%S.%NZ')
  ril_state=$("$ADB" -s "$ADB_SERIAL" shell getprop init.svc.vendor.ril-daemon \
    2>/dev/null | tr -d '\r\n' || true)
  ril_pids=$("$ADB" -s "$ADB_SERIAL" shell pidof libcuttlefish-rild \
    2>/dev/null | tr -d '\r' | tr '\n ' ',,' | sed 's/,,*/,/g; s/,$//' || true)
  "$ADB" -s "$ADB_SERIAL" shell service list \
    > "$OUTPUT_STAGE/modem-services.txt" 2>&1 || true
  radio_service=missing
  grep -Fq android.hardware.radio "$OUTPUT_STAGE/modem-services.txt" && radio_service=found
  operator_numeric=$("$ADB" -s "$ADB_SERIAL" shell getprop gsm.operator.numeric \
    2>/dev/null | tr -d '\r\n' || true)
  operator_alpha=$("$ADB" -s "$ADB_SERIAL" shell getprop gsm.operator.alpha \
    2>/dev/null | tr -d '\r\n' || true)
  modem_adapter_pids=$(pgrep -f '/hd-modem-adapter --serve-v2 --launch' \
    2>/dev/null | tr '\n' ',' | sed 's/,$//' || true)
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$lifecycle_timestamp" \
    "$ril_state" "$ril_pids" "$radio_service" "$operator_numeric" \
    "$operator_alpha" "$modem_adapter_pids" >> "$OUTPUT_STAGE/modem-lifecycle.tsv"
  if [ "$ril_state" = running ] && [ -n "$ril_pids" ] && \
      [ "$radio_service" = found ] && [ "$operator_numeric" = 00101 ] && \
      [ -n "$modem_adapter_pids" ]; then
    modem_ready=1
    break
  fi
  sleep 0.2
  attempt=$((attempt + 1))
done
[ "$modem_ready" -eq 1 ] || \
  fail "Android RIL, Radio HAL, deterministic operator or formal modem adapter was unavailable"
MODEM_LAUNCH=$(find "$DATA_ROOT/runs/$INSTANCE_ID" -type f \
  -name 'modem-adapter-launch-v2.json' -print -quit)
[ -n "$MODEM_LAUNCH" ] || fail "Worker did not create a modem launch contract"
MODEM_GUEST_CID=$(json_number guest_cid "$MODEM_LAUNCH")
[ -n "$MODEM_GUEST_CID" ] || fail "modem launch contract did not publish a Guest CID"
MODEM_SOCKET="/tmp/binder_rpc_vsock_${MODEM_GUEST_CID}_9697.sock"
[ -S "$MODEM_SOCKET" ] || fail "formal modem host-vsock UDS is missing: $MODEM_SOCKET"
[ "$(stat -f '%Lp' "$MODEM_SOCKET")" = 600 ] || \
  fail "formal modem host-vsock UDS is not owner-only"
pgrep -f '/hd-modem-adapter --serve-v2 --launch' \
  > "$OUTPUT_STAGE/modem-adapter-pids.txt"
[ "$(wc -l < "$OUTPUT_STAGE/modem-adapter-pids.txt" | tr -d ' ')" -eq 1 ] || \
  fail "installed Android run did not own exactly one modem adapter"
"$ADB" -s "$ADB_SERIAL" shell dumpsys telephony.registry \
  > "$OUTPUT_STAGE/modem-telephony-registry.txt"
require_contains '00101' "$OUTPUT_STAGE/modem-telephony-registry.txt" \
  "Android telephony framework did not publish the deterministic test operator"
"$ADB" -s "$ADB_SERIAL" shell getprop \
  > "$OUTPUT_STAGE/modem-getprop.txt"
require_contains '[init.svc.vendor.ril-daemon]: [running]' \
  "$OUTPUT_STAGE/modem-getprop.txt" \
  "Android vendor RIL did not remain running after the AT handshake"
uwb_ready=0
attempt=0
: > "$OUTPUT_STAGE/uwb-lifecycle.tsv"
printf 'timestamp\thal_pids\taidl\tframework\troot_pids\n' \
  >> "$OUTPUT_STAGE/uwb-lifecycle.tsv"
while [ "$attempt" -lt 150 ]; do
  lifecycle_timestamp=$(date -u '+%Y-%m-%dT%H:%M:%S.%NZ')
  uwb_hal_pids=$("$ADB" -s "$ADB_SERIAL" shell pidof \
    android.hardware.uwb-service 2>/dev/null | tr -d '\r' | \
    tr '\n ' ',,' | sed 's/,,*/,/g; s/,$//' || true)
  "$ADB" -s "$ADB_SERIAL" shell service list \
    > "$OUTPUT_STAGE/uwb-services.txt" 2>&1 || true
  uwb_aidl=missing
  grep -Fq android.hardware.uwb.IUwb/default "$OUTPUT_STAGE/uwb-services.txt" && uwb_aidl=found
  uwb_framework=missing
  grep -Fq 'uwb:' "$OUTPUT_STAGE/uwb-services.txt" && uwb_framework=found
  uwb_root_pids=$(pgrep -f '/hd-uwb-adapter --serve-v2 --launch' \
    2>/dev/null | tr '\n' ',' | sed 's/,$//' || true)
  printf '%s\t%s\t%s\t%s\t%s\n' "$lifecycle_timestamp" "$uwb_hal_pids" \
    "$uwb_aidl" "$uwb_framework" "$uwb_root_pids" \
    >> "$OUTPUT_STAGE/uwb-lifecycle.tsv"
  if [ -n "$uwb_hal_pids" ] && [ "$uwb_aidl" = found ] && \
      [ "$uwb_framework" = found ] && [ -n "$uwb_root_pids" ]; then
    uwb_ready=1
    break
  fi
  sleep 0.2
  attempt=$((attempt + 1))
done
[ "$uwb_ready" -eq 1 ] || \
  fail "Android UWB HAL, AIDL/framework service or formal adapter was unavailable"
UWB_LAUNCH=$(find "$DATA_ROOT/runs/$INSTANCE_ID" -type f \
  -name 'uwb-adapter-launch-v2.json' -print -quit)
[ -n "$UWB_LAUNCH" ] || fail "Worker did not create a UWB launch contract"
GUEST_UWB_OUTPUT=$(json_string guest_output "$UWB_LAUNCH")
require_abs_file "$GUEST_UWB_OUTPUT" uwb-guest-output
GUEST_UWB_BYTES=$(stat -f '%z' "$GUEST_UWB_OUTPUT")
[ "$GUEST_UWB_BYTES" -gt 0 ] || fail "Android UWB HAL did not emit UCI traffic"
printf '%s\n' "$GUEST_UWB_BYTES" > "$OUTPUT_STAGE/uwb-guest-output-bytes.txt"
pgrep -f '/hd-uwb-adapter --serve-v2 --launch' > "$OUTPUT_STAGE/uwb-adapter-pids.txt"
[ "$(wc -l < "$OUTPUT_STAGE/uwb-adapter-pids.txt" | tr -d ' ')" -eq 1 ] || \
  fail "installed Android run did not own exactly one UWB adapter"
uwb_policy_ready=0
attempt=0
while [ "$attempt" -lt 150 ]; do
  "$ADB" -s "$ADB_SERIAL" shell service list \
    > "$OUTPUT_STAGE/uwb-policy-services.txt" 2>&1 || true
  "$ADB" -s "$ADB_SERIAL" shell pidof android.hardware.uwb-service \
    > "$OUTPUT_STAGE/uwb-policy-hal-pids.txt" 2>/dev/null || true
  "$ADB" -s "$ADB_SERIAL" shell dumpsys uwb \
    > "$OUTPUT_STAGE/uwb-dumpsys.txt" 2>&1 || true
  if [ -s "$OUTPUT_STAGE/uwb-policy-hal-pids.txt" ] && \
      grep -Fq android.hardware.uwb.IUwb/default "$OUTPUT_STAGE/uwb-policy-services.txt" && \
      grep -Fq 'uwb:' "$OUTPUT_STAGE/uwb-policy-services.txt" && \
      grep -Fq 'mCountryCode:' "$OUTPUT_STAGE/uwb-dumpsys.txt" && \
      grep -Fq 'US' "$OUTPUT_STAGE/uwb-dumpsys.txt" && \
      [ "$(pgrep -f '/hd-uwb-adapter --serve-v2 --launch' 2>/dev/null | wc -l | tr -d ' ')" -eq 1 ]; then
    uwb_policy_ready=1
    break
  fi
  sleep 0.2
  attempt=$((attempt + 1))
done
uwb_dumpsys_attempts=$attempt
[ "$uwb_policy_ready" -eq 0 ] || uwb_dumpsys_attempts=$((attempt + 1))
printf '%s\n' "$uwb_dumpsys_attempts" > "$OUTPUT_STAGE/uwb-dumpsys-attempts.txt"
[ "$uwb_policy_ready" -eq 1 ] || \
  fail "Android UWB HAL/framework/adapter did not converge on the configured US country code"
verify_android_uwb_fira "$ADB_SERIAL" first
nfc_ready=0
attempt=0
: > "$OUTPUT_STAGE/nfc-lifecycle.tsv"
printf 'timestamp\tinit_state\thal_pids\tcasimir_pids\n' \
  >> "$OUTPUT_STAGE/nfc-lifecycle.tsv"
while [ "$attempt" -lt 100 ]; do
  lifecycle_timestamp=$(date -u '+%Y-%m-%dT%H:%M:%S.%NZ')
  hal_state=$("$ADB" -s "$ADB_SERIAL" shell getprop init.svc.nfc_hal_service \
    2>/dev/null | tr -d '\r\n' || true)
  hal_pids=$("$ADB" -s "$ADB_SERIAL" shell pidof \
    android.hardware.nfc-service.cuttlefish 2>/dev/null | tr -d '\r' | \
    tr '\n ' ',,' | sed 's/,,*/,/g; s/,$//' || true)
  casimir_pids=$(pgrep -f '/hd-casimir-adapter --serve-v2 --launch' \
    2>/dev/null | tr '\n' ',' | sed 's/,$//' || true)
  printf '%s\t%s\t%s\t%s\n' "$lifecycle_timestamp" "$hal_state" \
    "$hal_pids" "$casimir_pids" >> "$OUTPUT_STAGE/nfc-lifecycle.tsv"
  "$ADB" -s "$ADB_SERIAL" shell ps -A -o PID,ARGS \
    > "$OUTPUT_STAGE/nfc-processes.txt"
  if grep -Fq android.hardware.nfc-service.cuttlefish \
      "$OUTPUT_STAGE/nfc-processes.txt" &&
      "$ADB" -s "$ADB_SERIAL" shell service check nfc \
        > "$OUTPUT_STAGE/nfc-framework-service.txt" 2>&1 &&
      grep -Fq found "$OUTPUT_STAGE/nfc-framework-service.txt"; then
    nfc_ready=1
    break
  fi
  sleep 0.2
  attempt=$((attempt + 1))
done
"$ADB" -s "$ADB_SERIAL" shell getprop > "$OUTPUT_STAGE/nfc-getprop.txt" 2>&1 || true
"$ADB" -s "$ADB_SERIAL" shell dumpsys nfc \
  > "$OUTPUT_STAGE/nfc-dumpsys-failure.txt" 2>&1 || true
[ "$nfc_ready" -eq 1 ] || fail "Android NFC HAL process or framework binder service was unavailable"
CASIMIR_LAUNCH=$(find "$DATA_ROOT/runs/$INSTANCE_ID" -type f \
  -name 'casimir-adapter-launch-v2.json' -print -quit)
[ -n "$CASIMIR_LAUNCH" ] || fail "Worker did not create a Casimir launch contract"
GUEST_NFC_OUTPUT=$(json_string guest_output "$CASIMIR_LAUNCH")
require_abs_file "$GUEST_NFC_OUTPUT" casimir-guest-nfc-output
GUEST_NFC_BYTES=$(stat -f '%z' "$GUEST_NFC_OUTPUT")
[ "$GUEST_NFC_BYTES" -gt 0 ] || fail "Android NFC HAL did not emit NCI traffic to Casimir"
printf '%s\n' "$GUEST_NFC_BYTES" > "$OUTPUT_STAGE/nfc-guest-output-bytes.txt"
pgrep -f '/hd-casimir-adapter --serve-v2 --launch' \
  > "$OUTPUT_STAGE/casimir-pids.txt"
[ "$(wc -l < "$OUTPUT_STAGE/casimir-pids.txt" | tr -d ' ')" -eq 1 ] ||
  fail "installed Android run did not own exactly one Casimir adapter"
"$CTL" --data-root "$DATA_ROOT" --no-start-host action "$INSTANCE_ID" \
  nfc-type2 d1010a5402656e484420543254 > "$OUTPUT_STAGE/nfc-type2.json"
sleep 1
"$ADB" -s "$ADB_SERIAL" shell dumpsys nfc > "$OUTPUT_STAGE/nfc-dumpsys.txt"
"$CTL" --data-root "$DATA_ROOT" --no-start-host action "$INSTANCE_ID" \
  nfc-remove > "$OUTPUT_STAGE/nfc-remove-type2.json"
"$CTL" --data-root "$DATA_ROOT" --no-start-host action "$INSTANCE_ID" \
  nfc-type4 d1010a5402656e484420543454 > "$OUTPUT_STAGE/nfc-type4.json"
"$CTL" --data-root "$DATA_ROOT" --no-start-host action "$INSTANCE_ID" \
  nfc-remove > "$OUTPUT_STAGE/nfc-remove-type4.json"
"$ADB" -s "$ADB_SERIAL" shell getprop init.svc.nfc_hal_service \
  > "$OUTPUT_STAGE/nfc-post-actions-hal-state.txt"
require_contains running "$OUTPUT_STAGE/nfc-post-actions-hal-state.txt" \
  "Android NFC HAL stopped after the Type 2/Type 4 action sequence"
"$ADB" -s "$ADB_SERIAL" shell service check nfc \
  > "$OUTPUT_STAGE/nfc-post-actions-framework-service.txt"
require_contains found "$OUTPUT_STAGE/nfc-post-actions-framework-service.txt" \
  "Android NFC framework service disappeared after the tag action sequence"

"$CTL" --data-root "$DATA_ROOT" --no-start-host stop "$INSTANCE_ID" \
  > "$OUTPUT_STAGE/first-stop.json"
"$CTL" --data-root "$DATA_ROOT" --no-start-host show "$INSTANCE_ID" \
  > "$OUTPUT_STAGE/first-stopped-state.json"
require_contains '"state": "stopped"' "$OUTPUT_STAGE/first-stopped-state.json" \
  "first Android stop did not converge to Stopped"
require_contains '"active_run_id": null' "$OUTPUT_STAGE/first-stopped-state.json" \
  "first Android stop retained an active run"
FIRST_CONSOLE="$DATA_ROOT/runs/$INSTANCE_ID/$ACTIVE_RUN_ID/console-hvc0.txt"
[ -f "$FIRST_CONSOLE" ] || fail "first Android run did not preserve its Guest console"
cp "$FIRST_CONSOLE" "$OUTPUT_STAGE/first-powerdown-console.log"
chmod 600 "$OUTPUT_STAGE/first-powerdown-console.log"
require_contains 'reboot: Power down' "$OUTPUT_STAGE/first-powerdown-console.log" \
  "first Android stop did not complete the Guest power-down path"
if grep -q 'adb.debuggable_power_off.requested' \
  "$DATA_ROOT/logs/workers/$INSTANCE_ID.jsonl"*; then
  fail "Android stop incorrectly used the Microdroid Full-debug adb-root path"
fi
seed_run_retention_fixtures
"$CTL" --data-root "$DATA_ROOT" --no-start-host capabilities "$INSTANCE_ID" \
  > "$OUTPUT_STAGE/restart-capabilities.json"
require_contains '"disk_requirement_mode": "existing_instance_storage"' \
  "$OUTPUT_STAGE/restart-capabilities.json" \
  "existing Android storage was charged as a new allocation on restart"
require_contains '"required_disk_bytes": "1073741824"' \
  "$OUTPUT_STAGE/restart-capabilities.json" \
  "existing Android storage did not use the 1 GiB runtime headroom"
"$CTL" --data-root "$DATA_ROOT" --no-start-host start "$INSTANCE_ID" \
  > "$OUTPUT_STAGE/second-start.json"
require_contains '"state": "succeeded"' "$OUTPUT_STAGE/second-start.json" \
  "second Android start operation did not succeed"
wait_android_ready "$INSTANCE_ID" "$OUTPUT_STAGE/second-ready.json" second
require_contains '"observed": "ready"' "$OUTPUT_STAGE/second-ready.json" \
  "second Android boot did not reach Ready"
require_contains '"adb_ready": true' "$OUTPUT_STAGE/second-ready.json" \
  "second Android boot reached Ready without authenticated ADB"
require_contains '"last_bluetooth_hci_capture": null' \
  "$OUTPUT_STAGE/second-ready.json" \
  "new Android run retained a stale Bluetooth HCI capture record from the previous run"
verify_run_retention_fixtures
SECOND_SERIAL=$(json_string adb_serial "$OUTPUT_STAGE/second-ready.json")
[ -n "$SECOND_SERIAL" ] || fail "second Android boot did not publish an ADB serial"
SECOND_GENERATION=$(json_number frame_generation "$OUTPUT_STAGE/second-ready.json")
[ -n "$SECOND_GENERATION" ] && [ "$SECOND_GENERATION" -gt "$FIRST_GENERATION" ] ||
  fail "second Android frame generation was not monotonic"
"$ADB" -s "$SECOND_SERIAL" shell cat /data/local/tmp/hd-installed-marker \
  > "$OUTPUT_STAGE/second-marker.txt"
require_contains "$MARKER" "$OUTPUT_STAGE/second-marker.txt" \
  "Android userdata marker did not survive the second boot"
wait_android_network "$SECOND_SERIAL" "$OUTPUT_STAGE/second-network.txt" second
"$ADB" -s "$SECOND_SERIAL" shell getprop init.svc.nfc_hal_service \
  > "$OUTPUT_STAGE/second-nfc-hal-state.txt"
require_contains running "$OUTPUT_STAGE/second-nfc-hal-state.txt" \
  "second Android boot did not preserve the NFC HAL runtime policy"
"$ADB" -s "$SECOND_SERIAL" shell service check nfc \
  > "$OUTPUT_STAGE/second-nfc-framework-service.txt"
require_contains found "$OUTPUT_STAGE/second-nfc-framework-service.txt" \
  "second Android boot did not expose the NFC framework binder service"
"$ADB" -s "$SECOND_SERIAL" shell ps -A -o PID,ARGS \
  > "$OUTPUT_STAGE/second-nfc-processes.txt"
require_contains android.hardware.nfc-service.cuttlefish \
  "$OUTPUT_STAGE/second-nfc-processes.txt" \
  "second Android boot did not keep the cuttlefish NFC HAL process alive"
second_bluetooth_ready=0
attempt=0
while [ "$attempt" -lt 150 ]; do
  "$ADB" -s "$SECOND_SERIAL" shell dumpsys bluetooth_manager \
    > "$OUTPUT_STAGE/second-bluetooth-dumpsys.txt" 2>&1 || true
  "$ADB" -s "$SECOND_SERIAL" shell service check bluetooth_manager \
    > "$OUTPUT_STAGE/second-bluetooth-framework-service.txt" 2>&1 || true
  "$ADB" -s "$SECOND_SERIAL" shell pidof android.hardware.bluetooth-service.default \
    > "$OUTPUT_STAGE/second-bluetooth-hal-pids.txt" 2>/dev/null || true
  if [ -s "$OUTPUT_STAGE/second-bluetooth-hal-pids.txt" ] && \
      grep -Fq found "$OUTPUT_STAGE/second-bluetooth-framework-service.txt" && \
      grep -Fq 'enabled: true' "$OUTPUT_STAGE/second-bluetooth-dumpsys.txt" && \
      grep -Eq 'state: ON|state ON|STATE_ON' "$OUTPUT_STAGE/second-bluetooth-dumpsys.txt"; then
    second_bluetooth_ready=1
    break
  fi
  sleep 0.2
  attempt=$((attempt + 1))
done
[ "$second_bluetooth_ready" -eq 1 ] || \
  fail "second Android boot did not restore the Bluetooth HAL/framework runtime policy"
second_modem_ready=0
attempt=0
while [ "$attempt" -lt 200 ]; do
  "$ADB" -s "$SECOND_SERIAL" shell service list \
    > "$OUTPUT_STAGE/second-modem-services.txt" 2>&1 || true
  "$ADB" -s "$SECOND_SERIAL" shell getprop init.svc.vendor.ril-daemon \
    > "$OUTPUT_STAGE/second-modem-ril-state.txt" 2>/dev/null || true
  "$ADB" -s "$SECOND_SERIAL" shell pidof libcuttlefish-rild \
    > "$OUTPUT_STAGE/second-modem-ril-pids.txt" 2>/dev/null || true
  "$ADB" -s "$SECOND_SERIAL" shell getprop gsm.operator.numeric \
    > "$OUTPUT_STAGE/second-modem-operator-numeric.txt" 2>/dev/null || true
  if grep -Fq running "$OUTPUT_STAGE/second-modem-ril-state.txt" && \
      [ -s "$OUTPUT_STAGE/second-modem-ril-pids.txt" ] && \
      grep -Fq android.hardware.radio "$OUTPUT_STAGE/second-modem-services.txt" && \
      grep -Fq 00101 "$OUTPUT_STAGE/second-modem-operator-numeric.txt"; then
    second_modem_ready=1
    break
  fi
  sleep 0.2
  attempt=$((attempt + 1))
done
[ "$second_modem_ready" -eq 1 ] || \
  fail "second Android boot did not restore the RIL/Radio/framework modem policy"
"$ADB" -s "$SECOND_SERIAL" shell dumpsys telephony.registry \
  > "$OUTPUT_STAGE/second-modem-telephony-registry.txt"
require_contains '00101' "$OUTPUT_STAGE/second-modem-telephony-registry.txt" \
  "second Android boot did not publish the deterministic test operator"
second_uwb_ready=0
attempt=0
while [ "$attempt" -lt 150 ]; do
  "$ADB" -s "$SECOND_SERIAL" shell service list \
    > "$OUTPUT_STAGE/second-uwb-services.txt" 2>&1 || true
  "$ADB" -s "$SECOND_SERIAL" shell pidof android.hardware.uwb-service \
    > "$OUTPUT_STAGE/second-uwb-hal-pids.txt" 2>/dev/null || true
  "$ADB" -s "$SECOND_SERIAL" shell dumpsys uwb \
    > "$OUTPUT_STAGE/second-uwb-dumpsys.txt" 2>&1 || true
  if [ -s "$OUTPUT_STAGE/second-uwb-hal-pids.txt" ] && \
      grep -Fq android.hardware.uwb.IUwb/default "$OUTPUT_STAGE/second-uwb-services.txt" && \
      grep -Fq 'uwb:' "$OUTPUT_STAGE/second-uwb-services.txt" && \
      grep -Fq 'mCountryCode:' "$OUTPUT_STAGE/second-uwb-dumpsys.txt" && \
      grep -Fq 'US' "$OUTPUT_STAGE/second-uwb-dumpsys.txt"; then
    second_uwb_ready=1
    break
  fi
  sleep 0.2
  attempt=$((attempt + 1))
done
[ "$second_uwb_ready" -eq 1 ] || \
  fail "second Android boot did not restore the UWB HAL/framework runtime policy"

"$CTL" --data-root "$DATA_ROOT" --no-start-host shutdown --stop-all \
  > "$OUTPUT_STAGE/shutdown-stop-all.json" 2> "$OUTPUT_STAGE/shutdown-stop-all.stderr"
"$ADB" kill-server > "$OUTPUT_STAGE/adb-kill-server.txt" 2>&1 || true
[ -z "$UI_PID" ] || kill "$UI_PID" 2>/dev/null || true
attempt=0
while [ -n "$UI_PID" ] && [ "$attempt" -lt 100 ] && kill -0 "$UI_PID" 2>/dev/null; do
  sleep 0.05
  attempt=$((attempt + 1))
done
if [ -n "$UI_PID" ] && kill -0 "$UI_PID" 2>/dev/null; then
  fail "installed Android UI did not exit within the five-second cleanup budget"
fi
UI_PID=
sleep 1
if pgrep -fl "$INSTALL/HD.app" > "$OUTPUT_STAGE/process-leaks.txt"; then
  fail "installed Android distribution left an HD process running"
fi

VERSION=$(plutil -extract CFBundleShortVersionString raw -o - "$APP/Contents/Info.plist")
BUILD=$(plutil -extract CFBundleVersion raw -o - "$APP/Contents/Info.plist")
ARCHIVE_SHA256=$(shasum -a 256 "$ARCHIVE" | awk '{print $1}')
generated_at=$(date -u '+%Y-%m-%dT%H:%M:%SZ')
cat > "$OUTPUT_STAGE/result.json" <<EOF
{
  "schema_version": 1,
  "profile": "hd-macos-arm64-installed-android-v2",
  "archive_sha256": "$ARCHIVE_SHA256",
  "version": "$VERSION",
  "build": "$BUILD",
  "channel": "development",
  "artifact_distribution": "$ARTIFACT_DISTRIBUTION",
  "development_bypass": $ARTIFACT_DEVELOPMENT_BYPASS,
  "android_version": "15.0.0_r14",
  "android_data_profile": "development-unencrypted",
  "android_aggregate_sha256": "$ANDROID_AGGREGATE_SHA256",
  "guest_bundle_digest": "$GUEST_DIGEST",
  "host_bundle_digest": "$HOST_DIGEST",
  "instance_id": "$INSTANCE_ID",
  "first_frame_generation": $FIRST_GENERATION,
  "second_frame_generation": $SECOND_GENERATION,
  "first_boot": "ready",
  "second_boot": "ready",
  "initial_disk_requirement_mode": "new_instance_storage",
  "initial_required_disk_bytes": 10737418240,
  "restart_disk_requirement_mode": "existing_instance_storage",
  "restart_required_disk_bytes": 1073741824,
  "run_retention": "pass",
  "legacy_ephemeral_cleanup": "pass",
  "retained_finished_runs_before_second_stop": 20,
  "finalized_log_tail_bytes": 16777216,
  "userdata_persistence": "pass",
  "network": "pass",
  "aosp_android_controls": "pass",
  "android_bugreport": "pass-full-aosp-dumpstate",
  "android_bugreport_size_bytes": $BUGREPORT_SIZE,
  "android_bugreport_sha256": "$BUGREPORT_SHA256",
  "android_bugreport_duration_ms": $BUGREPORT_DURATION_MS,
  "android_bugreport_sensitive_data_disclosed": true,
  "android_bugreport_post_ready": true,
  "screen_recording": "pass-host-gfxstream-readback-avfoundation",
  "fixed_location_altitude": "pass",
  "fixed_location_framework_callback": "pass-gps-exact-fields-no-mock-provider",
  "location_route_import_playback": "pass",
  "location_route_framework_callback": "pass-gps-exact-fields-no-mock-provider",
  "sensor_guest_channel": "framework-verified-lazy-delivery-contract-pass",
  "bluetooth_backend": "aosp-rootcanal",
  "bluetooth_first_boot": "ready",
  "bluetooth_second_boot": "ready",
  "bluetooth_gatt_peer": "pass",
  "bluetooth_advertising": "pass",
  "bluetooth_hogp_keyboard": "pass",
  "bluetooth_hogp_pairing": "pass-legacy-just-works-encrypted",
  "bluetooth_hogp_input_notifications": "pass",
  "bluetooth_hogp_key_a": "pass",
  "bluetooth_hogp_reports_sent": 3,
  "bluetooth_hci_capture": "pass-bounded-btsnoop-h4",
  "bluetooth_hci_capture_duration_ms": 6000,
  "bluetooth_hci_packets_captured": $HCI_PACKETS_CAPTURED,
  "bluetooth_hci_capture_size_bytes": $HCI_CAPTURE_SIZE,
  "bluetooth_hci_diagnostics_delivery": "pass-manifest-and-archive-hash-bound",
  "bluetooth_hci_capture_state_new_run": "cleared",
  "diagnostic_zstd_version": "$ZSTD_VERSION",
  "diagnostic_zstd_sha256": "$ZSTD_SHA256",
  "modem_backend": "formal-host-vsock-at",
  "modem_first_boot": "ready",
  "modem_second_boot": "ready",
  "modem_test_operator": "00101",
  "uwb_backend": "formal-fira-v2-uci",
  "uwb_first_boot": "ready",
  "uwb_second_boot": "ready",
  "uwb_guest_uci": "pass",
  "uwb_fira_session": "pass-framework-321cm-status-0",
  "nfc_backend": "aosp-casimir",
  "nfc_first_boot": "ready",
  "nfc_second_boot": "ready",
  "nfc_type2": "pass",
  "nfc_type4": "pass",
  "zero_copy": true,
  "swiftshader": false,
  "process_cleanup": "pass",
  "production_security": "blocked-hardware-backed-keymint-unavailable"
}
EOF
cat > "$OUTPUT_STAGE/installed-android-gates.json" <<EOF
{
  "schema_version": 2,
  "generated_at": "$generated_at",
  "source": "scripts/macos-android-distribution-smoke.sh",
  "gates": [
    {
      "name": "macos-android-installed-guest",
      "command": "macos-android-distribution-smoke.sh --archive <candidate>",
      "status": "pass",
      "duration_ms": null,
      "log_path": null,
      "summary": "独立验证并解包 HD ${VERSION} build ${BUILD}；验证 ${ARTIFACT_DISTRIBUTION} 的精确闭包与签名状态，无外部 Android、ADB、aapt2 或 gfxstream 覆盖启动包内 Android 15 两次到 Ready。首次存储按完整 10 GiB new_instance_storage 准入，重启按 1 GiB existing_instance_storage 运行余量准入；ADB、网络、userdata 持久化、单调 frame generation 与零拷贝能力通过。包内 Worker 生成实例/run 绑定、owner-only、最大 256 MiB 的完整 AOSP bugreport，并在产物后保持 Android/ADB Ready。它还在第二次启动前把 24 个已完成 run 收敛到最近 20 个，将 64 MiB+1 历史日志压缩为 owner-only 16 MiB 尾部，删除旧完成 run 的可再生 patched initrd 且保护活动 run。AOSP RootCanal Bluetooth（含真实 HOGP 键盘输入及有界 HCI btsnoop/诊断包交付）、正式 FiRa v2 UCI UWB、Casimir NFC 与 host-vsock AT modem 在两次启动中均保持 HAL/框架服务存活；停止后无进程泄漏。开发数据盘明确未加密，硬件 KeyMint 仍是正式发布阻塞项。"
    },
    {
      "name": "macos-android-aosp-controls",
      "command": "macos-android-distribution-smoke.sh --archive <candidate>",
      "status": "pass",
      "duration_ms": null,
      "log_path": "android-aosp-controls.tsv",
      "summary": "候选包的 typed UI/CLI 控制经 Host/Worker 作用到 Android 15：四向旋转由 WindowManager 精确回读，Home/Recent/Back/音量/电源按键均执行且电源双击恢复 Awake，电池电量/充电/温度由 BatteryService 回读，网络延迟/丢包/带宽由 Guest netem 回读并恢复默认。"
    },
    {
      "name": "macos-android-bugreport-real-guest",
      "command": "hdctl bugreport <id>",
      "status": "pass",
      "duration_ms": $BUGREPORT_DURATION_MS,
      "log_path": "android-bugreport-record.json",
      "summary": "包内 ADB 对 Ready Android 15 生成完整 AOSP dumpstate ZIP；Host 生成 UUID 和固定路径，调用方不能传路径或 shell。产物绑定当前 instance/run，大小 ${BUGREPORT_SIZE} 字节、0600、SHA-256 精确回读，ZIP 包含主 bugreport 文本并明确标记可能含敏感数据；完成后 Android 和 ADB 仍为 Ready。"
    },
    {
      "name": "macos-android-screen-recording",
      "command": "hdctl screen-record-start <id> --max-duration-seconds 10",
      "status": "pass",
      "duration_ms": null,
      "log_path": "screen-recording-capability.tsv",
      "summary": "macOS 候选包继续禁用会破坏后续显示转换的 Guest screenrecord；录制期间由 gfxstream 专用 readback callback 把 Android 画面交给 AVFoundation 生成完整 MP4，停止后由 Worker 校验并发布摘要。CPU readback 仅在显式录制生命周期内启用，日常 Metal 显示链仍保持零拷贝。"
    },
    {
      "name": "macos-android-device-controls",
      "command": "macos-android-distribution-smoke.sh --archive <candidate>",
      "status": "pass",
      "duration_ms": null,
      "log_path": "android-aosp-controls.tsv",
      "summary": "同一个架构无关 test-only 探针遵守 Android 15 前台定位规则，订阅真实 GPS_PROVIDER 并从 LocationManager 回调精确读取固定点及 KML 路线点的经纬度、高度、精度与字段存在标志，未启用 mock provider，每个门结束后完成卸载；fixed-location HAL 的固定 virtio-console 请求与至少初始、受控两次交付证据同时保留。实例级 KML 路线还验证开始、暂停稳定、继续、循环和停止，DeviceSim 的 owner-only 原子交付证据确认恢复回放后 delivered sequence 递增。固定 r14 三轴姿态由 Guest 内置 AOSP motion 命令回读，不发布无对应 HAL 的独立/定时传感器。"
    },
    {
      "name": "macos-android-location-route",
      "command": "hdctl action <id> route-start <route.kml> --interval-ms 500 --repeat; route-pause; route-resume; route-stop",
      "status": "pass",
      "duration_ms": null,
      "log_path": "location-route-guest-poll.tsv",
      "summary": "候选包安全导入 KML 路线，通过 Worker 实例级任务应用首点并回放；暂停期间点位稳定，继续后 DeviceSim 成功写入 Guest fixed-location 通道且 delivered sequence 递增，同一个 test-only LocationManager 探针从真实 GPS_PROVIDER 精确回读路线点的经纬度、高度、精度与字段存在标志，停止后活动状态清空并保留 stopped 结果与已应用点数。"
    },
    {
      "name": "macos-installed-runtime-storage",
      "command": "macos-android-distribution-smoke.sh --archive <candidate>",
      "status": "pass",
      "duration_ms": null,
      "log_path": "run-retention.tsv",
      "summary": "候选包内 Worker 在隔离 Android 实例第二次启动前只维护带最终 result 的历史 run：24 个已完成记录收敛到最近 20 个，最旧记录删除，64 MiB+1 日志仅保留 owner-only 16 MiB 尾部且末尾标记精确一致，旧版本遗留的可再生 patched initrd 被删除；活动 run、其他实例和私有磁盘未被修改。"
    },
    {
      "name": "macos-nfc-real-guest",
      "command": "macos-android-distribution-smoke.sh --archive <candidate>",
      "status": "pass",
      "duration_ms": null,
      "log_path": "nfc-lifecycle.tsv",
      "summary": "包内 AOSP Casimir 以正式 component 启动并与 Guest /dev/hvc12 双向传输 NCI；NFC HAL、Cuttlefish HAL 进程与 framework binder 在 Type 2、Type 4、移除动作后仍存活，第二次 Android 启动后仍为 running/found，停止后 Casimir 与 Guest 进程均无泄漏。"
    },
    {
      "name": "macos-bluetooth-real-guest",
      "command": "macos-android-distribution-smoke.sh --archive <candidate>",
      "status": "pass",
      "duration_ms": null,
      "log_path": "bluetooth-lifecycle.tsv",
      "summary": "包内 AOSP RootCanal 以正式 component 启动并与 Guest H4 串口双向传输；Bluetooth HAL、framework binder 与 ON 状态在虚拟 GATT peer 创建、BLE 广告开关和移除动作后仍存活，第二次 Android 启动后自动恢复，停止后 RootCanal 与 Guest 进程均无泄漏。"
    },
    {
      "name": "macos-bluetooth-hogp-real-guest",
      "command": "hdctl action <id> bluetooth-hid-keyboard <peer-id> <name>; bluetooth-hid-keyboard-report <peer-id> --keys 4",
      "status": "pass",
      "duration_ms": null,
      "log_path": "bluetooth-hid-getevent.txt",
      "summary": "Android 15 设置发现并配对包内 RootCanal 模拟的 HOGP 键盘；LE legacy Just Works 完成加密与持久 bond，GATT 服务发现按有效 ATT MTU 分页读取 Report Map 和 Report Reference，Android HID Host 写入 input CCCD。Host 发出 readiness、KEY_A down 和 release 三份报告，Android /dev/input/event2 精确回读 KEY_A DOWN/UP。"
    },
    {
      "name": "macos-bluetooth-hci-capture-real-guest",
      "command": "hdctl action <id> bluetooth-hci-capture --duration-ms 6000",
      "status": "pass",
      "duration_ms": 6000,
      "log_path": "bluetooth-hci-capture.tsv",
      "summary": "显式请求只在当前实例/当前 run 的 RootCanal Guest↔Controller H4 数据面打开 6 秒录制；Android 15 自身 disable/enable Bluetooth 产生真实 HCI 流量，捕获 ${HCI_PACKETS_CAPTURED} 个 packet。输出是 datalink 1002 的标准 btsnoop、owner-only、最大 4 MiB，并由 UUID、sidecar、实际大小和 typed 状态相互绑定。诊断包精确包含 btsnoop 与 sidecar，逐文件 manifest SHA-256、manifest SHA-256 和 archive SHA-256 全部复验；下一 run 清除旧记录，不误导用户把旧文件当作当前诊断附件。"
    },
    {
      "name": "macos-uwb-real-guest",
      "command": "macos-android-distribution-smoke.sh --archive <candidate>",
      "status": "pass",
      "duration_ms": null,
      "log_path": "uwb-lifecycle.tsv",
      "summary": "包内正式 FiRa v2 UCI component 与 Guest /dev/hvc9 双向传输真实 UCI；首次启动通过 AOSP r14 framework shell 打开 session 77，回读 3.21 m、status 0 后停止并关闭。UWB HAL、AIDL HAL、framework service 与 US country-code 状态在首次和第二次 Android 启动后均存活，停止后 UWB adapter 与 Guest 进程均无泄漏。"
    },
    {
      "name": "macos-modem-real-guest",
      "command": "macos-android-distribution-smoke.sh --archive <candidate>",
      "status": "pass",
      "duration_ms": null,
      "log_path": "modem-lifecycle.tsv",
      "summary": "包内正式 modem component 通过按 Guest CID 隔离的 owner-only host-vsock UDS 与 Cuttlefish RIL 双向传输 AT；vendor RIL、Radio HAL、telephony framework 与测试运营商 00101 在首次和第二次 Android 启动后均成立，不声明物理运营商、通话、短信、IMS 或数据附着能力。"
    }
  ]
}
EOF

mv "$OUTPUT_STAGE" "$OUTPUT"
COMPLETED=1
remove_test_screen_recording
trap - EXIT HUP INT TERM
case "$INSTALL" in /private/tmp/hd-android-distribution-install.*) rm -rf -- "$INSTALL" ;; esac
case "$DATA_ROOT" in /private/tmp/hd-android-distribution-data.*) rm -rf -- "$DATA_ROOT" ;; esac
echo "evidence=$OUTPUT"
echo "gate_report=$OUTPUT/installed-android-gates.json"
echo "first_boot=ready"
echo "second_boot=ready"
echo "userdata_persistence=pass"
echo "network=pass"
echo "process_cleanup=pass"
