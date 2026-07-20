# Release verification

CI compiles, lints and unit-tests on all three platforms. What it cannot cover is the
part that needs a logged-in Discord client and a real agent session, so those checks are
manual and belong on the release checklist.

## macOS

1. `agent-presence doctor` → Discord reachable, hooks installed for every agent present.
2. Start a session, run an edit and a shell command. The card must move
   Thinking → Editing code → Running commands → Idle within ~2 s of each step.
3. Quit Discord mid-session. The daemon logs deferred updates and keeps running; reopen
   Discord and the card returns without restarting anything.
4. Three sessions in three windows: the card follows whichever terminal you focus and
   shows `+2 more`. Close them one at a time; the last close clears the activity.
5. `defaults read` is not involved — but do confirm *System Settings → Privacy &
   Security → Automation* lists the terminal under `agent-presence` after step 4.
6. Privacy: default config in a repo named `secret-thing` → the card must not contain it.
   `detail = "project"` → it appears. Add the path to `hidden_paths` → gone again.

## Windows

Everything above, plus the parts that differ from Unix. Both IPC endpoints are named
pipes here rather than sockets, and the daemon is spawned with `DETACHED_PROCESS`.

1. `scoop install jx-grxf/agent-presence`, then `agent-presence install`.
2. `agent-presence doctor` → confirms `\\.\pipe\discord-ipc-N` was reachable.
3. Run a session end to end. **No console window may flash** when a hook fires or when
   the daemon starts — that is what `CREATE_NO_WINDOW` is for and it is easy to regress.
4. `agent-presence status` → daemon pid; `agent-presence stop` → gone (`tasklist` path).
5. Focus following is not implemented on Windows. Confirm the card falls back to the
   most recently active session rather than clearing.
6. Config lands in `%APPDATA%\agent-presence\`, not `~/.config`.

## Non-interference (must pass on both)

Kill the daemon and delete the control socket, then use the agent normally:

- no text injected into the conversation (hooks must never write to stdout)
- no blocked tool calls
- no perceptible latency — `hyperfine` the hook under 10 ms:

```bash
echo '{"hook_event_name":"Stop","session_id":"bench","cwd":"/tmp"}' > /tmp/ev.json
hyperfine --warmup 3 'agent-presence hook --agent claude < /tmp/ev.json'
```
