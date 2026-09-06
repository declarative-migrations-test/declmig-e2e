#!/bin/sh
# ores-lint :: entry point
#
# Warn-only by default. This is wired into build and publish hooks across
# hundreds of repos, so it is designed to be incapable of breaking one unless a
# human explicitly sets ORES_LINT_STRICT=1.
#
# Both halves discover sub-projects rather than assuming the repo root is the
# only project: `eslint .` walks nested packages from the root config, and
# rust.sh finds every crate including ones buried under apps/ or clients/.

set -u
DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ROOT=$(dirname "$DIR")
. "$DIR/config.sh"

echo "ores-lint v$(cat "$DIR/VERSION" 2>/dev/null || echo '?') :: $(basename "$ROOT")"

FOUND=0
LOG=$(mktemp) || exit 0

# JS: an eslint flat config at the repo root is the trigger. The rollout puts
# one there whenever the repo contains any JS/TS at all, so nested packages in
# a Rust-rooted repo still get linted.
for c in eslint.config.mjs eslint.config.js eslint.config.cjs; do
  if [ -f "$ROOT/$c" ]; then
    sh "$DIR/js.sh" "$ROOT" | tee -a "$LOG"
    FOUND=1
    break
  fi
done

# Rust: any Cargo.toml anywhere in the repo, not just at the root.
if find "$ROOT" -maxdepth "$ORES_LINT_DEPTH" \
     \( -name node_modules -o -name target -o -name .git -o -name vendor -o -name .vendor \) -prune -o \
     -type f -name Cargo.toml -print 2>/dev/null | head -1 | grep -q .; then
  sh "$DIR/rust.sh" "$ROOT" | tee -a "$LOG"
  FOUND=1
fi

[ "$FOUND" = "0" ] && echo "ores-lint: no JS or Rust project found in this repo - nothing to do"

if [ "${ORES_LINT_STRICT}" = "1" ] && grep -q 'finding(s) across' "$LOG"; then
  rm -f "$LOG"
  echo "ores-lint: FAILING because ORES_LINT_STRICT=1"
  exit 1
fi

rm -f "$LOG"
exit 0
