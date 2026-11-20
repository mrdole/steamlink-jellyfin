#!/usr/bin/env bash
# Tag and publish the version in VERSION.        scripts/release.sh [--dry-run]
# Before: scripts/bump-version X.Y.Z[-alpha.N], commit, merge to main through a PR (CI green).
# This: checks main is clean and equal to origin/main, runs scripts/check.sh, makes the annotated tag vX.Y.Z...,
# pushes it, and creates the GitHub release with that version's CHANGELOG section (a pre-release for -alpha/-beta/-rc).
# Source only: no binaries are attached yet (ROADMAP).
set -euo pipefail
HERE=$(cd "$(dirname "$0")/.." && pwd)
cd "$HERE"
DRY=${1:-}
V=$(cat VERSION); TAG="v$V"

[ "$(git rev-parse --abbrev-ref HEAD)" = main ] || { echo "release from main only" >&2; exit 1; }
[ -z "$(git status --porcelain)" ] || { echo "working tree not clean" >&2; exit 1; }
git fetch --quiet origin main --tags
[ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] || { echo "main differs from origin/main: pull/push first" >&2; exit 1; }
git rev-parse -q --verify "refs/tags/$TAG" >/dev/null && { echo "tag $TAG exists" >&2; exit 1; }
scripts/check.sh

NOTES=$(python3 - "$V" <<'PY'
import re, sys
s = open("CHANGELOG.md").read()
m = re.search(r"^## \[" + re.escape(sys.argv[1]) + r"\][^\n]*\n(.*?)(?=^## \[|\Z)", s, re.S | re.M)
body = (m.group(1).strip() if m else "")
if not body:
    sys.exit("CHANGELOG section for this version is empty")
print(body)
PY
)
PRE=(); [[ $V == *-* ]] && PRE=(--prerelease)
echo "release $TAG ${PRE[*]:-}"; echo "$NOTES"
[ "$DRY" = --dry-run ] && { echo "(dry run: nothing tagged or pushed)"; exit 0; }

git tag -a "$TAG" -m "steamlink-jellyfin $V"
git push origin "$TAG"
gh release create "$TAG" "${PRE[@]}" --title "steamlink-jellyfin $V" --notes "$NOTES
$( [[ $V == *-alpha* ]] && printf '\n> **Early alpha.** Source only; build with the Steam Link SDK (docs/INSTALL.md).' )"
