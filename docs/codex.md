# Codex

`agent = "codex"` in an `[[environment]]` table. The adapter is
`src/agent/codex.rs`; its tests and its part of the privacy gate are in
`tests/codex.rs`. Tested with Codex CLI 0.172.0.

```toml
[[environment]]
name = "codex-client-a"
root = "/home/agent/client-a"
agent = "codex"
# That login's CODEX_HOME.
config_dir = "/home/agent/.codex-client-a"
# unit = "codex-server@codex-client-a.service"
```

`nucleated hooks print --env codex-client-a` prints the hooks file to
register, and every command in it names `--agent codex`. To a Watch that
takes the field, the environment's heartbeat names its agent, `codex`.

Codex has lifecycle hooks of the same shape as Claude Code's, so it is an
agent adapter, not a driver: Codex calls `nucleated hook`, and the daemon
asks Codex nothing. No file under `CODEX_HOME` is read, no socket is opened
and no `codex` process is started, at a heartbeat or at any other time.

## What it was built from

- Codex's source at tag `rust-v0.172.0`: `codex-rs/hooks/src/schema.rs` and
  `codex-rs/hooks/schema/generated/` (hook payloads),
  `codex-rs/protocol/src/hooks.rs` and `codex-rs/config/src/hook_config.rs`
  (the hooks file), `codex-rs/hooks/src/engine/` (where hooks load from,
  trust, time limits, matchers) and `codex-rs/utils/home-dir` (`CODEX_HOME`).
- Codex 0.172.0 run in a container environment, signed in, under `codex
  exec`, the terminal UI and `codex app-server`, with the vendor's model and
  with a stand-in model. Its hook payloads are the fixtures under
  `tests/fixtures/codex/`, scrubbed, with the sentinel written into every
  field that can carry content. `PreCompact`, `PostCompact`,
  `SubagentStart`, `SubagentStop` and the `SessionStart` sources other than
  `startup` never fired; their fixtures are written from the generated
  schemas at that tag. One session was started from the ChatGPT phone app.
- The hooks file as `nucleated hooks print --agent codex` writes it, run as
  `/etc/codex/hooks.json` with this adapter on 0.172.0: a question in Plan
  mode recorded `input_prompt` and then `activity`, a shell call started no
  `PreToolUse` hook, and `codex exec --ephemeral` recorded nothing.
- The hooks, auth, remote-connections and CLI reference pages under
  <https://learn.chatgpt.com/docs/>.

Codex releases every few days. Before the gate is trusted for a later
release, check that release's payload fields against the fixtures.

## Registering the hooks

`nucleated hooks print` writes the whole of a Codex `hooks.json`. Its only
top-level key is `hooks`, because Codex refuses a hooks file with a key it
does not know.

**Box-wide, pinned on.** Write the output of `nucleated hooks print
--agent codex` to `/etc/codex/hooks.json`, as root, and pin hooks on in
`/etc/codex/requirements.toml`:

```toml
[features]
hooks = true
```

Hooks from the system file run under `exec`, the terminal UI and sessions a
server hosts, and skip Codex's own review of hooks. Without the pin,
`[features] hooks = false` in the login's own `config.toml` silences every
hook with no error, and the environment shows alive with no sessions. With
it, neither that line nor `-c features.hooks=false` turns them off. Codex
did not rewrite the three `/etc/codex` files in any run.

The commands name no environment, so a hook is placed by its login's
`CODEX_HOME`: set `config_dir` on every Codex environment to that login's
`CODEX_HOME`, and start each login's Codex with `CODEX_HOME` set. Codex runs
a hook in the session's folder with its server's environment, `CODEX_HOME`
included.

**Per login.** `nucleated hooks print --env <name>` to that login's
`$CODEX_HOME/hooks.json`. `codex exec` skips hooks from a login's own file,
and the terminal UI asks "Hooks need review" at every start until its owner
runs `/hooks` and approves them, again whenever a command changes. Until
then nothing is reported.

`nucleated` writes neither file itself, writes nothing into `config.toml`,
and never uses `--dangerously-bypass-hook-trust`. Keep hooks in
`hooks.json`, not in a `[hooks]` table in `config.toml`.

**Folder trust.** Codex asks "Trust this folder?" for a git checkout, not
for a plain folder, and no hook says it is asking. Until a folder is
trusted, `codex exec` runs there with a read-only sandbox. Trust the folder
the server starts in ahead of time, in `/etc/codex/config.toml`:

```toml
[projects."/home/agent/client-a"]
trust_level = "trusted"
```

It takes effect for hosted sessions with no server restart. Trusting a
parent folder does not cover a checkout beneath it.

## Which login a hook belongs to

In this order: the `--env` in the command; else the Codex environment whose
`config_dir` is the `CODEX_HOME` the hook runs with; else the Codex
environment whose nucleus holds the session's folder; else the only Codex
environment there is. Two Codex logins that share a nucleus can be told
apart only by the first two. A hook that cannot be placed records nothing.

## What is read, and what each event becomes

From every hook's standard input the adapter reads `session_id` and `cwd`,
and whether `transcript_path` is null. From `SessionStart` it also reads
`source`, and from `PreToolUse` `tool_name`. It reads nothing else: the
parser names those keys and skips every other value without keeping it. The event is the one named on the
command line, not the payload's `hook_event_name`.

`session_id` is sent. `cwd` is used on the box to find the session's repos
and branch and is never sent. `source` and `tool_name` are each compared
with one word and dropped. A `transcript_path` that is not null is skipped
unread.

