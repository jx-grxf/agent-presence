#!/usr/bin/env bash
# Commit one rendered manifest into a tap repository.
#
#   DEPLOY_KEY=<private key> publish.sh <ssh-repo-url> <local-file> <path-in-repo> <tag>
#
# Authenticates with a per-repository deploy key rather than a personal access token, so
# a leaked secret can write to exactly one tap and nothing else. Without DEPLOY_KEY the
# script exits successfully and the tap simply stays behind — a release must not fail
# because packaging credentials are missing.
#
# Idempotent: re-running a release that changed nothing exits without a commit.
set -euo pipefail

repo="${1:?usage: publish.sh <ssh-repo-url> <local-file> <path-in-repo> <tag>}"
src="${2:?}"
dest="${3:?}"
tag="${4:?}"

if [ -z "${DEPLOY_KEY:-}" ]; then
  echo "DEPLOY_KEY unset, skipping $repo"
  exit 0
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

key="${work}/id"
umask 077
printf '%s\n' "$DEPLOY_KEY" > "$key"
# github.com's host key, so the clone neither prompts nor blindly trusts the network.
ssh-keyscan -t ed25519 github.com > "${work}/known_hosts" 2>/dev/null
export GIT_SSH_COMMAND="ssh -i ${key} -o IdentitiesOnly=yes -o UserKnownHostsFile=${work}/known_hosts"

clone="${work}/repo"
git clone --depth 1 "$repo" "$clone"
mkdir -p "$(dirname "${clone}/${dest}")"
cp "$src" "${clone}/${dest}"

cd "$clone"
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
