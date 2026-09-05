#!/usr/bin/env bash
# Build the body of a GitHub release.
#
#   release-notes.sh <tag> [notes-file]
#
# Layout: how to install it, then the hand-written notes for this tag, then a pointer to
# the generated commit list the workflow appends below. Install instructions lead because
# most people arriving at a release page want the binary, not the changelog.
#
# Exits 0 with a usable body even when the tag has no section, so a release is never
# blocked by missing notes.
set -euo pipefail

tag="${1:?usage: release-notes.sh <tag> [notes-file]}"
file="${2:-RELEASE_NOTES.md}"
version="${tag#v}"

cat <<EOF
### Install

\`\`\`bash
brew install jx-grxf/tap/agent-presence        # macOS, Linux
scoop install jx-grxf/agent-presence           # Windows
cargo install --git https://github.com/jx-grxf/agent-presence --tag ${tag}
\`\`\`

Already installed? \`agent-presence update\` upgrades through whatever package manager
owns the binary. Then run \`agent-presence\` once to wire up Claude Code and Codex, and
\`agent-presence doctor\` to check it took.

EOF

if [ -f "$file" ]; then
  # Everything between this version's heading and the next one at the same level.
  notes="$(
    awk -v want="## ${tag}" '
      $0 == want            { collecting = 1; next }
      collecting && /^## /  { exit }
      collecting            { print }
    ' "$file" |
      # Trim the blank lines the heading boundaries leave behind.
      awk 'NF {found = 1} found {print}' |
      awk '{lines[NR] = $0} END {last = NR; while (last > 0 && lines[last] ~ /^[[:space:]]*$/) last--; for (i = 1; i <= last; i++) print lines[i]}'
  )"
  if [ -n "$notes" ]; then
    printf '### What changed in %s\n\n%s\n\n' "$version" "$notes"
  fi
fi

cat <<'EOF'
### Verifying a download

Every archive ships a `.sha256` beside it:

```bash
shasum -a 256 -c agent-presence-*.tar.gz.sha256
```

---
EOF
