#!/bin/sh
set -eu

usage() {
  cat <<'EOF'
Usage:
  scripts/macos-release-distribution.sh create \
    --app <HD.app> \
    --app-checksums <HD.app.sha256> \
    --archive <fresh-HD-macos-arm64.tar.xz> \
    <pinned-toolchain-arguments>

  scripts/macos-release-distribution.sh verify \
    --archive <HD-macos-arm64.tar.xz> \
    [--gate-report <fresh-gate-report.json>] \
    <pinned-toolchain-arguments>

Pinned toolchain arguments:
  --node-root <node-v22.23.1-darwin-arm64>
  --node-archive <node-v22.23.1-darwin-arm64.tar.gz>
  --java-home <Temurin-21.0.12+8/Contents/Home>
  --java-archive <OpenJDK21U-jdk_aarch64_mac_hotspot_21.0.12_8.tar.gz>
  --android-build-tools <android-sdk/build-tools/36.0.0>

The archive sidecars are <archive>.sha256, <archive>.app.sha256 and
<archive>.distribution-v2.plist. Self-contained Android distributions must use
the sparse-preserving tar.xz format; zip remains accepted only for legacy apps
that contain no sparse Android aggregate.
EOF
}

COMMAND=${1:-}
[ -n "$COMMAND" ] || { usage >&2; exit 2; }
shift
case "$COMMAND" in
  -h|--help) usage; exit 0 ;;
esac

APP=
APP_CHECKSUMS_INPUT=
ARCHIVE=
NODE_ROOT=
NODE_ARCHIVE=
JAVA_HOME_INPUT=
JAVA_ARCHIVE=
ANDROID_BUILD_TOOLS=
GATE_REPORT=

while [ "$#" -gt 0 ]; do
  case "$1" in
    --app) APP=$2; shift 2 ;;
    --app-checksums) APP_CHECKSUMS_INPUT=$2; shift 2 ;;
    --archive) ARCHIVE=$2; shift 2 ;;
    --node-root) NODE_ROOT=$2; shift 2 ;;
    --node-archive) NODE_ARCHIVE=$2; shift 2 ;;
    --java-home) JAVA_HOME_INPUT=$2; shift 2 ;;
    --java-archive) JAVA_ARCHIVE=$2; shift 2 ;;
    --android-build-tools) ANDROID_BUILD_TOOLS=$2; shift 2 ;;
    --gate-report) GATE_REPORT=$2; shift 2 ;;
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

require_plist_value() {
  actual=$(plutil -extract "$2" raw -o - "$1" 2>/dev/null) ||
    fail "missing or invalid plist key $2 in $1"
  [ "$actual" = "$3" ] ||
    fail "unexpected $2 in $1: expected $3, found $actual"
}

tree_manifest() {
  root=$1
  output=$2
  reuse_relative=${3:-}
  reuse_digest=${4:-}
  require_abs_dir "$root" tree-root
  if find "$root" \( -type l -o \( ! -type d ! -type f \) \) -print -quit | grep -q .; then
    fail "tree contains a symlink or special file: $root"
  fi
  if find "$root" -type f \( -name '.DS_Store' -o -name '._*' \) -print -quit | grep -q .; then
    fail "tree contains Finder metadata: $root"
  fi
  (
    cd "$root"
    find . -type f -print | LC_ALL=C sort | while IFS= read -r relative; do
      case "$relative" in
        /*|../*|*/../*|*/..|.|..) fail "unsafe relative path in tree: $relative" ;;
      esac
      if [ -n "$reuse_relative" ] && [ "$relative" = "$reuse_relative" ]; then
        [ "${#reuse_digest}" -eq 64 ] || fail "invalid reused tree digest"
        digest=$reuse_digest
      else
        digest=$(shasum -a 256 "$relative" | awk '{print $1}')
      fi
      printf '%s  %s\n' "$digest" "$relative"
    done
  ) > "$output"
}

