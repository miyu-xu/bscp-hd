#!/bin/sh
set -eu

usage() {
  cat <<'EOF'
Usage: scripts/package-macos.sh \
  --target-dir <cargo-release-dir> \
  --runtime-dir <out/dist/macos> \
  --microdroid-product <products/microdroid/vsoc_arm64_only> \
  [--android-product <products/android/vsoc_arm64_only>] \
  [--android-artifact-store <signed-artifact-store-v2>] \
  --web-dist <web/dist> \
  --adb <adb-binary> \
  --aapt2 <aapt2-binary> \
  --apksigner <apksigner> \
  --node-root <node-v22.23.1-darwin-arm64> \
  --node-archive <node-v22.23.1-darwin-arm64.tar.gz> \
  --java-home <Temurin-21.0.12+8/Contents/Home> \
  --java-archive <OpenJDK21U-jdk_aarch64_mac_hotspot_21.0.12_8.tar.gz> \
  --android-build-tools <android-sdk/build-tools/36.0.0> \
  --release-toolchain-evidence <verified-web-build-evidence> \
  --microdroid-payload-bundle <versioned-payload-bundle> \
  --output <HD.app> \
  [--identity <Developer-ID-Application-or->] \
  [--release-materials <trusted-root-and-certifications-dir>] \
  [--microdroid-payload-signer-sha256 <release-signer-digest>] \
  [--development-package] \
  [--notary-profile <keychain-profile>] \
  [--version <short-version>] [--build <bundle-version>]
EOF
}

TARGET_DIR=
RUNTIME_DIR=
MICRODROID_PRODUCT=
ANDROID_PRODUCT=
ANDROID_ARTIFACT_STORE=
WEB_DIST=
ADB=
AAPT2=
APKSIGNER=
NODE_ROOT=
NODE_ARCHIVE=
RELEASE_JAVA_HOME=
JAVA_ARCHIVE=
ANDROID_BUILD_TOOLS=
TOOLCHAIN_EVIDENCE=
MICRODROID_PAYLOAD_BUNDLE=
MICRODROID_PAYLOAD_SIGNER_SHA256=
OUTPUT=
IDENTITY=-
NOTARY_PROFILE=
RELEASE_MATERIALS=
DEVELOPMENT_PACKAGE=0
VERSION=0.1.0
BUILD=1

while [ "$#" -gt 0 ]; do
  case "$1" in
    --target-dir) TARGET_DIR=$2; shift 2 ;;
    --runtime-dir) RUNTIME_DIR=$2; shift 2 ;;
    --microdroid-product) MICRODROID_PRODUCT=$2; shift 2 ;;
    --android-product) ANDROID_PRODUCT=$2; shift 2 ;;
    --android-artifact-store) ANDROID_ARTIFACT_STORE=$2; shift 2 ;;
    --web-dist) WEB_DIST=$2; shift 2 ;;
    --adb) ADB=$2; shift 2 ;;
    --aapt2) AAPT2=$2; shift 2 ;;
    --apksigner) APKSIGNER=$2; shift 2 ;;
    --node-root) NODE_ROOT=$2; shift 2 ;;
    --node-archive) NODE_ARCHIVE=$2; shift 2 ;;
    --java-home) RELEASE_JAVA_HOME=$2; shift 2 ;;
    --java-archive) JAVA_ARCHIVE=$2; shift 2 ;;
    --android-build-tools) ANDROID_BUILD_TOOLS=$2; shift 2 ;;
    --release-toolchain-evidence) TOOLCHAIN_EVIDENCE=$2; shift 2 ;;
    --microdroid-payload-bundle) MICRODROID_PAYLOAD_BUNDLE=$2; shift 2 ;;
    --microdroid-payload-signer-sha256)
      MICRODROID_PAYLOAD_SIGNER_SHA256=$2
      shift 2
      ;;
    --output) OUTPUT=$2; shift 2 ;;
    --identity) IDENTITY=$2; shift 2 ;;
    --release-materials) RELEASE_MATERIALS=$2; shift 2 ;;
    --development-package) DEVELOPMENT_PACKAGE=1; shift ;;
    --notary-profile) NOTARY_PROFILE=$2; shift 2 ;;
    --version) VERSION=$2; shift 2 ;;
    --build) BUILD=$2; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

