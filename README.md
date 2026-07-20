<div align="center">

# agent-presence

**Discord Rich Presence for Claude Code and Codex.**

Show what your coding agent is doing — live on your Discord profile.

[![CI](https://github.com/jx-grxf/agent-presence/actions/workflows/ci.yml/badge.svg)](https://github.com/jx-grxf/agent-presence/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/jx-grxf/agent-presence?color=blue)](https://github.com/jx-grxf/agent-presence/releases)
[![License](https://img.shields.io/badge/license-MIT-green.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org)
[![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Windows%20%7C%20Linux-lightgrey.svg)](#installation)

</div>

---

```
┌──────────────────────────────────────────┐
│  ▣   Claude Code                         │
│      Editing code · Opus 4.8             │
│      12:34 elapsed                       │
└──────────────────────────────────────────┘
```

A single static binary. No Node, no Python, no bot token.

## Why another one?

Existing Discord presence tools for Claude Code are Node or Python scripts that only
cover one agent, and often only one OS. `agent-presence` is one ~1 MB binary that:

- supports **both Claude Code and Codex** from one daemon
- runs on **macOS, Windows and Linux**
- adds **~6 ms** per agent event, and is a strict no-op if anything goes wrong
- shows **nothing identifying by default** — see [Privacy](#privacy)

## Installation

```bash
# Homebrew (macOS)
brew install jx-grxf/tap/agent-presence

# Scoop (Windows)
scoop bucket add jx-grxf https://github.com/jx-grxf/scoop-bucket
scoop install agent-presence

# Prebuilt binary (macOS arm64/x64, Linux x64, Windows x64)
# https://github.com/jx-grxf/agent-presence/releases/latest

# From source
cargo install --git https://github.com/jx-grxf/agent-presence
```

Claude Code users can skip the hook wiring and take the plugin instead — the binary is
still needed, see [`plugin/`](plugin/README.md):

```
/plugin marketplace add jx-grxf/agent-presence
/plugin install agent-presence@jx-grxf
```

Then wire it into whichever agents you have installed:

```bash
agent-presence install     # detects Claude Code and Codex
agent-presence doctor      # verifies Discord, hooks and daemon
```

That is the whole setup. The daemon starts itself with your next session and shuts
down on its own once you stop coding.

To remove it completely:

```bash
agent-presence install --uninstall
```

## Privacy

The default card shows **no repository name, no branch, and no file names**. It says
which agent is running, what kind of work it is doing, and how long you have been at it.

Anything more is opt-in, in `~/.config/agent-presence/config.toml`:

```toml
# "generic" (default) · "project" · "full"
detail = "generic"

# Show the model name, e.g. "Opus 4.8"
show_model = true

# Always generic for these paths, whatever `detail` says
hidden_paths = ["~/work/**", "~/clients/**"]

# Your own Discord application, if you would rather not use the bundled one
client_id = ""

# Sessions silent this long are dropped (agents can die without saying goodbye)
idle_timeout = "15m"

# Turn the card off without uninstalling the hooks
enabled = true

# With several sessions live, show the one in the focused terminal window.
# Off = always show the most recently active session.
follow_focus = true

# Up to two link buttons on the card
# [[buttons]]
# label = "GitHub"
# url = "https://github.com/jx-grxf/agent-presence"
```

| `detail` | Line 1 | Line 2 |
|---|---|---|
| `generic` | `Claude Code` | `Editing code · Opus 4.8` |
| `project` | `agent-presence · main` | `Editing code · Opus 4.8` |
| `full` | `agent-presence · main` | `Editing code: main.rs · Opus 4.8` |

`hidden_paths` always wins. A repo matching it falls back to `generic` even at
`detail = "full"`.

## What gets shown

| Agent is… | Card says |
|---|---|
| processing your prompt | Thinking |
| writing files | Editing code |
| running shell commands | Running commands |
| reading or searching | Reading code |
| searching the web | Researching |
| running subagents | Delegating to subagents |
| waiting on a permission prompt | Waiting for approval |
| done, awaiting input | Idle |

Running several sessions at once? The card follows **the terminal window you are
looking at**, and appends `+2 more` for the rest. Switch windows and the card switches
with you. The elapsed timer spans your whole coding stretch, not just the newest session.

Focus following is macOS-only for now (Ghostty, iTerm2 and Terminal.app) and needs the
**Automation** permission macOS asks for the first time it queries your terminal. Deny
it, set `follow_focus = false`, or run anywhere else, and the card falls back to the most
recently active session.

## How it works

```
Claude Code / Codex
       │  lifecycle hook, JSON on stdin
       ▼
agent-presence hook          short-lived · ~6 ms · always exits 0
       │  local socket
       ▼
agent-presence daemon        one instance · owns the Discord connection
       │  SET_ACTIVITY over IPC
       ▼
Discord desktop client
```

Both agents already emit lifecycle hooks (`SessionStart`, `PreToolUse`, `Stop`, …) with
almost identical JSON payloads, so one parser handles both.

The daemon exists for two reasons: Discord allows exactly one activity per application
while you may run many sessions, and `SET_ACTIVITY` is rate limited to roughly 5 calls
per 20 seconds — far slower than hooks fire during a busy turn. The daemon coalesces
updates onto a 2-second tick.

**Rich Presence needs no bot and no token.** The connection is authenticated by the
Discord desktop client you are already logged into. The only credential is a public
Application ID.

### Not breaking your agent

The hook binary runs inside your agent's process tree, so it follows three rules:

1. **Never writes to stdout** — Claude Code injects hook stdout into the model's
   context, so anything printed would become text the model reads.
2. **Always exits 0** — exit code 2 would block the agent's tool call outright.
3. **Never blocks** — everything is bounded by a 250 ms timeout. A missing or wedged
   daemon costs a few milliseconds, never a stalled session.

If Discord is closed, the daemon keeps running and reconnects when it comes back.

## Commands

| Command | Does |
|---|---|
| `agent-presence install` | Add hooks to Claude Code and Codex |
| `agent-presence install --uninstall` | Remove them again |
| `agent-presence status` | Daemon, config and Application ID |
| `agent-presence doctor` | Diagnose a card that is not appearing |
| `agent-presence stop` | Stop the daemon |
| `agent-presence debug-activity` | Push a test card, to check the Discord link |

Logs: `~/.config/agent-presence/agent-presence.log`. Raise the level with
`AGENT_PRESENCE_LOG=agent_presence=debug`.

## Discord application

Rich Presence identifies itself with a Discord **Application ID**. This is a public
value, not a secret — there is no bot user, no OAuth and no token, because the
connection is authenticated by the Discord desktop client you are already signed into.

An application ID ships with the binary, so nothing here is required. Set up your own
only if you want the card to carry your own name and artwork:

1. Create an application at [discord.com/developers/applications](https://discord.com/developers/applications).
   Its name becomes the bold first line of the card.
2. Under **Rich Presence → Art Assets**, upload two images keyed exactly `claude`
   and `codex`.
3. Copy the Application ID into your config:

```toml
client_id = "1234567890123456789"
```

Or export `AGENT_PRESENCE_CLIENT_ID` for a quick test without editing the config.

## Troubleshooting

**No card appears.** Run `agent-presence doctor`. Discord must be the desktop app —
the browser client has no IPC socket. Check that *Settings → Activity Privacy → Display
current activity as a status message* is on.

**Card is stuck.** `agent-presence stop`; it restarts with your next session.

**Card does not follow the focused window.** macOS needs to have granted
`agent-presence` Automation access to your terminal — check *System Settings → Privacy
& Security → Automation*. Ghostty is matched by working directory rather than terminal
device, so two sessions in the same repo can tie.

**Hooks not firing.** Restart the agent after `install` — both read their hook config at
startup.

## License

MIT