tree_logical_size() {
  find "$1" -type f -exec stat -f %z {} \; | awk '{total += $1} END {print total + 0}'
}

toolchain_args() {
  require_abs_dir "$NODE_ROOT" --node-root
  require_abs_file "$NODE_ARCHIVE" --node-archive
  require_abs_dir "$JAVA_HOME_INPUT" --java-home
  require_abs_file "$JAVA_ARCHIVE" --java-archive
  require_abs_dir "$ANDROID_BUILD_TOOLS" --android-build-tools
}

verify_signed_android() {
  app=$1
  app_checksums=$2
  signed_android=$3
  channel=$4
  trust_store=$5
  require_abs_dir "$signed_android" signed-android-artifact-store
  require_abs_file "$trust_store" signed-android-trust-store
  report=$(mktemp)
  if ! "$app/Contents/MacOS/hdctl" verify-android-artifact-store \
      --store-root "$signed_android" \
      --trust-store "$trust_store" \
      --channel "$channel" > "$report"; then
    rm -f -- "$report"
    fail "packaged signed Android artifact store verification failed"
  fi
  APP_ANDROID_PROFILE=$(plutil -extract data_profile raw -o - "$report") ||
    { rm -f -- "$report"; fail "signed Android report omitted data_profile"; }
  APP_ANDROID_AGGREGATE_SHA256=$(plutil -extract rootfs_sha256 raw -o - "$report") ||
    { rm -f -- "$report"; fail "signed Android report omitted rootfs_sha256"; }
  signed_guest_digest=$(plutil -extract guest_bundle_digest raw -o - "$report") ||
    { rm -f -- "$report"; fail "signed Android report omitted guest digest"; }
  signed_rootfs_relative=$(plutil -extract rootfs_relative_path raw -o - "$report") ||
    { rm -f -- "$report"; fail "signed Android report omitted rootfs path"; }
  rm -f -- "$report"
  APP_ANDROID_ROOTFS_PATH="$signed_android/bundles/$signed_guest_digest/$signed_rootfs_relative"
  [ "${#APP_ANDROID_AGGREGATE_SHA256}" -eq 64 ] ||
    fail "packaged signed Android rootfs digest is invalid"
  app_relative="./Contents/Resources/products/android/artifact-store-v2/bundles/$signed_guest_digest/$signed_rootfs_relative"
  grep -Fqx "$APP_ANDROID_AGGREGATE_SHA256  $app_relative" "$app_checksums" ||
    fail "signed Android rootfs manifest disagrees with the verified application tree"
}

