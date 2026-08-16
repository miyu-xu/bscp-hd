#!/bin/sh
set -eu

usage() {
  echo "Usage: $0 --target-dir <cargo-release-dir> --evidence-dir <new-directory>" >&2
}

TARGET_DIR=
EVIDENCE_DIR=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --target-dir) TARGET_DIR=$2; shift 2 ;;
    --evidence-dir) EVIDENCE_DIR=$2; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage; exit 2 ;;
  esac
done

if [ -z "$TARGET_DIR" ] || [ -z "$EVIDENCE_DIR" ]; then
  usage
  exit 2
fi
case "$TARGET_DIR:$EVIDENCE_DIR" in
  /*:/*) ;;
  *) echo "all paths must be absolute" >&2; exit 2 ;;
esac
if [ "$(uname -s)" != Darwin ] || [ "$(uname -m)" != arm64 ]; then
  echo "startup negative smoke requires Apple Silicon macOS" >&2
  exit 2
fi
if [ -e "$EVIDENCE_DIR" ]; then
  echo "refusing to replace existing evidence: $EVIDENCE_DIR" >&2
  exit 2
fi
for binary in hdctl hd-host; do
  if [ ! -x "$TARGET_DIR/$binary" ]; then
    echo "missing release binary: $TARGET_DIR/$binary" >&2
    exit 2
  fi
done

FAKE_APP="$EVIDENCE_DIR/InvalidRelease.app"
MACOS="$FAKE_APP/Contents/MacOS"
RELEASE="$FAKE_APP/Contents/Resources/release"
DATA_ROOT="$EVIDENCE_DIR/data"
mkdir -p "$MACOS" "$RELEASE/certifications" "$DATA_ROOT"
cp "$TARGET_DIR/hdctl" "$MACOS/hdctl"
cp "$TARGET_DIR/hd-host" "$MACOS/hd-host"
chmod 755 "$MACOS/hdctl" "$MACOS/hd-host"
# An empty trust store is deliberately invalid. The certificate exists only to make the bundle
# shape complete enough to reach runtime validation.
: > "$RELEASE/trusted-keys-v2.json"
: > "$RELEASE/certifications/invalid.json"

STARTED_AT=$(date +%s)
(
  set +e
  "$MACOS/hdctl" --data-root "$DATA_ROOT" health >"$EVIDENCE_DIR/client-1.log" 2>&1
  printf '%s\n' "$?" >"$EVIDENCE_DIR/client-1.status"
) &
CLIENT_1_PID=$!
(
  set +e
  "$MACOS/hdctl" --data-root "$DATA_ROOT" health >"$EVIDENCE_DIR/client-2.log" 2>&1
  printf '%s\n' "$?" >"$EVIDENCE_DIR/client-2.status"
) &
CLIENT_2_PID=$!
wait "$CLIENT_1_PID"
wait "$CLIENT_2_PID"
FINISHED_AT=$(date +%s)
ELAPSED_SECONDS=$((FINISHED_AT - STARTED_AT))

for client in 1 2; do
  STATUS=$(sed -n '1p' "$EVIDENCE_DIR/client-$client.status")
  if [ "$STATUS" -eq 0 ]; then
    echo "client $client unexpectedly succeeded" >&2
    exit 1
  fi
  if ! grep -Fq 'HD Host startup failed (release_materials_invalid)' \
      "$EVIDENCE_DIR/client-$client.log"; then
    echo "client $client did not receive the structured startup failure" >&2
    exit 1
  fi
done
if [ "$ELAPSED_SECONDS" -gt 5 ]; then
  echo "structured startup failure exceeded the five-second product budget" >&2
  exit 1
fi
if [ -e "$DATA_ROOT/trusted-keys-v2.json" ]; then
  echo "invalid release bundle installed a trust root" >&2
  exit 1
fi
if find "$DATA_ROOT/certifications" -type f -print -quit | grep -q .; then
  echo "invalid release bundle installed a certification" >&2
  exit 1
fi
if find "$DATA_ROOT" -maxdepth 1 -name 'host-startup-failure-v1-*.json' \
    -print -quit | grep -q .; then
  echo "consumed per-attempt startup failure records were not cleaned" >&2
  exit 1
fi
if ps -axo pid=,comm=,args= | awk -v needle="$DATA_ROOT" \
    '$2 ~ /hd-host$/ && index($0, needle) { found = 1 } END { exit found ? 0 : 1 }'; then
  echo "failed Host process leaked after startup rejection" >&2
  exit 1
fi
if ! grep -Fq '"host.startup.failed"' "$DATA_ROOT/logs/host-v2.jsonl."*; then
  echo "Host log does not contain the startup failure event" >&2
  exit 1
fi

printf '%s\n' \
  '{' \
  '  "schema_version": 1,' \
  '  "result": "pass",' \
  '  "clients": 2,' \
  '  "error_code": "release_materials_invalid",' \
  "  \"elapsed_seconds\": $ELAPSED_SECONDS," \
  '  "trust_root_installed": false,' \
  '  "certification_installed": false,' \
  '  "startup_record_leaked": false,' \
  '  "host_process_leaked": false' \
  '}' > "$EVIDENCE_DIR/result.json"
GENERATED_AT=$(date -u '+%Y-%m-%dT%H:%M:%SZ')
ELAPSED_MILLISECONDS=$((ELAPSED_SECONDS * 1000))
ESCAPED_EVIDENCE=$(printf '%s' "$EVIDENCE_DIR" | sed 's/\\/\\\\/g; s/"/\\"/g')
printf '%s\n' \
  '{' \
  '  "schema_version": 2,' \
  "  \"generated_at\": \"$GENERATED_AT\"," \
  '  "source": "scripts/macos-host-startup-negative-smoke.sh",' \
  '  "gates": [' \
  '    {' \
  '      "name": "host-startup-release-bootstrap",' \
  '      "command": "macos-host-startup-negative-smoke.sh --target-dir <release>",' \
  '      "status": "pass",' \
  "      \"duration_ms\": $ELAPSED_MILLISECONDS," \
  "      \"log_path\": \"$ESCAPED_EVIDENCE\"," \
  '      "summary": "Two concurrent clients received an attempt-scoped release_materials_invalid failure within the product budget; no partial trust state or Host process leaked."' \
  '    }' \
  '  ]' \
  '}' > "$EVIDENCE_DIR/host-startup-negative-gate.json"

echo "result=pass"
echo "clients=2"
echo "error_code=release_materials_invalid"
echo "elapsed_seconds=$ELAPSED_SECONDS"
echo "evidence=$EVIDENCE_DIR"
