#!/usr/bin/env bash
set -euo pipefail

repository="${1:-}"
sha="${2:-}"

if [[ ! "$repository" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]]; then
  echo "invalid repository syntax: $repository" >&2
  exit 64
fi

owner="${repository%%/*}"
case "$owner" in
  ORESoftware|takoda-automation|file-tunnel|shared-auth|beamscale|scintilla-run|lunatic-lorry|wasm-xprs|pony-expres|graal-show|gha-indie-worker|litegraph|iso-lattes)
    ;;
  *)
    echo "repository owner is not approved for private certification: $owner" >&2
    exit 65
    ;;
esac

if [[ ! "$sha" =~ ^[0-9a-f]{40}$ ]]; then
  echo "sha must be a full lowercase 40-character commit id" >&2
  exit 66
fi

printf 'repository=%s\n' "$repository"
printf 'sha=%s\n' "$sha"