verify_app() {
  app=$1
  app_checksums=$2
  require_abs_dir "$app" app
  require_abs_file "$app_checksums" app-checksums
  case "$app" in *.app) ;; *) fail "application path must end in .app" ;; esac
  [ "$(basename -- "$app")" = HD.app ] || fail "distribution application must be named HD.app"
  require_abs_file "$app/Contents/Info.plist" Info.plist

  payload="$app/Contents/Resources/products/microdroid/conformance-payload"
  require_abs_file "$payload/payload-bundle-v1.plist" payload-bundle
  APP_CHANNEL=$(plutil -extract channel raw -o - "$payload/payload-bundle-v1.plist")
  android_root="$app/Contents/Resources/products/android"
  android_marker="$android_root/development-direct-v1.plist"
  SIGNED_ANDROID_VERIFIED=0
  TREE_REUSE_RELATIVE=
  TREE_REUSE_DIGEST=
  if [ ! -e "$android_marker" ]; then
    signed_android="$android_root/artifact-store-v2"
    if [ "$APP_CHANNEL" = development ]; then
      signed_trust="$signed_android/trusted-keys-v2.json"
    else
      signed_trust="$app/Contents/Resources/release/trusted-keys-v2.json"
    fi
    verify_signed_android "$app" "$app_checksums" "$signed_android" \
      "$APP_CHANNEL" "$signed_trust"
    case "$APP_ANDROID_ROOTFS_PATH" in
      "$app"/*) TREE_REUSE_RELATIVE=".${APP_ANDROID_ROOTFS_PATH#"$app"}" ;;
      *) fail "signed Android rootfs escaped the application tree" ;;
    esac
    TREE_REUSE_DIGEST=$APP_ANDROID_AGGREGATE_SHA256
    SIGNED_ANDROID_VERIFIED=1
  fi

  actual_manifest=$(mktemp)
  tree_manifest "$app" "$actual_manifest" "$TREE_REUSE_RELATIVE" "$TREE_REUSE_DIGEST"
  cmp -s "$app_checksums" "$actual_manifest" ||
    fail "application tree does not match its portable checksum manifest"
  # tree_manifest already hashed every file and cmp proved byte-for-byte equality with the
  # supplied manifest. Running shasum -c here would read a sparse 16.5 GiB Android image twice.
  rm -f -- "$actual_manifest"

  codesign --verify --deep --strict --verbose=2 "$app" >/dev/null
  SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
  ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)
  closure="$app/Contents/Resources/products/microdroid/vsoc_arm64_only"
  "$ROOT/scripts/microdroid-runtime-closure.sh" verify --closure "$closure" >/dev/null
  "$ROOT/scripts/macos-release-toolchain.sh" verify-evidence \
    --node-root "$NODE_ROOT" \
    --node-archive "$NODE_ARCHIVE" \
    --java-home "$JAVA_HOME_INPUT" \
    --java-archive "$JAVA_ARCHIVE" \
    --android-build-tools "$ANDROID_BUILD_TOOLS" \
    --web-dist "$app/Contents/Resources/ui" \
    --evidence-dir "$app/Contents/Resources/provenance/toolchain" >/dev/null

  APP_PAYLOAD_SIGNER=$(plutil -extract signer_certificate_sha256 raw -o - \
    "$payload/payload-bundle-v1.plist")
  JAVA_HOME="$JAVA_HOME_INPUT" PATH="$JAVA_HOME_INPUT/bin:/usr/bin:/bin" \
    "$ROOT/scripts/microdroid-payload-bundle.sh" verify \
    --bundle "$payload" \
    --apksigner "$ANDROID_BUILD_TOOLS/apksigner" \
    --require-channel "$APP_CHANNEL" \
    --expected-signer-sha256 "$APP_PAYLOAD_SIGNER" >/dev/null

  if [ "$APP_CHANNEL" = development ] && [ -e "$android_marker" ]; then
    require_abs_file "$android_marker" android-development-marker
    require_plist_value "$android_marker" schema_version 1
    require_plist_value "$android_marker" channel development
    require_plist_value "$android_marker" guest_kind android
    require_plist_value "$android_marker" android_version 15.0.0_r14
    require_plist_value "$android_marker" data_profile development-unencrypted
    require_plist_value "$android_marker" mount_stage first_stage_mount
    android_direct="$android_root/vsoc_arm64_only/direct-linux"
    APP_ANDROID_ROOTFS_PATH="$android_direct/aggregate_android.img"
    require_abs_dir "$android_direct" android-direct-runtime
    android_manifest="$android_direct/runtime-files-v1.sha256"
    require_abs_file "$android_manifest" android-runtime-manifest
    [ "$(wc -l < "$android_manifest" | tr -d ' ')" = 4 ] ||
      fail "Android runtime manifest must contain exactly four files"
    for relative in aggregate_android.img android_fstab.dt initrd_android.img kernel; do
      require_abs_file "$android_direct/$relative" "android-$relative"
      grep -Eq "^[0-9a-f]{64}  $relative$" "$android_manifest" ||
        fail "Android runtime manifest is missing $relative"
      digest=$(sed -n "s/^\\([0-9a-f][0-9a-f]*\\)  $relative\$/\\1/p" "$android_manifest")
      app_relative="./Contents/Resources/products/android/vsoc_arm64_only/direct-linux/$relative"
      grep -Fqx "$digest  $app_relative" "$app_checksums" ||
        fail "Android runtime manifest disagrees with the verified application tree: $relative"
    done
    data_entries=$(awk '$2 == "/data" { count += 1 } END { print count + 0 }' \
      "$android_direct/android_fstab.dt")
    [ "$data_entries" = 1 ] ||
      fail "packaged Android fstab must contain exactly one /data entry"
    data_flags=$(awk '$2 == "/data" { print $5 }' "$android_direct/android_fstab.dt")
    case ",$data_flags," in
      *,first_stage_mount,*) ;;
      *) fail "packaged development Android /data must use first_stage_mount" ;;
    esac
    case ",$data_flags," in
      *,latemount,*|*,inlinecrypt,*|*,fileencryption=*|*,keydirectory=*)
        fail "packaged development Android /data must remain explicitly unencrypted"
        ;;
    esac
    APP_ANDROID_PROFILE=development-unencrypted
    APP_ANDROID_AGGREGATE_SHA256=$(sed -n \
      's/^\([0-9a-f][0-9a-f]*\)  aggregate_android\.img$/\1/p' "$android_manifest")
    [ "${#APP_ANDROID_AGGREGATE_SHA256}" -eq 64 ] ||
      fail "packaged Android aggregate digest is invalid"
  else
    [ ! -e "$android_marker" ] ||
      fail "signed Android application must not contain the direct development profile"
    [ "$SIGNED_ANDROID_VERIFIED" -eq 1 ] ||
      fail "signed Android artifact store was not verified"
  fi

  APP_VERSION=$(plutil -extract CFBundleShortVersionString raw -o - \
    "$app/Contents/Info.plist")
  APP_BUILD=$(plutil -extract CFBundleVersion raw -o - "$app/Contents/Info.plist")
  APP_FILE_COUNT=$(wc -l < "$app_checksums" | tr -d ' ')
  APP_LOGICAL_SIZE=$(tree_logical_size "$app")
  APP_MANIFEST_SHA256=$(sha256 "$app_checksums")
  identity="$app/Contents/Resources/products/microdroid/runtime-identity-v2.json"
  require_abs_file "$identity" microdroid-runtime-identity
  APP_GUEST_DIGEST=$(sed -n 's/.*"guest_digest": "\([0-9a-f]*\)".*/\1/p' "$identity")
  APP_HOST_DIGEST=$(sed -n 's/.*"host_digest": "\([0-9a-f]*\)".*/\1/p' "$identity")
  [ "${#APP_GUEST_DIGEST}" -eq 64 ] || fail "invalid packaged guest digest"
  [ "${#APP_HOST_DIGEST}" -eq 64 ] || fail "invalid packaged host digest"
  toolchain="$app/Contents/Resources/provenance/toolchain/toolchain-identity-v1.plist"
  APP_TOOLCHAIN_PROFILE=$(plutil -extract profile raw -o - "$toolchain")
  APP_WEB_DIST_DIGEST=$(plutil -extract web_dist_manifest_sha256 raw -o - "$toolchain")
}