for value in TARGET_DIR RUNTIME_DIR MICRODROID_PRODUCT WEB_DIST ADB AAPT2 APKSIGNER \
  NODE_ROOT NODE_ARCHIVE RELEASE_JAVA_HOME JAVA_ARCHIVE ANDROID_BUILD_TOOLS \
  TOOLCHAIN_EVIDENCE MICRODROID_PAYLOAD_BUNDLE OUTPUT; do
  case "$value" in
    TARGET_DIR) candidate=$TARGET_DIR ;;
    RUNTIME_DIR) candidate=$RUNTIME_DIR ;;
    MICRODROID_PRODUCT) candidate=$MICRODROID_PRODUCT ;;
    WEB_DIST) candidate=$WEB_DIST ;;
    ADB) candidate=$ADB ;;
    AAPT2) candidate=$AAPT2 ;;
    APKSIGNER) candidate=$APKSIGNER ;;
    NODE_ROOT) candidate=$NODE_ROOT ;;
    NODE_ARCHIVE) candidate=$NODE_ARCHIVE ;;
    RELEASE_JAVA_HOME) candidate=$RELEASE_JAVA_HOME ;;
    JAVA_ARCHIVE) candidate=$JAVA_ARCHIVE ;;
    ANDROID_BUILD_TOOLS) candidate=$ANDROID_BUILD_TOOLS ;;
    TOOLCHAIN_EVIDENCE) candidate=$TOOLCHAIN_EVIDENCE ;;
    MICRODROID_PAYLOAD_BUNDLE) candidate=$MICRODROID_PAYLOAD_BUNDLE ;;
    OUTPUT) candidate=$OUTPUT ;;
  esac
  if [ -z "$candidate" ]; then
    echo "missing required argument: $value" >&2
    usage >&2
    exit 2
  fi
done

if [ "$(uname -s)" != Darwin ] || [ "$(uname -m)" != arm64 ]; then
  echo "macOS arm64 packaging requires an Apple Silicon macOS host" >&2
  exit 2
