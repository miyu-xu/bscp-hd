#!/bin/sh
set -eu
umask 077

usage() {
  cat <<'EOF'
Usage: scripts/macos-microdroid-extra-apk-smoke.sh \
  --app <HD.app> \
  --output <fresh-evidence-directory> \
  --payload <v3-signed-main-payload.apk> \
  --extra-apk-0 <v3-signed-extra-0.apk> \
  --extra-apk-1 <v3-signed-extra-1.apk> \
  --asset-path-0 <safe-path-inside-extra-0> \
  --asset-path-1 <safe-path-inside-extra-1> \
  --invalid-signature-extra-apk <APK-without-v3-or-v3.1-signing-block> \
  [--development-package]

The main Payload must declare exactly two extra_apks in assets/vm_config.json.
The two asset paths must contain distinguishable bytes. The runner owns a fresh
data root and never terminates processes outside that root.
EOF
}

APP=
OUTPUT=
PAYLOAD=
EXTRA_APK_0=
EXTRA_APK_1=
ASSET_PATH_0=
ASSET_PATH_1=
INVALID_SIGNATURE_EXTRA_APK=
DEVELOPMENT_PACKAGE=0

while [ "$#" -gt 0 ]; do
  case "$1" in
    --app) APP=$2; shift 2 ;;
    --output) OUTPUT=$2; shift 2 ;;
    --payload) PAYLOAD=$2; shift 2 ;;
    --extra-apk-0) EXTRA_APK_0=$2; shift 2 ;;
    --extra-apk-1) EXTRA_APK_1=$2; shift 2 ;;
    --asset-path-0) ASSET_PATH_0=$2; shift 2 ;;
    --asset-path-1) ASSET_PATH_1=$2; shift 2 ;;
    --invalid-signature-extra-apk) INVALID_SIGNATURE_EXTRA_APK=$2; shift 2 ;;
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

require_contains() {
  expected=$1
  file=$2
  message=$3
  grep -Fq "$expected" "$file" || fail "$message (evidence: $file)"
}

