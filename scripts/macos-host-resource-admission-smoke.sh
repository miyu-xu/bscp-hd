#!/bin/sh
set -eu

usage() {
  cat <<'EOF'
Usage: scripts/macos-host-resource-admission-smoke.sh \
  --app <HD.app> \
  --output <fresh-absolute-evidence-directory> \
  [--development-package]
EOF
}

fail() {
  echo "$*" >&2
  exit 1
}

APP=
OUTPUT=
DEVELOPMENT_PACKAGE=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    --app) [ "$#" -ge 2 ] || { usage >&2; exit 2; }; APP=$2; shift 2 ;;
    --output) [ "$#" -ge 2 ] || { usage >&2; exit 2; }; OUTPUT=$2; shift 2 ;;
    --development-package) DEVELOPMENT_PACKAGE=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

case "$APP" in /*) ;; *) fail "--app must be absolute" ;; esac
[ -d "$APP" ] && [ ! -L "$APP" ] || fail "--app must be a real non-symlink directory"
case "$OUTPUT" in /*) ;; *) fail "--output must be absolute" ;; esac
[ ! -e "$OUTPUT" ] || fail "refusing to replace evidence output: $OUTPUT"
FAILURE_OUTPUT="$OUTPUT.failed"
[ ! -e "$FAILURE_OUTPUT" ] || fail "refusing to replace failure evidence: $FAILURE_OUTPUT"
[ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ] ||
  fail "resource admission smoke requires Apple Silicon macOS"

UI="$APP/Contents/MacOS/HD"
CTL="$APP/Contents/MacOS/hdctl"
SIGNED_STORE="$APP/Contents/Resources/products/android/artifact-store-v2"
DIRECT_MARKER="$APP/Contents/Resources/products/android/development-direct-v1.plist"
STORE=
TRUST=
INDEX=
ARTIFACT_DISTRIBUTION=
for binary in "$UI" "$CTL"; do
  [ -f "$binary" ] && [ ! -L "$binary" ] && [ -x "$binary" ] ||
    fail "HD.app is missing a real executable: $binary"
done
if [ -d "$SIGNED_STORE" ] && [ ! -L "$SIGNED_STORE" ]; then
  [ ! -e "$DIRECT_MARKER" ] ||
    fail "HD.app must not mix signed and direct-development Android distributions"
  STORE=$SIGNED_STORE
  TRUST="$STORE/trusted-keys-v2.json"
  INDEX="$STORE/index-v2.json"
  for file in "$TRUST" "$INDEX"; do
    [ -f "$file" ] && [ ! -L "$file" ] ||
      fail "HD.app is missing signed Android metadata: $file"
  done
  CHANNEL=$(jq -r .channel "$INDEX")
  DATA_PROFILE=$(jq -r .data_profile "$INDEX")
  ARTIFACT_DISTRIBUTION=signed-artifact-store-v2
else
  [ -f "$DIRECT_MARKER" ] && [ ! -L "$DIRECT_MARKER" ] ||
    fail "HD.app has neither a signed nor direct-development Android distribution"
  CHANNEL=$(plutil -extract channel raw -o - "$DIRECT_MARKER")
  DATA_PROFILE=$(plutil -extract data_profile raw -o - "$DIRECT_MARKER")
  ARTIFACT_DISTRIBUTION=direct-development-v1
fi
codesign --verify --deep --strict "$APP" || fail "HD.app codesign verification failed"
case "$CHANNEL:$DEVELOPMENT_PACKAGE" in
  development:1) ;;
  development:0) fail "development app requires --development-package" ;;
  release:0) ;;
  release:1) fail "release app rejects --development-package" ;;
  *) fail "unsupported Android artifact channel: $CHANNEL" ;;
esac

OUTPUT_PARENT=$(dirname -- "$OUTPUT")
mkdir -p -- "$OUTPUT_PARENT"
STAGE=$(mktemp -d "$OUTPUT_PARENT/.hd-resource-admission.XXXXXX")
WORK=$(mktemp -d /private/tmp/hd-resource-admission.XXXXXX)
MOUNT="$WORK/mount"
DATA="$MOUNT/data"
mkdir -p -- "$MOUNT"
ATTACHED=0
UI_PID=
COMPLETED=0

terminate_test_processes() {
  [ -z "$UI_PID" ] || kill "$UI_PID" 2>/dev/null || true
  pids=$(pgrep -f "$DATA" 2>/dev/null || true)
  [ -z "$pids" ] || kill $pids 2>/dev/null || true
  sleep 1
  pids=$(pgrep -f "$DATA" 2>/dev/null || true)
  [ -z "$pids" ] || kill -KILL $pids 2>/dev/null || true
}

cleanup() {
  status=$?
  if [ -x "$CTL" ] && [ "$ATTACHED" -eq 1 ]; then
    "$CTL" --data-root "$DATA" --no-start-host shutdown --stop-all >/dev/null 2>&1 || true
  fi
  terminate_test_processes
  if [ "$ATTACHED" -eq 1 ]; then
    hdiutil detach -quiet "$MOUNT" >/dev/null 2>&1 || hdiutil detach -force -quiet "$MOUNT" >/dev/null 2>&1 || true
  fi
  case "$WORK" in /private/tmp/hd-resource-admission.*) rm -rf -- "$WORK" ;; esac
  case "$STAGE" in
    "$OUTPUT_PARENT"/.hd-resource-admission.*)
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

hdiutil create -quiet -size 2g -fs APFS -type SPARSEBUNDLE \
  -volname HDResourceAdmission "$WORK/volume"
hdiutil attach -quiet -nobrowse -owners on -mountpoint "$MOUNT" "$WORK/volume.sparsebundle"
ATTACHED=1
mkdir -p -- "$DATA"
AVAILABLE_BEFORE=$(df -k "$DATA" | awk 'NR==2 {print $4 * 1024}')
[ "$AVAILABLE_BEFORE" -gt 1073741824 ] && [ "$AVAILABLE_BEFORE" -lt 10737418240 ] ||
  fail "isolated APFS volume did not provide the required 1-10 GiB admission window"

GUEST_DIGEST=
HOST_DIGEST=
if [ "$ARTIFACT_DISTRIBUTION" = signed-artifact-store-v2 ]; then
  jq '{schema_version,channel,data_profile,guest_bundle_digest,host_bundle_digest}' \
    "$INDEX" >"$STAGE/store-selection.json"
  GUEST_DIGEST=$(jq -r .guest_bundle_digest "$INDEX")
  HOST_DIGEST=$(jq -r .host_bundle_digest "$INDEX")
fi

mkdir -p -- "$STAGE/logs"
"$UI" --data-root "$DATA" >"$STAGE/logs/ui.stdout" 2>"$STAGE/logs/ui.stderr" &
UI_PID=$!
printf '%s\n' "$UI_PID" >"$STAGE/ui.pid"
attempt=0
while ! "$CTL" --data-root "$DATA" --no-start-host health \
  >"$STAGE/health.json" 2>"$STAGE/health.stderr"; do
  kill -0 "$UI_PID" 2>/dev/null || fail "installed UI exited before Host became healthy"
  [ "$attempt" -lt 300 ] || fail "installed Host did not become healthy within 30 seconds"
  attempt=$((attempt + 1))
  sleep 0.1
done

if [ "$ARTIFACT_DISTRIBUTION" = direct-development-v1 ]; then
  STORE="$DATA/cache/direct-dev-artifacts-v2"
  MANIFEST_COUNT=0
  for manifest in "$STORE"/bundles/*/manifest-v2.json; do
    [ -f "$manifest" ] && [ ! -L "$manifest" ] || continue
    MANIFEST_COUNT=$((MANIFEST_COUNT + 1))
    kind=$(jq -r '.kind // empty' "$manifest")
    digest=$(jq -r '.digest // empty' "$manifest")
    [ "${#digest}" -eq 64 ] || fail "direct-development manifest has an invalid digest"
    case "$kind" in
      guest)
        [ -z "$GUEST_DIGEST" ] || fail "direct-development store has multiple Guest bundles"
        GUEST_DIGEST=$digest
        ;;
      host_tools)
        [ -z "$HOST_DIGEST" ] || fail "direct-development store has multiple Host bundles"
        HOST_DIGEST=$digest
        ;;
      *) fail "direct-development store contains unexpected bundle kind: $kind" ;;
    esac
  done
  [ "$MANIFEST_COUNT" -eq 2 ] ||
    fail "direct-development store must contain exactly two bundle manifests"
  jq -n --arg channel "$CHANNEL" --arg data_profile "$DATA_PROFILE" \
    --arg distribution "$ARTIFACT_DISTRIBUTION" --arg guest "$GUEST_DIGEST" \
    --arg host "$HOST_DIGEST" \
    '{schema_version:2,channel:$channel,data_profile:$data_profile,
      artifact_distribution:$distribution,guest_bundle_digest:$guest,
      host_bundle_digest:$host}' >"$STAGE/store-selection.json"
