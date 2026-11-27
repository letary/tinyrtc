#!/usr/bin/env bash
# A release: X.Y.Z into core/Cargo.toml and mediaserver/Cargo.toml and into their Cargo.lock (CI
# builds with --locked), the commit "chore: vX.Y.Z" on main, the tag vX.Y.Z on it, then the push
# of both, which starts .github/workflows/release.yml. It asks before pushing; --push does not ask.
#
#   tools/release.sh 0.1.2 [--push]
#
# Needs: main checked out, nothing uncommitted, not behind origin/main; cargo (core/ uses the
# nightly core/rust-toolchain.toml names).
set -euo pipefail

usage() {
  echo "usage: tools/release.sh X.Y.Z [--push]" >&2
  exit 2
}
fail() {
  echo "release.sh: $*" >&2
  exit 1
}

new= push=ask
for arg in "$@"; do
  case $arg in
    --push) push=yes ;;
    -*) usage ;;
    *) [ -z "$new" ] || usage; new=${arg#v} ;;
  esac
done
[[ $new =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || usage
tag="v$new"

cd "$(dirname "$0")/.."

manifests=(core/Cargo.toml mediaserver/Cargo.toml)
locks=(core/Cargo.lock mediaserver/Cargo.lock)
# The packages whose own version each lock records: core's lock has the core; the media server's
# has itself and the core (a path dependency).
locked_packages=(core/Cargo.lock:tinyrtc mediaserver/Cargo.lock:tinyrtc mediaserver/Cargo.lock:tinyrtc-mediaserver)

# The first `version = ` of a manifest, as release.yml reads it.
version() { grep -m1 '^version = ' "$1" | cut -d'"' -f2; }
# The version a lock records for a package.
locked() { awk -v name="name = \"$2\"" '$0 == name { getline; print; exit }' "$1" | cut -d'"' -f2; }

# Where a release is made from.
[ "$(git symbolic-ref --short -q HEAD)" = main ] || fail "check out main first"
[ -z "$(git status --porcelain --untracked-files=no)" ] || fail "commit or stash the changes first"
git fetch -q origin main
git merge-base --is-ancestor origin/main HEAD || fail "main is behind origin/main: pull first"
! git rev-parse -q --verify "refs/tags/$tag" > /dev/null || fail "the tag $tag exists here"
! git ls-remote --exit-code -q --tags origin "refs/tags/$tag" > /dev/null || fail "the tag $tag exists on origin"

old=$(version core/Cargo.toml)
[ "$old" = "$(version mediaserver/Cargo.toml)" ] || fail "core/ and mediaserver/ say different versions"
[ "$old" != "$new" ] && [ "$(printf '%s\n%s\n' "$old" "$new" | sort -V | tail -1)" = "$new" ] ||
  fail "$new is not after $old"

# From here on a failure puts the files back as they were (they were clean).
committed=false
undo() {
  if [ $? -ne 0 ] && [ "$committed" = false ]; then
    git checkout -q -- "${manifests[@]}" "${locks[@]}"
    echo "release.sh: the version edits are undone" >&2
  fi
}
trap undo EXIT

for f in "${manifests[@]}"; do
  awk -v v="$new" '!done && /^version = "/ { sub(/"[^"]*"/, "\"" v "\""); done = 1 } { print }' "$f" > "$f.tmp"
  mv "$f.tmp" "$f"
  [ "$(version "$f")" = "$new" ] || fail "$f: the version did not change"
done

# Any cargo command without --locked writes the new versions into the lock; offline when the
# registry cache has every crate, so nothing else in the lock moves.
for dir in core mediaserver; do
  (cd "$dir" && { cargo metadata --format-version 1 --offline > /dev/null 2>&1 || cargo metadata --format-version 1 > /dev/null; })
done
for pair in "${locked_packages[@]}"; do
  [ "$(locked "${pair%%:*}" "${pair#*:}")" = "$new" ] || fail "${pair%%:*} does not lock ${pair#*:} at $new"
done
# Nothing but those version lines may have changed in the locks.
other=$(git diff -U0 -- "${locks[@]}" | grep -E '^[-+]' | grep -vE '^(---|\+\+\+) ' |
  grep -vxF -e "-version = \"$old\"" -e "+version = \"$new\"" || true)
[ -z "$other" ] || fail "the locks changed beyond the versions:"$'\n'"$other"

git add -- "${manifests[@]}" "${locks[@]}"
git commit -q -m "chore: $tag"
committed=true
git tag "$tag"
echo "release.sh: $old -> $new, committed $(git rev-parse --short HEAD) and tagged $tag"

if [ "$push" = ask ]; then
  push=no
  if [ -t 0 ]; then
    read -r -p "Push main and $tag to origin (starts the release)? [y/N] " answer
    [[ $answer =~ ^[Yy] ]] && push=yes
  fi
fi
if [ "$push" = yes ]; then
  git push -q --atomic origin main "$tag"
  echo "release.sh: pushed; the release: .github/workflows/release.yml on GitHub Actions"
else
  echo "release.sh: not pushed. To release:  git push --atomic origin main $tag"
  echo "release.sh: to take it back:         git tag -d $tag && git reset --hard HEAD~1"
fi
