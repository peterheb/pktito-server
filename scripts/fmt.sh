#!/bin/bash
# Every source file in the tree in its language's standard form, as the
# pktito app's scripts/fmt.sh keeps the app's: run before each commit.
#
#     scripts/fmt.sh            format in place
#     scripts/fmt.sh --check    change nothing; list what isn't formatted,
#                               and exit 1 if anything isn't
#
# Each formatter at its defaults, pinned as the app pins them: prettier for
# JavaScript, CSS, HTML, JSON and Markdown, cargo fmt for Rust, ruff for
# Python. A formatter this machine has no way to run is said and skipped,
# never silently.
set -u
cd "$(dirname "$0")/.."

PRETTIER=prettier@3.6.2
RUFF=ruff@0.14.3

check=0
[ "${1:-}" = "--check" ] && check=1
bad=0
skip() { echo "fmt: $1 skipped: $2" >&2; }

# JavaScript, CSS, HTML, JSON, Markdown
web=$(git ls-files '*.js' '*.mjs' '*.css' '*.html' '*.json' '*.md')
if [ -n "$web" ]; then
  if command -v npx >/dev/null; then
    if [ $check = 1 ]; then npx --yes $PRETTIER --check --log-level warn $web || bad=1
    else npx --yes $PRETTIER --write --log-level warn $web; fi
  else skip prettier "no npx (Node.js)"; fi
fi

# Rust
if command -v cargo >/dev/null; then
  if [ $check = 1 ]; then cargo fmt --check >/dev/null || { echo "fmt: Rust (cargo fmt)"; bad=1; }
  else cargo fmt; fi
else skip Rust "no cargo"; fi

# Python
py=$(git ls-files '*.py')
if [ -n "$py" ]; then
  if command -v uvx >/dev/null; then
    if [ $check = 1 ]; then uvx -q $RUFF format --check $py || bad=1
    else uvx -q $RUFF format -q $py; fi
  else skip Python "no uvx (uv)"; fi
fi

exit $bad
