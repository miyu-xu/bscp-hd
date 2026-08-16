#!/bin/sh
set -eu

usage() {
  echo "Usage: $0 --runtime-dir <out/dist/macos> --product-root <vsoc_arm64_only> --output-dir <directory>" >&2
}

RUNTIME_DIR=
PRODUCT_ROOT=
OUTPUT_DIR=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --runtime-dir) RUNTIME_DIR=$2; shift 2 ;;
    --product-root) PRODUCT_ROOT=$2; shift 2 ;;
    --output-dir) OUTPUT_DIR=$2; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage; exit 2 ;;
  esac
done

for path in "$RUNTIME_DIR/bin/vm" "$RUNTIME_DIR/bin/virtmgr" "$RUNTIME_DIR/bin/crosvm" \
  "$RUNTIME_DIR/lib/libbinder-rpc.1.dylib" "$PRODUCT_ROOT"; do
  if [ ! -e "$path" ]; then
    echo "required Microdroid identity input is missing: $path" >&2
    exit 1
  fi
done
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
"$SCRIPT_DIR/microdroid-runtime-closure.sh" verify --closure "$PRODUCT_ROOT" >/dev/null
case "$OUTPUT_DIR" in
  /*) ;;
  *) echo "--output-dir must be absolute" >&2; exit 2 ;;
esac
mkdir -p "$OUTPUT_DIR"
for name in guest-files-v2.sha256 host-files-v2.sha256 runtime-identity-v2.json; do
  if [ -e "$OUTPUT_DIR/$name" ]; then
    echo "refusing to replace identity output: $OUTPUT_DIR/$name" >&2
    exit 2
  fi
done

STAGE=$(mktemp -d "$OUTPUT_DIR/.microdroid-identity.XXXXXX")
cleanup() {
  case "$STAGE" in
    "$OUTPUT_DIR"/.microdroid-identity.*) rm -rf -- "$STAGE" ;;
  esac
}
trap cleanup EXIT HUP INT TERM

(
  cd "$PRODUCT_ROOT"
  find . -type f ! -name '.DS_Store' ! -name '._*' -print | LC_ALL=C sort |
    while IFS= read -r relative; do
    shasum -a 256 "$relative"
  done
) > "$STAGE/guest-files-v2.sha256"

hash_host() {
  digest=$(shasum -a 256 "$1" | awk '{print $1}')
  printf '%s  MacOS/%s\n' "$digest" "$2"
}
{
  hash_host "$RUNTIME_DIR/bin/vm" vm
  hash_host "$RUNTIME_DIR/bin/virtmgr" virtmgr
  hash_host "$RUNTIME_DIR/bin/crosvm" crosvm
  hash_host "$RUNTIME_DIR/lib/libbinder-rpc.1.dylib" libbinder-rpc.1.dylib
} > "$STAGE/host-files-v2.sha256"

GUEST_DIGEST=$(shasum -a 256 "$STAGE/guest-files-v2.sha256" | awk '{print $1}')
HOST_DIGEST=$(shasum -a 256 "$STAGE/host-files-v2.sha256" | awk '{print $1}')
cat > "$STAGE/runtime-identity-v2.json" <<EOF
{
  "schema_version": 2,
  "profile": "hd-microdroid-macos-arm64-v2",
  "guest_digest": "$GUEST_DIGEST",
  "host_digest": "$HOST_DIGEST"
}
EOF

for name in guest-files-v2.sha256 host-files-v2.sha256 runtime-identity-v2.json; do
  mv "$STAGE/$name" "$OUTPUT_DIR/$name"
done
trap - EXIT HUP INT TERM
cleanup
echo "guest_digest=$GUEST_DIGEST"
echo "host_digest=$HOST_DIGEST"
