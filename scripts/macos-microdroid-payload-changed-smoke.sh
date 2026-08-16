#!/bin/sh
set -eu
umask 077

usage() {
  cat >&2 <<'EOF'
Usage: scripts/macos-microdroid-payload-changed-smoke.sh \
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
  fail "Microdroid Payload-changed smoke requires Apple Silicon macOS"

CTL="$APP/Contents/MacOS/hdctl"
SOURCE_PAYLOAD="$APP/Contents/Resources/products/microdroid/conformance-payload/payload.apk"
[ -x "$CTL" ] && [ ! -L "$CTL" ] || fail "HD.app does not contain a real hdctl"
[ -f "$SOURCE_PAYLOAD" ] && [ ! -L "$SOURCE_PAYLOAD" ] ||
  fail "HD.app does not contain the conformance Payload"
codesign --verify --deep --strict "$APP" || fail "HD.app codesign verification failed"

OUTPUT_PARENT=$(dirname -- "$OUTPUT")
mkdir -p -- "$OUTPUT_PARENT"
STAGE=$(mktemp -d "$OUTPUT_PARENT/.hd-microdroid-payload-changed.XXXXXX")
DATA=$(mktemp -d /private/tmp/hd-microdroid-payload-changed-data.XXXXXX)
WORK=$(mktemp -d /private/tmp/hd-microdroid-payload-changed-work.XXXXXX)
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
  case "$DATA" in /private/tmp/hd-microdroid-payload-changed-data.*) rm -rf -- "$DATA" ;; esac
  case "$WORK" in /private/tmp/hd-microdroid-payload-changed-work.*) rm -rf -- "$WORK" ;; esac
  case "$STAGE" in
    "$OUTPUT_PARENT"/.hd-microdroid-payload-changed.*)
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

# Rebuild the same executable Payload with only JSON whitespace changed, then sign it with a
# disposable v2/v3 key. Both APKs are cryptographically valid; the Guest must own identity binding.
UNPACKED="$WORK/unpacked"
mkdir -p "$UNPACKED"
unzip -q "$SOURCE_PAYLOAD" -d "$UNPACKED"
jq -c . "$UNPACKED/assets/vm_config.json" >"$WORK/vm_config.compact.json"
mv -- "$WORK/vm_config.compact.json" "$UNPACKED/assets/vm_config.json"
UNALIGNED_PAYLOAD="$WORK/payload-changed-unaligned.apk"
ALIGNED_PAYLOAD="$WORK/payload-changed-aligned.apk"
CHANGED_PAYLOAD="$WORK/payload-changed-v3.apk"
(cd "$UNPACKED" && find . -type f -print | LC_ALL=C sort | zip -X -q "$UNALIGNED_PAYLOAD" -@)
"$ZIPALIGN" -f 4 "$UNALIGNED_PAYLOAD" "$ALIGNED_PAYLOAD"
KEYSTORE="$WORK/payload-changed.p12"
KEYSTORE_PASSWORD="hd-payload-changed-$INSTANCE_ID"
"$JAVA_HOME_INPUT/bin/keytool" -genkeypair -noprompt \
  -keystore "$KEYSTORE" -storetype PKCS12 -storepass "$KEYSTORE_PASSWORD" \
  -keypass "$KEYSTORE_PASSWORD" -alias hd-payload-changed -keyalg RSA -keysize 2048 \
  -validity 3650 -dname 'CN=HD Microdroid Payload Changed Test' \
  >"$STAGE/key-generation.txt" 2>&1
"$APKSIGNER" sign --ks "$KEYSTORE" --ks-key-alias hd-payload-changed \
  --ks-pass "pass:$KEYSTORE_PASSWORD" --key-pass "pass:$KEYSTORE_PASSWORD" \
  --v1-signing-enabled false --v2-signing-enabled true --v3-signing-enabled true \
  --out "$CHANGED_PAYLOAD" "$ALIGNED_PAYLOAD" >"$STAGE/signing.txt" 2>&1
"$APKSIGNER" verify --verbose --print-certs "$CHANGED_PAYLOAD" \
  >"$STAGE/changed-signature.txt" 2>&1
require_contains 'Verified using v3 scheme (APK Signature Scheme v3): true' \
  "$STAGE/changed-signature.txt" "changed Payload did not verify with APK Signature Scheme v3"
