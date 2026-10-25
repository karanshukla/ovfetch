#!/usr/bin/env bash
# Runs the `run:` script of .github/workflows/guard.yml against a stub `gh`,
# so the checks it makes are tested exactly as they ship. Usage:
#   bash .github/tests/guard.sh
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

workflow="$root/.github/workflows/guard.yml"
if command -v yq >/dev/null; then
  yq -r '.jobs.guard.steps[0].run' "$workflow" >"$work/guard"
else
  python3 -c 'import sys, yaml; print(yaml.safe_load(open(sys.argv[1]))["jobs"]["guard"]["steps"][0]["run"])' \
    "$workflow" >"$work/guard"
fi

# The stub answers the two queries guard makes: the changed-files list
# (`@tsv`) and the ledger patch. GH_FAIL makes the patch query fail.
mkdir "$work/bin"
cat >"$work/bin/gh" <<'STUB'
#!/usr/bin/env bash
case "$*" in
  *@tsv*) cat "$STUB_DIR/files.tsv" ;;
  *.patch*)
    [ -z "${GH_FAIL:-}" ] || { echo "gh: HTTP 502" >&2; exit 1; }
    cat "$STUB_DIR/patch" ;;
  *) echo "unexpected gh call: $*" >&2; exit 2 ;;
esac
STUB
chmod +x "$work/bin/gh"

# guard writes files.tsv into its working directory, so it runs in its own.
mkdir "$work/run"
failures=0
# expect <pass|fail> <name> <author> [GH_FAIL]; files.tsv and patch are
# written by the caller.
expect() {
  local want="$1" name="$2" author="$3" got
  if (cd "$work/run" && env PATH="$work/bin:$PATH" STUB_DIR="$work" GH_FAIL="${4:-}" \
    GITHUB_STEP_SUMMARY="$work/summary" REPO=o/r PR=1 AUTHOR="$author" OWNER=karanshukla \
    bash "$work/guard" >/dev/null 2>&1); then got=pass; else got=fail; fi
  if [ "$got" = "$want" ]; then
    echo "ok   $name"
  else
    echo "FAIL $name: wanted $want, got $got"
    failures=$((failures + 1))
  fi
}

files() { printf '%s\n' "$@" >"$work/files.tsv"; }
patch() { printf '%s\n' "$@" >"$work/patch"; }

bot='github-actions[bot]'
append=('@@ -10,3 +10,8 @@' ' provenance = false' '+' '+[[list]]' '+id = "pypi/x/x-1.whl"')

files $'src/net.rs\tmodified\t'
expect pass "the owner may change anything" karanshukla
expect fail "anyone else may not change src/" someone

files $'README.md\tmodified\t'
expect pass "anyone may change the README" someone

files $'data/ledger.toml\tmodified\t' $'data/platforms.toml\tmodified\t'
patch "${append[@]}"
expect pass "discover may append to the ledger" "$bot"
expect fail "nobody else may append to the ledger" someone

patch '@@ -10,3 +10,3 @@' '-digest = "aaaa"' '+digest = "bbbb"'
expect fail "discover may not edit a recorded hash" "$bot"

patch "${append[@]}"
expect fail "an unreadable ledger diff fails" "$bot" 1

printf 'MISSING\n' >"$work/patch"
expect fail "a ledger diff too large for GitHub to show fails" "$bot"

: >"$work/patch"
expect fail "an empty ledger diff fails" "$bot"

files $'data/ledger.toml\tadded\t'
patch "${append[@]}"
expect fail "discover may not add the ledger as a new file" "$bot"

files $'docs/net.rs\trenamed\tsrc/net.rs'
expect fail "moving a file out of src/ is judged on its old path" "$bot"

files $'src/main.rs\tmodified\t'
expect fail "discover may not change src/" "$bot"

[ "$failures" -eq 0 ] || { echo "$failures guard case(s) failed" >&2; exit 1; }
