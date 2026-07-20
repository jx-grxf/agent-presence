#!/usr/bin/env bash
# Print the RELEASE_NOTES.md section for one tag.
#
#   release-notes.sh <tag> [notes-file]
#
# Exits 0 with empty output when the tag has no section, so a release is never blocked
# by missing notes — the workflow falls back to the generated changelog alone.
set -euo pipefail

tag="${1:?usage: release-notes.sh <tag> [notes-file]}"
file="${2:-RELEASE_NOTES.md}"
[ -f "$file" ] || exit 0

# Everything between this version's heading and the next one at the same level.
awk -v want="## ${tag}" '
  $0 == want            { collecting = 1; next }
  collecting && /^## /  { exit }
  collecting            { print }
' "$file" |
  # Trim the blank lines the heading boundaries leave behind.
  awk 'NF {found = 1} found {print}' |
  awk '{lines[NR] = $0} END {last = NR; while (last > 0 && lines[last] ~ /^[[:space:]]*$/) last--; for (i = 1; i <= last; i++) print lines[i]}'
