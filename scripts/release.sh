#!/usr/bin/env bash
# Cut a release: bump version, finalize CHANGELOG, verify, commit, tag and push.
#
# Usage: scripts/release.sh [--dry-run] [--no-push] <version>
#   --dry-run  do everything except commit/tag/push, then restore the touched files
#   --no-push  commit and tag locally but do not push
set -euo pipefail

REPO_URL="https://github.com/maxylev/subscribe"
FILES=(Cargo.toml Cargo.lock CHANGELOG.md)

usage() {
  sed -n '2,6p' "$0" | sed 's/^# \{0,1\}//'
}

die() {
  echo "error: $*" >&2
  exit 1
}

dry_run=0
push=1
version=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --dry-run) dry_run=1 ;;
    --no-push) push=0 ;;
    -h | --help) usage; exit 0 ;;
    -*) usage >&2; die "unknown option: $1" ;;
    *)
      [[ -z "$version" ]] || die "version given twice"
      version="$1"
      ;;
  esac
  shift
done

[[ -n "$version" ]] || { usage >&2; die "missing <version>"; }
version="${version#v}"
semver_re='^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$'
[[ "$version" =~ $semver_re ]] || die "'$version' is not a valid semver version (e.g. 1.2.3 or 1.2.3-rc.1)"
tag="v$version"

cd "$(git rev-parse --show-toplevel)"

# ---- preconditions ---------------------------------------------------------
[[ "$(git rev-parse --abbrev-ref HEAD)" == "main" ]] || die "must be on branch main"
[[ -z "$(git status --porcelain)" ]] || die "working tree is not clean"
git fetch --quiet origin main
[[ "$(git rev-parse HEAD)" == "$(git rev-parse origin/main)" ]] || die "main is not identical to origin/main (pull or push first)"
if git rev-parse -q --verify "refs/tags/$tag" >/dev/null; then die "tag $tag already exists locally"; fi
if git ls-remote --exit-code --tags origin "refs/tags/$tag" >/dev/null 2>&1; then die "tag $tag already exists on origin"; fi

[[ -f CHANGELOG.md ]] || die "CHANGELOG.md not found"
grep -q '^## \[Unreleased\]' CHANGELOG.md || die "CHANGELOG.md has no '## [Unreleased]' section"
unreleased="$(awk '
  /^## \[Unreleased\]/ { in_s = 1; next }
  in_s && /^## \[/ { exit }
  in_s && /^\[[^]]+\]: / { exit }
  in_s { print }
' CHANGELOG.md)"
grep -q '[^[:space:]]' <<<"$unreleased" || die "the [Unreleased] section of CHANGELOG.md is empty"

current="$(awk '
  /^\[package\]/ { in_p = 1; next }
  /^\[/ { in_p = 0 }
  in_p && /^version[[:space:]]*=/ { gsub(/^[^"]*"|".*$/, ""); print; exit }
' Cargo.toml)"
[[ -n "$current" ]] || die "could not read package version from Cargo.toml"

# ---- restore on failure / dry run -----------------------------------------
committed=0
restore() {
  if [[ "$committed" -eq 0 ]]; then
    git checkout --quiet -- "${FILES[@]}" 2>/dev/null || true
  fi
}
trap restore EXIT

# ---- bump Cargo.toml / Cargo.lock ------------------------------------------
if [[ "$current" != "$version" ]]; then
  echo "==> bumping Cargo.toml $current -> $version"
  awk -v v="$version" '
    /^\[package\]/ { in_p = 1; print; next }
    /^\[/ { in_p = 0 }
    in_p && !done && /^version[[:space:]]*=/ { print "version = \"" v "\""; done = 1; next }
    { print }
  ' Cargo.toml > Cargo.toml.new
  mv Cargo.toml.new Cargo.toml
  cargo update --workspace --offline --quiet
else
  echo "==> Cargo.toml already at $version (first release); skipping bump"
fi

# ---- finalize CHANGELOG ----------------------------------------------------
echo "==> finalizing CHANGELOG.md"
today="$(date -u +%Y-%m-%d)"
prev="$(awk '/^## \[[0-9]/ { s = $2; gsub(/[][]/, "", s); print s; exit }' CHANGELOG.md)"
if [[ -n "$prev" ]]; then
  version_link="[$version]: $REPO_URL/compare/v$prev...$tag"
else
  version_link="[$version]: $REPO_URL/releases/tag/$tag"
fi
unreleased_link="[Unreleased]: $REPO_URL/compare/$tag...HEAD"
awk -v v="$version" -v d="$today" -v vl="$version_link" -v ul="$unreleased_link" '
  /^## \[Unreleased\]/ { print; print ""; print "## [" v "] - " d; next }
  /^\[Unreleased\]: / { if (!linked) { print ul; print vl; linked = 1 } next }
  { print }
  END { if (!linked) { print ""; print ul; print vl } }
' CHANGELOG.md > CHANGELOG.md.new
mv CHANGELOG.md.new CHANGELOG.md

# ---- verification ----------------------------------------------------------
echo "==> cargo fmt --check"
cargo fmt --all --check
echo "==> cargo clippy"
cargo clippy --all-targets --locked -- -D warnings
echo "==> cargo test"
cargo test --locked

# ---- commit, tag, push -----------------------------------------------------
if [[ "$dry_run" -eq 1 ]]; then
  echo "==> dry run: changes that would be committed as 'chore(release): $tag':"
  git --no-pager diff --stat -- "${FILES[@]}"
  git --no-pager diff -- CHANGELOG.md Cargo.toml
  echo "==> dry run: restoring files; no commit, tag or push performed"
  exit 0
fi

if ! git diff --quiet -- "${FILES[@]}"; then
  git add -- "${FILES[@]}"
  git commit --quiet -m "chore(release): $tag" -- "${FILES[@]}"
fi
committed=1
git tag -a "$tag" -m "$tag"
echo "==> created tag $tag at $(git rev-parse --short HEAD)"

if [[ "$push" -eq 1 ]]; then
  git push --atomic origin main "$tag"
  echo "==> pushed main and $tag; the Release workflow will take over"
else
  echo "==> --no-push: run 'git push --atomic origin main $tag' when ready"
fi
