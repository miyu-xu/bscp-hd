#!/bin/sh
set -eu

usage() {
  cat <<'EOF'
Usage:
  scripts/macos-release-toolchain.sh verify-tools \
    --node-root <node-v22.23.1-darwin-arm64> \
    --node-archive <node-v22.23.1-darwin-arm64.tar.gz> \
    --java-home <Temurin-21.0.12+8/Contents/Home> \
    --java-archive <OpenJDK21U-jdk_aarch64_mac_hotspot_21.0.12_8.tar.gz> \
    --android-build-tools <android-sdk/build-tools/36.0.0>

  scripts/macos-release-toolchain.sh build-web \
    <the verify-tools arguments above> \
    --web-root <hd/web> \
    --output <fresh-web-dist> \
    --evidence-dir <fresh-evidence-dir>

  scripts/macos-release-toolchain.sh verify-evidence \
    <the verify-tools arguments above> \
    --web-dist <web-dist> \
    --evidence-dir <evidence-dir>
EOF
}

PROFILE=hd-macos-arm64-release-toolchain-v1
NODE_VERSION=v22.23.1
NPM_VERSION=10.9.8
NODE_ARCHIVE_SHA256=ef28d8fab2c0e4314522d4bb1b7173270aa3937e93b92cb7de79c112ac1fa953
NODE_BINARY_SHA256=2e3f1286a7eb3736346ed1803e458a0ff909e2b2d5bc746144dcb76970e9b99d
NPM_CLI_SHA256=8e5f6f3429f8cdbe693cdc29904e9d5a7b127a494bd15c804bd54c7403bfcbe7
JAVA_VERSION=21.0.12+8
JAVA_ARCHIVE_SHA256=021d629349ebc12a409faa517b837ec80ceee8f58a5ac85c788ecad07ca6881c
JAVA_BINARY_SHA256=34b9c157bedcebafc6033b8beaa72c2ff14e2b697e33f45aa959a8373d6581a0
JAVA_RELEASE_SHA256=9f83d93dbf2ea22bc92e88cc81e537d404e133a15f866a128f1cf5af50817e15
JAVA_LIBJLI_SHA256=ff515e7ee7a0487fc9072d168eaf7bdb95da9348cda1f165cc26574b16c08e65
JAVA_LIBJVM_SHA256=0e3dc9a33266678c4befcb015939a12b468faba0ca3f79fbfc80c288375a3155
JAVA_MODULES_SHA256=a7f216b5c4f946d84f4c64aaa921f30f2c8aaf2c12feddf1058e3b5ad01ac858
ANDROID_BUILD_TOOLS_VERSION=36.0.0
APKSIGNER_VERSION=0.9
ANDROID_SOURCE_PROPERTIES_SHA256=7dee6632e9ad6cb111da2bb99d747211e27927061b1276d040bb1d71fded5ebb
APKSIGNER_SHA256=b47549e373b895ce6ca620d0c7887e674d9615ffa837a86ac601dcfd04adb0f0
APKSIGNER_JAR_SHA256=3716d9311e55d2b0918a2fd9d54ba9e406c5f6abeea700b287f11259bc163dec
ZIPALIGN_SHA256=0427144f4a3fd242c5a159e7088637082539ae556bc1d2bbc2032bb775d47cea
AAPT2_SHA256=a8844d4089b442b034aed8953deee1893253053c900e03141ae7173e3edd8157

COMMAND=${1:-}
[ -n "$COMMAND" ] || { usage >&2; exit 2; }
shift

NODE_ROOT=
NODE_ARCHIVE=
JAVA_HOME_INPUT=
JAVA_ARCHIVE=
ANDROID_BUILD_TOOLS=
WEB_ROOT=
WEB_DIST=
OUTPUT=
EVIDENCE_DIR=

while [ "$#" -gt 0 ]; do
  case "$1" in
    --node-root) NODE_ROOT=$2; shift 2 ;;
    --node-archive) NODE_ARCHIVE=$2; shift 2 ;;
    --java-home) JAVA_HOME_INPUT=$2; shift 2 ;;
    --java-archive) JAVA_ARCHIVE=$2; shift 2 ;;
    --android-build-tools) ANDROID_BUILD_TOOLS=$2; shift 2 ;;
    --web-root) WEB_ROOT=$2; shift 2 ;;
    --web-dist) WEB_DIST=$2; shift 2 ;;
    --output) OUTPUT=$2; shift 2 ;;
    --evidence-dir) EVIDENCE_DIR=$2; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

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

