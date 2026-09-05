# Release notes

The section matching the tag being built becomes the body of the GitHub release.
Add a new `## vX.Y.Z` heading before tagging; a tag with no section here still
releases, it just carries the generated changelog alone.

## v0.3.0

**The card stopped switching between sessions on its own.** With several agents live it
showed whichever had fired most recently, so four busy sessions traded it back and forth
every couple of seconds — project, agent and icon all flipping while you worked. A rival
now has to have been active ten seconds longer before it takes the card over, which two
genuinely busy sessions never manage. Focusing a terminal window still switches instantly:
that is you pointing at a session, not a race.

**`agent-presence sessions`.** Lists what the daemon is tracking — agent, project,
activity, how long each has been quiet, and which one is on the card. `status` and the
settings editor show the same summary. This is the answer to "why is my profile showing
that repository", which previously required reading the log.

**Two daemons could run at once.** The single-instance lock made the pid file's existence
the lock and its contents the owner, leaving a window where the file existed but named
nobody — and a second daemon reading it there decided the lock was stale and took it. Both
then bound the control socket, and since binding unlinks whatever it finds, the newcomer
silently stole every event from the incumbent while the two cleared each other's card. The
lock is now held by the kernel through `flock`, which also releases it correctly when a
daemon is killed outright.

**`agent-presence update` reported restarts it had not performed.** It signalled the old
daemon and launched the replacement immediately, but the old one still had to clear the
Discord card first — a round trip that can wait on a socket that never answers. The
replacement lost the lock and exited, while the success line printed anyway.

**Installing no longer rewrites your settings.json.** Adding hook entries reordered the
entire file alphabetically, because the JSON library sorts keys by default. Anyone
tracking their dotfiles got a diff over every unrelated setting.

**`hidden_paths` no longer drops brace patterns.** `~/{work,clients}/**` was split on its
comma into two fragments, neither of which compiles as a glob, so both were discarded and
the repositories you meant to hide appeared on the card. The editor now also refuses a
pattern that would not compile, rather than leaving it to fail quietly at load time.

**Settings are picked up without a restart**, the editor is grouped into sections with a
live status line, link buttons are editable there instead of only by hand, and `w` shows
what changed in the version you are running. `agent-presence on` and `off` flip the card
from the command line.

**Optional auto-update**, off by default. With `auto_update = true` the daemon installs a
newer release through whatever package manager owns the binary, but only as it shuts down
with no session live — replacing the binary mid-turn would leave the next hook pointing at
a file that no longer exists. Nothing ever self-overwrites; a standalone install has no
manager to delegate to and is skipped.

Also: the card reports context compaction and stops claiming to be delegating once a
subagent has finished; approvals show up immediately rather than several seconds late;
Codex sessions end when they end instead of lingering until the idle timeout; a hook
whose daemon is still starting retries delivery instead of losing the event; and the
control socket is owner-only.

## v0.2.3

**`agent-presence update`.** It works out what installed the binary — Homebrew, Scoop,
`cargo install`, or a file you placed yourself — runs that manager's own upgrade, and
restarts the daemon on the new build. Nothing self-overwrites: a Homebrew binary that
replaced itself would leave the Cellar disagreeing with its receipt and break every
later `brew upgrade`. `--check` reports what is available without installing anything.

**The card comes back on its own now.** A daemon that died mid-session — a crash,
`agent-presence stop`, or a package upgrade replacing the binary underneath it — stayed
dead until you happened to open a new agent session, because only `SessionStart` was
allowed to start one. Any hook event can now, so the gap closes on your next tool call.
A five-second cooldown and an O_EXCL lock on the pid file keep a burst of events from
spawning more than one.

**Optional update check.** With `update_check = true` (the default) the daemon asks
GitHub once a day and caches the answer, so `status` and `doctor` can mention a newer
release without going to the network themselves. It only ever reports — installing stays
a command you run.

## v0.2.2

**`detail = "full"` no longer publishes anything it has not reduced first.** It used to
send the first line of every shell command to Discord as typed, which meant an API key in
`STRIPE_KEY=… ./deploy.sh`, a bearer token in a `curl -H` flag, or the credentials inside
a `postgres://user:pass@host` string went out to anyone who could see your profile. Web
fetches sent the full URL including its query string, and web searches sent the query.

Now every field is reduced to a shape that cannot carry a secret. A command keeps its
program and subcommand and nothing else — `git push origin main 2>&1 | tail -3` becomes
`git push`, and `STRIPE_KEY=… ./deploy.sh` becomes `deploy.sh`. A URL keeps only its
host. A search query is not sent at all. This is an allowlist rather than a scan for
secret-looking text, because the scan is the approach that always has one more hole in it.

If you run at `detail = "full"`, upgrading is worth doing now. `generic` and `project`
were never affected — neither one sends the command in the first place.

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