write_metadata() (
  metadata=$1
  archive_digest=$2
  archive_size=$3
  cat > "$metadata" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "https://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>schema_version</key><integer>2</integer>
  <key>profile</key><string>hd-macos-arm64-distribution-v2</string>
  <key>archive_format</key><string>$ARCHIVE_FORMAT</string>
  <key>archive_sha256</key><string>$archive_digest</string>
  <key>archive_size</key><integer>$archive_size</integer>
  <key>app_version</key><string>$APP_VERSION</string>
  <key>app_build</key><string>$APP_BUILD</string>
  <key>app_channel</key><string>$APP_CHANNEL</string>
  <key>android_data_profile</key><string>$APP_ANDROID_PROFILE</string>
  <key>android_aggregate_sha256</key><string>$APP_ANDROID_AGGREGATE_SHA256</string>
  <key>app_file_count</key><integer>$APP_FILE_COUNT</integer>
  <key>app_logical_size</key><integer>$APP_LOGICAL_SIZE</integer>
  <key>app_manifest_sha256</key><string>$APP_MANIFEST_SHA256</string>
  <key>microdroid_guest_digest</key><string>$APP_GUEST_DIGEST</string>
  <key>microdroid_host_digest</key><string>$APP_HOST_DIGEST</string>
  <key>toolchain_profile</key><string>$APP_TOOLCHAIN_PROFILE</string>
  <key>web_dist_manifest_sha256</key><string>$APP_WEB_DIST_DIGEST</string>
</dict>
</plist>
EOF
  plutil -lint "$metadata" >/dev/null
)

