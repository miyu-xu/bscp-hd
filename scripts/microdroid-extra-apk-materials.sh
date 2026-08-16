#!/bin/sh
set -eu
umask 077

usage() {
  cat <<'EOF'
Usage: scripts/microdroid-extra-apk-materials.sh \
  --main-template <v3-signed-payload.apk> \
  --extra-template-0 <apk-with-distinct-package-name> \
  --extra-template-1 <apk-with-distinct-package-name> \
  --android-build-tools <android-sdk/build-tools/36.0.0> \
  --java-home <Temurin-21/Contents/Home> \
  --output <fresh-material-directory>

Generates one main Payload declaring two caller-overridden extra APKs, two
v3-signed extra APKs with distinguishable assets, and one structurally valid
but unsigned negative APK. The ephemeral QA private key is never retained.
EOF
}

MAIN_TEMPLATE=
EXTRA_TEMPLATE_0=
EXTRA_TEMPLATE_1=
ANDROID_BUILD_TOOLS=
JAVA_HOME_INPUT=
OUTPUT=

while [ "$#" -gt 0 ]; do
  case "$1" in
    --main-template) MAIN_TEMPLATE=$2; shift 2 ;;
    --extra-template-0) EXTRA_TEMPLATE_0=$2; shift 2 ;;
    --extra-template-1) EXTRA_TEMPLATE_1=$2; shift 2 ;;
    --android-build-tools) ANDROID_BUILD_TOOLS=$2; shift 2 ;;
    --java-home) JAVA_HOME_INPUT=$2; shift 2 ;;
    --output) OUTPUT=$2; shift 2 ;;
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