fi
[ "${#GUEST_DIGEST}" -eq 64 ] && [ "${#HOST_DIGEST}" -eq 64 ] ||
  fail "packaged Android distribution did not publish both bundle digests"

INSTANCE_ID=$(uuidgen | tr '[:upper:]' '[:lower:]')
jq -n \
  --arg id "$INSTANCE_ID" \
  --arg store "$STORE" \
  --arg guest "$GUEST_DIGEST" \
  --arg host "$HOST_DIGEST" \
  --arg data_profile "$DATA_PROFILE" \
  '{schema_version:2, id:$id, name:"Android Resource Admission", guest_kind:"android",
    microdroid:null, cpu_count:4, memory_mib:4096,
    display:{width:1080,height:1920,dpi:420,refresh_rate_hz:60,
      orientation:"portrait",vsync:"on",show_host_fps:false},
    adb:{mode:"loopback",host_port:null,executable:null},
    artifacts:{store_root:$store,guest_bundle_digest:$guest,host_bundle_digest:$host},
    boot:{kernel_log_level:4,panic_timeout_seconds:5,boot_animation:true},
    devices:{bluetooth:true,nfc:true,uwb:true,modem:true,gnss:true,sensors:true,
      network:true,audio:true,camera:true,power:true},
    restart_policy:"never",
    labels:{purpose:"resource-admission",data_profile:$data_profile}}' \
  >"$STAGE/spec.json"
