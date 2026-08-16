#!/bin/sh
set -eu
umask 077

usage() {
  cat <<'EOF'
Usage: scripts/macos-microdroid-finite-payload-smoke.sh \
  --app <HD.app> \
  --output <fresh-absolute-evidence-directory> \
  --exit-0-payload <v3-signed-arm64-payload.apk> \
  --exit-17-payload <v3-signed-arm64-payload.apk> \
  --apksigner <pinned-Android-build-tools-apksigner> \
  --zipalign <pinned-Android-build-tools-zipalign> \
  [--timeout-seconds <seconds>] \
  [--development-package]

Runs two isolated Microdroid instances and verifies natural Payload completion:
exit 0 must converge to clean Stopped; exit 17 must preserve the typed
microdroid_payload_failed state. The runner owns a fresh data root and never
terminates processes outside that root.
EOF
}

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

APP=
OUTPUT=
EXIT_0_PAYLOAD=
EXIT_17_PAYLOAD=
APKSIGNER=
ZIPALIGN=
TIMEOUT_SECONDS=60
DEVELOPMENT_PACKAGE=0

while [ "$#" -gt 0 ]; do
  case "$1" in
    --app) APP=$2; shift 2 ;;
    --output) OUTPUT=$2; shift 2 ;;
    --exit-0-payload) EXIT_0_PAYLOAD=$2; shift 2 ;;
    --exit-17-payload) EXIT_17_PAYLOAD=$2; shift 2 ;;
    --apksigner) APKSIGNER=$2; shift 2 ;;
    --zipalign) ZIPALIGN=$2; shift 2 ;;
    --timeout-seconds) TIMEOUT_SECONDS=$2; shift 2 ;;
    --development-package) DEVELOPMENT_PACKAGE=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

for command in codesign jq plutil shasum unzip uuidgen awk grep ps date find; do
  command -v "$command" >/dev/null 2>&1 || fail "required tool is missing: $command"
done
[ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ] ||
  fail "Microdroid finite Payload Guest smoke requires Apple Silicon macOS"