unzip -t "$CHANGED_PAYLOAD" >"$STAGE/changed-zip.txt"
SOURCE_SHA256=$(shasum -a 256 "$SOURCE_PAYLOAD" | awk '{print $1}')
CHANGED_SHA256=$(shasum -a 256 "$CHANGED_PAYLOAD" | awk '{print $1}')
[ "$SOURCE_SHA256" != "$CHANGED_SHA256" ] || fail "re-signed Payload did not change SHA-256"
printf 'source_sha256\tchanged_sha256\n%s\t%s\n' "$SOURCE_SHA256" "$CHANGED_SHA256" \
  >"$STAGE/payload-identities.tsv"

run_ctl upload --microdroid-payload "$SOURCE_PAYLOAD" >"$STAGE/source-upload.json"
SOURCE_UPLOAD_ID=$(jq -r .id "$STAGE/source-upload.json")
[ "$(jq -r .sha256 "$STAGE/source-upload.json")" = "$SOURCE_SHA256" ] ||
  fail "source upload SHA-256 changed the fixture"
run_ctl upload --microdroid-payload "$CHANGED_PAYLOAD" >"$STAGE/changed-upload.json"
CHANGED_UPLOAD_ID=$(jq -r .id "$STAGE/changed-upload.json")
[ "$(jq -r .sha256 "$STAGE/changed-upload.json")" = "$CHANGED_SHA256" ] ||
  fail "changed upload SHA-256 changed the fixture"

jq -n --arg id "$INSTANCE_ID" --arg upload_id "$SOURCE_UPLOAD_ID" \
  --arg sha256 "$SOURCE_SHA256" \
  '{schema_version:2,id:$id,name:"Microdroid Payload Identity",guest_kind:"microdroid",
    microdroid:{debug_level:"full",payload:{kind:"uploaded",upload_id:$upload_id,
      sha256:$sha256,config_path:"assets/vm_config.json"},encrypted_storage_mib:null},
    cpu_count:1,memory_mib:512,
    display:{width:1080,height:1920,dpi:420,refresh_rate_hz:60,
      orientation:"portrait",vsync:"on",show_host_fps:false},
    adb:{mode:"disabled",host_port:null,executable:null},artifacts:null,
    boot:{kernel_log_level:4,panic_timeout_seconds:5,boot_animation:true},
    devices:{bluetooth:false,nfc:false,uwb:false,modem:false,gnss:false,sensors:false,
      network:false,audio:false,camera:false,power:false},restart_policy:"never",
    labels:{purpose:"microdroid-payload-changed"}}' >"$STAGE/source-spec.json"
run_ctl create --spec "$STAGE/source-spec.json" >"$STAGE/create.json"
run_ctl start "$INSTANCE_ID" >"$STAGE/source-start.json"
run_ctl show "$INSTANCE_ID" >"$STAGE/source-ready-instance.json"
jq -e '.status.observed == "ready" and (.active_run_id | type == "string")' \
  "$STAGE/source-ready-instance.json" >/dev/null || fail "source Payload did not reach Ready"
SOURCE_RUN_ID=$(jq -r .active_run_id "$STAGE/source-ready-instance.json")
run_ctl stop "$INSTANCE_ID" --force >"$STAGE/source-stop.json"
run_ctl show "$INSTANCE_ID" >"$STAGE/source-stopped-instance.json"
jq -e '.status.observed == "stopped" and .active_run_id == null' \
  "$STAGE/source-stopped-instance.json" >/dev/null || fail "source Payload did not stop cleanly"

jq --arg upload_id "$CHANGED_UPLOAD_ID" --arg sha256 "$CHANGED_SHA256" \
  '.name = "Microdroid Changed Payload" |
   .microdroid.payload.upload_id = $upload_id |
   .microdroid.payload.sha256 = $sha256' "$STAGE/source-spec.json" >"$STAGE/changed-spec.json"
run_ctl update "$INSTANCE_ID" "$STAGE/changed-spec.json" >"$STAGE/update.json"
set +e
run_ctl start "$INSTANCE_ID" >"$STAGE/changed-start.stdout" 2>"$STAGE/changed-start.stderr"
START_STATUS=$?
set -e
[ "$START_STATUS" -ne 0 ] || fail "changed Microdroid Payload unexpectedly started"
require_contains 'code: "microdroid_payload_changed"' "$STAGE/changed-start.stderr" \
  "CLI did not return the typed Payload-changed code"
require_contains 'restore the original Payload or recreate the instance' \
  "$STAGE/changed-start.stderr" "CLI error did not provide an actionable identity repair message"
run_ctl show "$INSTANCE_ID" >"$STAGE/blocked-instance.json"
jq -e '.status.observed == "blocked" and
  .status.error_code == "microdroid_payload_changed" and
  (.status.reason | contains("restore the original Payload or recreate the instance")) and
  (.active_run_id | type == "string")' "$STAGE/blocked-instance.json" >/dev/null ||
  fail "instance did not preserve the typed Payload-changed blocked state"