"$CTL" --data-root "$DATA" --no-start-host create --spec "$STAGE/spec.json" \
  >"$STAGE/create.json"
RESOURCE_STARTED=$(perl -MTime::HiRes=time -e 'printf "%.6f", time')
"$CTL" --data-root "$DATA" --no-start-host resource-admission "$INSTANCE_ID" \
  >"$STAGE/resource-admission.json"
RESOURCE_FINISHED=$(perl -MTime::HiRes=time -e 'printf "%.6f", time')
RESOURCE_MILLISECONDS=$(awk -v start="$RESOURCE_STARTED" -v finish="$RESOURCE_FINISHED" \
  'BEGIN { printf "%.0f", (finish - start) * 1000 }')
[ "$RESOURCE_MILLISECONDS" -le 2000 ] ||
  fail "lightweight resource admission exceeded the two second product budget"
jq -e '.id == "host.resources" and .status == "blocked" and
  .properties.disk_requirement_mode == "new_instance_storage" and
  (.properties.required_disk_bytes | tonumber) == 10737418240 and
  (.properties.available_disk_bytes | tonumber) < (.properties.required_disk_bytes | tonumber) and
  (has("probes") | not)' "$STAGE/resource-admission.json" >/dev/null ||
  fail "resource-only endpoint did not return an isolated host.resources probe"