fi
case "$OUTPUT" in
  /*.app) ;;
  *) echo "--output must be an absolute .app path" >&2; exit 2 ;;
esac
case "$APKSIGNER" in
  /*) ;;
  *) echo "--apksigner must be absolute" >&2; exit 2 ;;
esac
for toolchain_path in "$NODE_ROOT" "$NODE_ARCHIVE" "$RELEASE_JAVA_HOME" "$JAVA_ARCHIVE" \
  "$ANDROID_BUILD_TOOLS" "$TOOLCHAIN_EVIDENCE"; do
  case "$toolchain_path" in
    /*) ;;
    *) echo "release toolchain inputs must be absolute" >&2; exit 2 ;;
  esac
done
case "$MICRODROID_PAYLOAD_BUNDLE" in
  /*) ;;
  *) echo "--microdroid-payload-bundle must be absolute" >&2; exit 2 ;;
esac
if [ -e "$OUTPUT" ]; then
  echo "refusing to replace existing output: $OUTPUT" >&2
  exit 2
fi
if [ -n "$NOTARY_PROFILE" ] && [ "$IDENTITY" = "-" ]; then
  echo "notarization requires a Developer ID Application identity" >&2
  exit 2
fi
if [ "$DEVELOPMENT_PACKAGE" -eq 0 ] && [ "$IDENTITY" = "-" ]; then
  echo "release packaging requires a Developer ID identity; use --development-package for local QA" >&2
  exit 2
fi
if [ "$DEVELOPMENT_PACKAGE" -eq 0 ] && [ -z "$RELEASE_MATERIALS" ]; then
  echo "release packaging requires --release-materials" >&2
  exit 2
fi
if [ "$DEVELOPMENT_PACKAGE" -eq 0 ] && [ -z "$MICRODROID_PAYLOAD_SIGNER_SHA256" ]; then
  echo "release packaging requires --microdroid-payload-signer-sha256" >&2
  exit 2
fi
if [ "$DEVELOPMENT_PACKAGE" -eq 1 ] && [ -z "$ANDROID_PRODUCT" ] \
    && [ -z "$ANDROID_ARTIFACT_STORE" ]; then
  echo "development packaging requires exactly one self-contained Android source" >&2
  exit 2
fi
if [ -n "$ANDROID_PRODUCT" ] && [ -n "$ANDROID_ARTIFACT_STORE" ]; then
  echo "--android-product and --android-artifact-store are mutually exclusive" >&2
  exit 2
fi
if [ "$DEVELOPMENT_PACKAGE" -eq 0 ] && [ -n "$ANDROID_PRODUCT" ]; then
  echo "--android-product is a direct development image and may only be used with --development-package" >&2
  exit 2
fi
if [ "$DEVELOPMENT_PACKAGE" -eq 0 ] && [ -z "$ANDROID_ARTIFACT_STORE" ]; then
  echo "release packaging requires --android-artifact-store" >&2
  exit 2
fi
if [ -n "$ANDROID_PRODUCT" ]; then
  case "$ANDROID_PRODUCT" in
    /*) ;;
    *) echo "--android-product must be an absolute directory" >&2; exit 2 ;;
  esac
  if [ ! -d "$ANDROID_PRODUCT" ] || [ -L "$ANDROID_PRODUCT" ]; then
    echo "--android-product must be a real directory, not a symbolic link" >&2
    exit 2
  fi
fi
if [ -n "$ANDROID_ARTIFACT_STORE" ]; then
  case "$ANDROID_ARTIFACT_STORE" in
    /*) ;;
    *) echo "--android-artifact-store must be an absolute directory" >&2; exit 2 ;;
  esac
  if [ ! -d "$ANDROID_ARTIFACT_STORE" ] || [ -L "$ANDROID_ARTIFACT_STORE" ]; then
    echo "--android-artifact-store must be a real directory, not a symbolic link" >&2
    exit 2
  fi
  if [ "$DEVELOPMENT_PACKAGE" -eq 0 ] && [ -z "$RELEASE_MATERIALS" ]; then
    echo "release --android-artifact-store requires --release-materials with its trust root" >&2
    exit 2
  fi
fi
if [ -n "$RELEASE_MATERIALS" ]; then
  case "$RELEASE_MATERIALS" in
    /*) ;;
    *) echo "--release-materials must be an absolute directory" >&2; exit 2 ;;
  esac
  if [ ! -d "$RELEASE_MATERIALS" ] || [ -L "$RELEASE_MATERIALS" ]; then
    echo "--release-materials must be a real directory, not a symbolic link" >&2
    exit 2
  fi
  require_release_trust="$RELEASE_MATERIALS/trusted-keys-v2.json"
  require_release_certifications="$RELEASE_MATERIALS/certifications"
  if [ ! -f "$require_release_trust" ] || [ -L "$require_release_trust" ] \
      || [ ! -d "$require_release_certifications" ] || [ -L "$require_release_certifications" ]; then
    echo "--release-materials must contain trusted-keys-v2.json and certifications/" >&2
    exit 2
  fi
  if find "$require_release_certifications" -mindepth 2 -print -quit | grep -q .; then
    echo "--release-materials certifications/ must be flat" >&2
    exit 2
  fi
  if find "$require_release_certifications" -mindepth 1 -maxdepth 1 \
      ! -type f -print -quit | grep -q .; then
    echo "--release-materials certifications/ may contain only regular files" >&2
    exit 2
  fi
  if find "$require_release_certifications" -mindepth 1 -maxdepth 1 \
      -type f ! -name '*.json' -print -quit | grep -q .; then
    echo "--release-materials certifications/ may contain only .json certificates" >&2
    exit 2
  fi
  if ! find "$require_release_certifications" -mindepth 1 -maxdepth 1 \
      -type f -name '*.json' -print -quit | grep -q .; then
    echo "--release-materials certifications/ contains no JSON certificate" >&2
    exit 2
  fi
fi

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)
if [ -n "$ANDROID_ARTIFACT_STORE" ]; then
  if [ "$DEVELOPMENT_PACKAGE" -eq 1 ]; then
    ANDROID_STORE_CHANNEL=development
  else
    ANDROID_STORE_CHANNEL=release
  fi
fi
"$ROOT/scripts/macos-release-toolchain.sh" verify-tools \
  --node-root "$NODE_ROOT" \
  --node-archive "$NODE_ARCHIVE" \
  --java-home "$RELEASE_JAVA_HOME" \
  --java-archive "$JAVA_ARCHIVE" \
  --android-build-tools "$ANDROID_BUILD_TOOLS" >/dev/null
"$ROOT/scripts/macos-release-toolchain.sh" verify-evidence \
  --node-root "$NODE_ROOT" \
  --node-archive "$NODE_ARCHIVE" \
  --java-home "$RELEASE_JAVA_HOME" \
  --java-archive "$JAVA_ARCHIVE" \
  --android-build-tools "$ANDROID_BUILD_TOOLS" \
  --web-dist "$WEB_DIST" \
  --evidence-dir "$TOOLCHAIN_EVIDENCE" >/dev/null
if [ ! "$APKSIGNER" -ef "$ANDROID_BUILD_TOOLS/apksigner" ]; then
  echo "--apksigner must be the pinned Android build-tools apksigner" >&2
  exit 2
fi
if [ ! "$AAPT2" -ef "$ANDROID_BUILD_TOOLS/aapt2" ]; then
  echo "--aapt2 must be the pinned Android build-tools aapt2" >&2
  exit 2
fi
OUTPUT_PARENT=$(dirname -- "$OUTPUT")
mkdir -p "$OUTPUT_PARENT"
STAGE=$(mktemp -d "$OUTPUT_PARENT/.hd-macos-package.XXXXXX")
cleanup() {
  case "$STAGE" in
    "$OUTPUT_PARENT"/.hd-macos-package.*) rm -rf -- "$STAGE" ;;
  esac
}
trap cleanup EXIT HUP INT TERM

APP="$STAGE/HD.app"
CONTENTS="$APP/Contents"
MACOS="$CONTENTS/MacOS"
RESOURCES="$CONTENTS/Resources"
mkdir -p "$MACOS" "$RESOURCES/scripts" "$RESOURCES/legal"

require_file() {
  if [ ! -f "$1" ]; then
    echo "required regular file is missing: $1" >&2
    exit 1
  fi
}

copy_exec() {
  require_file "$1"
  install -m 755 "$1" "$2"
}

copy_data() {
  require_file "$1"
  install -m 644 "$1" "$2"
}

for name in hd hdctl hd-host hd-worker hd-device-sim hd-adb-bridge \
  hd-casimir-adapter hd-rootcanal-adapter hd-frame-producer \
  hd-uwb-adapter hd-modem-adapter hd-network-adapter \
  hd-audio-adapter hd-camera-adapter hd-native-display-probe; do
  destination=$name
  [ "$name" = hd ] && destination=HD
  copy_exec "$TARGET_DIR/$name" "$MACOS/$destination"
done

copy_exec "$RUNTIME_DIR/bin/crosvm" "$MACOS/crosvm"
copy_exec "$RUNTIME_DIR/bin/vm" "$MACOS/vm"
copy_exec "$RUNTIME_DIR/bin/virtmgr" "$MACOS/virtmgr"
copy_exec "$ADB" "$MACOS/adb"
copy_exec "$AAPT2" "$MACOS/aapt2"
copy_data "$RUNTIME_DIR/lib/libbinder-rpc.1.0.0.dylib" "$MACOS/libbinder-rpc.1.0.0.dylib"
copy_data "$RUNTIME_DIR/lib/libbinder-rpc.1.dylib" "$MACOS/libbinder-rpc.1.dylib"
copy_data "$RUNTIME_DIR/lib/libgfxstream_backend.dylib" "$MACOS/libgfxstream_backend.dylib"
copy_data "$RUNTIME_DIR/lib/libvulkan.dylib" "$MACOS/libvulkan.dylib"
copy_data "$RUNTIME_DIR/bin/libGLESv2.dylib" "$MACOS/libGLESv2.dylib"
copy_data "$RUNTIME_DIR/bin/libEGL.dylib" "$MACOS/libEGL.dylib"

ensure_executable_rpath() {
  binary=$1
  if ! otool -l "$binary" | grep -A3 LC_RPATH | grep -q 'path @executable_path '; then
    install_name_tool -add_rpath @executable_path "$binary"
  fi
}

# Developer ID hardened-runtime processes must not depend on DYLD_LIBRARY_PATH. Crosvm loads the
# packaged gfxstream backend through @rpath, while both AVF host tools load libbinder-rpc through
# @rpath, so bind every lookup to the signed application directory before signing.
ensure_executable_rpath "$MACOS/crosvm"
ensure_executable_rpath "$MACOS/vm"
ensure_executable_rpath "$MACOS/virtmgr"

require_file "$WEB_DIST/index.html"
ditto "$WEB_DIST" "$RESOURCES/ui"
if [ -n "$ANDROID_PRODUCT" ]; then
  ANDROID_DIRECT="$ANDROID_PRODUCT/direct-linux"
  for artifact in kernel initrd_android.img aggregate_android.img android_fstab.dt; do
    require_file "$ANDROID_DIRECT/$artifact"
    if [ -L "$ANDROID_DIRECT/$artifact" ]; then
      echo "Android development artifact must not be a symbolic link: $ANDROID_DIRECT/$artifact" >&2
      exit 1
    fi
  done
  data_entries=$(awk '$2 == "/data" { count += 1 } END { print count + 0 }' \
    "$ANDROID_DIRECT/android_fstab.dt")
  if [ "$data_entries" -ne 1 ]; then
    echo "Android development fstab must contain exactly one /data entry" >&2
    exit 1
  fi
  data_flags=$(awk '$2 == "/data" { print $5 }' "$ANDROID_DIRECT/android_fstab.dt")
  case ",$data_flags," in
    *,first_stage_mount,*) ;;
    *) echo "Android development /data must use first_stage_mount" >&2; exit 1 ;;
  esac
  case ",$data_flags," in
    *,latemount,*|*,inlinecrypt,*|*,fileencryption=*|*,keydirectory=*)
      echo "Android development /data must be first-stage and unencrypted" >&2
      exit 1
      ;;
  esac
  ANDROID_RESOURCES="$RESOURCES/products/android/vsoc_arm64_only/direct-linux"
  mkdir -p "$ANDROID_RESOURCES"
  # APFS clone copies preserve the sparse 16 GiB logical rootfs without inflating the package
  # staging tree to its full logical size.
  cp -c "$ANDROID_DIRECT/kernel" "$ANDROID_RESOURCES/kernel"
  cp -c "$ANDROID_DIRECT/initrd_android.img" "$ANDROID_RESOURCES/initrd_android.img"
  cp -c "$ANDROID_DIRECT/aggregate_android.img" "$ANDROID_RESOURCES/aggregate_android.img"
  cp -c "$ANDROID_DIRECT/android_fstab.dt" "$ANDROID_RESOURCES/android_fstab.dt"
  chmod 755 "$ANDROID_RESOURCES/kernel"
  chmod 644 "$ANDROID_RESOURCES/initrd_android.img" \
    "$ANDROID_RESOURCES/aggregate_android.img" "$ANDROID_RESOURCES/android_fstab.dt"
  if [ -f "$ANDROID_DIRECT/README.txt" ] && [ ! -L "$ANDROID_DIRECT/README.txt" ]; then
    cp -c "$ANDROID_DIRECT/README.txt" "$ANDROID_RESOURCES/README.txt"
    chmod 644 "$ANDROID_RESOURCES/README.txt"
  fi
  (
    cd "$ANDROID_RESOURCES"
    for relative in aggregate_android.img android_fstab.dt initrd_android.img kernel; do
      digest=$(shasum -a 256 "$relative" | awk '{print $1}')
      printf '%s  %s\n' "$digest" "$relative"
    done
  ) > "$ANDROID_RESOURCES/runtime-files-v1.sha256"
  cat > "$RESOURCES/products/android/development-direct-v1.plist" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "https://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>schema_version</key><integer>1</integer>
  <key>channel</key><string>development</string>
  <key>guest_kind</key><string>android</string>
  <key>android_version</key><string>15.0.0_r14</string>
  <key>data_profile</key><string>development-unencrypted</string>
  <key>mount_stage</key><string>first_stage_mount</string>
  <key>runtime_manifest</key><string>vsoc_arm64_only/direct-linux/runtime-files-v1.sha256</string>
</dict>
</plist>
EOF
  plutil -lint "$RESOURCES/products/android/development-direct-v1.plist" >/dev/null
fi
if [ -n "$ANDROID_ARTIFACT_STORE" ]; then
  ANDROID_STORE_RESOURCES="$RESOURCES/products/android/artifact-store-v2"
  mkdir -p "$(dirname -- "$ANDROID_STORE_RESOURCES")"
  cp -cR "$ANDROID_ARTIFACT_STORE" "$ANDROID_STORE_RESOURCES"
  if [ "$DEVELOPMENT_PACKAGE" -eq 1 ]; then
    ANDROID_STAGED_STORE_TRUST="$ANDROID_STORE_RESOURCES/trusted-keys-v2.json"
  else
    ANDROID_STAGED_STORE_TRUST="$RELEASE_MATERIALS/trusted-keys-v2.json"
  fi
  ANDROID_STAGED_VERIFICATION="$STAGE/android-artifact-store-verification.json"
  "$TARGET_DIR/xtask" verify-android-artifact-store \
    --store-root "$ANDROID_STORE_RESOURCES" \
    --trust-store "$ANDROID_STAGED_STORE_TRUST" \
    --channel "$ANDROID_STORE_CHANNEL" > "$ANDROID_STAGED_VERIFICATION"
  ANDROID_SIGNED_ROOTFS_SHA256=$(plutil -extract rootfs_sha256 raw -o - \
    "$ANDROID_STAGED_VERIFICATION")
  ANDROID_SIGNED_GUEST_DIGEST=$(plutil -extract guest_bundle_digest raw -o - \
    "$ANDROID_STAGED_VERIFICATION")
  ANDROID_SIGNED_ROOTFS_RELATIVE=$(plutil -extract rootfs_relative_path raw -o - \
    "$ANDROID_STAGED_VERIFICATION")
  [ "${#ANDROID_SIGNED_ROOTFS_SHA256}" -eq 64 ] || {
    echo "staged signed Android rootfs digest is invalid" >&2
    exit 1
  }
  ANDROID_SIGNED_ROOTFS_APP_RELATIVE="./Contents/Resources/products/android/artifact-store-v2/bundles/$ANDROID_SIGNED_GUEST_DIGEST/$ANDROID_SIGNED_ROOTFS_RELATIVE"
fi
for artifact in \
  "$MICRODROID_PRODUCT/apex_dir/apex/com.android.virt/etc/microdroid.json" \
  "$MICRODROID_PRODUCT/apex_dir/apex/com.android.virt/etc/fs/microdroid_kernel" \
  "$MICRODROID_PRODUCT/apex_dir/apex/com.android.virt/etc/fs/microdroid_super.img"; do
  require_file "$artifact"
done
mkdir -p "$RESOURCES/products/microdroid"
MICRODROID_CLOSURE="$RESOURCES/products/microdroid/vsoc_arm64_only"
"$ROOT/scripts/microdroid-runtime-closure.sh" create \
  --product-root "$MICRODROID_PRODUCT" \
  --output-dir "$MICRODROID_CLOSURE" >/dev/null
"$ROOT/scripts/microdroid-runtime-closure.sh" verify \
  --closure "$MICRODROID_CLOSURE" >/dev/null
CLOSURE_FILE_COUNT=$(plutil -extract runtime_file_count raw -o - \
  "$MICRODROID_CLOSURE/runtime-closure-v2.plist")
CLOSURE_MANIFEST_DIGEST=$(plutil -extract runtime_manifest_sha256 raw -o - \
  "$MICRODROID_CLOSURE/runtime-closure-v2.plist")
if [ "$DEVELOPMENT_PACKAGE" -eq 1 ]; then
  PAYLOAD_CHANNEL=development
else
  PAYLOAD_CHANNEL=release
fi
if [ -n "$MICRODROID_PAYLOAD_SIGNER_SHA256" ]; then
  JAVA_HOME="$RELEASE_JAVA_HOME" PATH="$RELEASE_JAVA_HOME/bin:/usr/bin:/bin" \
    "$ROOT/scripts/microdroid-payload-bundle.sh" verify \
    --bundle "$MICRODROID_PAYLOAD_BUNDLE" \
    --apksigner "$APKSIGNER" \
    --require-channel "$PAYLOAD_CHANNEL" \
    --expected-signer-sha256 "$MICRODROID_PAYLOAD_SIGNER_SHA256" >/dev/null
else
  JAVA_HOME="$RELEASE_JAVA_HOME" PATH="$RELEASE_JAVA_HOME/bin:/usr/bin:/bin" \
    "$ROOT/scripts/microdroid-payload-bundle.sh" verify \
    --bundle "$MICRODROID_PAYLOAD_BUNDLE" \
    --apksigner "$APKSIGNER" \
    --require-channel "$PAYLOAD_CHANNEL" >/dev/null
fi
mkdir -p "$RESOURCES/provenance/toolchain"
ditto "$TOOLCHAIN_EVIDENCE" "$RESOURCES/provenance/toolchain"
TOOLCHAIN_IDENTITY="$RESOURCES/provenance/toolchain/toolchain-identity-v1.plist"
TOOLCHAIN_PROFILE=$(plutil -extract profile raw -o - "$TOOLCHAIN_IDENTITY")
WEB_DIST_MANIFEST_DIGEST=$(plutil -extract web_dist_manifest_sha256 raw -o - \
  "$TOOLCHAIN_IDENTITY")
PAYLOAD_RESOURCES="$RESOURCES/products/microdroid/conformance-payload"
mkdir -p "$PAYLOAD_RESOURCES"
copy_data "$MICRODROID_PAYLOAD_BUNDLE/payload.apk" "$PAYLOAD_RESOURCES/payload.apk"
copy_data "$MICRODROID_PAYLOAD_BUNDLE/payload-bundle-v1.plist" \
  "$PAYLOAD_RESOURCES/payload-bundle-v1.plist"
PAYLOAD_VERSION=$(plutil -extract version raw -o - \
  "$MICRODROID_PAYLOAD_BUNDLE/payload-bundle-v1.plist")
PAYLOAD_DIGEST=$(plutil -extract sha256 raw -o - \
  "$MICRODROID_PAYLOAD_BUNDLE/payload-bundle-v1.plist")
if [ -n "$RELEASE_MATERIALS" ]; then
  mkdir -p "$RESOURCES/release"
  copy_data "$RELEASE_MATERIALS/trusted-keys-v2.json" "$RESOURCES/release/trusted-keys-v2.json"
  ditto "$RELEASE_MATERIALS/certifications" "$RESOURCES/release/certifications"
fi
copy_exec "$ROOT/scripts/macos-network-setup.sh" "$RESOURCES/scripts/macos-network-setup.sh"
copy_data "$ROOT/README.md" "$RESOURCES/legal/README.md"
copy_data "$ROOT/LICENSE" "$RESOURCES/legal/LICENSE"
copy_data "$ROOT/AGENTS.md" "$RESOURCES/legal/AGENTS.md"

# Finder metadata is host-local noise. It must not affect the release identity or enter a signed
# application bundle, regardless of which input tree introduced it.
find "$APP" -type f \( -name '.DS_Store' -o -name '._*' \) -delete
if find "$APP" -type f \( -name '.DS_Store' -o -name '._*' \) -print -quit | grep -q .; then
  echo "failed to remove Finder metadata from staged application" >&2
  exit 1
fi

cat > "$CONTENTS/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "https://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDisplayName</key><string>HD</string>
  <key>CFBundleExecutable</key><string>HD</string>
  <key>CFBundleIdentifier</key><string>com.bscp.hd</string>
  <key>CFBundleName</key><string>HD</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$BUILD</string>
  <key>LSMinimumSystemVersion</key><string>15.0</string>
  <key>NSMicrophoneUsageDescription</key><string>HD uses the microphone only for instances where Host microphone input is explicitly enabled.</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
EOF
plutil -lint "$CONTENTS/Info.plist"

CROSVM_ENTITLEMENTS="$STAGE/crosvm-entitlements.plist"
cat > "$CROSVM_ENTITLEMENTS" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "https://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>com.apple.security.cs.allow-unsigned-executable-memory</key><true/>
  <key>com.apple.security.cs.disable-library-validation</key><true/>
  <key>com.apple.security.hypervisor</key><true/>
</dict>
</plist>
EOF
plutil -lint "$CROSVM_ENTITLEMENTS"

sign_file() {
  if [ "$IDENTITY" = "-" ]; then
    codesign --force --sign - "$1"
  else
    codesign --force --options runtime --timestamp --sign "$IDENTITY" "$1"
  fi
}

# The certificate binds these deterministic pre-signing source manifests. Developer ID then seals
# the manifests and every final resource, including signatures and the complete product tree.
"$ROOT/scripts/microdroid-release-identity.sh" \
  --runtime-dir "$RUNTIME_DIR" \
  --product-root "$MICRODROID_CLOSURE" \
  --output-dir "$RESOURCES/products/microdroid" >/dev/null
IDENTITY_JSON="$RESOURCES/products/microdroid/runtime-identity-v2.json"
GUEST_DIGEST=$(sed -n 's/.*"guest_digest": "\([0-9a-f]*\)".*/\1/p' "$IDENTITY_JSON")
HOST_DIGEST=$(sed -n 's/.*"host_digest": "\([0-9a-f]*\)".*/\1/p' "$IDENTITY_JSON")
if [ "${#GUEST_DIGEST}" -ne 64 ] || [ "${#HOST_DIGEST}" -ne 64 ]; then
  echo "generated Microdroid identity contains invalid digests" >&2
  exit 1
