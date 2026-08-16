#!/bin/sh
set -eu

usage() {
  echo "Usage: $0 --active-script <macos-host-runtime-active-upgrade-smoke.sh> --app <HD.app> --evidence-dir <fresh-directory>" >&2
}

ACTIVE_SCRIPT=
APP=
EVIDENCE_DIR=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --active-script) ACTIVE_SCRIPT=$2; shift 2 ;;
    --app) APP=$2; shift 2 ;;
    --evidence-dir) EVIDENCE_DIR=$2; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage; exit 2 ;;
  esac
done

case "$ACTIVE_SCRIPT:$APP:$EVIDENCE_DIR" in
  /*:/*:/*) ;;
  *) usage; exit 2 ;;
esac
[ -x "$ACTIVE_SCRIPT" ] && [ ! -L "$ACTIVE_SCRIPT" ] || {
  echo "active upgrade script must be an executable non-symlink file" >&2
  exit 2
}
[ -d "$APP" ] && [ ! -L "$APP" ] || {
  echo "--app must be a real directory, not a symbolic link" >&2
  exit 2
}
[ ! -e "$EVIDENCE_DIR" ] || {
  echo "refusing to replace evidence: $EVIDENCE_DIR" >&2
  exit 2
}
mkdir -p "$EVIDENCE_DIR"

attempt="$EVIDENCE_DIR/identical-attempt"
if "$ACTIVE_SCRIPT" \
    --old-app "$APP" \
    --target-dir "$APP/Contents/MacOS" \
    --evidence-dir "$attempt" \
    --development-package \
    >"$EVIDENCE_DIR/identical.stdout" \
    2>"$EVIDENCE_DIR/identical.stderr"; then
  echo "identical Host artifacts unexpectedly passed the upgrade gate" >&2
  exit 1
fi
grep -Fq "previous and current Host artifacts are identical" \
  "$EVIDENCE_DIR/identical.stderr" || {
  echo "identical Host rejection did not emit the stable diagnostic" >&2
  exit 1
}
[ ! -e "$attempt" ] || {
  echo "identical Host rejection created runtime evidence/data before failing" >&2
  exit 1
}

link="$EVIDENCE_DIR/app-symlink"
ln -s "$APP" "$link"
symlink_attempt="$EVIDENCE_DIR/symlink-attempt"
if "$ACTIVE_SCRIPT" \
    --old-app "$link" \
    --target-dir "$APP/Contents/MacOS" \
    --evidence-dir "$symlink_attempt" \
    --development-package \
    >"$EVIDENCE_DIR/symlink.stdout" \
    2>"$EVIDENCE_DIR/symlink.stderr"; then
  echo "symlink old-app unexpectedly passed the upgrade gate" >&2
  exit 1
fi
grep -Fq -- "--old-app must be a real directory, not a symbolic link" \
  "$EVIDENCE_DIR/symlink.stderr" || {
  echo "symlink old-app rejection did not emit the stable diagnostic" >&2
  exit 1
}
[ ! -e "$symlink_attempt" ] || {
  echo "symlink old-app rejection created runtime evidence/data before failing" >&2
  exit 1
}

GENERATED_AT=$(date -u '+%Y-%m-%dT%H:%M:%SZ')
cat >"$EVIDENCE_DIR/host-runtime-upgrade-contract-gate.json" <<EOF
{
  "schema_version": 2,
  "generated_at": "$GENERATED_AT",
  "source": "scripts/macos-host-runtime-upgrade-contract-smoke.sh",
  "gates": [
    {
      "name": "host-runtime-upgrade-contract",
      "command": "macos-host-runtime-upgrade-contract-smoke.sh --app <HD.app>",
      "status": "pass",
      "duration_ms": null,
      "log_path": "$EVIDENCE_DIR",
      "summary": "The upgrade runner rejected identical previous/current Host artifacts and a symlink old-app before creating an isolated data root or starting a process."
    }
  ]
}
EOF

echo "result=pass"
echo "negative_cases=2"
echo "evidence=$EVIDENCE_DIR"
