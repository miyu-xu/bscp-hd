#!/bin/sh
set -eu

usage() {
  cat <<'EOF'
Usage: scripts/macos-artifact-hash-performance-smoke.sh \
  --app <HD.app> \
  --output <fresh-absolute-evidence-directory> \
  [--budget-seconds <1..600>]
EOF
}

app=
output=
budget_seconds=20
while [ "$#" -gt 0 ]; do
  case "$1" in
    --app) app=${2-}; shift 2 ;;
    --output) output=${2-}; shift 2 ;;
    --budget-seconds) budget_seconds=${2-}; shift 2 ;;
    --help|-h) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

[ "$(uname -s)" = Darwin ] || { echo "macOS is required" >&2; exit 2; }
[ -n "$app" ] && [ -d "$app" ] || { echo "--app must identify HD.app" >&2; exit 2; }
case "$app" in /*) ;; *) echo "--app must be absolute" >&2; exit 2;; esac
[ -n "$output" ] || { echo "--output is required" >&2; exit 2; }
case "$output" in /*) ;; *) echo "--output must be absolute" >&2; exit 2;; esac
[ ! -e "$output" ] || { echo "output already exists: $output" >&2; exit 2; }
case "$budget_seconds" in *[!0-9]*|'') echo "invalid --budget-seconds" >&2; exit 2;; esac
[ "$budget_seconds" -ge 1 ] && [ "$budget_seconds" -le 600 ] || { echo "invalid --budget-seconds" >&2; exit 2; }

repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
store="$app/Contents/Resources/products/android/artifact-store-v2"
index="$store/index-v2.json"
[ -f "$index" ] || { echo "packaged Android index is missing: $index" >&2; exit 1; }
guest=$(jq -er '.guest_bundle_digest' "$index")
manifest="$store/bundles/$guest/manifest-v2.json"
rootfs_relative=$(jq -er '.files[] | select(.role == "rootfs") | .relative_path' "$manifest")
expected=$(jq -er '.files[] | select(.role == "rootfs") | .sha256' "$manifest")
rootfs="$store/bundles/$guest/$rootfs_relative"
[ -f "$rootfs" ] || { echo "rootfs is missing: $rootfs" >&2; exit 1; }

mkdir -p "$output"
cd "$repo"
cargo build --release -p hd-runtime --bin hd-artifact-verification-smoke \
  >"$output/build.stdout" 2>"$output/build.stderr"
"$repo/target/release/hd-artifact-verification-smoke" \
  --file "$rootfs" \
  --expected-sha256 "$expected" \
  --budget-seconds "$budget_seconds" \
  >"$output/result.json" 2>"$output/hash.stderr"

jq -n \
  --arg summary "CommonCrypto 完整读取包内稀疏 Android rootfs，SHA-256 与签名 manifest 一致且未使用缓存或外部进程；耗时 $(jq -r '.duration_ms' "$output/result.json") ms，预算 $((budget_seconds * 1000)) ms。" \
  --arg log "$output/result.json" \
  '{schema_version:2,generated_at:(now|todate),source:"scripts/macos-artifact-hash-performance-smoke.sh",gates:[{name:"macos-artifact-verification-performance",command:"macos-artifact-hash-performance-smoke.sh --app <candidate>",status:"pass",duration_ms:null,log_path:$log,summary:$summary}]}' \
  >"$output/artifact-hash-performance-gate.json"

echo "evidence=$output"
echo "gate_report=$output/artifact-hash-performance-gate.json"
echo "duration_ms=$(jq -r '.duration_ms' "$output/result.json")"
