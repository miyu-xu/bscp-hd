#!/bin/sh
set -eu

usage() {
  echo "Usage: $0 --package-script <package-macos.sh> --direct-product <android-product> --signed-store <development-store-v2> --evidence-dir <fresh-directory>" >&2
}

PACKAGE_SCRIPT=
DIRECT_PRODUCT=
SIGNED_STORE=
EVIDENCE_DIR=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --package-script) PACKAGE_SCRIPT=$2; shift 2 ;;
    --direct-product) DIRECT_PRODUCT=$2; shift 2 ;;
    --signed-store) SIGNED_STORE=$2; shift 2 ;;
    --evidence-dir) EVIDENCE_DIR=$2; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage; exit 2 ;;
  esac
done

case "$PACKAGE_SCRIPT:$DIRECT_PRODUCT:$SIGNED_STORE:$EVIDENCE_DIR" in
  /*:/*:/*:/*) ;;
  *) usage; exit 2 ;;
esac
[ -x "$PACKAGE_SCRIPT" ] && [ ! -L "$PACKAGE_SCRIPT" ] || {
  echo "package script must be an executable non-symlink file" >&2
  exit 2
}
[ -d "$DIRECT_PRODUCT" ] && [ ! -L "$DIRECT_PRODUCT" ] || {
  echo "direct product must be a non-symlink directory" >&2
  exit 2
}
[ -d "$SIGNED_STORE" ] && [ ! -L "$SIGNED_STORE" ] || {
  echo "signed store must be a non-symlink directory" >&2
  exit 2
}
[ ! -e "$EVIDENCE_DIR" ] || {
  echo "refusing to replace evidence: $EVIDENCE_DIR" >&2
  exit 2
}
mkdir -p "$EVIDENCE_DIR"

MATERIALS="$EVIDENCE_DIR/release-materials"
mkdir -p "$MATERIALS/certifications"
printf '%s\n' '{}' > "$MATERIALS/trusted-keys-v2.json"
printf '%s\n' '{}' > "$MATERIALS/certifications/contract.json"

run_rejection() {
  name=$1
  expected=$2
  shift 2
  log="$EVIDENCE_DIR/$name.log"
  if "$PACKAGE_SCRIPT" \
      --target-dir /invalid/target \
      --runtime-dir /invalid/runtime \
      --microdroid-product /invalid/microdroid \
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
      --output "$EVIDENCE_DIR/output.app" \
      "$@" > "$log" 2>&1; then
    echo "$name unexpectedly passed packaging validation" >&2
    exit 1
  fi
  grep -Fq -- "$expected" "$log" || {
    echo "$name did not emit the expected rejection: $expected" >&2
    exit 1
  }
  [ ! -e "$EVIDENCE_DIR/output.app" ] || {
    echo "$name created output before rejecting its Android source contract" >&2
    exit 1
  }
}

run_rejection development-missing-source \
  "development packaging requires exactly one self-contained Android source" \
  --development-package
run_rejection development-both-sources \
  "--android-product and --android-artifact-store are mutually exclusive" \
  --development-package --android-product "$DIRECT_PRODUCT" \
  --android-artifact-store "$SIGNED_STORE"
run_rejection release-direct-source \
  "--android-product is a direct development image and may only be used with --development-package" \
  --android-product "$DIRECT_PRODUCT" \
  --identity "Developer ID Application: HD Contract Test" \
  --release-materials "$MATERIALS" \
  --microdroid-payload-signer-sha256 \
    0000000000000000000000000000000000000000000000000000000000000000
run_rejection release-missing-store \
  "release packaging requires --android-artifact-store" \
  --identity "Developer ID Application: HD Contract Test" \
  --release-materials "$MATERIALS" \
  --microdroid-payload-signer-sha256 \
    0000000000000000000000000000000000000000000000000000000000000000

STORE_LINK="$EVIDENCE_DIR/store-symlink"
ln -s "$SIGNED_STORE" "$STORE_LINK"
run_rejection development-store-symlink \
  "--android-artifact-store must be a real directory, not a symbolic link" \
  --development-package --android-artifact-store "$STORE_LINK"

GENERATED_AT=$(date -u '+%Y-%m-%dT%H:%M:%SZ')
cat > "$EVIDENCE_DIR/android-package-contract-gates.json" <<EOF
{
  "schema_version": 2,
  "generated_at": "$GENERATED_AT",
  "source": "scripts/macos-android-package-contract-smoke.sh",
  "gates": [
    {
      "name": "macos-android-package-contract",
      "command": "macos-android-package-contract-smoke.sh --package-script <package-macos.sh>",
      "status": "pass",
      "duration_ms": null,
      "log_path": "$EVIDENCE_DIR",
      "summary": "Packaging rejected a development package with zero or two Android sources, a release package with a direct development image or no signed store, and a symlink signed-store root before staging output."
    }
  ]
}
EOF

echo "result=pass"
echo "negative_cases=5"
echo "evidence=$EVIDENCE_DIR"