write_gate_report() {
  [ -n "$GATE_REPORT" ] || return 0
  case "$GATE_REPORT" in /*.json) ;; *) fail "--gate-report must be an absolute .json path" ;; esac
  [ ! -e "$GATE_REPORT" ] || fail "refusing to replace gate report: $GATE_REPORT"
  gate_parent=$(dirname -- "$GATE_REPORT")
  mkdir -p "$gate_parent"
  gate_stage=$(mktemp "$gate_parent/.macos-distribution-gate.XXXXXX")
  generated_at=$(date -u '+%Y-%m-%dT%H:%M:%SZ')
  cat > "$gate_stage" <<EOF
{
  "schema_version": 2,
  "generated_at": "$generated_at",
  "source": "scripts/macos-release-distribution.sh",
  "gates": [
    {
      "name": "macos-release-toolchain-distribution",
      "command": "macos-release-distribution.sh verify --archive <candidate>",
      "status": "pass",
      "duration_ms": null,
      "log_path": null,
      "summary": "固定 arm64 Node/npm、Temurin 与 Android build-tools 摘要通过；归档 SHA-256、安全路径、稀疏 Android aggregate、相对路径应用清单、deep codesign、Microdroid v1 闭包、版本化 Payload 和 Web provenance 在独立解包位置全部复验。候选 ${APP_VERSION} build ${APP_BUILD}，${APP_FILE_COUNT} 个应用文件，archive ${archive_digest}。"
    }
  ]
}
EOF
  mv "$gate_stage" "$GATE_REPORT"
  echo "gate_report=$GATE_REPORT"
}

verify_archive() {
  require_abs_file "$ARCHIVE" archive
  archive_sha_file="$ARCHIVE.sha256"
  archive_app_checksums="$ARCHIVE.app.sha256"
  metadata="$ARCHIVE.distribution-v2.plist"
  require_abs_file "$archive_sha_file" archive-sha256
  require_abs_file "$archive_app_checksums" archive-app-sha256
  require_abs_file "$metadata" distribution-metadata

  archive_digest=$(sha256 "$ARCHIVE")
  archive_size=$(stat -f %z "$ARCHIVE")
  expected_checksum_line="$archive_digest  $(basename -- "$ARCHIVE")"
  actual_checksum_line=$(cat "$archive_sha_file")
  [ "$actual_checksum_line" = "$expected_checksum_line" ] ||
    fail "archive checksum sidecar does not match the archive"
  require_plist_value "$metadata" schema_version 2
  require_plist_value "$metadata" profile hd-macos-arm64-distribution-v2
  require_plist_value "$metadata" archive_format "$ARCHIVE_FORMAT"
  require_plist_value "$metadata" archive_sha256 "$archive_digest"
  require_plist_value "$metadata" archive_size "$archive_size"

  unpack_parent=$(dirname -- "$ARCHIVE")
  unpack=$(mktemp -d "$unpack_parent/.hd-distribution-verify.XXXXXX")
  cleanup_verify() {
    rm -rf -- "$unpack"
  }
  trap cleanup_verify EXIT HUP INT TERM
  archive_entries="$unpack.entries"
  if [ "$ARCHIVE_FORMAT" = tar-xz ]; then
    bsdtar -tJf "$ARCHIVE" > "$archive_entries"
  else
    zipinfo -1 "$ARCHIVE" > "$archive_entries"
  fi
  [ -s "$archive_entries" ] || fail "archive is empty"
  if ! awk '
    /^\// { exit 1 }
    /(^|\/)\.\.(\/|$)/ { exit 1 }
    $0 !~ /^HD[.]app\// { exit 1 }
    END { if (NR == 0) exit 1 }
  ' "$archive_entries"; then
    fail "archive contains an unsafe or unexpected path"
  fi
  if [ "$ARCHIVE_FORMAT" = tar-xz ]; then
    (cd "$unpack" && COPYFILE_DISABLE=1 bsdtar --safe-writes --no-xattrs -xJf "$ARCHIVE")
  else
    ditto -x -k "$ARCHIVE" "$unpack"
  fi
  rm -f -- "$archive_entries"
  [ -d "$unpack/HD.app" ] || fail "archive did not extract HD.app"
  top_count=$(find "$unpack" -mindepth 1 -maxdepth 1 -print | wc -l | tr -d ' ')
  [ "$top_count" = 1 ] || fail "archive extracted unexpected top-level entries"
  verify_app "$unpack/HD.app" "$archive_app_checksums"
  if [ -f "$APP_ANDROID_ROOTFS_PATH" ]; then
    [ "$ARCHIVE_FORMAT" = tar-xz ] ||
      fail "self-contained Android distributions require sparse-preserving tar.xz"
    logical=$(stat -f %z "$APP_ANDROID_ROOTFS_PATH")
    physical=$(($(stat -f %b "$APP_ANDROID_ROOTFS_PATH") * 512))
    [ "$physical" -lt "$logical" ] ||
      fail "archive extraction inflated the sparse Android aggregate"
  fi

  require_plist_value "$metadata" app_version "$APP_VERSION"
  require_plist_value "$metadata" app_build "$APP_BUILD"
  require_plist_value "$metadata" app_channel "$APP_CHANNEL"
  require_plist_value "$metadata" android_data_profile "$APP_ANDROID_PROFILE"
  require_plist_value "$metadata" android_aggregate_sha256 "$APP_ANDROID_AGGREGATE_SHA256"
  require_plist_value "$metadata" app_file_count "$APP_FILE_COUNT"
  require_plist_value "$metadata" app_logical_size "$APP_LOGICAL_SIZE"
  require_plist_value "$metadata" app_manifest_sha256 "$APP_MANIFEST_SHA256"
  require_plist_value "$metadata" microdroid_guest_digest "$APP_GUEST_DIGEST"
  require_plist_value "$metadata" microdroid_host_digest "$APP_HOST_DIGEST"
  require_plist_value "$metadata" toolchain_profile "$APP_TOOLCHAIN_PROFILE"
  require_plist_value "$metadata" web_dist_manifest_sha256 "$APP_WEB_DIST_DIGEST"
  trap - EXIT HUP INT TERM
  rm -rf -- "$unpack"
  echo "archive=$ARCHIVE"
  echo "archive_sha256=$archive_digest"
  echo "archive_size=$archive_size"
  echo "app_version=$APP_VERSION"
  echo "app_build=$APP_BUILD"
  echo "app_channel=$APP_CHANNEL"
  echo "android_data_profile=$APP_ANDROID_PROFILE"
  echo "android_aggregate_sha256=$APP_ANDROID_AGGREGATE_SHA256"
  echo "app_file_count=$APP_FILE_COUNT"
  echo "app_manifest_sha256=$APP_MANIFEST_SHA256"
  echo "microdroid_guest_digest=$APP_GUEST_DIGEST"
  echo "microdroid_host_digest=$APP_HOST_DIGEST"
  echo "toolchain_profile=$APP_TOOLCHAIN_PROFILE"
  echo "web_dist_manifest_sha256=$APP_WEB_DIST_DIGEST"
}

toolchain_args
[ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ] ||
  fail "macOS distribution requires an Apple Silicon macOS host"
case "$ARCHIVE" in
  /*.tar.xz) ARCHIVE_FORMAT=tar-xz ;;
  /*.zip) ARCHIVE_FORMAT=zip ;;
  *) fail "--archive must be an absolute .tar.xz or .zip path" ;;
esac

case "$COMMAND" in
  create)
    require_abs_dir "$APP" --app
    require_abs_file "$APP_CHECKSUMS_INPUT" --app-checksums
    archive_sha_file="$ARCHIVE.sha256"
    archive_app_checksums="$ARCHIVE.app.sha256"
    metadata="$ARCHIVE.distribution-v2.plist"
    for output in "$ARCHIVE" "$archive_sha_file" "$archive_app_checksums" "$metadata"; do
      [ ! -e "$output" ] || fail "refusing to replace distribution output: $output"
    done
    verify_app "$APP" "$APP_CHECKSUMS_INPUT"
    output_parent=$(dirname -- "$ARCHIVE")
    mkdir -p "$output_parent"
    # BSD mktemp only replaces a trailing X run. A suffix after XXXXXX creates a predictable
    # literal filename on macOS and is unsafe under concurrent release jobs.
    archive_stage=$(mktemp "$output_parent/.hd-distribution.XXXXXX")
    cleanup_create() {
      rm -f -- "$archive_stage"
    }
    trap cleanup_create EXIT HUP INT TERM
    if [ "$ARCHIVE_FORMAT" = tar-xz ]; then
      (
        cd "$(dirname -- "$APP")"
        # xz level 3 materially shortens release and install feedback loops for the already
        # filesystem-compressed Android image while retaining a compact, deterministic archive.
        COPYFILE_DISABLE=1 bsdtar --options xz:compression-level=3 --no-xattrs \
          -cJf "$archive_stage" "$(basename -- "$APP")"
      )
    else
      if [ -f "$APP_ANDROID_ROOTFS_PATH" ]; then
        logical=$(stat -f %z "$APP_ANDROID_ROOTFS_PATH")
        physical=$(($(stat -f %b "$APP_ANDROID_ROOTFS_PATH") * 512))
        if [ "$physical" -lt "$logical" ]; then
          fail "zip cannot preserve the sparse Android aggregate; use a .tar.xz archive"
        fi
      fi
      ditto --norsrc --noextattr -c -k --keepParent "$APP" "$archive_stage"
    fi
    mv "$archive_stage" "$ARCHIVE"
    trap - EXIT HUP INT TERM
    app_checksums_stage=$(mktemp "$output_parent/.hd-distribution-app-checksums.XXXXXX")
    archive_sha_stage=$(mktemp "$output_parent/.hd-distribution-archive-sha.XXXXXX")
    metadata_stage=$(mktemp "$output_parent/.hd-distribution-metadata.XXXXXX")
    cleanup_sidecars() {
      rm -f -- "$app_checksums_stage" "$archive_sha_stage" "$metadata_stage"
    }
    trap cleanup_sidecars EXIT HUP INT TERM
    install -m 644 "$APP_CHECKSUMS_INPUT" "$app_checksums_stage"
    archive_digest=$(sha256 "$ARCHIVE")
    archive_size=$(stat -f %z "$ARCHIVE")
    printf '%s  %s\n' "$archive_digest" "$(basename -- "$ARCHIVE")" > "$archive_sha_stage"
    chmod 644 "$archive_sha_stage"
    write_metadata "$metadata_stage" "$archive_digest" "$archive_size"
    chmod 644 "$metadata_stage"
    mv "$app_checksums_stage" "$archive_app_checksums"
    mv "$archive_sha_stage" "$archive_sha_file"
    mv "$metadata_stage" "$metadata"
    trap - EXIT HUP INT TERM
    verify_archive
    ;;
  verify)
    verify_archive
    ;;
  *)
    echo "unknown command: $COMMAND" >&2
    usage >&2
    exit 2
    ;;
esac
write_gate_report