| Codex event | Reported as |
|---|---|
| Any event whose `transcript_path` is null | nothing |
| `SessionStart` | `session_start`, unless `source` is `compact`: then nothing |
| `UserPromptSubmit` | `user_prompt_submit` |
| `PreToolUse` for `request_user_input` | `input_prompt`: a question to the owner |
| `PermissionRequest` | `permission_prompt` |
| `PostToolUse` | `activity` |
| `Stop` | `stop` |
| `Interrupt` | `stop` |
| `SessionEnd` | `session_end` |
| `PreToolUse` for any other tool, `PreCompact`, `PostCompact`, `SubagentStart`, `SubagentStop`, `PreModelRequest`, `PostModelRequest`, anything else | nothing; no command is registered for these |

`PreToolUse` is registered with the matcher `request_user_input`, so Codex
starts no hook for any other tool. Codex calls that tool to ask its owner a
question, in Plan mode among others, and asks for no approval while it
waits. The question's `PostToolUse` puts the session back to working.

`Interrupt` is the owner stopping a turn. Codex runs no `Stop` for a turn
that was interrupted, so without it the session would look as if it were
still working.

All but `SessionStart` and `SessionEnd` run in the background. Codex always
runs `SessionEnd` in the foreground and gives it, and `Interrupt`, at most
three seconds; the entry point gives up after two.

`SessionStart` is registered without `async`, so Codex waits for it. Codex
runs it when a session's first turn begins, just ahead of that turn's
`UserPromptSubmit`, and the box orders a session's events by the time each
hook process took. With both in the background the two could be recorded
either way round, and a session whose start landed second would show as idle
through a turn that was running. The wait is the time it takes to write one
small file, and never more than the entry point's two seconds. If you
registered an earlier file, print it again and replace it.

A payload that is cut short, empty, not JSON, not an object, or has no
`session_id` records nothing, and the hook still exits 0 without printing.

Fields beside those that are skipped unread: `transcript_path` (but
whether it is null), `agent_transcript_path`, `prompt`, `last_assistant_message`, `tool_name`
(but on `PreToolUse`), `tool_use_id`, `tool_input`, `tool_response`, `model`,
`turn_id`, `permission_mode`, `agent_id`, `agent_type`, `stop_hook_active`,
`reason`, `trigger`, and on the model-request events `status`, `error`,
`kind`, `request_id`, `response_id`, `thread_id`, `token_usage` and
`attempt_number`. The gate in `tests/codex.rs` puts a sentinel in every one
of them for every event and finds it nowhere afterwards.

## How a session shows

A session first shows at its first prompt: a hosted session's
`SessionStart` comes with its first prompt, not when a client opens it.

| What happens in Codex | What Watch shows |
|---|---|
| A prompt | working |
| An approval asked for | needs input |
| The approval given | working, at the tool's `PostToolUse` |
| Esc at the approval | idle: Codex ends the turn and runs `Interrupt` |
| A decline that lets the turn go on, sent by a client over Codex's protocol | needs input until `Stop`: no `PostToolUse` runs for a declined tool |
| A question to the owner | needs input, then working when it is answered |
| The turn finishes | idle |
| A client quits | nothing: the session goes on |
| The server stops, or 30 idle minutes pass with no client | ended |

`PermissionRequest` runs before the approval is settled, also when an
automatic reviewer settles it with nobody waiting, so a "needs input" can
clear a moment later. Tool hooks inside a subagent carry the root session's
id, so a subagent's tool use counts as the session's activity. A CLI killed
by a signal runs no `SessionEnd`.

**The title session.** On a server, with the vendor's model, a session's
first prompt starts a second, hidden session that writes the task's title.
It never shows. Its hooks carry `transcript_path: null`, and nothing from a
hook with a null `transcript_path` is recorded. `codex exec --ephemeral`
sends the same, so an ephemeral `exec` never shows either. A session
started from the phone app carries a `transcript_path` and shows.

## What Codex never reports

| Not reported | Why | What Watch shows instead |
|---|---|---|
| A turn that failed | No hook runs when a turn ends in an error. `PostModelRequest` says `failed` once per attempt, with nothing on the last, and its `error` is a sentence | working until the next prompt or `SessionEnd`, and stuck once Watch's quiet limit passes |
| A usage limit and when it resets | No hook carries an error kind or a reset time. The turn fails | as a failed turn |
| A login that expired | No hook says so, and Codex documents no expiry. A server signed out stays up | the environment alive; each turn as a failed turn |
| An MCP server asking the owner something | No hook runs for it | working |
| The installed version | No documented file holds it. Asking would mean starting `codex` | no version |
| A link that opens the session on a phone | No documented address opens a Codex session in the ChatGPT app | what an alert opens today |

These states do exist in Codex's app-server protocol, but its stream
carries message and tool content as well. It is not read.

## The agent server

`server_alive` is what the environment's `unit` or `check` says, as for any
agent. `contrib/systemd/codex-server@.service` is an example user unit, one
instance per login, that runs `codex app-server --listen unix://
--remote-control` in the folder new sessions should start in, with that
login's `CODEX_HOME`. A session a client starts without naming a folder
runs in the server's folder.

That is the form a phone can be paired with: `codex remote-control pair`,
run against it as the same login, prints a code to type into the ChatGPT
phone app. `codex remote-control --no-daemon` cannot be paired. Ctrl-C and
SIGTERM end the server in under a tenth of a second, exit 0, and run
`SessionEnd` for its open sessions.

Signed out, the server stays up, so active does not mean signed in, and
active does not mean a phone is paired. Phone access needs a ChatGPT plan
from Plus up; an API-key login passes `codex login status` and cannot use
it. Codex labels the app server and remote control experimental.

## When Codex is absent or broken

Nothing in `nucleated` calls Codex. With Codex not installed, its hooks not
registered or not yet reviewed, no events arrive and the heartbeat still
reports the box and the unit's true state. The hook entry point exits 0
within two seconds and prints nothing whatever it is handed.