RESOURCE_EVENT_PATTERN='"event":"resource.admission.probed"'
attempt=0
while :; do
  RESOURCE_PROBE_EVENTS=$(grep -h -F "$RESOURCE_EVENT_PATTERN" \
    "$DATA"/logs/host-v2.jsonl* 2>/dev/null | grep -F '"scope":"resource_only"' | wc -l | tr -d ' ')
  [ "$RESOURCE_PROBE_EVENTS" -ge 2 ] && break
  kill -0 "$UI_PID" 2>/dev/null || fail "installed UI exited before resource fast-path observation"
  [ "$attempt" -lt 50 ] ||
    fail "UI did not use the resource-only endpoint after instance creation"
  attempt=$((attempt + 1))
  sleep 0.1
done
CAPABILITIES_STARTED=$(date +%s)
"$CTL" --data-root "$DATA" --no-start-host capabilities "$INSTANCE_ID" \
  >"$STAGE/new-storage-capabilities.json"
CAPABILITIES_FINISHED=$(date +%s)
CAPABILITIES_SECONDS=$((CAPABILITIES_FINISHED - CAPABILITIES_STARTED))
[ "$CAPABILITIES_SECONDS" -le 10 ] ||
  fail "blocked resource capabilities exceeded the 10 second product budget"
jq -e '.probes[] | select(.id == "host.resources") |
  .status == "blocked" and
  .properties.disk_requirement_mode == "new_instance_storage" and
  (.properties.required_disk_bytes | tonumber) == 10737418240 and
  (.properties.available_disk_bytes | tonumber) < (.properties.required_disk_bytes | tonumber)' \
  "$STAGE/new-storage-capabilities.json" >/dev/null ||
  fail "new Android storage was not blocked by the 10 GiB admission requirement"
jq -e '.probes[] | select(.id == "artifact.bundles") |
  .status == "blocked" and .properties.deferred_by == "host.resources"' \
  "$STAGE/new-storage-capabilities.json" >/dev/null ||
  fail "expensive artifact validation was not deferred behind resource admission"

START_STARTED=$(date +%s)
if "$CTL" --data-root "$DATA" --no-start-host start "$INSTANCE_ID" \
    >"$STAGE/blocked-start.json" 2>"$STAGE/blocked-start.stderr"; then
  fail "low disk start unexpectedly succeeded"
fi
START_FINISHED=$(date +%s)
START_SECONDS=$((START_FINISHED - START_STARTED))
[ "$START_SECONDS" -le 5 ] || fail "blocked start exceeded the five second product budget"
grep -Fq 'code: "capability_blocked"' "$STAGE/blocked-start.stderr" ||
  fail "blocked start did not return capability_blocked"
grep -Fq 'host.resources:' "$STAGE/blocked-start.stderr" ||
  fail "blocked start did not identify the host.resources blocker"
grep -Fq 'required_disk_bytes=10737418240' "$STAGE/blocked-start.stderr" ||
  fail "blocked start did not report the actionable disk requirement"
grep -Fq 'disk_requirement_mode=new_instance_storage' "$STAGE/blocked-start.stderr" ||
  fail "blocked start did not report the storage admission mode"
grep -Fq 'remaining artifact, device and release checks are deferred' \
  "$STAGE/blocked-start.stderr" ||
  fail "blocked start did not explain that downstream checks were deferred"
if grep -Fq 'artifact.bundles:' "$STAGE/blocked-start.stderr"; then
  fail "blocked start exposed a derived artifact blocker instead of the resource root cause"
fi
"$CTL" --data-root "$DATA" --no-start-host show "$INSTANCE_ID" \
  >"$STAGE/blocked-instance.json"
jq -e '.status.observed == "blocked"' "$STAGE/blocked-instance.json" >/dev/null ||
  fail "low disk start did not leave the instance Blocked"
if pgrep -f "hd-worker.*$DATA|crosvm.*$DATA|$DATA.*crosvm" \
    >"$STAGE/blocked-process-leaks.txt" 2>/dev/null; then
  fail "low disk start launched a Worker or crosvm before admission"
fi
[ ! -e "$DATA/disks/$INSTANCE_ID.img" ] || fail "blocked new storage start allocated an Android disk"