require_safe_asset_path() {
  value=$1
  label=$2
  [ -n "$value" ] || fail "$label cannot be empty"
  case "$value" in
    /*|*..*|*//*|*[!A-Za-z0-9._/-]*) fail "$label is not a safe APK-relative path: $value" ;;
  esac
}

for command in codesign jq lsof plutil shasum unzip uuidgen awk grep ps; do
  command -v "$command" >/dev/null 2>&1 || fail "required tool is missing: $command"
done
[ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ] ||
  fail "Microdroid extra APK Guest smoke requires Apple Silicon macOS"
require_abs_dir "$APP" --app
require_abs_file "$PAYLOAD" --payload
require_abs_file "$EXTRA_APK_0" --extra-apk-0
require_abs_file "$EXTRA_APK_1" --extra-apk-1
require_abs_file "$INVALID_SIGNATURE_EXTRA_APK" --invalid-signature-extra-apk
require_safe_asset_path "$ASSET_PATH_0" --asset-path-0
require_safe_asset_path "$ASSET_PATH_1" --asset-path-1
case "$OUTPUT" in /*) ;; *) fail "--output must be absolute" ;; esac
[ ! -e "$OUTPUT" ] || fail "refusing to replace evidence output: $OUTPUT"
FAILURE_OUTPUT="$OUTPUT.failed"
[ ! -e "$FAILURE_OUTPUT" ] || fail "refusing to replace failure evidence: $FAILURE_OUTPUT"

CTL="$APP/Contents/MacOS/hdctl"
ADB="$APP/Contents/MacOS/adb"
PACKAGE_PAYLOAD_MANIFEST="$APP/Contents/Resources/products/microdroid/conformance-payload/payload-bundle-v1.plist"
require_abs_file "$CTL" hdctl
require_abs_file "$ADB" adb
require_abs_file "$PACKAGE_PAYLOAD_MANIFEST" payload-bundle-v1.plist
codesign --verify --deep --strict "$APP" || fail "HD.app code signature verification failed"
CHANNEL=$(plutil -extract channel raw -o - "$PACKAGE_PAYLOAD_MANIFEST")
case "$CHANNEL:$DEVELOPMENT_PACKAGE" in
  development:1|release:0) ;;
  development:0) fail "development HD.app requires --development-package" ;;
  release:1) fail "release HD.app rejects --development-package" ;;
  *) fail "unsupported HD.app channel: $CHANNEL" ;;
esac

output_parent=$(dirname -- "$OUTPUT")
mkdir -p "$output_parent"
OUTPUT_STAGE=$(mktemp -d "$output_parent/.hd-microdroid-extra-evidence.XXXXXX")
DATA_ROOT=$(mktemp -d /private/tmp/hd-microdroid-extra-data.XXXXXX)
COMPLETED=0

own_process_ids() {
  HD_PROCESS_ROOT="$DATA_ROOT" \
    awk 'index($0, ENVIRON["HD_PROCESS_ROOT"]) &&
         $2 ~ /(^|\/)(hd-host|hd-worker|crosvm|vm|virtmgr)$/ {
           print $1
         }' <<EOF
$(ps -axo pid=,comm=,command=)
EOF
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
  case "$DATA_ROOT" in /private/tmp/hd-microdroid-extra-data.*) rm -rf -- "$DATA_ROOT" ;; esac
  case "$OUTPUT_STAGE" in
    "$output_parent"/.hd-microdroid-extra-evidence.*)
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

upload_apk() {
  apk=$1
  mode=$2
  prefix=$3
  if [ "$mode" = payload ]; then
    run_ctl "$OUTPUT_STAGE/$prefix.json" "$OUTPUT_STAGE/$prefix.stderr" \
      upload --microdroid-payload "$apk"
  else
    run_ctl "$OUTPUT_STAGE/$prefix.json" "$OUTPUT_STAGE/$prefix.stderr" upload "$apk"
  fi
  UPLOAD_ID=$(jq -er '.id' "$OUTPUT_STAGE/$prefix.json")
  UPLOAD_SHA=$(jq -er '.sha256 | select(test("^[0-9a-f]{64}$"))' \
    "$OUTPUT_STAGE/$prefix.json")
}

wait_for_adb() {
  instance_id=$1
  prefix=$2
  attempt=0
  while [ "$attempt" -lt 60 ]; do
    run_ctl "$OUTPUT_STAGE/$prefix-show-$attempt.json" \
      "$OUTPUT_STAGE/$prefix-show-$attempt.stderr" show "$instance_id"
    if [ "$(jq -r '.status.observed' "$OUTPUT_STAGE/$prefix-show-$attempt.json")" = ready ] &&
       [ "$(jq -r '.adb_ready' "$OUTPUT_STAGE/$prefix-show-$attempt.json")" = true ]; then
      cp "$OUTPUT_STAGE/$prefix-show-$attempt.json" "$OUTPUT_STAGE/$prefix-ready.json"
      READY_SERIAL=$(jq -er '.adb_serial' "$OUTPUT_STAGE/$prefix-ready.json")
      READY_RUN_ID=$(jq -er '.active_run_id' "$OUTPUT_STAGE/$prefix-ready.json")
      return
    fi
    sleep 1
    attempt=$((attempt + 1))
  done
  fail "$prefix did not reach ADB Ready within 60 seconds"
}

verity_root() {
  table=$1
  awk '{ for (i = 1; i <= NF; i++) if ($i ~ /^[0-9a-fA-F]{64}$/) { print tolower($i); exit } }' \
    "$table"
}

verify_guest_extra_apks() {
  serial=$1
  prefix=$2
  "$ADB" -s "$serial" shell getprop ro.build.version.sdk \
    >"$OUTPUT_STAGE/$prefix-sdk.txt" 2>"$OUTPUT_STAGE/$prefix-sdk.stderr"
  [ "$(tr -d '\r\n' <"$OUTPUT_STAGE/$prefix-sdk.txt")" = 35 ] ||
    fail "$prefix Microdroid SDK is not Android 15"
  "$ADB" -s "$serial" exec-out cat "/mnt/extra-apk/0/$ASSET_PATH_0" \
    >"$OUTPUT_STAGE/$prefix-extra-0.asset"
  "$ADB" -s "$serial" exec-out cat "/mnt/extra-apk/1/$ASSET_PATH_1" \
    >"$OUTPUT_STAGE/$prefix-extra-1.asset"
  GUEST_ASSET_SHA_0=$(shasum -a 256 "$OUTPUT_STAGE/$prefix-extra-0.asset" | awk '{print $1}')
  GUEST_ASSET_SHA_1=$(shasum -a 256 "$OUTPUT_STAGE/$prefix-extra-1.asset" | awk '{print $1}')
  [ "$GUEST_ASSET_SHA_0" = "$HOST_ASSET_SHA_0" ] || fail "$prefix extra APK 0 asset mismatch"
  [ "$GUEST_ASSET_SHA_1" = "$HOST_ASSET_SHA_1" ] || fail "$prefix extra APK 1 asset mismatch"
  [ "$GUEST_ASSET_SHA_0" != "$GUEST_ASSET_SHA_1" ] || fail "extra APK marker assets are not distinguishable"
  "$ADB" -s "$serial" shell dmctl table extra-apk-0 \
    >"$OUTPUT_STAGE/$prefix-extra-0.dm" 2>"$OUTPUT_STAGE/$prefix-extra-0.dm.stderr"
  "$ADB" -s "$serial" shell dmctl table extra-apk-1 \
    >"$OUTPUT_STAGE/$prefix-extra-1.dm" 2>"$OUTPUT_STAGE/$prefix-extra-1.dm.stderr"
  require_contains verity "$OUTPUT_STAGE/$prefix-extra-0.dm" "$prefix extra APK 0 is not dm-verity backed"
  require_contains verity "$OUTPUT_STAGE/$prefix-extra-1.dm" "$prefix extra APK 1 is not dm-verity backed"
  VERITY_ROOT_0=$(verity_root "$OUTPUT_STAGE/$prefix-extra-0.dm")
  VERITY_ROOT_1=$(verity_root "$OUTPUT_STAGE/$prefix-extra-1.dm")
  [ "${#VERITY_ROOT_0}" -eq 64 ] && [ "${#VERITY_ROOT_1}" -eq 64 ] ||
    fail "$prefix did not expose two dm-verity root digests"
  [ "$VERITY_ROOT_0" != "$VERITY_ROOT_1" ] || fail "$prefix extra APKs reused one verity root"
}

verify_run_manifest() {
  run_id=$1
  prefix=$2
  manifest="$DATA_ROOT/runs/$SUCCESS_ID/$run_id/manifest.json"
  require_abs_file "$manifest" "$prefix manifest"
  cp "$manifest" "$OUTPUT_STAGE/$prefix-manifest.json"
  [ "$(jq '[.launch.arguments[] | select(. == "--extra-apk-override")] | length' "$manifest")" -eq 2 ] ||
    fail "$prefix manifest does not contain exactly two caller-opened extra APK overrides"
  [ "$(jq '[.launch.arguments[] | select(. == "--extra-idsig")] | length' "$manifest")" -eq 2 ] ||
    fail "$prefix manifest does not contain exactly two extra idsig descriptors"
  require_contains "$EXTRA_UPLOAD_ID_0.apk" "$manifest" "$prefix manifest omitted managed extra APK 0"
  require_contains "$EXTRA_UPLOAD_ID_1.apk" "$manifest" "$prefix manifest omitted managed extra APK 1"
  for declared_path in "$DECLARED_PATH_0" "$DECLARED_PATH_1"; do
    if jq -e --arg path "$declared_path" 'any(.launch.arguments[]; . == $path)' "$manifest" \
      >/dev/null; then
      fail "$prefix manifest used Payload-declared Host path '$declared_path'"
    fi
  done
}

verify_finished_idsig_cleanup() {
  run_id=$1
  prefix=$2
  run_dir="$DATA_ROOT/runs/$SUCCESS_ID/$run_id"
  run_ctl "$OUTPUT_STAGE/$prefix-stopped.json" "$OUTPUT_STAGE/$prefix-stopped.stderr" \
    show "$SUCCESS_ID"
  jq -e '.status.observed == "stopped" and .active_run_id == null' \
    "$OUTPUT_STAGE/$prefix-stopped.json" >/dev/null ||
    fail "$prefix did not converge to Stopped before idsig cleanup"
  require_abs_file "$run_dir/result.json" "$prefix result"
  [ ! -e "$run_dir/microdroid-extra-0.idsig" ] || fail "$prefix retained extra idsig 0"
  [ ! -e "$run_dir/microdroid-extra-1.idsig" ] || fail "$prefix retained extra idsig 1"
  if ps -axo command= | grep -E "[v]m create-idsig.*$run_dir|[v]irtmgr.*$run_dir" >/dev/null; then
    fail "$prefix retained an idsig helper process"
  fi
  if lsof +L1 -Fn 2>/dev/null | grep -F "$run_dir/microdroid-extra-" >/dev/null; then
    fail "$prefix retained an open descriptor for a deleted extra idsig"
  fi
}

mkdir -p "$OUTPUT_STAGE/preflight" "$OUTPUT_STAGE/first" "$OUTPUT_STAGE/second"
unzip -p "$PAYLOAD" assets/vm_config.json >"$OUTPUT_STAGE/main-vm-config.json" ||
  fail "main Payload does not contain assets/vm_config.json"
[ "$(jq -er '.extra_apks | length' "$OUTPUT_STAGE/main-vm-config.json")" -eq 2 ] ||
  fail "main Payload must declare exactly two extra APKs"
jq -e '.extra_apks | all(.path | type == "string" and length > 0)' \
  "$OUTPUT_STAGE/main-vm-config.json" >/dev/null || fail "main Payload has an invalid extra APK declaration"
DECLARED_PATH_0=$(jq -er '.extra_apks[0].path' "$OUTPUT_STAGE/main-vm-config.json")
DECLARED_PATH_1=$(jq -er '.extra_apks[1].path' "$OUTPUT_STAGE/main-vm-config.json")
unzip -p "$EXTRA_APK_0" "$ASSET_PATH_0" >"$OUTPUT_STAGE/host-extra-0.asset" ||
  fail "extra APK 0 does not contain $ASSET_PATH_0"
unzip -p "$EXTRA_APK_1" "$ASSET_PATH_1" >"$OUTPUT_STAGE/host-extra-1.asset" ||
  fail "extra APK 1 does not contain $ASSET_PATH_1"
HOST_ASSET_SHA_0=$(shasum -a 256 "$OUTPUT_STAGE/host-extra-0.asset" | awk '{print $1}')
HOST_ASSET_SHA_1=$(shasum -a 256 "$OUTPUT_STAGE/host-extra-1.asset" | awk '{print $1}')
[ "$HOST_ASSET_SHA_0" != "$HOST_ASSET_SHA_1" ] || fail "extra APK marker assets must differ"

run_ctl "$OUTPUT_STAGE/health.json" "$OUTPUT_STAGE/health.stderr" health
upload_apk "$PAYLOAD" payload upload-main
PAYLOAD_UPLOAD_ID=$UPLOAD_ID
PAYLOAD_UPLOAD_SHA=$UPLOAD_SHA
upload_apk "$EXTRA_APK_0" extra upload-extra-0
EXTRA_UPLOAD_ID_0=$UPLOAD_ID
EXTRA_UPLOAD_SHA_0=$UPLOAD_SHA
upload_apk "$EXTRA_APK_1" extra upload-extra-1
EXTRA_UPLOAD_ID_1=$UPLOAD_ID
EXTRA_UPLOAD_SHA_1=$UPLOAD_SHA
upload_apk "$INVALID_SIGNATURE_EXTRA_APK" extra upload-invalid-signature
INVALID_UPLOAD_ID=$UPLOAD_ID
INVALID_UPLOAD_SHA=$UPLOAD_SHA

SUCCESS_ID=$(uuidgen | tr '[:upper:]' '[:lower:]')
cat >"$OUTPUT_STAGE/spec.json" <<EOF
{
  "schema_version": 2,
  "id": "$SUCCESS_ID",
  "name": "Microdroid Extra APK Acceptance",
  "guest_kind": "microdroid",
  "microdroid": {
    "debug_level": "full",
    "cpu_topology": "one_cpu",
    "payload": {
      "kind": "uploaded",
      "upload_id": "$PAYLOAD_UPLOAD_ID",
      "sha256": "$PAYLOAD_UPLOAD_SHA",
      "config_path": "assets/vm_config.json"
    },
    "payload_extra_apk_count": 2,
    "extra_apks": [
      { "upload_id": "$EXTRA_UPLOAD_ID_0", "sha256": "$EXTRA_UPLOAD_SHA_0" },
      { "upload_id": "$EXTRA_UPLOAD_ID_1", "sha256": "$EXTRA_UPLOAD_SHA_1" }
    ],
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
  "labels": { "purpose": "microdroid-extra-apk-acceptance" }
}
EOF

INCOMPLETE_ID=$(uuidgen | tr '[:upper:]' '[:lower:]')
jq --arg id "$INCOMPLETE_ID" '.id = $id | .name = "Microdroid Extra APK Incomplete" | .microdroid.extra_apks |= .[0:1]' \
  "$OUTPUT_STAGE/spec.json" >"$OUTPUT_STAGE/preflight/incomplete-spec.json"
run_ctl "$OUTPUT_STAGE/preflight/incomplete-create.json" "$OUTPUT_STAGE/preflight/incomplete-create.stderr" \
  create --spec "$OUTPUT_STAGE/preflight/incomplete-spec.json"
run_ctl "$OUTPUT_STAGE/preflight/incomplete-capabilities.json" \
  "$OUTPUT_STAGE/preflight/incomplete-capabilities.stderr" capabilities "$INCOMPLETE_ID"
jq -e '.probes[] | select(.id == "microdroid.profile" and .status == "blocked" and .properties.extra_apk_selection_complete == "false")' \
  "$OUTPUT_STAGE/preflight/incomplete-capabilities.json" >/dev/null ||
  fail "incomplete extra APK selection was not blocked by capability preflight"
expect_ctl_failure capability_blocked "$OUTPUT_STAGE/preflight/incomplete-start.json" \
  "$OUTPUT_STAGE/preflight/incomplete-start.stderr" start "$INCOMPLETE_ID"
run_ctl "$OUTPUT_STAGE/preflight/incomplete-delete.json" "$OUTPUT_STAGE/preflight/incomplete-delete.stderr" \
  delete "$INCOMPLETE_ID"

DUPLICATE_ID=$(uuidgen | tr '[:upper:]' '[:lower:]')
jq --arg id "$DUPLICATE_ID" '.id = $id | .name = "Microdroid Extra APK Duplicate" | .microdroid.extra_apks[1] = .microdroid.extra_apks[0]' \
  "$OUTPUT_STAGE/spec.json" >"$OUTPUT_STAGE/preflight/duplicate-spec.json"
expect_ctl_failure 'cannot be repeated' "$OUTPUT_STAGE/preflight/duplicate-create.json" \
  "$OUTPUT_STAGE/preflight/duplicate-create.stderr" create --spec "$OUTPUT_STAGE/preflight/duplicate-spec.json"

OVERSELECTED_ID=$(uuidgen | tr '[:upper:]' '[:lower:]')
jq --arg id "$OVERSELECTED_ID" --arg upload "$INVALID_UPLOAD_ID" --arg digest "$INVALID_UPLOAD_SHA" \
  '.id = $id | .name = "Microdroid Extra APK Overselected" | .microdroid.extra_apks += [{"upload_id":$upload,"sha256":$digest}]' \
  "$OUTPUT_STAGE/spec.json" >"$OUTPUT_STAGE/preflight/overselected-spec.json"
expect_ctl_failure 'exceed the count declared' "$OUTPUT_STAGE/preflight/overselected-create.json" \
  "$OUTPUT_STAGE/preflight/overselected-create.stderr" create --spec "$OUTPUT_STAGE/preflight/overselected-spec.json"

BAD_DIGEST_ID=$(uuidgen | tr '[:upper:]' '[:lower:]')
BAD_DIGEST=0000000000000000000000000000000000000000000000000000000000000000
jq --arg id "$BAD_DIGEST_ID" --arg digest "$BAD_DIGEST" \
  '.id = $id | .name = "Microdroid Extra APK Bad Digest" | .microdroid.extra_apks[0].sha256 = $digest' \
  "$OUTPUT_STAGE/spec.json" >"$OUTPUT_STAGE/preflight/bad-digest-spec.json"
run_ctl "$OUTPUT_STAGE/preflight/bad-digest-create.json" "$OUTPUT_STAGE/preflight/bad-digest-create.stderr" \
  create --spec "$OUTPUT_STAGE/preflight/bad-digest-spec.json"
expect_ctl_failure upload_digest_mismatch "$OUTPUT_STAGE/preflight/bad-digest-start.json" \
  "$OUTPUT_STAGE/preflight/bad-digest-start.stderr" start "$BAD_DIGEST_ID"
run_ctl "$OUTPUT_STAGE/preflight/bad-digest-delete.json" "$OUTPUT_STAGE/preflight/bad-digest-delete.stderr" \
  delete "$BAD_DIGEST_ID"

INVALID_SIGNATURE_ID=$(uuidgen | tr '[:upper:]' '[:lower:]')
jq --arg id "$INVALID_SIGNATURE_ID" --arg upload "$INVALID_UPLOAD_ID" --arg digest "$INVALID_UPLOAD_SHA" \
  '.id = $id | .name = "Microdroid Extra APK Invalid Signature" | .microdroid.extra_apks[0] = {"upload_id":$upload,"sha256":$digest}' \
  "$OUTPUT_STAGE/spec.json" >"$OUTPUT_STAGE/preflight/invalid-signature-spec.json"
run_ctl "$OUTPUT_STAGE/preflight/invalid-signature-create.json" \
  "$OUTPUT_STAGE/preflight/invalid-signature-create.stderr" create --spec "$OUTPUT_STAGE/preflight/invalid-signature-spec.json"
expect_ctl_failure 'Signature Scheme v3' "$OUTPUT_STAGE/preflight/invalid-signature-start.json" \
  "$OUTPUT_STAGE/preflight/invalid-signature-start.stderr" start "$INVALID_SIGNATURE_ID"
run_ctl "$OUTPUT_STAGE/preflight/invalid-signature-delete.json" \
  "$OUTPUT_STAGE/preflight/invalid-signature-delete.stderr" delete "$INVALID_SIGNATURE_ID"

run_ctl "$OUTPUT_STAGE/create.json" "$OUTPUT_STAGE/create.stderr" create --spec "$OUTPUT_STAGE/spec.json"
run_ctl "$OUTPUT_STAGE/capabilities.json" "$OUTPUT_STAGE/capabilities.stderr" capabilities "$SUCCESS_ID"
jq -e '.probes[] | select(.id == "microdroid.profile" and .status == "supported" and .properties.extra_apk_declared_count == "2" and .properties.extra_apk_count == "2" and .properties.extra_apk_selection_complete == "true")' \
  "$OUTPUT_STAGE/capabilities.json" >/dev/null || fail "complete extra APK profile was not supported"

run_ctl "$OUTPUT_STAGE/first/start.json" "$OUTPUT_STAGE/first/start.stderr" start "$SUCCESS_ID"
wait_for_adb "$SUCCESS_ID" first
FIRST_SERIAL=$READY_SERIAL
FIRST_RUN_ID=$READY_RUN_ID
verify_run_manifest "$FIRST_RUN_ID" first
verify_guest_extra_apks "$FIRST_SERIAL" first
FIRST_VERITY_ROOT_0=$VERITY_ROOT_0
FIRST_VERITY_ROOT_1=$VERITY_ROOT_1
run_ctl "$OUTPUT_STAGE/first/stop.json" "$OUTPUT_STAGE/first/stop.stderr" stop "$SUCCESS_ID"
verify_finished_idsig_cleanup "$FIRST_RUN_ID" first

run_ctl "$OUTPUT_STAGE/second/start.json" "$OUTPUT_STAGE/second/start.stderr" start "$SUCCESS_ID"
wait_for_adb "$SUCCESS_ID" second
SECOND_SERIAL=$READY_SERIAL
SECOND_RUN_ID=$READY_RUN_ID
[ "$FIRST_RUN_ID" != "$SECOND_RUN_ID" ] || fail "second start reused the first run id"
verify_run_manifest "$SECOND_RUN_ID" second
verify_guest_extra_apks "$SECOND_SERIAL" second
[ "$FIRST_VERITY_ROOT_0" = "$VERITY_ROOT_0" ] || fail "extra APK 0 verity identity changed across restart"
[ "$FIRST_VERITY_ROOT_1" = "$VERITY_ROOT_1" ] || fail "extra APK 1 verity identity changed across restart"
run_ctl "$OUTPUT_STAGE/second/stop.json" "$OUTPUT_STAGE/second/stop.stderr" stop "$SUCCESS_ID"
verify_finished_idsig_cleanup "$SECOND_RUN_ID" second
run_ctl "$OUTPUT_STAGE/delete.json" "$OUTPUT_STAGE/delete.stderr" delete "$SUCCESS_ID"
run_ctl "$OUTPUT_STAGE/shutdown.json" "$OUTPUT_STAGE/shutdown.stderr" shutdown
sleep 1
[ -z "$(own_process_ids)" ] || fail "extra APK smoke left a process bound to its private data root"

VERSION=$(plutil -extract CFBundleShortVersionString raw -o - "$APP/Contents/Info.plist")
BUILD=$(plutil -extract CFBundleVersion raw -o - "$APP/Contents/Info.plist")
cat >"$OUTPUT_STAGE/result.json" <<EOF
{
  "schema_version": 1,
  "gate": "macos-microdroid-extra-apk-real-guest",
  "status": "pass",
  "version": "$VERSION",
  "build": "$BUILD",
  "channel": "$CHANNEL",
  "development_bypass": $([ "$DEVELOPMENT_PACKAGE" -eq 1 ] && echo true || echo false),
  "instance_id": "$SUCCESS_ID",
  "main_payload_sha256": "$PAYLOAD_UPLOAD_SHA",
  "extra_apk_sha256": ["$EXTRA_UPLOAD_SHA_0", "$EXTRA_UPLOAD_SHA_1"],
  "asset_sha256": ["$HOST_ASSET_SHA_0", "$HOST_ASSET_SHA_1"],
  "first_run_id": "$FIRST_RUN_ID",
  "second_run_id": "$SECOND_RUN_ID",
  "first_adb_serial": "$FIRST_SERIAL",
  "second_adb_serial": "$SECOND_SERIAL",
  "verity_root_sha256": ["$FIRST_VERITY_ROOT_0", "$FIRST_VERITY_ROOT_1"],
  "negative_preflight": {
    "incomplete_selection": "capability_blocked",
    "overselected_count": "config_rejected",
    "duplicate_upload": "config_rejected",
    "digest_mismatch": "upload_digest_mismatch",
    "invalid_v3_signature": "rejected_before_vm"
  },
  "caller_opened_descriptor_override": true,
  "guest_mount_order_verified": true,
  "second_start_identity_stable": true,
  "finished_run_extra_idsig_cleanup": true,
  "process_cleanup": "pass"
}
EOF

mv "$OUTPUT_STAGE" "$OUTPUT"
COMPLETED=1
trap - EXIT HUP INT TERM
case "$DATA_ROOT" in /private/tmp/hd-microdroid-extra-data.*) rm -rf -- "$DATA_ROOT" ;; esac
echo "evidence=$OUTPUT"
echo "gate=macos-microdroid-extra-apk-real-guest"
echo "status=pass"
