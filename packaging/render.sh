#!/usr/bin/env bash
# Fill the Homebrew formula and Scoop manifest from a release's artifacts.
#
#   render.sh <tag> <artifact-dir> <out-dir>
#
# The artifact dir is the flattened release payload: one archive per target plus a
# matching `.sha256` next to each. Run locally against a downloaded release to
# reproduce exactly what CI publishes.
set -euo pipefail

tag="${1:?usage: render.sh <tag> <artifact-dir> <out-dir>}"
artifacts="${2:?}"
out="${3:?}"
version="${tag#v}"
base="https://github.com/jx-grxf/agent-presence/releases/download/${tag}"

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
mkdir -p "$out"

# Digest for one target, read from the .sha256 CI wrote beside the archive.
digest() {
  local file="${artifacts}/agent-presence-${tag}-${1}.${2}.sha256"
  [ -f "$file" ] || { echo "missing $file" >&2; exit 1; }
  tr -d '[:space:]' < "$file"
}
url() {
  echo "${base}/agent-presence-${tag}-${1}.${2}"
}

render() {
  sed \
    -e "s|@@VERSION@@|${version}|g" \
    -e "s|@@URL_MACOS_ARM@@|$(url aarch64-apple-darwin tar.gz)|g" \
    -e "s|@@SHA_MACOS_ARM@@|$(digest aarch64-apple-darwin tar.gz)|g" \
    -e "s|@@URL_MACOS_X64@@|$(url x86_64-apple-darwin tar.gz)|g" \
    -e "s|@@SHA_MACOS_X64@@|$(digest x86_64-apple-darwin tar.gz)|g" \
    -e "s|@@URL_LINUX_X64@@|$(url x86_64-unknown-linux-gnu tar.gz)|g" \
    -e "s|@@SHA_LINUX_X64@@|$(digest x86_64-unknown-linux-gnu tar.gz)|g" \
    -e "s|@@URL_WINDOWS_X64@@|$(url x86_64-pc-windows-msvc zip)|g" \
    -e "s|@@SHA_WINDOWS_X64@@|$(digest x86_64-pc-windows-msvc zip)|g" \
    "$1" > "$2"
}

render "${here}/agent-presence.rb.tmpl" "${out}/agent-presence.rb"
render "${here}/agent-presence.json.tmpl" "${out}/agent-presence.json"

# A leftover placeholder means a target was renamed without updating the templates.
if grep -l '@@' "${out}"/agent-presence.*; then
  echo "unsubstituted placeholders remain" >&2
  exit 1
fi
echo "rendered ${version} into ${out}"
