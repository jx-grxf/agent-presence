# Release notes

The section matching the tag being built becomes the body of the GitHub release.
Add a new `## vX.Y.Z` heading before tagging; a tag with no section here still
releases, it just carries the generated changelog alone.

## v0.2.1

**Setup now walks you through it.** Running `agent-presence` with no arguments opens a
first-run wizard: it finds which agents you have, asks how much the card should reveal
while previewing it live, asks whether to follow the focused window, installs the hooks,
and then reads the config files back to confirm. Quitting before the install step leaves
your machine untouched.

This exists because package managers cannot do that step for you. Homebrew sandboxes
`post_install` and forbids a formula from writing outside its own prefix, so
`~/.claude/settings.json` is out of reach — verified against a probe formula rather than
assumed, because the write is denied *silently*. Scoop has no such restriction and now
wires up your agents during `scoop install`.

## v0.2.0

**An interactive settings menu.** `agent-presence config` edits everything in place with
the Discord card rendered live underneath, built by the same code that talks to Discord
so the preview cannot drift from what actually gets sent. That matters most for
`detail`: watching the repository name appear the moment you leave `generic` says more
than any paragraph about it. The TOML file stays the source of truth and hand-editing is
unaffected.

`status`, `doctor` and `install` group their output, `doctor` spins while it waits on
Discord and ends with the card it would show. All of it collapses to plain text when
stdout is not a terminal or `NO_COLOR` is set, so piping into a file or `grep` still
works.

## v0.1.1

**Fixes hooks silently dying on `brew upgrade`.** `current_exe` resolves symlinks, so
the installer wrote the versioned Cellar path into `settings.json`. The next upgrade
deleted that directory and every hook stopped firing with no error anywhere. The
installer now writes whichever `PATH` entry resolves to the same binary, which package
managers keep stable across versions.

If you installed v0.1.0, run `agent-presence install` once to repair the paths.

## v0.1.0

First release.

- Discord Rich Presence for **Claude Code and Codex** from one daemon, on macOS,
  Windows and Linux.
- **Nothing identifying by default** — no repository name, no branch, no file names.
  Everything beyond that is opt-in, and `hidden_paths` overrides it per project.
- Follows the **focused terminal window** when several sessions are live (macOS).
- One static binary, no Node or Python. Adds ~6 ms per agent event and is a strict
  no-op if anything goes wrong: never writes to stdout, always exits 0, never blocks.
- No bot and no token — the connection is authenticated by the Discord desktop client
  you are already signed into.
