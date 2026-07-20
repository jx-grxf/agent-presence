#!/usr/bin/env bash
# Commit one rendered manifest into a tap repository.
#
#   publish.sh <repo-url> <local-file> <path-in-repo> <tag>
#
# Idempotent: re-running a release that changed nothing exits without a commit.
set -euo pipefail

repo="${1:?usage: publish.sh <repo-url> <local-file> <path-in-repo> <tag>}"
src="${2:?}"
dest="${3:?}"
tag="${4:?}"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

git clone --depth 1 "$repo" "$work"
mkdir -p "$(dirname "${work}/${dest}")"
cp "$src" "${work}/${dest}"

cd "$work"
# Staged first, so a manifest that does not exist in the tap yet counts as a change.
git add "$dest"
if git diff --cached --quiet; then
  echo "$dest already at $tag"
  exit 0
fi

git -c user.name="release" -c user.email="release@users.noreply.github.com" \
  commit -q -m "agent-presence ${tag}"
git push -q origin HEAD
echo "published $dest at $tag"