sha256() {
  shasum -a 256 "$1" | awk '{print $1}'
}

require_sha256() {
  actual=$(sha256 "$1")
  [ "$actual" = "$2" ] || fail "digest mismatch for $1: expected $2, found $actual"
}

require_plist_value() {
  actual=$(plutil -extract "$2" raw -o - "$1" 2>/dev/null) ||
    fail "missing or invalid plist key $2 in $1"
  [ "$actual" = "$3" ] ||
    fail "unexpected $2 in $1: expected $3, found $actual"
}

tree_manifest() {
  root=$1
  output=$2
  require_abs_dir "$root" tree-root
  if find "$root" \( -type l -o \( ! -type d ! -type f \) \) -print -quit | grep -q .; then
    fail "tree contains a symlink or special file: $root"
  fi
  if find "$root" -type f \( -name '.DS_Store' -o -name '._*' \) -print -quit | grep -q .; then
    fail "tree contains Finder metadata: $root"
  fi
  paths=$(mktemp)
  (
    cd "$root"
    find . -type f -print | sed 's#^\./##' | LC_ALL=C sort
  ) > "$paths"
  : > "$output"
  while IFS= read -r relative; do
    [ -n "$relative" ] || continue
    case "$relative" in
      /*|../*|*/../*|*/..|.|..) fail "unsafe relative path in tree: $relative" ;;
    esac
    digest=$(sha256 "$root/$relative")
    printf '%s  %s\n' "$digest" "$relative" >> "$output"
  done < "$paths"
  rm -f -- "$paths"
}

