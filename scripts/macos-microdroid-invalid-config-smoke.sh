#!/bin/sh
set -eu
umask 077

usage() {
  cat >&2 <<'EOF'
Usage: scripts/macos-microdroid-invalid-config-smoke.sh \
  --app <HD.app> \
  --output <fresh-absolute-evidence-directory> \
  --apksigner <pinned-Android-build-tools-apksigner> \
  --zipalign <pinned-Android-build-tools-zipalign> \
  --java-home <pinned-Temurin-home> \
  --development-package
EOF
}

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

APP=
OUTPUT=
APKSIGNER=
ZIPALIGN=
JAVA_HOME_INPUT=
DEVELOPMENT_PACKAGE=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    --app) APP=$2; shift 2 ;;
    --output) OUTPUT=$2; shift 2 ;;
    --apksigner) APKSIGNER=$2; shift 2 ;;
    --zipalign) ZIPALIGN=$2; shift 2 ;;
    --java-home) JAVA_HOME_INPUT=$2; shift 2 ;;
    --development-package) DEVELOPMENT_PACKAGE=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage; exit 2 ;;
  esac
done

case "$APP:$OUTPUT:$APKSIGNER:$ZIPALIGN:$JAVA_HOME_INPUT" in
  /*:/*:/*:/*:/*) ;;
  *) fail "all paths must be absolute" ;;
esac
[ -d "$APP" ] && [ ! -L "$APP" ] || fail "--app must be a real directory"
for tool in "$APKSIGNER" "$ZIPALIGN"; do
  [ -x "$tool" ] && [ ! -L "$tool" ] || fail "Android build tool must be a real executable: $tool"
done
[ -d "$JAVA_HOME_INPUT" ] && [ ! -L "$JAVA_HOME_INPUT" ] ||
  fail "--java-home must be a real directory"
[ -x "$JAVA_HOME_INPUT/bin/java" ] || fail "--java-home does not contain java"
[ -x "$JAVA_HOME_INPUT/bin/keytool" ] || fail "--java-home does not contain keytool"
[ "$DEVELOPMENT_PACKAGE" -eq 1 ] ||
  fail "current destructive Payload fixture requires --development-package"
[ ! -e "$OUTPUT" ] || fail "refusing to replace evidence output: $OUTPUT"
FAILURE_OUTPUT="$OUTPUT.failed"
[ ! -e "$FAILURE_OUTPUT" ] || fail "refusing to replace failure evidence: $FAILURE_OUTPUT"
[ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ] ||
  fail "Microdroid invalid-config smoke requires Apple Silicon macOS"

CTL="$APP/Contents/MacOS/hdctl"
SOURCE_PAYLOAD="$APP/Contents/Resources/products/microdroid/conformance-payload/payload.apk"
[ -x "$CTL" ] && [ ! -L "$CTL" ] || fail "HD.app does not contain a real hdctl"
[ -f "$SOURCE_PAYLOAD" ] && [ ! -L "$SOURCE_PAYLOAD" ] ||
  fail "HD.app does not contain the conformance Payload"
codesign --verify --deep --strict "$APP" || fail "HD.app codesign verification failed"

OUTPUT_PARENT=$(dirname -- "$OUTPUT")
mkdir -p -- "$OUTPUT_PARENT"
STAGE=$(mktemp -d "$OUTPUT_PARENT/.hd-microdroid-invalid-config.XXXXXX")
DATA=$(mktemp -d /private/tmp/hd-microdroid-invalid-config-data.XXXXXX)
WORK=$(mktemp -d /private/tmp/hd-microdroid-invalid-config-work.XXXXXX)
INSTANCE_ID=$(uuidgen | tr '[:upper:]' '[:lower:]')
COMPLETED=0

matching_pids() {
  ps ax -o pid=,command= | grep -F -- "$DATA" | grep -v grep | awk '{print $1}'
}

capture_runtime_evidence() {
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

terminate_test_processes() {
  pids=$(matching_pids || true)
  [ -z "$pids" ] || kill $pids 2>/dev/null || true
  sleep 1
  pids=$(matching_pids || true)
  [ -z "$pids" ] || kill -KILL $pids 2>/dev/null || true
}

cleanup() {
  status=$?
  HD_MICRODROID_DEV_BYPASS=1 "$CTL" --data-root "$DATA" --no-start-host \
    shutdown --stop-all >/dev/null 2>&1 || true
  capture_runtime_evidence || true
  terminate_test_processes
  case "$DATA" in /private/tmp/hd-microdroid-invalid-config-data.*) rm -rf -- "$DATA" ;; esac
  case "$WORK" in /private/tmp/hd-microdroid-invalid-config-work.*) rm -rf -- "$WORK" ;; esac
  case "$STAGE" in
    "$OUTPUT_PARENT"/.hd-microdroid-invalid-config.*)
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

export JAVA_HOME="$JAVA_HOME_INPUT"
"$APKSIGNER" verify --verbose --print-certs "$SOURCE_PAYLOAD" \
  >"$STAGE/source-signature.txt" 2>&1
require_contains 'Verified using v3 scheme (APK Signature Scheme v3): true' \
  "$STAGE/source-signature.txt" "source Payload did not verify with APK Signature Scheme v3"

# Match AOSP MicrodroidTests.bootFailsWhenConfigIsInvalid: the config is valid JSON and present at
# the requested path, but deliberately has no task. Re-signing keeps all Host structure checks
# valid so the real microdroid_manager owns the semantic rejection.
UNPACKED="$WORK/unpacked"
mkdir -p "$UNPACKED"
unzip -q "$SOURCE_PAYLOAD" -d "$UNPACKED"
printf '%s\n' '{"export_tombstones":true}' >"$UNPACKED/assets/vm_config.json"
INVALID_CONFIG_SHA256=$(shasum -a 256 "$UNPACKED/assets/vm_config.json" | awk '{print $1}')
UNALIGNED_PAYLOAD="$WORK/payload-invalid-config-unaligned.apk"
ALIGNED_PAYLOAD="$WORK/payload-invalid-config-aligned.apk"
INVALID_PAYLOAD="$WORK/payload-invalid-config-v3.apk"
(cd "$UNPACKED" && find . -type f -print | LC_ALL=C sort | zip -X -q "$UNALIGNED_PAYLOAD" -@)
"$ZIPALIGN" -f 4 "$UNALIGNED_PAYLOAD" "$ALIGNED_PAYLOAD"
KEYSTORE="$WORK/payload-invalid-config.p12"
KEYSTORE_PASSWORD="hd-invalid-config-$INSTANCE_ID"
"$JAVA_HOME_INPUT/bin/keytool" -genkeypair -noprompt \
  -keystore "$KEYSTORE" -storetype PKCS12 -storepass "$KEYSTORE_PASSWORD" \
  -keypass "$KEYSTORE_PASSWORD" -alias hd-invalid-config -keyalg RSA -keysize 2048 \
  -validity 3650 -dname 'CN=HD Microdroid Invalid Config Test' \
  >"$STAGE/key-generation.txt" 2>&1
"$APKSIGNER" sign --ks "$KEYSTORE" --ks-key-alias hd-invalid-config \
  --ks-pass "pass:$KEYSTORE_PASSWORD" --key-pass "pass:$KEYSTORE_PASSWORD" \
  --v1-signing-enabled false --v2-signing-enabled true --v3-signing-enabled true \
  --out "$INVALID_PAYLOAD" "$ALIGNED_PAYLOAD" >"$STAGE/signing.txt" 2>&1
"$APKSIGNER" verify --verbose --print-certs "$INVALID_PAYLOAD" \
  >"$STAGE/invalid-config-signature.txt" 2>&1
require_contains 'Verified using v3 scheme (APK Signature Scheme v3): true' \
  "$STAGE/invalid-config-signature.txt" "invalid-config Payload did not verify with v3"
unzip -t "$INVALID_PAYLOAD" >"$STAGE/invalid-config-zip.txt"
unzip -p "$INVALID_PAYLOAD" assets/vm_config.json >"$STAGE/invalid-vm-config.json"
jq -e 'has("task") | not' "$STAGE/invalid-vm-config.json" >/dev/null ||
  fail "invalid-config fixture unexpectedly contained a task"
SOURCE_SHA256=$(shasum -a 256 "$SOURCE_PAYLOAD" | awk '{print $1}')
INVALID_PAYLOAD_SHA256=$(shasum -a 256 "$INVALID_PAYLOAD" | awk '{print $1}')
[ "$SOURCE_SHA256" != "$INVALID_PAYLOAD_SHA256" ] || fail "invalid fixture did not change"

run_ctl upload --microdroid-payload "$INVALID_PAYLOAD" >"$STAGE/upload.json"
UPLOAD_ID=$(jq -r .id "$STAGE/upload.json")
[ "$(jq -r .sha256 "$STAGE/upload.json")" = "$INVALID_PAYLOAD_SHA256" ] ||
  fail "upload SHA-256 changed the fixture"

jq -n --arg id "$INSTANCE_ID" --arg upload_id "$UPLOAD_ID" \
  --arg sha256 "$INVALID_PAYLOAD_SHA256" \
  '{schema_version:2,id:$id,name:"Microdroid Invalid Config",guest_kind:"microdroid",
    microdroid:{debug_level:"full",payload:{kind:"uploaded",upload_id:$upload_id,
      sha256:$sha256,config_path:"assets/vm_config.json"},encrypted_storage_mib:null},
    cpu_count:1,memory_mib:512,
    display:{width:1080,height:1920,dpi:420,refresh_rate_hz:60,
      orientation:"portrait",vsync:"on",show_host_fps:false},
    adb:{mode:"disabled",host_port:null,executable:null},artifacts:null,
    boot:{kernel_log_level:4,panic_timeout_seconds:5,boot_animation:true},
    devices:{bluetooth:false,nfc:false,uwb:false,modem:false,gnss:false,sensors:false,
      network:false,audio:false,camera:false,power:false},restart_policy:"never",
    labels:{purpose:"microdroid-invalid-config"}}' >"$STAGE/spec.json"
run_ctl create --spec "$STAGE/spec.json" >"$STAGE/create.json"
set +e
run_ctl start "$INSTANCE_ID" >"$STAGE/start.stdout" 2>"$STAGE/start.stderr"
START_STATUS=$?
set -e
[ "$START_STATUS" -ne 0 ] || fail "invalid-config Microdroid unexpectedly started"
require_contains 'code: "microdroid_invalid_payload_config"' "$STAGE/start.stderr" \
  "CLI did not return the typed invalid-config code"
require_contains 'assets/vm_config.json' "$STAGE/start.stderr" \
  "CLI error did not provide an actionable config repair message"
run_ctl show "$INSTANCE_ID" >"$STAGE/blocked-instance.json"
jq -e '.status.observed == "blocked" and
  .status.error_code == "microdroid_invalid_payload_config" and
  (.status.reason | contains("assets/vm_config.json")) and
  (.active_run_id | type == "string")' "$STAGE/blocked-instance.json" >/dev/null ||
  fail "instance did not preserve the typed invalid-config blocked state"
RUN_ID=$(jq -r .active_run_id "$STAGE/blocked-instance.json")
RUN_DIR="$DATA/runs/$INSTANCE_ID/$RUN_ID"
[ -f "$RUN_DIR/result.json" ] || fail "failed run did not publish result.json"
jq -e '.final_state == "blocked" and
  .error_code == "microdroid_invalid_payload_config"' "$RUN_DIR/result.json" >/dev/null ||
  fail "run result lost the invalid-config reason"
require_contains 'VM ended: MicrodroidInvalidPayloadConfig' \
  "$RUN_DIR/microdroid.stdout.log" "vm client did not report the AOSP death reason"
require_contains 'reason=MicrodroidInvalidPayloadConfig' \
  "$RUN_DIR/microdroid-vmclient-trace.log" "vmclient trace lost the AOSP death reason"
require_contains 'MICRODROID_INVALID_PAYLOAD_CONFIG' \
  "$RUN_DIR/microdroid-virtmgr-trace.log" "virtmgr trace lost the Guest config error"
capture_runtime_evidence

run_ctl stop "$INSTANCE_ID" --force >"$STAGE/stop.json"
run_ctl delete "$INSTANCE_ID" >"$STAGE/delete.json"
run_ctl shutdown --stop-all >"$STAGE/shutdown.json"
attempt=0
while [ "$attempt" -lt 100 ] && matching_pids | grep -q .; do
  attempt=$((attempt + 1))
  sleep 0.05
done
if matching_pids | grep -q .; then
  matching_pids >"$STAGE/process-leaks.txt"
  fail "invalid-config smoke left an isolated process running"
fi

VERSION=$(plutil -extract CFBundleShortVersionString raw -o - "$APP/Contents/Info.plist")
BUILD=$(plutil -extract CFBundleVersion raw -o - "$APP/Contents/Info.plist")
GENERATED_AT=$(date -u +%Y-%m-%dT%H:%M:%SZ)
jq -n --arg generated_at "$GENERATED_AT" --arg version "$VERSION" --arg build "$BUILD" \
  --arg instance_id "$INSTANCE_ID" --arg run_id "$RUN_ID" \
  --arg source_sha256 "$SOURCE_SHA256" --arg payload_sha256 "$INVALID_PAYLOAD_SHA256" \
  --arg config_sha256 "$INVALID_CONFIG_SHA256" \
  '{schema_version:1,profile:"macos-arm64-microdroid-invalid-config-v1",status:"pass",
    generated_at:$generated_at,version:$version,build:$build,instance_id:$instance_id,
    run_id:$run_id,fixture:{source_sha256:$source_sha256,payload_sha256:$payload_sha256,
      config_sha256:$config_sha256,config_path:"assets/vm_config.json",task_present:false,
      zip_structure:"valid",v3_signature:"valid",host_preflight:"accepted"},
    aosp_reference:"MicrodroidTests.bootFailsWhenConfigIsInvalid",
    guest_death_reason:"MicrodroidInvalidPayloadConfig",
    api_error_code:"microdroid_invalid_payload_config",observed_state:"blocked",
    process_cleanup:"pass"}' >"$STAGE/result.json"
jq -n --arg generated_at "$GENERATED_AT" --arg log_path "$OUTPUT/result.json" \
  '{schema_version:2,generated_at:$generated_at,
    source:"scripts/macos-microdroid-invalid-config-smoke.sh",
    gates:[{name:"macos-microdroid-invalid-config",
      command:"macos-microdroid-invalid-config-smoke.sh --app <HD.app> --output <fresh-dir> --apksigner <pinned> --zipalign <pinned> --java-home <pinned> --development-package",
      status:"pass",duration_ms:null,log_path:$log_path,
      summary:"A structurally valid, independently v3-signed Payload matching the AOSP no-task fixture passed HD bounded preflight, was rejected by the real Guest as MicrodroidInvalidPayloadConfig, and remained Blocked with the actionable microdroid_invalid_payload_config API code; cleanup left no test process."}]}' \
  >"$STAGE/invalid-config-gate.json"
chmod 600 "$STAGE/result.json" "$STAGE/invalid-config-gate.json"

case "$DATA" in /private/tmp/hd-microdroid-invalid-config-data.*) rm -rf -- "$DATA" ;; esac
case "$WORK" in /private/tmp/hd-microdroid-invalid-config-work.*) rm -rf -- "$WORK" ;; esac
mv -- "$STAGE" "$OUTPUT"
COMPLETED=1
trap - EXIT HUP INT TERM
echo "evidence=$OUTPUT"
echo "gate_report=$OUTPUT/invalid-config-gate.json"
