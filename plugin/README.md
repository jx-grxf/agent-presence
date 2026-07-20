# agent-presence plugin

Wires Claude Code's lifecycle hooks to `agent-presence` without editing
`~/.claude/settings.json` by hand.

```
/plugin marketplace add jx-grxf/agent-presence
/plugin install agent-presence@jx-grxf
```

**The binary still has to be installed and on `PATH`** — a plugin cannot carry a native
binary per platform:

```bash
brew install jx-grxf/tap/agent-presence   # or scoop, or cargo install
```

Without it the hooks fail to spawn and are logged; they never block a tool call. The
plugin covers Claude Code only. For Codex, run `agent-presence install --agent codex`.

Use either the plugin or `agent-presence install`, not both — the daemon would receive
every event twice. That is harmless (same session, same activity) but pointless.