verify_tools() {
  require_abs_dir "$NODE_ROOT" --node-root
  require_abs_file "$NODE_ARCHIVE" --node-archive
  require_abs_dir "$JAVA_HOME_INPUT" --java-home
  require_abs_file "$JAVA_ARCHIVE" --java-archive
  require_abs_dir "$ANDROID_BUILD_TOOLS" --android-build-tools
  [ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ] ||
    fail "release toolchain requires an Apple Silicon macOS host"

  require_abs_file "$NODE_ROOT/bin/node" node
  require_abs_file "$NODE_ROOT/lib/node_modules/npm/bin/npm-cli.js" npm-cli
  require_sha256 "$NODE_ARCHIVE" "$NODE_ARCHIVE_SHA256"
  require_sha256 "$NODE_ROOT/bin/node" "$NODE_BINARY_SHA256"
  require_sha256 "$NODE_ROOT/lib/node_modules/npm/bin/npm-cli.js" "$NPM_CLI_SHA256"
  actual_node=$("$NODE_ROOT/bin/node" --version)
  [ "$actual_node" = "$NODE_VERSION" ] ||
    fail "unexpected Node version: expected $NODE_VERSION, found $actual_node"
  actual_npm=$(PATH="$NODE_ROOT/bin:/usr/bin:/bin" "$NODE_ROOT/bin/npm" --version)
  [ "$actual_npm" = "$NPM_VERSION" ] ||
    fail "unexpected npm version: expected $NPM_VERSION, found $actual_npm"
  file "$NODE_ROOT/bin/node" | grep -q 'arm64' || fail "Node is not arm64"

  require_abs_file "$JAVA_HOME_INPUT/bin/java" java
  require_abs_file "$JAVA_HOME_INPUT/release" java-release
  require_abs_file "$JAVA_HOME_INPUT/lib/libjli.dylib" java-libjli
  require_abs_file "$JAVA_HOME_INPUT/lib/server/libjvm.dylib" java-libjvm
  require_abs_file "$JAVA_HOME_INPUT/lib/modules" java-modules
  require_sha256 "$JAVA_ARCHIVE" "$JAVA_ARCHIVE_SHA256"
  require_sha256 "$JAVA_HOME_INPUT/bin/java" "$JAVA_BINARY_SHA256"
  require_sha256 "$JAVA_HOME_INPUT/release" "$JAVA_RELEASE_SHA256"
  require_sha256 "$JAVA_HOME_INPUT/lib/libjli.dylib" "$JAVA_LIBJLI_SHA256"
  require_sha256 "$JAVA_HOME_INPUT/lib/server/libjvm.dylib" "$JAVA_LIBJVM_SHA256"
  require_sha256 "$JAVA_HOME_INPUT/lib/modules" "$JAVA_MODULES_SHA256"
  file "$JAVA_HOME_INPUT/bin/java" | grep -q 'arm64' || fail "Java is not arm64"
  java_version=$("$JAVA_HOME_INPUT/bin/java" -version 2>&1 | sed -n '1p')
  [ "$java_version" = 'openjdk version "21.0.12" 2026-07-21 LTS' ] ||
    fail "unexpected Java version: $java_version"

  require_abs_file "$ANDROID_BUILD_TOOLS/source.properties" source.properties
  require_abs_file "$ANDROID_BUILD_TOOLS/apksigner" apksigner
  require_abs_file "$ANDROID_BUILD_TOOLS/lib/apksigner.jar" apksigner.jar
  require_abs_file "$ANDROID_BUILD_TOOLS/zipalign" zipalign
  require_abs_file "$ANDROID_BUILD_TOOLS/aapt2" aapt2
  require_sha256 "$ANDROID_BUILD_TOOLS/source.properties" "$ANDROID_SOURCE_PROPERTIES_SHA256"
  require_sha256 "$ANDROID_BUILD_TOOLS/apksigner" "$APKSIGNER_SHA256"
  require_sha256 "$ANDROID_BUILD_TOOLS/lib/apksigner.jar" "$APKSIGNER_JAR_SHA256"
  require_sha256 "$ANDROID_BUILD_TOOLS/zipalign" "$ZIPALIGN_SHA256"
  require_sha256 "$ANDROID_BUILD_TOOLS/aapt2" "$AAPT2_SHA256"
  grep -qx "Pkg.Revision=$ANDROID_BUILD_TOOLS_VERSION" \
    "$ANDROID_BUILD_TOOLS/source.properties" ||
    fail "Android build-tools revision is not $ANDROID_BUILD_TOOLS_VERSION"
  JAVA_HOME="$JAVA_HOME_INPUT" PATH="$JAVA_HOME_INPUT/bin:/usr/bin:/bin" \
    "$ANDROID_BUILD_TOOLS/apksigner" version |
    grep -qx "$APKSIGNER_VERSION" ||
    fail "apksigner did not report version $APKSIGNER_VERSION"
}

write_identity() {
  identity=$1
  source_manifest_sha=$2
  source_file_count=$3
  dist_manifest_sha=$4
  dist_file_count=$5
  lock_sha=$6
  cat > "$identity" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "https://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>schema_version</key><integer>1</integer>
  <key>profile</key><string>$PROFILE</string>
  <key>platform</key><string>macos</string>
  <key>architecture</key><string>arm64</string>
  <key>node_version</key><string>$NODE_VERSION</string>
  <key>node_archive_sha256</key><string>$NODE_ARCHIVE_SHA256</string>
  <key>node_binary_sha256</key><string>$NODE_BINARY_SHA256</string>
  <key>npm_version</key><string>$NPM_VERSION</string>
  <key>npm_cli_sha256</key><string>$NPM_CLI_SHA256</string>
  <key>java_version</key><string>$JAVA_VERSION</string>
  <key>java_archive_sha256</key><string>$JAVA_ARCHIVE_SHA256</string>
  <key>java_modules_sha256</key><string>$JAVA_MODULES_SHA256</string>
  <key>android_build_tools_version</key><string>$ANDROID_BUILD_TOOLS_VERSION</string>
  <key>apksigner_version</key><string>$APKSIGNER_VERSION</string>
  <key>apksigner_jar_sha256</key><string>$APKSIGNER_JAR_SHA256</string>
  <key>zipalign_sha256</key><string>$ZIPALIGN_SHA256</string>
  <key>aapt2_sha256</key><string>$AAPT2_SHA256</string>
  <key>web_lock_sha256</key><string>$lock_sha</string>
  <key>web_source_manifest_sha256</key><string>$source_manifest_sha</string>
  <key>web_source_file_count</key><integer>$source_file_count</integer>
  <key>web_dist_manifest_sha256</key><string>$dist_manifest_sha</string>
  <key>web_dist_file_count</key><integer>$dist_file_count</integer>
</dict>
</plist>
EOF
  plutil -lint "$identity" >/dev/null
}

