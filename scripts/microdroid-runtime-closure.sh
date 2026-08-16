#!/bin/sh
set -eu

usage() {
  cat <<'EOF'
Usage:
  microdroid-runtime-closure.sh create \
    --product-root <vsoc_arm64_only> --output-dir <new-directory>

  microdroid-runtime-closure.sh verify --closure <directory>
EOF
}

[ "$#" -gt 0 ] || { usage >&2; exit 2; }
MODE=$1
shift

PRODUCT_ROOT=
OUTPUT_DIR=
CLOSURE=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --product-root) PRODUCT_ROOT=$2; shift 2 ;;
    --output-dir) OUTPUT_DIR=$2; shift 2 ;;
    --closure) CLOSURE=$2; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

PROFILE=hd-microdroid-macos-arm64-runtime-v2
MANIFEST=runtime-files-v2.sha256
METADATA=runtime-closure-v2.plist
VIRT_ROOT=apex_dir/apex/com.android.virt
APEX_INFO_LIST=apex_dir/apex/apex-info-list.xml
ADBD_APEX=apex_dir/system/apex/com.android.adbd.apex
ADBD_CAPEX=apex_dir/system/apex/com.android.adbd.capex

require_absolute() {
  case "$2" in
    /*) ;;
    *) echo "$1 must be absolute" >&2; exit 2 ;;
  esac
}

copy_relative() {
  relative=$1
  source_path="$PRODUCT_ROOT/$relative"
  [ -f "$source_path" ] && [ ! -L "$source_path" ] || {
    echo "required Microdroid runtime file is missing or unsafe: $source_path" >&2
    exit 1
  }
  destination="$stage/$relative"
  mkdir -p "$(dirname -- "$destination")"
  install -m 644 "$source_path" "$destination"
}

if [ "$MODE" = create ]; then
  [ -n "$PRODUCT_ROOT" ] && [ -n "$OUTPUT_DIR" ] || {
    echo "create requires --product-root and --output-dir" >&2
    exit 2
  }
  require_absolute --product-root "$PRODUCT_ROOT"
  require_absolute --output-dir "$OUTPUT_DIR"
  [ -d "$PRODUCT_ROOT" ] || {
    echo "Microdroid product root is not a directory: $PRODUCT_ROOT" >&2
    exit 1
  }
  [ ! -e "$OUTPUT_DIR" ] || {
    echo "refusing to replace existing output: $OUTPUT_DIR" >&2
    exit 2
  }
  empty_root="$PRODUCT_ROOT/$VIRT_ROOT/app"
  empty_count=$(find "$empty_root" -type f -name 'EmptyPayloadApp*.apk' | wc -l | tr -d ' ')
  [ "$empty_count" = 1 ] || {
    echo "Microdroid product must contain exactly one EmptyPayloadApp APK" >&2
    exit 1
  }
  empty_apk=$(find "$empty_root" -type f -name 'EmptyPayloadApp*.apk' -print -quit)
  empty_relative=${empty_apk#"$PRODUCT_ROOT"/}
  output_parent=$(dirname -- "$OUTPUT_DIR")
  mkdir -p "$output_parent"
  stage=$(mktemp -d "$output_parent/.microdroid-runtime-closure.XXXXXX")
  cleanup() {
    case "$stage" in
      "$output_parent"/.microdroid-runtime-closure.*) rm -rf -- "$stage" ;;
    esac
  }
  trap cleanup EXIT HUP INT TERM
  copy_relative "$empty_relative"
  for relative in \
    "$VIRT_ROOT/etc/microdroid.json" \
    "$VIRT_ROOT/etc/microdroid_initrd_normal.img" \
    "$VIRT_ROOT/etc/microdroid_initrd_debuggable.img" \
    "$VIRT_ROOT/etc/fs/microdroid_kernel" \
    "$VIRT_ROOT/etc/fs/microdroid_super.img" \
    "$VIRT_ROOT/etc/fs/microdroid_vbmeta.img"; do
    copy_relative "$relative"
  done
  adbd_capex="$PRODUCT_ROOT/$ADBD_CAPEX"
  [ -f "$adbd_capex" ] && [ ! -L "$adbd_capex" ] || {
    echo "required Microdroid adbd CAPEX is missing or unsafe: $adbd_capex" >&2
    exit 1
  }
  [ -x /usr/bin/unzip ] || {
    echo "macOS /usr/bin/unzip is required to extract the sealed adbd APEX" >&2
    exit 1
  }
  mkdir -p "$stage/$(dirname -- "$ADBD_APEX")" "$stage/$(dirname -- "$APEX_INFO_LIST")"
  /usr/bin/unzip -p "$adbd_capex" original_apex >"$stage/$ADBD_APEX"
  [ -s "$stage/$ADBD_APEX" ] || {
    echo "Microdroid adbd CAPEX did not contain a non-empty original_apex" >&2
    exit 1
  }
  chmod 644 "$stage/$ADBD_APEX"
  cat >"$stage/$APEX_INFO_LIST" <<'EOF'
<?xml version="1.0" encoding="utf-8"?>
<apex-info-list>
  <apex-info moduleName="com.android.adbd" versionCode="1" modulePath="/system/apex/com.android.adbd.apex" lastUpdateMillis="0" isFactory="true" isActive="true" provideSharedApexLibs="false" preinstalledModulePath="/system/apex/com.android.adbd.apex"/>
</apex-info-list>
EOF
  chmod 644 "$stage/$APEX_INFO_LIST"
  (
    cd "$stage"
    find . -type f ! -name "$MANIFEST" ! -name "$METADATA" -print |
      LC_ALL=C sort | while IFS= read -r relative; do
      shasum -a 256 "$relative"
    done
  ) >"$stage/$MANIFEST"
  runtime_file_count=$(wc -l <"$stage/$MANIFEST" | tr -d ' ')
  [ "$runtime_file_count" = 9 ] || {
    echo "Microdroid runtime closure contains an unexpected file count" >&2
    exit 1
  }
  manifest_sha256=$(shasum -a 256 "$stage/$MANIFEST" | awk '{print $1}')
  cat >"$stage/$METADATA" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "https://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>schema_version</key><integer>2</integer>
  <key>profile</key><string>$PROFILE</string>
  <key>payload_contract</key><string>empty_or_uploaded_with_debug_adbd_apex</string>
  <key>debug_apex_module</key><string>com.android.adbd</string>
  <key>runtime_file_count</key><integer>$runtime_file_count</integer>
  <key>runtime_manifest_sha256</key><string>$manifest_sha256</string>
</dict>
</plist>
EOF
  plutil -lint "$stage/$METADATA" >/dev/null
  mv "$stage" "$OUTPUT_DIR"
  trap - EXIT HUP INT TERM
  echo "closure=$OUTPUT_DIR"
  echo "profile=$PROFILE"
  echo "runtime_file_count=$runtime_file_count"
  echo "runtime_manifest_sha256=$manifest_sha256"
  exit 0
fi

[ "$MODE" = verify ] || {
  echo "unknown mode: $MODE" >&2
  usage >&2
  exit 2
}
[ -n "$CLOSURE" ] || { echo "verify requires --closure" >&2; exit 2; }
require_absolute --closure "$CLOSURE"
[ -d "$CLOSURE" ] || { echo "closure is not a directory: $CLOSURE" >&2; exit 1; }
[ -f "$CLOSURE/$MANIFEST" ] && [ ! -L "$CLOSURE/$MANIFEST" ] &&
  [ -f "$CLOSURE/$METADATA" ] && [ ! -L "$CLOSURE/$METADATA" ] || {
  echo "closure manifest or metadata is missing or unsafe" >&2
  exit 1
}
[ -z "$(find "$CLOSURE" -type l -print -quit)" ] || {
  echo "closure must not contain symbolic links" >&2
  exit 1
}
plutil -lint "$CLOSURE/$METADATA" >/dev/null
schema_version=$(plutil -extract schema_version raw -o - "$CLOSURE/$METADATA")
profile=$(plutil -extract profile raw -o - "$CLOSURE/$METADATA")
payload_contract=$(plutil -extract payload_contract raw -o - "$CLOSURE/$METADATA")
debug_apex_module=$(plutil -extract debug_apex_module raw -o - "$CLOSURE/$METADATA")
expected_file_count=$(plutil -extract runtime_file_count raw -o - "$CLOSURE/$METADATA")
expected_manifest_sha256=$(plutil -extract runtime_manifest_sha256 raw -o - "$CLOSURE/$METADATA")
[ "$schema_version" = 2 ] || { echo "unsupported closure schema" >&2; exit 1; }
[ "$profile" = "$PROFILE" ] || { echo "unsupported closure profile" >&2; exit 1; }
[ "$payload_contract" = empty_or_uploaded_with_debug_adbd_apex ] || {
  echo "unsupported closure Payload contract" >&2
  exit 1
}
[ "$debug_apex_module" = com.android.adbd ] || {
  echo "unsupported closure debug APEX module" >&2
  exit 1
}
[ "$expected_file_count" = 9 ] || { echo "invalid closure file count" >&2; exit 1; }
if [ "${#expected_manifest_sha256}" -ne 64 ] ||
  printf '%s' "$expected_manifest_sha256" | LC_ALL=C grep -q '[^0-9a-f]'; then
  echo "invalid closure manifest digest" >&2
  exit 1
fi
actual_manifest_sha256=$(shasum -a 256 "$CLOSURE/$MANIFEST" | awk '{print $1}')
[ "$actual_manifest_sha256" = "$expected_manifest_sha256" ] || {
  echo "closure manifest digest does not match metadata" >&2
  exit 1
}
awk '
  length($1) != 64 || $1 ~ /[^0-9a-f]/ || $2 !~ /^\.\// ||
    $2 ~ /(^|\/)\.\.(\/|$)/ { exit 1 }
' "$CLOSURE/$MANIFEST" || {
  echo "closure manifest contains an unsafe entry" >&2
  exit 1
}
(
  cd "$CLOSURE"
  shasum -a 256 -c "$MANIFEST" >/dev/null
)
manifest_file_count=$(wc -l <"$CLOSURE/$MANIFEST" | tr -d ' ')
actual_file_count=$(find "$CLOSURE" -type f | wc -l | tr -d ' ')
[ "$manifest_file_count" = "$expected_file_count" ] &&
  [ "$actual_file_count" -eq $((expected_file_count + 2)) ] || {
  echo "closure contains missing or unexpected files" >&2
  exit 1
}
empty_count=$(find "$CLOSURE/$VIRT_ROOT/app" -type f -name 'EmptyPayloadApp*.apk' |
  wc -l | tr -d ' ')
[ "$empty_count" = 1 ] || { echo "closure EmptyPayload APK is invalid" >&2; exit 1; }
for relative in \
  "$VIRT_ROOT/etc/microdroid.json" \
  "$VIRT_ROOT/etc/microdroid_initrd_normal.img" \
  "$VIRT_ROOT/etc/microdroid_initrd_debuggable.img" \
  "$VIRT_ROOT/etc/fs/microdroid_kernel" \
  "$VIRT_ROOT/etc/fs/microdroid_super.img" \
  "$VIRT_ROOT/etc/fs/microdroid_vbmeta.img"; do
  [ -f "$CLOSURE/$relative" ] || {
    echo "closure is missing required runtime file: $relative" >&2
    exit 1
  }
done
[ -s "$CLOSURE/$ADBD_APEX" ] || {
  echo "closure is missing the extracted adbd APEX" >&2
  exit 1
}
[ -f "$CLOSURE/$APEX_INFO_LIST" ] && [ ! -L "$CLOSURE/$APEX_INFO_LIST" ] || {
  echo "closure is missing the APEX inventory" >&2
  exit 1
}
grep -Fq 'moduleName="com.android.adbd"' "$CLOSURE/$APEX_INFO_LIST" &&
  grep -Fq 'modulePath="/system/apex/com.android.adbd.apex"' "$CLOSURE/$APEX_INFO_LIST" &&
  grep -Fq 'isActive="true"' "$CLOSURE/$APEX_INFO_LIST" || {
  echo "closure APEX inventory does not activate the sealed adbd APEX" >&2
  exit 1
}
echo "closure=$CLOSURE"
echo "profile=$profile"
echo "runtime_file_count=$manifest_file_count"
echo "runtime_manifest_sha256=$actual_manifest_sha256"