"$CTL" --data-root "$DATA" --no-start-host delete "$INSTANCE_ID" >"$STAGE/delete.json"
"$CTL" --data-root "$DATA" --no-start-host shutdown --stop-all \
  >"$STAGE/shutdown.json" 2>"$STAGE/shutdown.stderr"
kill "$UI_PID" 2>/dev/null || true
UI_PID=
sleep 1
if pgrep -f "$DATA" >"$STAGE/final-process-leaks.txt" 2>/dev/null; then
  fail "resource admission smoke left an isolated process running"
fi
for log in "$DATA"/logs/host-v2.jsonl*; do
  [ -f "$log" ] || continue
  cp -- "$log" "$STAGE/logs/"
done

AVAILABLE_NEW=$(jq -r '.probes[] | select(.id == "host.resources") | .properties.available_disk_bytes' \
  "$STAGE/new-storage-capabilities.json")
VERSION=$(plutil -extract CFBundleShortVersionString raw -o - "$APP/Contents/Info.plist")
BUILD=$(plutil -extract CFBundleVersion raw -o - "$APP/Contents/Info.plist")
GENERATED_AT=$(date -u +%Y-%m-%dT%H:%M:%SZ)
jq -n \
  --arg version "$VERSION" --arg build "$BUILD" --arg channel "$CHANNEL" \
  --arg data_profile "$DATA_PROFILE" --arg artifact_distribution "$ARTIFACT_DISTRIBUTION" \
  --argjson available_new "$AVAILABLE_NEW" \
  --argjson resource_milliseconds "$RESOURCE_MILLISECONDS" \
  --argjson resource_probe_events "$RESOURCE_PROBE_EVENTS" \
  --argjson capabilities_seconds "$CAPABILITIES_SECONDS" \
  --argjson start_seconds "$START_SECONDS" \
  '{schema_version:1,profile:"macos-arm64-resource-admission-v1",status:"pass",
    version:$version,build:$build,channel:$channel,data_profile:$data_profile,
    artifact_distribution:$artifact_distribution,
    isolated_volume_bytes:2147483648,
    latency:{resource_admission_milliseconds:$resource_milliseconds,
      resource_admission_budget_milliseconds:2000,
      capabilities_seconds:$capabilities_seconds,
      start_rejection_seconds:$start_seconds,
      capabilities_budget_seconds:10,start_budget_seconds:5},
    new_instance:{mode:"new_instance_storage",available_bytes:$available_new,
      required_bytes:10737418240,capability:"blocked",start:"blocked",
      worker_started:false,crosvm_started:false,disk_allocated:false},
    resource_fast_path:{scope:"resource_only",ui_observed:true,
      probe_events:$resource_probe_events,artifact_or_device_discovery:false},
    process_cleanup:"pass"}' >"$STAGE/result.json"
jq -n --arg generated_at "$GENERATED_AT" --arg log_path "$OUTPUT/result.json" \
  '{schema_version:2,generated_at:$generated_at,
    source:"scripts/macos-host-resource-admission-smoke.sh",
    gates:[{name:"macos-resource-admission",
      command:"macos-host-resource-admission-smoke.sh --app <HD.app> --output <fresh-dir>",
      status:"pass",duration_ms:null,log_path:$log_path,
      summary:"On an isolated 2 GiB APFS volume, the resource-only endpoint returned the isolated host.resources probe within two seconds without artifact/device discovery; a new Android instance was rejected within bounded full-capability/start budgets before disk allocation, Worker or crosvm launch by the 10 GiB requirement. The typed error identified the resource root cause and folded deferred downstream checks; shutdown left no test process."}]}' \
  >"$STAGE/resource-admission-gate.json"

hdiutil detach -quiet "$MOUNT"
ATTACHED=0
case "$WORK" in /private/tmp/hd-resource-admission.*) rm -rf -- "$WORK" ;; esac
mv -- "$STAGE" "$OUTPUT"
COMPLETED=1
trap - EXIT HUP INT TERM
echo "evidence=$OUTPUT"
echo "gate_report=$OUTPUT/resource-admission-gate.json"
