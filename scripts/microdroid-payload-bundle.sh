#!/bin/sh
set -eu

usage() {
  cat <<'EOF'
Usage:
  microdroid-payload-bundle.sh create \
    --apk <v3-signed-payload.apk> --apksigner <apksigner> \
    --output-dir <new-directory> --artifact-id <id> --version <version> \
    --channel <development|release> [--expected-signer-sha256 <digest>]

  microdroid-payload-bundle.sh verify \
    --bundle <directory> --apksigner <apksigner> \
    --require-channel <development|release> [--expected-signer-sha256 <digest>]
EOF
}

[ "$#" -gt 0 ] || { usage >&2; exit 2; }
MODE=$1
shift

APK=
APKSIGNER=
OUTPUT_DIR=
ARTIFACT_ID=
VERSION=
CHANNEL=
BUNDLE=
REQUIRE_CHANNEL=
EXPECTED_SIGNER_SHA256=

while [ "$#" -gt 0 ]; do
  case "$1" in
    --apk) APK=$2; shift 2 ;;
    --apksigner) APKSIGNER=$2; shift 2 ;;
    --output-dir) OUTPUT_DIR=$2; shift 2 ;;
    --artifact-id) ARTIFACT_ID=$2; shift 2 ;;
    --version) VERSION=$2; shift 2 ;;
    --channel) CHANNEL=$2; shift 2 ;;
    --bundle) BUNDLE=$2; shift 2 ;;
    --require-channel) REQUIRE_CHANNEL=$2; shift 2 ;;
    --expected-signer-sha256) EXPECTED_SIGNER_SHA256=$2; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

valid_digest() {
  [ "${#1}" -eq 64 ] &&
    ! printf '%s' "$1" | LC_ALL=C grep -q '[^0-9a-f]'
}

validate_identifier() {
  case "$1" in
    ''|*[!a-z0-9._-]*) return 1 ;;
    *) return 0 ;;
  esac
}

validate_version() {
  case "$1" in
    ''|*[!A-Za-z0-9._+-]*) return 1 ;;
    *) return 0 ;;
  esac
}

validate_channel() {
  [ "$1" = development ] || [ "$1" = release ]
}

verify_payload_apk() {
  candidate=$1
  [ -f "$candidate" ] || {
    echo "Payload APK is not a regular file: $candidate" >&2
    exit 1
  }
  entry_count=$(unzip -Z1 "$candidate" | grep -c '^assets/vm_config.json$' || true)
  if [ "$entry_count" -ne 1 ]; then
    echo "Payload APK must contain exactly one assets/vm_config.json" >&2
    exit 1
  fi
  verification=$("$APKSIGNER" verify --verbose --print-certs "$candidate" 2>&1) || {
    echo "$verification" >&2
    echo "Payload APK signature verification failed" >&2
    exit 1
  }
  if ! printf '%s\n' "$verification" |
    grep -Eq '^Verified using v3(\\.1)? scheme .*: true$'; then
    echo "Payload APK must verify with APK Signature Scheme v3 or v3.1" >&2
    exit 1
  fi
  signer_count=$(printf '%s\n' "$verification" |
    sed -n 's/^Number of signers: //p')
  if [ "$signer_count" != 1 ]; then
    echo "Payload APK must have exactly one signer" >&2
    exit 1
  fi
  VERIFIED_SIGNER_SHA256=$(printf '%s\n' "$verification" |
    sed -n 's/^Signer #1 certificate SHA-256 digest: //p')
  if ! valid_digest "$VERIFIED_SIGNER_SHA256"; then
    echo "apksigner did not return one lowercase SHA-256 signer digest" >&2
    exit 1
  fi
  if [ -n "$EXPECTED_SIGNER_SHA256" ] &&
    [ "$VERIFIED_SIGNER_SHA256" != "$EXPECTED_SIGNER_SHA256" ]; then
    echo "Payload APK signer does not match --expected-signer-sha256" >&2
    exit 1
  fi
}

plist_raw() {
  plutil -extract "$2" raw -o - "$1"
}

