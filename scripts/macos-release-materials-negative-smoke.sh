#!/bin/sh
set -eu

usage() {
  echo "Usage: $0 --package-script <package-macos.sh> --evidence-dir <new-directory>" >&2
}

PACKAGE_SCRIPT=
EVIDENCE_DIR=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --package-script) PACKAGE_SCRIPT=$2; shift 2 ;;
    --evidence-dir) EVIDENCE_DIR=$2; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage; exit 2 ;;
  esac
done

if [ -z "$PACKAGE_SCRIPT" ] || [ -z "$EVIDENCE_DIR" ]; then
  usage
  exit 2
fi
case "$PACKAGE_SCRIPT:$EVIDENCE_DIR" in
  /*:/*) ;;
  *) echo "all paths must be absolute" >&2; exit 2 ;;
esac
if [ ! -x "$PACKAGE_SCRIPT" ]; then
  echo "package script is not executable: $PACKAGE_SCRIPT" >&2
  exit 2
fi
if [ -e "$EVIDENCE_DIR" ]; then
  echo "refusing to replace existing evidence: $EVIDENCE_DIR" >&2
  exit 2
fi
mkdir -p "$EVIDENCE_DIR"

run_rejection() {
  name=$1
  materials=$2
  expected=$3
  log="$EVIDENCE_DIR/$name.log"
  if "$PACKAGE_SCRIPT" \
      --target-dir /invalid/target \
      --runtime-dir /invalid/runtime \
      --microdroid-product /invalid/product \
      --web-dist /invalid/web \
      --adb /invalid/adb \
      --aapt2 /invalid/aapt2 \
      --apksigner /invalid/apksigner \
      --node-root /invalid/node \
      --node-archive /invalid/node.tar.gz \
      --java-home /invalid/java \
      --java-archive /invalid/java.tar.gz \
      --android-build-tools /invalid/build-tools \
      --release-toolchain-evidence /invalid/evidence \
      --microdroid-payload-bundle /invalid/payload \
      --microdroid-payload-signer-sha256 \
        0000000000000000000000000000000000000000000000000000000000000000 \
      --output "$EVIDENCE_DIR/output.app" \
      --identity "Developer ID Application: HD Contract Test" \
      --release-materials "$materials" >"$log" 2>&1; then
    echo "$name unexpectedly passed packaging validation" >&2
    exit 1
  fi
  if ! grep -Fq -- "$expected" "$log"; then
    echo "$name did not emit the expected rejection: $expected" >&2
    exit 1
  fi
  if [ -e "$EVIDENCE_DIR/output.app" ]; then
    echo "$name created package output before rejecting release materials" >&2
    exit 1
  fi
}

ROOT_TARGET="$EVIDENCE_DIR/root-target"
ROOT_SYMLINK="$EVIDENCE_DIR/root-symlink"
mkdir -p "$ROOT_TARGET/certifications"
: > "$ROOT_TARGET/trusted-keys-v2.json"
: > "$ROOT_TARGET/certifications/cert.json"
ln -s "$ROOT_TARGET" "$ROOT_SYMLINK"
run_rejection root-symlink "$ROOT_SYMLINK" \
  "--release-materials must be a real directory, not a symbolic link"

TRUST_SYMLINK="$EVIDENCE_DIR/trust-symlink"
mkdir -p "$TRUST_SYMLINK/certifications"
ln -s /dev/null "$TRUST_SYMLINK/trusted-keys-v2.json"
: > "$TRUST_SYMLINK/certifications/cert.json"
run_rejection trust-symlink "$TRUST_SYMLINK" \
  "--release-materials must contain trusted-keys-v2.json and certifications/"

CERT_SYMLINK="$EVIDENCE_DIR/cert-symlink"
mkdir -p "$CERT_SYMLINK/certifications"
: > "$CERT_SYMLINK/trusted-keys-v2.json"
ln -s /dev/null "$CERT_SYMLINK/certifications/cert.json"
run_rejection cert-symlink "$CERT_SYMLINK" \
  "--release-materials certifications/ may contain only regular files"

NESTED="$EVIDENCE_DIR/nested"
mkdir -p "$NESTED/certifications/nested"
: > "$NESTED/trusted-keys-v2.json"
: > "$NESTED/certifications/nested/cert.json"
run_rejection nested "$NESTED" \
  "--release-materials certifications/ must be flat"

EXTRA="$EVIDENCE_DIR/extra"
mkdir -p "$EXTRA/certifications"
: > "$EXTRA/trusted-keys-v2.json"
: > "$EXTRA/certifications/cert.json"
: > "$EXTRA/certifications/README.txt"
run_rejection extra "$EXTRA" \
  "--release-materials certifications/ may contain only .json certificates"

EMPTY="$EVIDENCE_DIR/empty"
mkdir -p "$EMPTY/certifications"
: > "$EMPTY/trusted-keys-v2.json"
run_rejection empty "$EMPTY" \
  "--release-materials certifications/ contains no JSON certificate"

GENERATED_AT=$(date -u '+%Y-%m-%dT%H:%M:%SZ')
ESCAPED_EVIDENCE=$(printf '%s' "$EVIDENCE_DIR" | sed 's/\\/\\\\/g; s/"/\\"/g')
printf '%s\n' \
  '{' \
  '  "schema_version": 2,' \
  "  \"generated_at\": \"$GENERATED_AT\"," \
  '  "source": "scripts/macos-release-materials-negative-smoke.sh",' \
  '  "gates": [' \
  '    {' \
  '      "name": "macos-release-contract",' \
  '      "command": "macos-release-materials-negative-smoke.sh --package-script <package-macos.sh>",' \
  '      "status": "pass",' \
  '      "duration_ms": null,' \
  "      \"log_path\": \"$ESCAPED_EVIDENCE\"," \
  '      "summary": "Packaging rejected a symlink release root, symlink trust root, symlink certification, nested certification, non-JSON entry, and empty certification set before staging output."' \
  '    }' \
  '  ]' \
  '}' > "$EVIDENCE_DIR/release-materials-negative-gates.json"

echo "result=pass"
echo "negative_cases=6"
echo "evidence=$EVIDENCE_DIR"
