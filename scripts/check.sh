#!/usr/bin/env bash
# Checks that need no Steam Link SDK: run before every merge (CI runs the same).   scripts/check.sh
#   versions agree (VERSION = daemon/Cargo.toml = Cargo.lock), CHANGELOG has the version or [Unreleased],
#   the daemon compiles and its tests pass on the host, shell and Python scripts parse.
set -euo pipefail
HERE=$(cd "$(dirname "$0")/.." && pwd)
cd "$HERE"
fail=0; ok() { echo "ok   $*"; }; bad() { echo "FAIL $*"; fail=1; }

V=$(cat VERSION)
[[ $V =~ ^[0-9]+\.[0-9]+\.[0-9]+(-(alpha|beta|rc)\.[0-9]+)?$ ]] && ok "VERSION $V is SemVer" || bad "VERSION '$V' is not SemVer"
grep -q "^version = \"$V\"" daemon/Cargo.toml && ok "daemon/Cargo.toml = $V" || bad "daemon/Cargo.toml version != VERSION (scripts/bump-version)"
python3 - "$V" <<'PY' && ok "Cargo.lock = $V" || bad "Cargo.lock sljfd version != VERSION (cargo update -p sljfd)"
import re, sys
lock = open("daemon/Cargo.lock").read()
m = re.search(r'name = "sljfd"\nversion = "([^"]+)"', lock)
sys.exit(0 if m and m.group(1) == sys.argv[1] else 1)
PY
grep -q '^## \[Unreleased\]' CHANGELOG.md && ok "CHANGELOG has [Unreleased]" || bad "CHANGELOG.md needs a '## [Unreleased]' section"
grep -q "^## \[$V\]" CHANGELOG.md && ok "CHANGELOG has [$V]" || bad "CHANGELOG.md has no section for $V"

export PATH="$HOME/.cargo/bin:$PATH"
if command -v cargo >/dev/null; then
    ( cd daemon && cargo check --quiet --locked && cargo test --quiet --locked ) && ok "daemon: cargo check + test (host)" || bad "daemon: cargo check/test"
else
    bad "cargo not found"
fi
for f in scripts/*.sh scripts/bump-version tools/make-sync-test.sh; do bash -n "$f" || bad "bash -n $f"; done
for f in daemon/sljfd.init daemon/arm-cc app/run.sh tools/sljf-memwatch; do sh -n "$f" || bad "sh -n $f"; done
ok "shell scripts parse"
if command -v node >/dev/null; then
    node app/test/guide.test.js >/dev/null && ok "app/guide.js tests" || bad "app/guide.js tests (node app/test/guide.test.js)"
else
    echo "skip app/guide.js tests (node not found)"
fi
if command -v cc >/dev/null; then
    T=$(mktemp -d)
    cc -std=c99 -Wall -Wextra -O2 player/test/static-gen-test.c player/static-gen.c -lm -o "$T/sgt" && "$T/sgt" >/dev/null && ok "player static generators" || bad "player static generators (player/test/static-gen-test.c)"
    cc -std=c99 -Wall -Wextra -O2 player/test/live-buf-test.c player/live-buf.c -o "$T/lbt" && "$T/lbt" >/dev/null && ok "player live-buf" || bad "player live-buf (player/test/live-buf-test.c)"
    cc -std=c99 -Wall -Wextra -O2 player/test/cutplan-test.c player/cutplan.c -o "$T/cpt" && "$T/cpt" >/dev/null && ok "player cutplan" || bad "player cutplan (player/test/cutplan-test.c)"
    rm -rf "$T"
else
    echo "skip player static generators + live-buf + cutplan (cc not found)"
fi
for f in tools/sljf-login tools/sljf-test tools/sljf-chantest tools/sljf-remote app/shot.py; do
    python3 -c "import ast,sys; ast.parse(open(sys.argv[1]).read())" "$f" || bad "python parse $f"
done
ok "python scripts parse"
python3 -c "import json; json.load(open('examples/config.example.json'))" && ok "examples/config.example.json" || bad "example config JSON"
exit $fail
