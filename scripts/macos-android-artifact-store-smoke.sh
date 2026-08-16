#!/bin/sh
set -eu

usage() {
  cat >&2 <<'EOF'
Usage: scripts/macos-android-artifact-store-smoke.sh \
  --xtask <release-xtask> \
  --store <signed-development-artifact-store-v2> \
  --output <fresh-evidence-directory>
EOF
}

XTASK=
STORE=
OUTPUT=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --xtask) XTASK=$2; shift 2 ;;
    --store) STORE=$2; shift 2 ;;
    --output) OUTPUT=$2; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage; exit 2 ;;
  esac
done

fail() {
  echo "$*" >&2
  exit 1
}

case "$XTASK" in /*) ;; *) fail "--xtask must be absolute" ;; esac
case "$STORE" in /*) ;; *) fail "--store must be absolute" ;; esac
case "$OUTPUT" in /*) ;; *) fail "--output must be absolute" ;; esac
[ -x "$XTASK" ] && [ ! -L "$XTASK" ] || fail "xtask is not an executable regular file"
[ -d "$STORE" ] && [ ! -L "$STORE" ] || fail "store is not a regular directory"
[ ! -e "$OUTPUT" ] || fail "refusing to replace output: $OUTPUT"

parent=$(dirname -- "$OUTPUT")
mkdir -p "$parent"
stage=$(mktemp -d "$parent/.android-artifact-store-smoke.XXXXXX")
work=$(mktemp -d /private/tmp/hd-android-artifact-store-negative.XXXXXX)
cleanup() {
  case "$stage" in "$parent"/.android-artifact-store-smoke.*) rm -rf -- "$stage" ;; esac
  case "$work" in /private/tmp/hd-android-artifact-store-negative.*) rm -rf -- "$work" ;; esac
}
trap cleanup EXIT HUP INT TERM

verify() {
  candidate=$1
  trust=$2
  channel=$3
  "$XTASK" verify-android-artifact-store \
    --store-root "$candidate" --trust-store "$trust" --channel "$channel"
}

copy_case() {
  name=$1
  destination="$work/$name"
  [ ! -e "$destination" ] || fail "duplicate negative case: $name"
  cp -cR "$STORE" "$destination"
  printf '%s\n' "$destination"
}

expect_failure() {
  name=$1
  shift
  if "$@" > "$stage/$name.stdout" 2> "$stage/$name.stderr"; then
    fail "negative case unexpectedly passed: $name"
  fi
  [ -s "$stage/$name.stderr" ] || fail "negative case produced no diagnostic: $name"
}

trust="$STORE/trusted-keys-v2.json"
verify "$STORE" "$trust" development > "$stage/positive.json"
guest_digest=$(plutil -extract guest_bundle_digest raw -o - "$STORE/index-v2.json")
rootfs_relative=$(plutil -extract rootfs_relative_path raw -o - "$stage/positive.json")

expect_failure wrong-channel verify "$STORE" "$trust" release

extra=$(copy_case extra-file)
printf '%s\n' unexpected > "$extra/unlisted.txt"
expect_failure extra-file verify "$extra" "$extra/trusted-keys-v2.json" development

tampered=$(copy_case tampered-rootfs)
printf x >> "$tampered/bundles/$guest_digest/$rootfs_relative"
expect_failure tampered-rootfs verify "$tampered" \
  "$tampered/trusted-keys-v2.json" development

wrong_trust=$(copy_case wrong-trust)
printf '%s\n' '{}' > "$wrong_trust/trusted-keys-v2.json"
expect_failure wrong-trust verify "$wrong_trust" \
  "$wrong_trust/trusted-keys-v2.json" development

release_profile=$(copy_case release-profile)
plutil -replace channel -string release "$release_profile/index-v2.json"
plutil -replace data_profile -string metadata-encrypted "$release_profile/index-v2.json"
expect_failure release-profile verify "$release_profile" \
  "$release_profile/trusted-keys-v2.json" release

ln -s "$STORE" "$work/store-symlink"
expect_failure store-symlink verify "$work/store-symlink" "$trust" development

nested_parent=$(copy_case nested-parent-symlink)
nested_source="$nested_parent/bundles/$guest_digest"
nested_outside="$work/nested-parent-outside"
cp -cR "$nested_source" "$nested_outside"
rm -rf -- "$nested_source"
ln -s "$nested_outside" "$nested_source"
expect_failure nested-parent-symlink verify "$nested_parent" \
  "$nested_parent/trusted-keys-v2.json" development

generated_at=$(date -u '+%Y-%m-%dT%H:%M:%SZ')
cat > "$stage/result.json" <<EOF
{
  "schema_version": 1,
  "profile": "hd-macos-arm64-android-artifact-store-v2",
  "positive": "pass",
  "signature_verified": true,
  "exact_closure": true,
  "wrong_channel": "rejected",
  "extra_file": "rejected",
  "tampered_rootfs": "rejected",
  "wrong_trust": "rejected",
  "release_profile_mismatch": "rejected",
  "store_symlink": "rejected",
  "nested_parent_symlink": "rejected"
}
EOF
cat > "$stage/android-artifact-store-gates.json" <<EOF
{
  "schema_version": 2,
  "generated_at": "$generated_at",
  "source": "scripts/macos-android-artifact-store-smoke.sh",
  "gates": [
    {
      "name": "macos-android-artifact-store-contract",
      "command": "macos-android-artifact-store-smoke.sh --store <signed-store>",
      "status": "pass",
      "duration_ms": null,
      "log_path": null,
      "summary": "QA Ed25519 signed Android store passed exact closure verification; wrong channel, extra file, rootfs tamper, wrong trust root, release data-profile mismatch, store-root symlink and nested artifact-parent symlink were rejected."
    }
  ]
}
EOF
mv "$stage" "$OUTPUT"
trap - EXIT HUP INT TERM
case "$work" in /private/tmp/hd-android-artifact-store-negative.*) rm -rf -- "$work" ;; esac
echo "evidence=$OUTPUT"
echo "gate_report=$OUTPUT/android-artifact-store-gates.json"
