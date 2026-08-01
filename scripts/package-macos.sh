#!/bin/sh
set -eu

usage() {
  cat <<'EOF'
Usage: scripts/package-macos.sh \
  --target-dir <cargo-release-dir> \
  --runtime-dir <out/dist/macos> \
  --web-dist <web/dist> \
  --adb <adb-binary> \
  --aapt2 <aapt2-binary> \
  --output <HD.app> \
  [--identity <Developer-ID-Application-or->] \
  [--notary-profile <keychain-profile>] \
  [--version <short-version>] [--build <bundle-version>]
EOF
}

TARGET_DIR=
RUNTIME_DIR=
WEB_DIST=
ADB=
AAPT2=
OUTPUT=
IDENTITY=-
NOTARY_PROFILE=
VERSION=0.1.0
BUILD=1

while [ "$#" -gt 0 ]; do
  case "$1" in
    --target-dir) TARGET_DIR=$2; shift 2 ;;
    --runtime-dir) RUNTIME_DIR=$2; shift 2 ;;
    --web-dist) WEB_DIST=$2; shift 2 ;;
    --adb) ADB=$2; shift 2 ;;
    --aapt2) AAPT2=$2; shift 2 ;;
    --output) OUTPUT=$2; shift 2 ;;
    --identity) IDENTITY=$2; shift 2 ;;
    --notary-profile) NOTARY_PROFILE=$2; shift 2 ;;
    --version) VERSION=$2; shift 2 ;;
    --build) BUILD=$2; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

for value in TARGET_DIR RUNTIME_DIR WEB_DIST ADB AAPT2 OUTPUT; do
  case "$value" in
    TARGET_DIR) candidate=$TARGET_DIR ;;
    RUNTIME_DIR) candidate=$RUNTIME_DIR ;;
    WEB_DIST) candidate=$WEB_DIST ;;
    ADB) candidate=$ADB ;;
    AAPT2) candidate=$AAPT2 ;;
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
if [ -e "$OUTPUT" ]; then
  echo "refusing to replace existing output: $OUTPUT" >&2
  exit 2
fi
if [ -n "$NOTARY_PROFILE" ] && [ "$IDENTITY" = "-" ]; then
  echo "notarization requires a Developer ID Application identity" >&2
  exit 2
fi

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)
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
copy_exec "$ADB" "$MACOS/adb"
copy_exec "$AAPT2" "$MACOS/aapt2"
copy_data "$RUNTIME_DIR/lib/libgfxstream_backend.dylib" "$MACOS/libgfxstream_backend.dylib"
copy_data "$RUNTIME_DIR/lib/libvulkan.dylib" "$MACOS/libvulkan.dylib"
copy_data "$RUNTIME_DIR/bin/libGLESv2.dylib" "$MACOS/libGLESv2.dylib"
copy_data "$RUNTIME_DIR/bin/libEGL.dylib" "$MACOS/libEGL.dylib"

require_file "$WEB_DIST/index.html"
ditto "$WEB_DIST" "$RESOURCES/ui"
copy_exec "$ROOT/scripts/macos-network-setup.sh" "$RESOURCES/scripts/macos-network-setup.sh"
copy_data "$ROOT/README.md" "$RESOURCES/legal/README.md"
copy_data "$ROOT/LICENSE" "$RESOURCES/legal/LICENSE"
copy_data "$ROOT/AGENTS.md" "$RESOURCES/legal/AGENTS.md"

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

for dependency in libgfxstream_backend.dylib libvulkan.dylib libGLESv2.dylib libEGL.dylib; do
  require_file "$MACOS/$dependency"
done
otool -L "$MACOS/crosvm" > "$STAGE/crosvm-otool.txt"
grep -q '@rpath/libgfxstream_backend.dylib' "$STAGE/crosvm-otool.txt"

mv "$APP" "$OUTPUT"
trap - EXIT HUP INT TERM
cleanup

CHECKSUMS="$OUTPUT_PARENT/$(basename -- "$OUTPUT").sha256"
find "$OUTPUT" -type f -exec shasum -a 256 {} + | sort -k 2 > "$CHECKSUMS"

if [ -n "$NOTARY_PROFILE" ]; then
  ARCHIVE="$OUTPUT_PARENT/HD-macos-arm64-notarization.zip"
  ditto -c -k --keepParent "$OUTPUT" "$ARCHIVE"
  xcrun notarytool submit "$ARCHIVE" --keychain-profile "$NOTARY_PROFILE" --wait
  xcrun stapler staple "$OUTPUT"
  spctl --assess --type execute --verbose=4 "$OUTPUT"
  codesign --verify --deep --strict --verbose=2 "$OUTPUT"
fi

echo "app=$OUTPUT"
echo "checksums=$CHECKSUMS"