build_web() {
  verify_tools
  require_abs_dir "$WEB_ROOT" --web-root
  case "$OUTPUT" in /*) ;; *) fail "--output must be absolute" ;; esac
  case "$EVIDENCE_DIR" in /*) ;; *) fail "--evidence-dir must be absolute" ;; esac
  [ ! -e "$OUTPUT" ] || fail "refusing to replace web output: $OUTPUT"
  [ ! -e "$EVIDENCE_DIR" ] || fail "refusing to replace evidence: $EVIDENCE_DIR"

  for input in package.json package-lock.json index.html postcss.config.cjs \
    tailwind.config.cjs tsconfig.json vite.config.ts NOTICE.md; do
    require_abs_file "$WEB_ROOT/$input" "web/$input"
  done
  require_abs_dir "$WEB_ROOT/src" web/src

  parent=$(dirname -- "$OUTPUT")
  evidence_parent=$(dirname -- "$EVIDENCE_DIR")
  mkdir -p "$parent" "$evidence_parent"
  work=$(mktemp -d "$parent/.hd-web-build.XXXXXX")
  evidence_stage=$(mktemp -d "$evidence_parent/.hd-toolchain-evidence.XXXXXX")
  cleanup_build_web() {
    rm -rf -- "$work" "$evidence_stage"
  }
  trap cleanup_build_web EXIT HUP INT TERM

  for input in package.json package-lock.json index.html postcss.config.cjs \
    tailwind.config.cjs tsconfig.json vite.config.ts NOTICE.md; do
    install -m 644 "$WEB_ROOT/$input" "$work/$input"
  done
  ditto "$WEB_ROOT/src" "$work/src"
  find "$work" -type f \( -name '.DS_Store' -o -name '._*' \) -delete
  tree_manifest "$work" "$evidence_stage/web-source-v1.sha256"
  source_manifest_sha=$(sha256 "$evidence_stage/web-source-v1.sha256")
  source_file_count=$(wc -l < "$evidence_stage/web-source-v1.sha256" | tr -d ' ')
  lock_sha=$(sha256 "$work/package-lock.json")

  npm_home="$work/.home"
  npm_cache="$work/.npm-cache"
  mkdir -p "$npm_home" "$npm_cache"
  (
    cd "$work"
    env -i \
      HOME="$npm_home" \
      PATH="$NODE_ROOT/bin:/usr/bin:/bin" \
      TMPDIR="${TMPDIR:-/tmp}" \
      npm_config_audit=false \
      npm_config_cache="$npm_cache" \
      npm_config_fund=false \
      npm_config_update_notifier=false \
      "$NODE_ROOT/bin/npm" ci
    env -i \
      HOME="$npm_home" \
      PATH="$NODE_ROOT/bin:/usr/bin:/bin" \
      TMPDIR="${TMPDIR:-/tmp}" \
      "$NODE_ROOT/bin/npm" run build
  )
  rm -rf -- "$work/node_modules" "$npm_home" "$npm_cache"
  [ -f "$work/dist/index.html" ] || fail "Web build did not produce dist/index.html"
  tree_manifest "$work/dist" "$evidence_stage/web-dist-v1.sha256"
  dist_manifest_sha=$(sha256 "$evidence_stage/web-dist-v1.sha256")
  dist_file_count=$(wc -l < "$evidence_stage/web-dist-v1.sha256" | tr -d ' ')
  write_identity \
    "$evidence_stage/toolchain-identity-v1.plist" \
    "$source_manifest_sha" "$source_file_count" \
    "$dist_manifest_sha" "$dist_file_count" "$lock_sha"

  mv "$work/dist" "$OUTPUT"
  mv "$evidence_stage" "$EVIDENCE_DIR"
  trap - EXIT HUP INT TERM
  rm -rf -- "$work"
  echo "web_dist=$OUTPUT"
  echo "toolchain_evidence=$EVIDENCE_DIR"
  echo "web_source_manifest_sha256=$source_manifest_sha"
  echo "web_dist_manifest_sha256=$dist_manifest_sha"
}

verify_evidence() {
  verify_tools
  require_abs_dir "$WEB_DIST" --web-dist
  require_abs_dir "$EVIDENCE_DIR" --evidence-dir
  identity="$EVIDENCE_DIR/toolchain-identity-v1.plist"
  source_manifest="$EVIDENCE_DIR/web-source-v1.sha256"
  dist_manifest="$EVIDENCE_DIR/web-dist-v1.sha256"
  require_abs_file "$identity" toolchain-identity
  require_abs_file "$source_manifest" web-source-manifest
  require_abs_file "$dist_manifest" web-dist-manifest
  extra_count=$(find "$EVIDENCE_DIR" -mindepth 1 -maxdepth 1 -type f | wc -l | tr -d ' ')
  [ "$extra_count" = 3 ] || fail "toolchain evidence must contain exactly three regular files"
  if find "$EVIDENCE_DIR" -mindepth 1 -maxdepth 1 ! -type f -print -quit | grep -q .; then
    fail "toolchain evidence contains a non-regular entry"
  fi

  require_plist_value "$identity" schema_version 1
  require_plist_value "$identity" profile "$PROFILE"
  require_plist_value "$identity" platform macos
  require_plist_value "$identity" architecture arm64
  require_plist_value "$identity" node_version "$NODE_VERSION"
  require_plist_value "$identity" node_archive_sha256 "$NODE_ARCHIVE_SHA256"
  require_plist_value "$identity" node_binary_sha256 "$NODE_BINARY_SHA256"
  require_plist_value "$identity" npm_version "$NPM_VERSION"
  require_plist_value "$identity" npm_cli_sha256 "$NPM_CLI_SHA256"
  require_plist_value "$identity" java_version "$JAVA_VERSION"
  require_plist_value "$identity" java_archive_sha256 "$JAVA_ARCHIVE_SHA256"
  require_plist_value "$identity" java_modules_sha256 "$JAVA_MODULES_SHA256"
  require_plist_value "$identity" android_build_tools_version "$ANDROID_BUILD_TOOLS_VERSION"
  require_plist_value "$identity" apksigner_version "$APKSIGNER_VERSION"
  require_plist_value "$identity" apksigner_jar_sha256 "$APKSIGNER_JAR_SHA256"
  require_plist_value "$identity" zipalign_sha256 "$ZIPALIGN_SHA256"
  require_plist_value "$identity" aapt2_sha256 "$AAPT2_SHA256"
  require_plist_value "$identity" web_source_manifest_sha256 "$(sha256 "$source_manifest")"
  require_plist_value "$identity" web_source_file_count \
    "$(wc -l < "$source_manifest" | tr -d ' ')"

  actual_dist_manifest=$(mktemp)
  trap 'rm -f -- "$actual_dist_manifest"' EXIT HUP INT TERM
  tree_manifest "$WEB_DIST" "$actual_dist_manifest"
  cmp -s "$dist_manifest" "$actual_dist_manifest" ||
    fail "Web dist does not match its sealed manifest"
  require_plist_value "$identity" web_dist_manifest_sha256 "$(sha256 "$dist_manifest")"
  require_plist_value "$identity" web_dist_file_count \
    "$(wc -l < "$dist_manifest" | tr -d ' ')"
  rm -f -- "$actual_dist_manifest"
  trap - EXIT HUP INT TERM
  echo "toolchain_profile=$PROFILE"
  echo "web_dist_manifest_sha256=$(sha256 "$dist_manifest")"
}

case "$COMMAND" in
  verify-tools)
    verify_tools
    echo "toolchain_profile=$PROFILE"
    ;;
  build-web)
    build_web
    ;;
  verify-evidence)
    verify_evidence
    ;;
  *)
    echo "unknown command: $COMMAND" >&2
    usage >&2
    exit 2
    ;;
esac
