#!/bin/sh
set -eu
umask 077

usage() {
  cat >&2 <<'EOF'
Usage: scripts/macos-microdroid-death-reason-smoke.sh \
  --app <HD.app> \
  --output <fresh-absolute-evidence-directory> \
  --apksigner <pinned-Android-build-tools-apksigner> \
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
JAVA_HOME_INPUT=
DEVELOPMENT_PACKAGE=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    --app) APP=$2; shift 2 ;;
    --output) OUTPUT=$2; shift 2 ;;
    --apksigner) APKSIGNER=$2; shift 2 ;;
    --java-home) JAVA_HOME_INPUT=$2; shift 2 ;;
    --development-package) DEVELOPMENT_PACKAGE=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage; exit 2 ;;
  esac
done

case "$APP:$OUTPUT:$APKSIGNER:$JAVA_HOME_INPUT" in
  /*:/*:/*:/*) ;;
  *) fail "all paths must be absolute" ;;
esac
[ -d "$APP" ] && [ ! -L "$APP" ] || fail "--app must be a real directory"
[ -x "$APKSIGNER" ] && [ ! -L "$APKSIGNER" ] ||
  fail "--apksigner must be a real executable"
[ -d "$JAVA_HOME_INPUT" ] && [ ! -L "$JAVA_HOME_INPUT" ] ||
  fail "--java-home must be a real directory"
[ -x "$JAVA_HOME_INPUT/bin/java" ] || fail "--java-home does not contain java"
[ "$DEVELOPMENT_PACKAGE" -eq 1 ] ||
  fail "current destructive Payload fixture requires --development-package"
[ ! -e "$OUTPUT" ] || fail "refusing to replace evidence output: $OUTPUT"
FAILURE_OUTPUT="$OUTPUT.failed"
[ ! -e "$FAILURE_OUTPUT" ] || fail "refusing to replace failure evidence: $FAILURE_OUTPUT"
[ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ] ||
  fail "Microdroid death-reason smoke requires Apple Silicon macOS"

CTL="$APP/Contents/MacOS/hdctl"
SOURCE_PAYLOAD="$APP/Contents/Resources/products/microdroid/conformance-payload/payload.apk"
[ -x "$CTL" ] && [ ! -L "$CTL" ] || fail "HD.app does not contain a real hdctl"
[ -f "$SOURCE_PAYLOAD" ] && [ ! -L "$SOURCE_PAYLOAD" ] ||
  fail "HD.app does not contain the conformance Payload"
codesign --verify --deep --strict "$APP" || fail "HD.app codesign verification failed"

OUTPUT_PARENT=$(dirname -- "$OUTPUT")
mkdir -p -- "$OUTPUT_PARENT"
STAGE=$(mktemp -d "$OUTPUT_PARENT/.hd-microdroid-death.XXXXXX")
DATA=$(mktemp -d /private/tmp/hd-microdroid-death-data.XXXXXX)
WORK=$(mktemp -d /private/tmp/hd-microdroid-death-work.XXXXXX)
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
  case "$DATA" in /private/tmp/hd-microdroid-death-data.*) rm -rf -- "$DATA" ;; esac
  case "$WORK" in /private/tmp/hd-microdroid-death-work.*) rm -rf -- "$WORK" ;; esac
  case "$STAGE" in
    "$OUTPUT_PARENT"/.hd-microdroid-death.*)
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

CORRUPTED_PAYLOAD="$WORK/payload-invalid-v3.apk"
cp "$SOURCE_PAYLOAD" "$CORRUPTED_PAYLOAD"
# Locate the v3 pair ID (0xf05368c0, little-endian) without parsing untrusted ZIP offsets. The
# mutation is inside the v3 signed-data digest, leaving the ZIP and signing-block shape intact so
# Host preflight accepts it and the real Guest verifier owns the rejection.
V3_PATTERN=$(printf '\300\150\123\360')
V3_MATCHES=$(LC_ALL=C grep -abo "$V3_PATTERN" "$CORRUPTED_PAYLOAD" || true)
[ "$(printf '%s\n' "$V3_MATCHES" | grep -c .)" -eq 1 ] ||
  fail "source Payload did not contain exactly one v3 signing pair"
V3_ID_OFFSET=$(printf '%s\n' "$V3_MATCHES" | cut -d: -f1)
CORRUPT_OFFSET=$((V3_ID_OFFSET + 42))
ORIGINAL_BYTE=$(od -An -tx1 -N1 -j"$CORRUPT_OFFSET" "$CORRUPTED_PAYLOAD" | tr -d ' ')
case "$ORIGINAL_BYTE" in
  ff) REPLACEMENT='\000' ;;
  ??) REPLACEMENT='\377' ;;
  *) fail "could not read the v3 signed-data byte" ;;
esac
printf '%b' "$REPLACEMENT" | dd of="$CORRUPTED_PAYLOAD" bs=1 \
  seek="$CORRUPT_OFFSET" count=1 conv=notrunc 2>"$STAGE/mutation-dd.txt"
SOURCE_SHA256=$(shasum -a 256 "$SOURCE_PAYLOAD" | awk '{print $1}')
CORRUPTED_SHA256=$(shasum -a 256 "$CORRUPTED_PAYLOAD" | awk '{print $1}')
[ "$SOURCE_SHA256" != "$CORRUPTED_SHA256" ] || fail "Payload mutation did not change SHA-256"
printf 'source_sha256\tcorrupted_sha256\tv3_id_offset\tcorrupt_offset\n%s\t%s\t%s\t%s\n' \
  "$SOURCE_SHA256" "$CORRUPTED_SHA256" "$V3_ID_OFFSET" "$CORRUPT_OFFSET" \
  >"$STAGE/payload-mutation.tsv"
unzip -t "$CORRUPTED_PAYLOAD" >"$STAGE/corrupted-zip.txt"
set +e
"$APKSIGNER" verify --verbose "$CORRUPTED_PAYLOAD" \
  >"$STAGE/corrupted-signature.txt" 2>&1
VERIFY_STATUS=$?
set -e
[ "$VERIFY_STATUS" -ne 0 ] || fail "corrupted Payload still passed cryptographic verification"
require_contains 'APK Signature Scheme v3 signer #1' "$STAGE/corrupted-signature.txt" \
  "corrupted Payload failed for a reason other than its v3 signature"

# The upload must pass HD's bounded ZIP/signing-block shape preflight. A rejection here would only
# prove duplicate Host validation, not the AOSP Microdroid death-reason data path.
run_ctl upload --microdroid-payload "$CORRUPTED_PAYLOAD" >"$STAGE/upload.json"
UPLOAD_ID=$(jq -r .id "$STAGE/upload.json")
UPLOAD_SHA256=$(jq -r .sha256 "$STAGE/upload.json")
[ "$UPLOAD_SHA256" = "$CORRUPTED_SHA256" ] || fail "upload SHA-256 changed the fixture"

jq -n --arg id "$INSTANCE_ID" --arg upload_id "$UPLOAD_ID" \
  --arg sha256 "$UPLOAD_SHA256" \
  '{schema_version:2,id:$id,name:"Microdroid Invalid Signature",guest_kind:"microdroid",
    microdroid:{debug_level:"full",payload:{kind:"uploaded",upload_id:$upload_id,
      sha256:$sha256,config_path:"assets/vm_config.json"},encrypted_storage_mib:null},
    cpu_count:1,memory_mib:512,
    display:{width:1080,height:1920,dpi:420,refresh_rate_hz:60,
      orientation:"portrait",vsync:"on",show_host_fps:false},
    adb:{mode:"disabled",host_port:null,executable:null},artifacts:null,
    boot:{kernel_log_level:4,panic_timeout_seconds:5,boot_animation:true},
    devices:{bluetooth:false,nfc:false,uwb:false,modem:false,gnss:false,sensors:false,
      network:false,audio:false,camera:false,power:false},restart_policy:"never",
    labels:{purpose:"microdroid-death-reason"}}' >"$STAGE/spec.json"
run_ctl create --spec "$STAGE/spec.json" >"$STAGE/create.json"
set +e
run_ctl start "$INSTANCE_ID" >"$STAGE/start.stdout" 2>"$STAGE/start.stderr"
START_STATUS=$?
set -e
[ "$START_STATUS" -ne 0 ] || fail "invalid-signature Microdroid unexpectedly started"
require_contains 'code: "microdroid_payload_verification_failed"' "$STAGE/start.stderr" \
  "CLI did not return the typed Payload verification code"
require_contains 'APK signature or idsig' "$STAGE/start.stderr" \
  "CLI error did not provide an actionable Payload repair message"
run_ctl show "$INSTANCE_ID" >"$STAGE/blocked-instance.json"
jq -e '.status.observed == "blocked" and
  .status.error_code == "microdroid_payload_verification_failed" and
  (.status.reason | contains("APK signature or idsig")) and
  (.active_run_id | type == "string")' "$STAGE/blocked-instance.json" >/dev/null ||
  fail "instance did not preserve the typed blocked state"
RUN_ID=$(jq -r .active_run_id "$STAGE/blocked-instance.json")
RUN_DIR="$DATA/runs/$INSTANCE_ID/$RUN_ID"
[ -f "$RUN_DIR/result.json" ] || fail "failed run did not publish result.json"
jq -e '.final_state == "blocked" and
  .error_code == "microdroid_payload_verification_failed"' \
  "$RUN_DIR/result.json" >/dev/null || fail "run result lost the typed death reason"
require_contains 'VM ended: MicrodroidPayloadVerificationFailed' \
  "$RUN_DIR/microdroid.stdout.log" "vm client did not report the AOSP death reason"
require_contains 'reason=MicrodroidPayloadVerificationFailed' \
  "$RUN_DIR/microdroid-vmclient-trace.log" "vmclient trace lost the AOSP death reason"
require_contains 'PAYLOAD_VERIFICATION_FAILED' "$RUN_DIR/microdroid-virtmgr-trace.log" \
  "virtmgr trace lost the Guest verification error"
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
  fail "death-reason smoke left an isolated process running"
fi

VERSION=$(plutil -extract CFBundleShortVersionString raw -o - "$APP/Contents/Info.plist")
BUILD=$(plutil -extract CFBundleVersion raw -o - "$APP/Contents/Info.plist")
GENERATED_AT=$(date -u +%Y-%m-%dT%H:%M:%SZ)
jq -n --arg generated_at "$GENERATED_AT" --arg version "$VERSION" --arg build "$BUILD" \
  --arg instance_id "$INSTANCE_ID" --arg run_id "$RUN_ID" \
  --arg source_sha256 "$SOURCE_SHA256" --arg corrupted_sha256 "$CORRUPTED_SHA256" \
  '{schema_version:1,profile:"macos-arm64-microdroid-death-reason-v1",status:"pass",
    generated_at:$generated_at,version:$version,build:$build,instance_id:$instance_id,
    run_id:$run_id,fixture:{source_sha256:$source_sha256,
      corrupted_sha256:$corrupted_sha256,zip_structure:"valid",v3_signature:"invalid",
      host_preflight:"accepted"},guest_death_reason:"MicrodroidPayloadVerificationFailed",
    api_error_code:"microdroid_payload_verification_failed",observed_state:"blocked",
    process_cleanup:"pass"}' >"$STAGE/result.json"
jq -n --arg generated_at "$GENERATED_AT" --arg log_path "$OUTPUT/result.json" \
  '{schema_version:2,generated_at:$generated_at,
    source:"scripts/macos-microdroid-death-reason-smoke.sh",
    gates:[{name:"macos-microdroid-death-reason",
      command:"macos-microdroid-death-reason-smoke.sh --app <HD.app> --output <fresh-dir> --apksigner <pinned> --java-home <pinned> --development-package",
      status:"pass",duration_ms:null,log_path:$log_path,
      summary:"A structurally valid APK with a deliberately corrupted v3 signed-data digest passed HD bounded preflight, was rejected by the real Microdroid verifier as MicrodroidPayloadVerificationFailed, and remained Blocked with the actionable microdroid_payload_verification_failed API code; cleanup left no test process."}]}' \
  >"$STAGE/death-reason-gate.json"
chmod 600 "$STAGE/result.json" "$STAGE/death-reason-gate.json"

case "$DATA" in /private/tmp/hd-microdroid-death-data.*) rm -rf -- "$DATA" ;; esac
case "$WORK" in /private/tmp/hd-microdroid-death-work.*) rm -rf -- "$WORK" ;; esac
mv -- "$STAGE" "$OUTPUT"
COMPLETED=1
trap - EXIT HUP INT TERM
echo "evidence=$OUTPUT"
echo "gate_report=$OUTPUT/death-reason-gate.json"