require_abs_dir "$APP" --app
require_abs_file "$EXIT_0_PAYLOAD" --exit-0-payload
require_abs_file "$EXIT_17_PAYLOAD" --exit-17-payload
require_abs_file "$APKSIGNER" --apksigner
require_abs_file "$ZIPALIGN" --zipalign
case "$OUTPUT" in /*) ;; *) fail "--output must be absolute" ;; esac
case "$TIMEOUT_SECONDS" in ''|*[!0-9]*) fail "--timeout-seconds must be an integer" ;; esac
[ "$TIMEOUT_SECONDS" -ge 10 ] && [ "$TIMEOUT_SECONDS" -le 300 ] ||
  fail "--timeout-seconds must be between 10 and 300"
[ ! -e "$OUTPUT" ] || fail "refusing to replace evidence output: $OUTPUT"
FAILURE_OUTPUT="$OUTPUT.failed"
[ ! -e "$FAILURE_OUTPUT" ] || fail "refusing to replace failure evidence: $FAILURE_OUTPUT"

CTL="$APP/Contents/MacOS/hdctl"
PACKAGE_PAYLOAD_MANIFEST="$APP/Contents/Resources/products/microdroid/conformance-payload/payload-bundle-v1.plist"
require_abs_file "$CTL" hdctl
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
OUTPUT_STAGE=$(mktemp -d "$output_parent/.hd-microdroid-finite-evidence.XXXXXX")
DATA_ROOT=$(mktemp -d /private/tmp/hd-microdroid-finite-data.XXXXXX)
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

own_vm_process_ids() {
  HD_PROCESS_ROOT="$DATA_ROOT" \
    awk 'index($0, ENVIRON["HD_PROCESS_ROOT"]) &&
         $2 ~ /(^|\/)(crosvm|vm|virtmgr)$/ {
           print $1
         }' <<EOF
$(ps -axo pid=,comm=,command=)
EOF
}

capture_runtime_evidence() {
  [ -d "$DATA_ROOT/logs" ] && cp -Rp "$DATA_ROOT/logs" "$OUTPUT_STAGE/host-logs" 2>/dev/null || true
  [ -d "$DATA_ROOT/runs" ] && cp -Rp "$DATA_ROOT/runs" "$OUTPUT_STAGE/runtime-runs" 2>/dev/null || true
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
  capture_runtime_evidence
  pids=$(own_process_ids 2>/dev/null || true)
  [ -z "$pids" ] || kill $pids 2>/dev/null || true
  sleep 1
  pids=$(own_process_ids 2>/dev/null || true)
  [ -z "$pids" ] || kill -KILL $pids 2>/dev/null || true
  case "$DATA_ROOT" in /private/tmp/hd-microdroid-finite-data.*) rm -rf -- "$DATA_ROOT" ;; esac
  case "$OUTPUT_STAGE" in
    "$output_parent"/.hd-microdroid-finite-evidence.*)
      if [ "$COMPLETED" -eq 0 ] && [ -d "$OUTPUT_STAGE" ]; then
        printf '%s\n' "$exit_code" >"$OUTPUT_STAGE/exit.code"
        mv -- "$OUTPUT_STAGE" "$FAILURE_OUTPUT"
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

verify_payload() {
  payload=$1
  expected_library=$2
  prefix=$3
  "$ZIPALIGN" -c -v 4 "$payload" >"$OUTPUT_STAGE/$prefix-zipalign.txt" 2>&1 ||
    fail "$prefix Payload is not zipaligned"
  "$APKSIGNER" verify --verbose --print-certs "$payload" \
    >"$OUTPUT_STAGE/$prefix-signature.txt" 2>&1 || fail "$prefix Payload signature is invalid"
  grep -Fq 'Verified using v3 scheme (APK Signature Scheme v3): true' \
    "$OUTPUT_STAGE/$prefix-signature.txt" || fail "$prefix Payload is not v3 signed"
  unzip -p "$payload" assets/vm_config.json >"$OUTPUT_STAGE/$prefix-vm-config.json" ||
    fail "$prefix Payload omits assets/vm_config.json"
  jq -e --arg library "$expected_library" \
    '.task.type == "microdroid_launcher" and .task.command == $library' \
    "$OUTPUT_STAGE/$prefix-vm-config.json" >/dev/null ||
    fail "$prefix Payload config does not select $expected_library"
  unzip -l "$payload" | grep -Fq "lib/arm64-v8a/$expected_library" ||
    fail "$prefix Payload omits its arm64 library"
  PAYLOAD_SHA=$(shasum -a 256 "$payload" | awk '{print $1}')
}

upload_payload() {
  payload=$1
  prefix=$2
  expected_sha=$3
  run_ctl "$OUTPUT_STAGE/$prefix-upload.json" "$OUTPUT_STAGE/$prefix-upload.stderr" \
    upload --microdroid-payload "$payload"
  UPLOAD_ID=$(jq -er '.id' "$OUTPUT_STAGE/$prefix-upload.json")
  UPLOAD_SHA=$(jq -er '.sha256 | select(test("^[0-9a-f]{64}$"))' \
    "$OUTPUT_STAGE/$prefix-upload.json")
  [ "$UPLOAD_SHA" = "$expected_sha" ] || fail "$prefix upload SHA-256 changed the Payload"
}

write_spec() {
  instance_id=$1
  upload_id=$2
  upload_sha=$3
  prefix=$4
  jq -n --arg id "$instance_id" --arg upload_id "$upload_id" --arg sha256 "$upload_sha" \
    --arg name "Microdroid Finite Payload $prefix" \
    '{schema_version:2,id:$id,name:$name,guest_kind:"microdroid",
      microdroid:{debug_level:"full",payload:{kind:"uploaded",upload_id:$upload_id,
        sha256:$sha256,config_path:"assets/vm_config.json"},encrypted_storage_mib:null},
      cpu_count:1,memory_mib:512,
      display:{width:1080,height:1920,dpi:420,refresh_rate_hz:60,
        orientation:"portrait",vsync:"on",show_host_fps:false,secondary_displays:[]},
      adb:{mode:"disabled",host_port:null,executable:null},artifacts:null,
      boot:{kernel_log_level:4,panic_timeout_seconds:5,boot_animation:true},
      devices:{bluetooth:false,nfc:false,uwb:false,modem:false,gnss:false,sensors:false,
        network:false,audio:false,camera:false,power:false,touchpad:false},
      host_audio_input:"disabled",restart_policy:"never",
      labels:{gate:"macos-microdroid-finite-payload"}}' >"$OUTPUT_STAGE/$prefix-spec.json"
}

wait_for_terminal() {
  instance_id=$1
  expected_state=$2
  prefix=$3
  deadline=$(( $(date +%s) + TIMEOUT_SECONDS ))
  attempt=0
  while [ "$(date +%s)" -le "$deadline" ]; do
    run_ctl "$OUTPUT_STAGE/$prefix-show-$attempt.json" \
      "$OUTPUT_STAGE/$prefix-show-$attempt.stderr" show "$instance_id"
    observed=$(jq -r '.status.observed' "$OUTPUT_STAGE/$prefix-show-$attempt.json")
    if [ "$observed" = "$expected_state" ]; then
      cp "$OUTPUT_STAGE/$prefix-show-$attempt.json" "$OUTPUT_STAGE/$prefix-terminal.json"
      return
    fi
    case "$observed" in
      blocked|failed|deleted)
        fail "$prefix entered unexpected terminal state $observed"
        ;;
    esac
    sleep 1
    attempt=$((attempt + 1))
  done
  fail "$prefix did not reach $expected_state within $TIMEOUT_SECONDS seconds"
}

verify_run_evidence() {
  instance_id=$1
  expected_exit=$2
  expected_state=$3
  expected_error=$4
  expected_upload_id=$5
  prefix=$6
  run_parent="$DATA_ROOT/runs/$instance_id"
  [ -d "$run_parent" ] || fail "$prefix did not create a run directory"
  run_count=$(find "$run_parent" -mindepth 1 -maxdepth 1 -type d | wc -l | tr -d ' ')
  [ "$run_count" -eq 1 ] || fail "$prefix produced $run_count run directories instead of one"
  run_dir=$(find "$run_parent" -mindepth 1 -maxdepth 1 -type d -print)
  run_id=$(basename -- "$run_dir")
  for name in manifest.json microdroid.stdout.log microdroid.stderr.log \
    microdroid-console.txt microdroid-guest.log result.json; do
    require_abs_file "$run_dir/$name" "$prefix $name"
  done
  finish_count=$(grep -c '^payload finished with exit code ' "$run_dir/microdroid.stderr.log" || true)
  [ "$finish_count" -eq 1 ] &&
    grep -Fxq "payload finished with exit code $expected_exit" "$run_dir/microdroid.stderr.log" ||
    fail "$prefix launcher did not report exactly one exit code $expected_exit"
  shutdown_count=$(grep -c '^VM ended: Shutdown$' "$run_dir/microdroid.stdout.log" || true)
  [ "$shutdown_count" -eq 1 ] || fail "$prefix launcher did not report exactly one Shutdown"
  grep -Fq 'notifying payload finished' "$run_dir/microdroid-guest.log" ||
    fail "$prefix Guest omitted its payload-finished notification"
  jq -e --arg upload_id "$expected_upload_id" '
    (.launch.arguments | index("run-app")) != null and
    (.launch.arguments | index("--config-path")) != null and
    (.launch.arguments | index("assets/vm_config.json")) != null and
    (.launch.arguments | index("--debug")) != null and
    (.launch.arguments | index("full")) != null and
    any(.launch.arguments[]; contains($upload_id))' "$run_dir/manifest.json" >/dev/null ||
    fail "$prefix manifest is not bound to the managed Full-debug Payload"
  if [ "$expected_error" = null ]; then
    jq -e --arg instance_id "$instance_id" --arg run_id "$run_id" --argjson exit "$expected_exit" \
      '.instance_id == $instance_id and .run_id == $run_id and .final_state == "stopped" and
       .exit_code == $exit and .error_code == null' "$run_dir/result.json" >/dev/null ||
      fail "$prefix result did not preserve clean exit $expected_exit"
  else
    jq -e --arg instance_id "$instance_id" --arg run_id "$run_id" --argjson exit "$expected_exit" \
      --arg error "$expected_error" \
      '.instance_id == $instance_id and .run_id == $run_id and .final_state == "failed" and
       .exit_code == $exit and .error_code == $error' "$run_dir/result.json" >/dev/null ||
      fail "$prefix result did not preserve typed exit $expected_exit"
  fi
  cp -Rp "$run_dir" "$OUTPUT_STAGE/$prefix-run"
  RUN_ID=$run_id
}

run_case() {
  payload=$1
  expected_exit=$2
  expected_state=$3
  expected_error=$4
  library=$5
  prefix=$6
  verify_payload "$payload" "$library" "$prefix"
  payload_sha=$PAYLOAD_SHA
  upload_payload "$payload" "$prefix" "$payload_sha"
  upload_id=$UPLOAD_ID
  upload_sha=$UPLOAD_SHA
  instance_id=$(uuidgen | tr '[:upper:]' '[:lower:]')
  write_spec "$instance_id" "$upload_id" "$upload_sha" "$prefix"
  run_ctl "$OUTPUT_STAGE/$prefix-create.json" "$OUTPUT_STAGE/$prefix-create.stderr" \
    create --spec "$OUTPUT_STAGE/$prefix-spec.json"
  jq -e --arg id "$instance_id" '.spec.id == $id and .spec.guest_kind == "microdroid"' \
    "$OUTPUT_STAGE/$prefix-create.json" >/dev/null || fail "$prefix created the wrong instance"
  started_at=$(date +%s)
  run_ctl "$OUTPUT_STAGE/$prefix-start.json" "$OUTPUT_STAGE/$prefix-start.stderr" \
    start "$instance_id" --no-wait
  wait_for_terminal "$instance_id" "$expected_state" "$prefix"
  elapsed_seconds=$(( $(date +%s) - started_at ))
  if [ "$expected_exit" -eq 0 ]; then
    jq -e '.status.desired == "stopped" and .status.observed == "stopped" and
      .status.error_code == null and .active_run_id == null' \
      "$OUTPUT_STAGE/$prefix-terminal.json" >/dev/null ||
      fail "$prefix did not converge to clean Stopped"
  else
    jq -e --arg exit "$expected_exit" '.status.observed == "failed" and
      .status.error_code == "microdroid_payload_failed" and
      (.status.reason | type == "string" and contains($exit)) and .active_run_id == null' \
      "$OUTPUT_STAGE/$prefix-terminal.json" >/dev/null ||
      fail "$prefix did not preserve microdroid_payload_failed and exit $expected_exit"
  fi
  verify_run_evidence "$instance_id" "$expected_exit" "$expected_state" "$expected_error" \
    "$upload_id" "$prefix"
  remaining=$(own_vm_process_ids | wc -l | tr -d ' ')
  [ "$remaining" -eq 0 ] || fail "$prefix retained $remaining owned VM processes"
  jq -n --arg instance_id "$instance_id" --arg run_id "$RUN_ID" --arg payload_sha "$payload_sha" \
    --arg upload_id "$upload_id" --arg final_state "$expected_state" \
    --arg expected_error "$expected_error" --argjson exit "$expected_exit" \
    --argjson elapsed "$elapsed_seconds" \
    '{instance_id:$instance_id,run_id:$run_id,payload_sha256:$payload_sha,upload_id:$upload_id,
      expected_payload_exit_code:$exit,observed_payload_exit_code:$exit,
      final_state:$final_state,error_code:(if $expected_error == "null" then null else $expected_error end),
      completion_elapsed_seconds:$elapsed,natural_shutdown:true,remaining_vm_process_count:0}' \
    >"$OUTPUT_STAGE/$prefix-result.json"
}

mkdir -p "$OUTPUT_STAGE"
run_ctl "$OUTPUT_STAGE/health.json" "$OUTPUT_STAGE/health.stderr" health
run_case "$EXIT_0_PAYLOAD" 0 stopped null HdFinitePayloadExit0.so exit-0
run_case "$EXIT_17_PAYLOAD" 17 failed microdroid_payload_failed HdFinitePayloadExit17.so exit-17

remaining=$(own_vm_process_ids | wc -l | tr -d ' ')
[ "$remaining" -eq 0 ] || fail "finite Payload gate retained $remaining owned runtime processes"
jq -n --slurpfile exit0 "$OUTPUT_STAGE/exit-0-result.json" \
  --slurpfile exit17 "$OUTPUT_STAGE/exit-17-result.json" \
  --arg channel "$CHANNEL" \
  '{schema_version:1,gate:"macos-microdroid-finite-payload",platform:"macos-arm64",
    package_channel:$channel,cases:[$exit0[0],$exit17[0]],passed:true}' \
  >"$OUTPUT_STAGE/result.json"
COMPLETED=1
mv -- "$OUTPUT_STAGE" "$OUTPUT"
echo "evidence=$OUTPUT"