CHANGED_RUN_ID=$(jq -r .active_run_id "$STAGE/blocked-instance.json")
CHANGED_RUN_DIR="$DATA/runs/$INSTANCE_ID/$CHANGED_RUN_ID"
[ -f "$CHANGED_RUN_DIR/result.json" ] || fail "changed run did not publish result.json"
jq -e '.final_state == "blocked" and .error_code == "microdroid_payload_changed"' \
  "$CHANGED_RUN_DIR/result.json" >/dev/null || fail "run result lost the Payload-changed reason"
require_contains 'VM ended: MicrodroidPayloadHasChanged' \
  "$CHANGED_RUN_DIR/microdroid.stdout.log" "vm client did not report the AOSP death reason"
require_contains 'reason=MicrodroidPayloadHasChanged' \
  "$CHANGED_RUN_DIR/microdroid-vmclient-trace.log" "vmclient trace lost the AOSP death reason"
require_contains 'PAYLOAD_HAS_CHANGED' "$CHANGED_RUN_DIR/microdroid-virtmgr-trace.log" \
  "virtmgr trace lost the Guest Payload identity error"
capture_runtime_evidence

run_ctl stop "$INSTANCE_ID" --force >"$STAGE/changed-stop.json"
run_ctl delete "$INSTANCE_ID" >"$STAGE/delete.json"
run_ctl shutdown --stop-all >"$STAGE/shutdown.json"
attempt=0
while [ "$attempt" -lt 100 ] && matching_pids | grep -q .; do
  attempt=$((attempt + 1))
  sleep 0.05
done
if matching_pids | grep -q .; then
  matching_pids >"$STAGE/process-leaks.txt"
  fail "Payload-changed smoke left an isolated process running"
fi

VERSION=$(plutil -extract CFBundleShortVersionString raw -o - "$APP/Contents/Info.plist")
BUILD=$(plutil -extract CFBundleVersion raw -o - "$APP/Contents/Info.plist")
GENERATED_AT=$(date -u +%Y-%m-%dT%H:%M:%SZ)
jq -n --arg generated_at "$GENERATED_AT" --arg version "$VERSION" --arg build "$BUILD" \
  --arg instance_id "$INSTANCE_ID" --arg source_run_id "$SOURCE_RUN_ID" \
  --arg changed_run_id "$CHANGED_RUN_ID" --arg source_sha256 "$SOURCE_SHA256" \
  --arg changed_sha256 "$CHANGED_SHA256" \
  '{schema_version:1,profile:"macos-arm64-microdroid-payload-changed-v1",status:"pass",
    generated_at:$generated_at,version:$version,build:$build,instance_id:$instance_id,
    source_run_id:$source_run_id,changed_run_id:$changed_run_id,
    fixture:{source_sha256:$source_sha256,changed_sha256:$changed_sha256,
      source_v3_signature:"valid",changed_v3_signature:"valid",host_preflight:"accepted"},
    guest_death_reason:"MicrodroidPayloadHasChanged",
    api_error_code:"microdroid_payload_changed",observed_state:"blocked",
    process_cleanup:"pass"}' >"$STAGE/result.json"
jq -n --arg generated_at "$GENERATED_AT" --arg log_path "$OUTPUT/result.json" \
  '{schema_version:2,generated_at:$generated_at,
    source:"scripts/macos-microdroid-payload-changed-smoke.sh",
    gates:[{name:"macos-microdroid-payload-changed",
      command:"macos-microdroid-payload-changed-smoke.sh --app <HD.app> --output <fresh-dir> --apksigner <pinned> --zipalign <pinned> --java-home <pinned> --development-package",
      status:"pass",duration_ms:null,log_path:$log_path,
      summary:"The original valid v3 Payload reached Ready; a semantically equivalent, independently v3-signed Payload on the same instance was rejected by the real Guest as MicrodroidPayloadHasChanged and remained Blocked with the actionable microdroid_payload_changed API code; cleanup left no test process."}]}' \
  >"$STAGE/payload-changed-gate.json"
chmod 600 "$STAGE/result.json" "$STAGE/payload-changed-gate.json"

case "$DATA" in /private/tmp/hd-microdroid-payload-changed-data.*) rm -rf -- "$DATA" ;; esac
case "$WORK" in /private/tmp/hd-microdroid-payload-changed-work.*) rm -rf -- "$WORK" ;; esac
mv -- "$STAGE" "$OUTPUT"
COMPLETED=1
trap - EXIT HUP INT TERM
echo "evidence=$OUTPUT"
echo "gate_report=$OUTPUT/payload-changed-gate.json"