case "$MODE" in
  create)
    [ -n "$APK" ] && [ -n "$APKSIGNER" ] && [ -n "$OUTPUT_DIR" ] &&
      [ -n "$ARTIFACT_ID" ] && [ -n "$VERSION" ] && [ -n "$CHANNEL" ] || {
      echo "create is missing a required argument" >&2
      usage >&2
      exit 2
    }
    ;;
  verify)
    [ -n "$BUNDLE" ] && [ -n "$APKSIGNER" ] && [ -n "$REQUIRE_CHANNEL" ] || {
      echo "verify is missing a required argument" >&2
      usage >&2
      exit 2
    }
    ;;
  *)
    echo "unknown mode: $MODE" >&2
    usage >&2
    exit 2
    ;;
esac

[ -x "$APKSIGNER" ] || {
  echo "--apksigner must be an executable file" >&2
  exit 2
}
if [ -n "$EXPECTED_SIGNER_SHA256" ] && ! valid_digest "$EXPECTED_SIGNER_SHA256"; then
  echo "--expected-signer-sha256 must be a lowercase SHA-256 digest" >&2
  exit 2
fi

if [ "$MODE" = create ]; then
  case "$APK" in /*) ;; *) echo "--apk must be absolute" >&2; exit 2 ;; esac
  case "$OUTPUT_DIR" in /*) ;; *) echo "--output-dir must be absolute" >&2; exit 2 ;; esac
  validate_identifier "$ARTIFACT_ID" || {
    echo "--artifact-id must use lowercase letters, digits, dot, underscore or dash" >&2
    exit 2
  }
  validate_version "$VERSION" || {
    echo "--version contains unsupported characters" >&2
    exit 2
  }
  validate_channel "$CHANNEL" || {
    echo "--channel must be development or release" >&2
    exit 2
  }
  if [ "$CHANNEL" = release ] && [ -z "$EXPECTED_SIGNER_SHA256" ]; then
    echo "release Payload creation requires --expected-signer-sha256" >&2
    exit 2
  fi
  [ ! -e "$OUTPUT_DIR" ] || {
    echo "refusing to replace existing output: $OUTPUT_DIR" >&2
    exit 2
  }
  verify_payload_apk "$APK"
  output_parent=$(dirname -- "$OUTPUT_DIR")
  mkdir -p "$output_parent"
  stage=$(mktemp -d "$output_parent/.microdroid-payload-bundle.XXXXXX")
  cleanup() {
    case "$stage" in
      "$output_parent"/.microdroid-payload-bundle.*) rm -rf -- "$stage" ;;
    esac
  }
  trap cleanup EXIT HUP INT TERM
  install -m 644 "$APK" "$stage/payload.apk"
  apk_sha256=$(shasum -a 256 "$stage/payload.apk" | awk '{print $1}')
  apk_size=$(stat -f '%z' "$stage/payload.apk")
  cat >"$stage/payload-bundle-v1.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "https://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>schema_version</key><integer>1</integer>
  <key>artifact_id</key><string>$ARTIFACT_ID</string>
  <key>version</key><string>$VERSION</string>
  <key>channel</key><string>$CHANNEL</string>
  <key>apk</key><string>payload.apk</string>
  <key>config_path</key><string>assets/vm_config.json</string>
  <key>sha256</key><string>$apk_sha256</string>
  <key>size_bytes</key><integer>$apk_size</integer>
  <key>signature_scheme</key><string>v3_or_newer</string>
  <key>signer_certificate_sha256</key><string>$VERIFIED_SIGNER_SHA256</string>
</dict>
</plist>
EOF
  plutil -lint "$stage/payload-bundle-v1.plist" >/dev/null
  mv "$stage" "$OUTPUT_DIR"
  trap - EXIT HUP INT TERM
  echo "bundle=$OUTPUT_DIR"
  echo "payload_sha256=$apk_sha256"
  echo "signer_certificate_sha256=$VERIFIED_SIGNER_SHA256"
  exit 0
fi

case "$BUNDLE" in /*) ;; *) echo "--bundle must be absolute" >&2; exit 2 ;; esac
validate_channel "$REQUIRE_CHANNEL" || {
  echo "--require-channel must be development or release" >&2
  exit 2
}
if [ "$REQUIRE_CHANNEL" = release ] && [ -z "$EXPECTED_SIGNER_SHA256" ]; then
  echo "release Payload verification requires --expected-signer-sha256" >&2
  exit 2
fi
manifest="$BUNDLE/payload-bundle-v1.plist"
payload="$BUNDLE/payload.apk"
[ -f "$manifest" ] && [ -f "$payload" ] || {
  echo "Payload bundle must contain payload-bundle-v1.plist and payload.apk" >&2
  exit 1
}
[ ! -L "$manifest" ] && [ ! -L "$payload" ] || {
  echo "Payload bundle files must not be symbolic links" >&2
  exit 1
}
bundle_entries=$(find "$BUNDLE" -mindepth 1 -maxdepth 1 | wc -l | tr -d ' ')
[ "$bundle_entries" = 2 ] || {
  echo "Payload bundle contains unexpected files" >&2
  exit 1
}
plutil -lint "$manifest" >/dev/null
schema_version=$(plist_raw "$manifest" schema_version)
artifact_id=$(plist_raw "$manifest" artifact_id)
version=$(plist_raw "$manifest" version)
channel=$(plist_raw "$manifest" channel)
apk_name=$(plist_raw "$manifest" apk)
config_path=$(plist_raw "$manifest" config_path)
expected_sha256=$(plist_raw "$manifest" sha256)
expected_size=$(plist_raw "$manifest" size_bytes)
signature_scheme=$(plist_raw "$manifest" signature_scheme)
manifest_signer=$(plist_raw "$manifest" signer_certificate_sha256)
[ "$schema_version" = 1 ] || { echo "unsupported Payload bundle schema" >&2; exit 1; }
validate_identifier "$artifact_id" || { echo "invalid Payload artifact id" >&2; exit 1; }
validate_version "$version" || { echo "invalid Payload version" >&2; exit 1; }
[ "$channel" = "$REQUIRE_CHANNEL" ] || {
  echo "Payload bundle channel $channel does not match required $REQUIRE_CHANNEL" >&2
  exit 1
}
[ "$apk_name" = payload.apk ] || { echo "Payload bundle APK path is invalid" >&2; exit 1; }
[ "$config_path" = assets/vm_config.json ] || {
  echo "Payload bundle config path is invalid" >&2
  exit 1
}
[ "$signature_scheme" = v3_or_newer ] || {
  echo "Payload bundle signature scheme is invalid" >&2
  exit 1
}
valid_digest "$expected_sha256" || { echo "Payload bundle digest is invalid" >&2; exit 1; }
valid_digest "$manifest_signer" || { echo "Payload bundle signer is invalid" >&2; exit 1; }
case "$expected_size" in ''|*[!0-9]*) echo "Payload bundle size is invalid" >&2; exit 1 ;; esac
actual_sha256=$(shasum -a 256 "$payload" | awk '{print $1}')
actual_size=$(stat -f '%z' "$payload")
[ "$actual_sha256" = "$expected_sha256" ] || {
  echo "Payload bundle APK digest does not match its manifest" >&2
  exit 1
}
[ "$actual_size" = "$expected_size" ] || {
  echo "Payload bundle APK size does not match its manifest" >&2
  exit 1
}
if [ -n "$EXPECTED_SIGNER_SHA256" ] &&
  [ "$manifest_signer" != "$EXPECTED_SIGNER_SHA256" ]; then
  echo "Payload bundle manifest signer does not match --expected-signer-sha256" >&2
  exit 1
fi
verify_payload_apk "$payload"
[ "$VERIFIED_SIGNER_SHA256" = "$manifest_signer" ] || {
  echo "Payload APK signer does not match its bundle manifest" >&2
  exit 1
}
echo "bundle=$BUNDLE"
echo "artifact_id=$artifact_id"
echo "version=$version"
echo "channel=$channel"
echo "payload_sha256=$actual_sha256"
echo "signer_certificate_sha256=$VERIFIED_SIGNER_SHA256"
