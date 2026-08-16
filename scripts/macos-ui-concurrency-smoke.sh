#!/bin/sh
set -eu

usage() {
  echo "usage: macos-ui-concurrency-smoke.sh --output ABSOLUTE_DIR" >&2
  exit 2
}

fail() {
  echo "$*" >&2
  exit 1
}

OUTPUT=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --output) [ "$#" -ge 2 ] || usage; OUTPUT=$2; shift 2 ;;
    *) usage ;;
  esac
done

[ -n "$OUTPUT" ] || usage
case "$OUTPUT" in /*) ;; *) fail "--output must be absolute" ;; esac
[ ! -L "$OUTPUT" ] || fail "--output must not be a symbolic link"
if [ -e "$OUTPUT" ]; then
  [ -d "$OUTPUT" ] || fail "--output must be a directory"
else
  mkdir -p -- "$OUTPUT"
fi
OUTPUT=$(cd -- "$OUTPUT" && pwd -P)

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)
ARTIFACT="$OUTPUT/macos-ui-interaction.json"
REPORT="$OUTPUT/web-ui-concurrency-gate.json"
TEMP_REPORT=$(mktemp "$OUTPUT/.web-ui-concurrency.XXXXXX")
trap 'rm -f -- "$TEMP_REPORT"' EXIT HUP INT TERM

STARTED=$(date +%s)
(
  cd -- "$ROOT"
  cargo run -p hd-ui --bin hd-ui-contract-smoke -- --output "$ARTIFACT"
)

jq -e '
  .gate == "macos-ui-interaction" and
  .status == "pass" and
  .snapshot_concurrency.stale_selection_rejected == true and
  .snapshot_concurrency.stale_mutation_rejected == true and
  .snapshot_concurrency.duplicate_completion_rejected == true and
  .snapshot_concurrency.operation_target_bound == true and
  .microdroid_instance_ui.guest_kind_selectable == true and
  .microdroid_instance_ui.instance_settings_revisioned == true and
  .microdroid_instance_ui.guest_kind_immutable == true and
  .microdroid_instance_ui.android_player_tools_hidden == true and
  .microdroid_instance_ui.android_devices_hidden == true and
  .microdroid_instance_ui.headless_workload_page == true and
  .microdroid_instance_ui.payload_import == true and
  .microdroid_instance_ui.adb_shell == true and
  .microdroid_instance_ui.graphics_settings_hidden == true and
  .microdroid_instance_ui.host_display_actions_rejected == true
' "$ARTIFACT" >/dev/null || fail "macOS UI concurrency evidence is incomplete"

FINISHED=$(date +%s)
DURATION_MS=$(( (FINISHED - STARTED) * 1000 ))
GENERATED_AT=$(date -u +%Y-%m-%dT%H:%M:%SZ)
jq -n \
  --arg generated_at "$GENERATED_AT" \
  --arg log_path "$ARTIFACT" \
  --argjson duration_ms "$DURATION_MS" \
  '{
    schema_version: 2,
    generated_at: $generated_at,
    source: "scripts/macos-ui-concurrency-smoke.sh",
    gates: [{
      name: "web-ui-concurrency",
      command: "macos-ui-concurrency-smoke.sh --output <absolute-dir>",
      status: "pass",
      duration_ms: $duration_ms,
      log_path: $log_path,
      summary: "Stale Host snapshots cannot revert instance selection or overwrite post-mutation state; duplicate completions cannot consume a newer request; sidebar power actions are bound to a hydrated target instance; async settings and Payload results cannot overwrite a newly selected instance. Microdroid remains a selectable, revisioned, immutable headless instance type with only workload, Payload, Shell and diagnostics surfaces; Android Player, device and graphics controls remain unavailable."
    }]
  }' >"$TEMP_REPORT"
chmod 600 "$TEMP_REPORT"
mv -f -- "$TEMP_REPORT" "$REPORT"
trap - EXIT HUP INT TERM
printf '%s\n' "$REPORT"