require_abs_file "$MAIN_TEMPLATE" --main-template
require_abs_file "$EXTRA_TEMPLATE_0" --extra-template-0
require_abs_file "$EXTRA_TEMPLATE_1" --extra-template-1
require_abs_dir "$ANDROID_BUILD_TOOLS" --android-build-tools
require_abs_dir "$JAVA_HOME_INPUT" --java-home
case "$OUTPUT" in /*) ;; *) fail "--output must be absolute" ;; esac
[ ! -e "$OUTPUT" ] || fail "refusing to replace material output: $OUTPUT"
FAILURE_OUTPUT="$OUTPUT.failed"
[ ! -e "$FAILURE_OUTPUT" ] || fail "refusing to replace failed material output: $FAILURE_OUTPUT"

APKSIGNER="$ANDROID_BUILD_TOOLS/apksigner"
ZIPALIGN="$ANDROID_BUILD_TOOLS/zipalign"
KEYTOOL="$JAVA_HOME_INPUT/bin/keytool"
require_abs_file "$APKSIGNER" apksigner
require_abs_file "$ZIPALIGN" zipalign
require_abs_file "$KEYTOOL" keytool
export JAVA_HOME="$JAVA_HOME_INPUT"
PATH="$JAVA_HOME/bin:$PATH"
export PATH
for command in jq shasum unzip uuidgen zip; do
  command -v "$command" >/dev/null 2>&1 || fail "required tool is missing: $command"
done

output_parent=$(dirname -- "$OUTPUT")
mkdir -p "$output_parent"
OUTPUT_STAGE=$(mktemp -d "$output_parent/.hd-microdroid-extra-materials.XXXXXX")
WORK=$(mktemp -d /private/tmp/hd-microdroid-extra-materials.XXXXXX)
COMPLETED=0

cleanup() {
  exit_code=$?
  case "$WORK" in /private/tmp/hd-microdroid-extra-materials.*) rm -rf -- "$WORK" ;; esac
  case "$OUTPUT_STAGE" in
    "$output_parent"/.hd-microdroid-extra-materials.*)
      if [ "$COMPLETED" -eq 0 ] && [ -d "$OUTPUT_STAGE" ]; then
        printf '%s\n' "$exit_code" >"$OUTPUT_STAGE/exit.code"
        mv "$OUTPUT_STAGE" "$FAILURE_OUTPUT"
        echo "failure_materials=$FAILURE_OUTPUT" >&2
      else
        rm -rf -- "$OUTPUT_STAGE"
      fi
      ;;
  esac
}
trap cleanup EXIT HUP INT TERM

unpack_template() {
  source_apk=$1
  destination=$2
  mkdir -p "$destination"
  COPYFILE_DISABLE=1 unzip -q "$source_apk" -d "$destination"
  [ -f "$destination/AndroidManifest.xml" ] || fail "template has no AndroidManifest.xml: $source_apk"
  if find "$destination" -type l -print | grep . >/dev/null; then
    fail "template extracted a symbolic link: $source_apk"
  fi
  rm -rf -- "$destination/META-INF"
  find "$destination" -name '.DS_Store' -type f -delete
}

pack_aligned() {
  source_dir=$1
  unsigned_apk=$2
  aligned_apk=$3
  # Microdroid dlopens the Payload library directly from the APK. Keep every entry stored so
  # native libraries remain mmap-compatible on both x86_64 and arm64; zipalign alone cannot
  # repair a compressed .so entry.
  (cd "$source_dir" && COPYFILE_DISABLE=1 zip -0 -X -q -r "$unsigned_apk" .)
  "$ZIPALIGN" -p -f 4 "$unsigned_apk" "$aligned_apk"
}

pack_native_first_aligned() {
  source_dir=$1
  unsigned_apk=$2
  aligned_apk=$3
  native_list=$WORK/main-native.list
  other_list=$WORK/main-other.list
  (
    cd "$source_dir"
    find lib -type f -print | LC_ALL=C sort >"$native_list"
    [ -s "$native_list" ] || fail "main payload has no native library"
    find . -type f ! -path './lib/*' -print |
      sed 's#^\./##' | LC_ALL=C sort >"$other_list"
    COPYFILE_DISABLE=1 zip -0 -X -q "$unsigned_apk" -@ <"$native_list"
    if [ -s "$other_list" ]; then
      COPYFILE_DISABLE=1 zip -0 -X -q "$unsigned_apk" -@ <"$other_list"
    fi
  )
  "$ZIPALIGN" -p -f 4 "$unsigned_apk" "$aligned_apk"
}

replace_main_config_aligned() {
  source_apk=$1
  config_file=$2
  mutated_apk=$3
  aligned_apk=$4
  config_stage=$5
  first_entry=$(unzip -Z1 "$source_apk" | sed -n '1p')
  native_entries_are_stored=0
  if unzip -lv "$source_apk" |
      awk '$8 ~ /^lib\/.*\.so$/ { found=1; if ($2 != "Stored") bad=1 }
           END { exit !(found && !bad) }'; then
    native_entries_are_stored=1
  fi

  case "$first_entry" in
    lib/*.so)
      if [ "$native_entries_are_stored" -eq 1 ]; then
        cp "$source_apk" "$mutated_apk"
        zip -q -d "$mutated_apk" 'META-INF/*' >/dev/null 2>&1 || true
        zip -q -d "$mutated_apk" assets/vm_config.json
        mkdir -p "$config_stage/assets"
        cp "$config_file" "$config_stage/assets/vm_config.json"
        # Preserve Soong's original native-library-first local-entry layout. Repacking the
        # whole APK is unnecessary when every native library is already stored.
        (cd "$config_stage" && COPYFILE_DISABLE=1 zip -0 -X -q "$mutated_apk" assets/vm_config.json)
        "$ZIPALIGN" -p -f 4 "$mutated_apk" "$aligned_apk"
        return
      fi
      ;;
  esac

  # Some arm64 AOSP payload templates compress the native library and put the manifest first.
  # Rebuild those templates with native libraries first, stored, and page aligned so zipfuse
  # can expose the library for direct mmap/dlopen.
  pack_native_first_aligned "$WORK/main" "$mutated_apk" "$aligned_apk"
}

sign_v3() {
  aligned_apk=$1
  signed_apk=$2
  verify_log=$3
  HD_QA_KEY_PASSWORD="$QA_PASSWORD" "$APKSIGNER" sign \
    --ks "$KEYSTORE" \
    --ks-key-alias hd-microdroid-extra-qa \
    --ks-pass env:HD_QA_KEY_PASSWORD \
    --key-pass env:HD_QA_KEY_PASSWORD \
    --min-sdk-version 35 \
    --v1-signing-enabled false \
    --v2-signing-enabled true \
    --v3-signing-enabled true \
    --v4-signing-enabled false \
    --out "$signed_apk" \
    "$aligned_apk"
  "$APKSIGNER" verify --verbose --print-certs "$signed_apk" >"$verify_log"
  grep -Eq 'Verified using v3 scheme.*true' "$verify_log" ||
    fail "signed APK did not verify with APK Signature Scheme v3: $signed_apk"
}

unzip -p "$MAIN_TEMPLATE" assets/vm_config.json >"$WORK/original-vm-config.json" ||
  fail "main template has no assets/vm_config.json"
jq -e '.task.type == "microdroid_launcher" and (.task.command | type == "string" and length > 0)' \
  "$WORK/original-vm-config.json" >/dev/null ||
  fail "main template does not contain a Microdroid launcher task"

unpack_template "$MAIN_TEMPLATE" "$WORK/main"
mkdir -p "$WORK/main/assets"
jq '.extra_apks = [
      {"path":"/hd/declared/path/extra-0.apk"},
      {"path":"/hd/declared/path/extra-1.apk"}
    ]' "$WORK/original-vm-config.json" >"$WORK/main/assets/vm_config.json"

unpack_template "$EXTRA_TEMPLATE_0" "$WORK/extra-0"
unpack_template "$EXTRA_TEMPLATE_1" "$WORK/extra-1"
mkdir -p "$WORK/extra-0/assets" "$WORK/extra-1/assets"
printf 'HD Microdroid extra APK marker 0\n' >"$WORK/extra-0/assets/hd-extra-0.txt"
printf 'HD Microdroid extra APK marker 1\n' >"$WORK/extra-1/assets/hd-extra-1.txt"

replace_main_config_aligned "$MAIN_TEMPLATE" "$WORK/main/assets/vm_config.json" \
  "$WORK/main-mutated.apk" "$WORK/main-aligned.apk" "$WORK/main-config-stage"
pack_aligned "$WORK/extra-0" "$WORK/extra-0-unsigned.apk" "$WORK/extra-0-aligned.apk"
pack_aligned "$WORK/extra-1" "$WORK/extra-1-unsigned.apk" "$WORK/extra-1-aligned.apk"
cp "$WORK/extra-0-aligned.apk" "$OUTPUT_STAGE/extra-invalid-signature.apk"

QA_PASSWORD="hd-extra-qa-$(uuidgen | tr -d '-')"
KEYSTORE="$WORK/hd-microdroid-extra-qa.p12"
HD_QA_KEY_PASSWORD="$QA_PASSWORD" "$KEYTOOL" -genkeypair \
  -storetype PKCS12 \
  -keystore "$KEYSTORE" \
  -storepass:env HD_QA_KEY_PASSWORD \
  -keypass:env HD_QA_KEY_PASSWORD \
  -alias hd-microdroid-extra-qa \
  -keyalg RSA \
  -keysize 3072 \
  -validity 3650 \
  -dname 'CN=HD Microdroid Extra APK QA,OU=Development,O=HD,L=Shanghai,C=CN' \
  >/dev/null
HD_QA_KEY_PASSWORD="$QA_PASSWORD" "$KEYTOOL" -exportcert \
  -keystore "$KEYSTORE" \
  -storepass:env HD_QA_KEY_PASSWORD \
  -alias hd-microdroid-extra-qa \
  -file "$OUTPUT_STAGE/qa-signer.der" \
  >/dev/null

sign_v3 "$WORK/main-aligned.apk" "$OUTPUT_STAGE/main-payload.apk" \
  "$OUTPUT_STAGE/main-payload.verify.txt"
sign_v3 "$WORK/extra-0-aligned.apk" "$OUTPUT_STAGE/extra-0.apk" \
  "$OUTPUT_STAGE/extra-0.verify.txt"
sign_v3 "$WORK/extra-1-aligned.apk" "$OUTPUT_STAGE/extra-1.apk" \
  "$OUTPUT_STAGE/extra-1.verify.txt"
if "$APKSIGNER" verify "$OUTPUT_STAGE/extra-invalid-signature.apk" \
  >"$OUTPUT_STAGE/extra-invalid-signature.verify.txt" 2>&1; then
  fail "unsigned negative extra APK unexpectedly passed apksigner verification"
fi

MAIN_SHA=$(shasum -a 256 "$OUTPUT_STAGE/main-payload.apk" | awk '{print $1}')
EXTRA_SHA_0=$(shasum -a 256 "$OUTPUT_STAGE/extra-0.apk" | awk '{print $1}')
EXTRA_SHA_1=$(shasum -a 256 "$OUTPUT_STAGE/extra-1.apk" | awk '{print $1}')
INVALID_SHA=$(shasum -a 256 "$OUTPUT_STAGE/extra-invalid-signature.apk" | awk '{print $1}')
ASSET_SHA_0=$(shasum -a 256 "$WORK/extra-0/assets/hd-extra-0.txt" | awk '{print $1}')
ASSET_SHA_1=$(shasum -a 256 "$WORK/extra-1/assets/hd-extra-1.txt" | awk '{print $1}')
CERT_SHA=$(shasum -a 256 "$OUTPUT_STAGE/qa-signer.der" | awk '{print $1}')
MAIN_TEMPLATE_SHA=$(shasum -a 256 "$MAIN_TEMPLATE" | awk '{print $1}')
EXTRA_TEMPLATE_SHA_0=$(shasum -a 256 "$EXTRA_TEMPLATE_0" | awk '{print $1}')
EXTRA_TEMPLATE_SHA_1=$(shasum -a 256 "$EXTRA_TEMPLATE_1" | awk '{print $1}')
APKSIGNER_SHA=$(shasum -a 256 "$APKSIGNER" | awk '{print $1}')
ZIPALIGN_SHA=$(shasum -a 256 "$ZIPALIGN" | awk '{print $1}')

cat >"$OUTPUT_STAGE/materials.json" <<EOF
{
  "schema_version": 1,
  "profile": "hd-microdroid-extra-apk-qa-materials-v1",
  "main_payload": {"file":"main-payload.apk","sha256":"$MAIN_SHA","declared_extra_apks":2},
  "extra_apks": [
    {"file":"extra-0.apk","sha256":"$EXTRA_SHA_0","asset_path":"assets/hd-extra-0.txt","asset_sha256":"$ASSET_SHA_0"},
    {"file":"extra-1.apk","sha256":"$EXTRA_SHA_1","asset_path":"assets/hd-extra-1.txt","asset_sha256":"$ASSET_SHA_1"}
  ],
  "invalid_signature_extra_apk": {"file":"extra-invalid-signature.apk","sha256":"$INVALID_SHA","apksigner_verified":false},
  "declared_host_paths": ["/hd/declared/path/extra-0.apk","/hd/declared/path/extra-1.apk"],
  "qa_signer_certificate_sha256": "$CERT_SHA",
  "private_key_retained": false,
  "source_templates": {
    "main_sha256":"$MAIN_TEMPLATE_SHA",
    "extra_0_sha256":"$EXTRA_TEMPLATE_SHA_0",
    "extra_1_sha256":"$EXTRA_TEMPLATE_SHA_1"
  },
  "toolchain": {"apksigner_sha256":"$APKSIGNER_SHA","zipalign_sha256":"$ZIPALIGN_SHA","min_sdk":35,"v3":true}
}
EOF

chmod 0600 "$OUTPUT_STAGE"/*
rm -f -- "$KEYSTORE"
[ ! -e "$KEYSTORE" ] || fail "ephemeral QA private key was retained"
mv "$OUTPUT_STAGE" "$OUTPUT"
COMPLETED=1
trap - EXIT HUP INT TERM
case "$WORK" in /private/tmp/hd-microdroid-extra-materials.*) rm -rf -- "$WORK" ;; esac
echo "materials=$OUTPUT"
echo "main_payload_sha256=$MAIN_SHA"
echo "extra_apk_0_sha256=$EXTRA_SHA_0"
echo "extra_apk_1_sha256=$EXTRA_SHA_1"
echo "private_key_retained=false"