fi
if [ "$DEVELOPMENT_PACKAGE" -eq 0 ]; then
  MICRODROID_CERTIFICATION="$RESOURCES/release/certifications/macos-arm64-$GUEST_DIGEST-$HOST_DIGEST.json"
  if [ ! -f "$MICRODROID_CERTIFICATION" ]; then
    echo "release materials do not contain the exact Microdroid certification: $MICRODROID_CERTIFICATION" >&2
    exit 2
  fi
fi

for path in "$MACOS"/*; do
  [ -f "$path" ] || continue
  if file "$path" | grep -q 'Mach-O'; then
    component=$(basename -- "$path")
    if [ "$component" = HD ]; then
      continue
    elif [ "$component" = crosvm ]; then
      if [ "$IDENTITY" = "-" ]; then
        codesign --force --sign - --entitlements "$CROSVM_ENTITLEMENTS" "$path"
      else
        codesign --force --options runtime --timestamp --sign "$IDENTITY" \
          --entitlements "$CROSVM_ENTITLEMENTS" "$path"
      fi
    else
      sign_file "$path"
    fi
  fi
done
sign_file "$APP"
codesign --verify --deep --strict --verbose=2 "$APP"

# Prove the packaged native tools can reach their private runtime dependencies without a DYLD
# environment escape hatch. This catches a newly built crosvm that omitted @executable_path even
# when its dylib install names are otherwise correct.
env -i PATH=/usr/bin:/bin "$MACOS/crosvm" --help >/dev/null
env -i PATH=/usr/bin:/bin "$MACOS/vm" --help >/dev/null

for dependency in libbinder-rpc.1.0.0.dylib libbinder-rpc.1.dylib libgfxstream_backend.dylib \
  libvulkan.dylib libGLESv2.dylib libEGL.dylib; do
  require_file "$MACOS/$dependency"
done
otool -L "$MACOS/crosvm" > "$STAGE/crosvm-otool.txt"
grep -q '@rpath/libgfxstream_backend.dylib' "$STAGE/crosvm-otool.txt"

mv "$APP" "$OUTPUT"
trap - EXIT HUP INT TERM
cleanup

if [ -n "$NOTARY_PROFILE" ]; then
  ARCHIVE="$OUTPUT_PARENT/HD-macos-arm64-notarization.zip"
  ditto -c -k --keepParent "$OUTPUT" "$ARCHIVE"
  xcrun notarytool submit "$ARCHIVE" --keychain-profile "$NOTARY_PROFILE" --wait
  xcrun stapler staple "$OUTPUT"
  spctl --assess --type execute --verbose=4 "$OUTPUT"
  codesign --verify --deep --strict --verbose=2 "$OUTPUT"
fi

CHECKSUMS="$OUTPUT_PARENT/$(basename -- "$OUTPUT").sha256"
CHECKSUMS_STAGE=$(mktemp "$OUTPUT_PARENT/.hd-app-checksums.XXXXXX")
cleanup_checksums() {
  rm -f -- "$CHECKSUMS_STAGE"
}
trap cleanup_checksums EXIT HUP INT TERM
ANDROID_RUNTIME_MANIFEST="$OUTPUT/Contents/Resources/products/android/vsoc_arm64_only/direct-linux/runtime-files-v1.sha256"
(
  cd "$OUTPUT"
  find . -type f -print | LC_ALL=C sort | while IFS= read -r relative; do
    if [ -n "${ANDROID_SIGNED_ROOTFS_APP_RELATIVE:-}" ] &&
        [ "$relative" = "$ANDROID_SIGNED_ROOTFS_APP_RELATIVE" ]; then
      digest=$ANDROID_SIGNED_ROOTFS_SHA256
    else
      case "$relative" in
      ./Contents/Resources/products/android/vsoc_arm64_only/direct-linux/aggregate_android.img|\
      ./Contents/Resources/products/android/vsoc_arm64_only/direct-linux/android_fstab.dt|\
      ./Contents/Resources/products/android/vsoc_arm64_only/direct-linux/initrd_android.img|\
      ./Contents/Resources/products/android/vsoc_arm64_only/direct-linux/kernel)
        name=$(basename -- "$relative")
        digest=$(sed -n "s/^\\([0-9a-f][0-9a-f]*\\)  $name\$/\\1/p" \
          "$ANDROID_RUNTIME_MANIFEST")
        [ "${#digest}" -eq 64 ] ||
          { echo "invalid packaged Android runtime digest for $name" >&2; exit 1; }
        ;;
      *) digest=$(shasum -a 256 "$relative" | awk '{print $1}') ;;
      esac
    fi
    printf '%s  %s\n' "$digest" "$relative"
  done
) > "$CHECKSUMS_STAGE"
chmod 644 "$CHECKSUMS_STAGE"
mv "$CHECKSUMS_STAGE" "$CHECKSUMS"
trap - EXIT HUP INT TERM

echo "app=$OUTPUT"
echo "checksums=$CHECKSUMS"
echo "checksum_paths=app_relative"
echo "microdroid_guest_digest=$GUEST_DIGEST"
echo "microdroid_host_digest=$HOST_DIGEST"
echo "microdroid_runtime_file_count=$CLOSURE_FILE_COUNT"
echo "microdroid_runtime_manifest_sha256=$CLOSURE_MANIFEST_DIGEST"
echo "microdroid_payload_version=$PAYLOAD_VERSION"
echo "microdroid_payload_sha256=$PAYLOAD_DIGEST"
echo "release_toolchain_profile=$TOOLCHAIN_PROFILE"
echo "web_dist_manifest_sha256=$WEB_DIST_MANIFEST_DIGEST"
if [ "$DEVELOPMENT_PACKAGE" -eq 1 ]; then
  echo "channel=development"
else
  echo "channel=release"
fi
