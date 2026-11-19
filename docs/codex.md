# Codex

`agent = "codex"` in an `[[environment]]` table. The adapter is
`src/agent/codex.rs`; its tests and its part of the privacy gate are in
`tests/codex.rs`.

```toml
[[environment]]
name = "codex-client-a"
root = "/home/agent/client-a"
agent = "codex"
# That login's CODEX_HOME.
config_dir = "/home/agent/.codex-client-a"
# unit = "codex-server@codex-client-a.service"
```

`nucleated hooks print --env codex-client-a` prints the block to register,
and every command in it names `--agent codex`.

Codex has lifecycle hooks of the same shape as Claude Code's, so it is an
agent adapter, not a driver: Codex calls `nucleated hook`, and the daemon
asks Codex nothing. No file under `CODEX_HOME` is read, no socket is opened
and no `codex` process is started, at a heartbeat or at any other time.

## What was read

The adapter is written from Codex's own source and documentation, and from
nothing else:

- openai/codex `main` at commit `39c29c3bb620f91e84a3bb266bb496dfb697c101`
  (2026-11-19). The files that matter are `codex-rs/hooks/src/schema.rs` and
  `codex-rs/hooks/schema/generated/` (hook payloads),
  `codex-rs/protocol/src/hooks.rs` and `codex-rs/config/src/hook_config.rs`
  (the hooks file), `codex-rs/hooks/src/engine/discovery.rs` (where hooks
  load from, trust, time limits) and `codex-rs/utils/home-dir` (`CODEX_HOME`).
- The hooks, auth, remote-connections and CLI reference pages under
  <https://learn.chatgpt.com/docs/>, fetched 2026-11-20.

That commit is not a release, and the pages carry no version. Before the
gate is trusted for a Codex release, check that release's payload fields
against the list in `tests/codex.rs`.

## Registering the hooks

`nucleated hooks print` writes the whole of a Codex `hooks.json`. Its only
top-level key is `hooks`, because Codex refuses a hooks file with a key it
does not know. There are two places to put it.

**Per login.** Write the output of `nucleated hooks print --env <name>` to
that login's `$CODEX_HOME/hooks.json` (`~/.codex/hooks.json` when
`CODEX_HOME` is not set). Codex does not run a hook from a user's own file
until its owner has reviewed it: open Codex as that login, run `/hooks` and
approve them. The approval is tied to the hook's exact text, so it is asked
for again whenever the command changes. Until then Codex skips the hooks
without saying so, and the environment shows as alive with no sessions.

**Box-wide.** Write the output of `nucleated hooks print --agent codex` to
`/etc/codex/hooks.json`, as root. Hooks from the system file run without a
review, for every Codex user on the box. The commands name no environment,
so a hook is placed by its login's `CODEX_HOME`: set `config_dir` on every
Codex environment to that login's `CODEX_HOME`, and start each login's Codex
with `CODEX_HOME` set. That hooks from the system file need no review was
read from Codex's source; its documentation says only that managed hooks are
trusted by policy.

`nucleated` writes neither file itself, writes nothing into `config.toml`,
and never uses `--dangerously-bypass-hook-trust`. If the login also keeps a
`[hooks]` table in `config.toml`, Codex warns that one layer has hooks in two
forms; keep them in `hooks.json`.

Hooks are on by default in Codex. `[features] hooks = false` in its config
turns them off, and then nothing is reported.

## Which login a hook belongs to

In this order: the `--env` in the command; else the Codex environment whose
`config_dir` is the `CODEX_HOME` the hook runs with; else the Codex
environment whose nucleus holds the session's folder; else the only Codex
environment there is. Two Codex logins that share a nucleus can be told
apart only by the first two. A hook that cannot be placed records nothing.

Codex runs a hook with its session's own environment, so `CODEX_HOME` is
there whenever Codex itself was started with it.

## What is read, and what each event becomes

From every hook's standard input the adapter reads `session_id` and `cwd`.
From `SessionStart` it also reads `source`. It reads nothing else: the
parser names those three keys and skips every other value without keeping
it. The event is the one named on the command line, not the payload's
`hook_event_name`.

`session_id` is sent. `cwd` is used on the box to find the session's repos
and branch and is never sent. `source` is compared with one word and
dropped.

| Codex event | Reported as |
|---|---|
| `SessionStart` | `session_start`, unless `source` is `compact`: then nothing |
| `UserPromptSubmit` | `user_prompt_submit` |
| `PermissionRequest` | `permission_prompt` |
| `PostToolUse` | `activity` |
| `Stop` | `stop` |
| `Interrupt` | `stop` |
| `SessionEnd` | `session_end` |
| `PreToolUse`, `PreCompact`, `PostCompact`, `SubagentStart`, `SubagentStop`, `PreModelRequest`, `PostModelRequest`, anything else | nothing; no command is registered for these |

`Interrupt` is the owner stopping a turn. Codex runs no `Stop` for a turn
that was interrupted, so without it the session would look as if it were
still working.

All but `SessionStart` and `SessionEnd` are registered to run in the
background. Codex always runs `SessionEnd` in the foreground and gives it,
and `Interrupt`, at most three seconds; the entry point gives up after two.

`SessionStart` is registered without `async`, so Codex waits for it. Codex
runs it when a session's first turn begins, just ahead of that turn's
`UserPromptSubmit`, and the box orders a session's events by the time each
hook process took. With both in the background the two could be recorded
either way round, and a session whose start landed second would show as idle
through a turn that was running. Codex's hooks page says it waits for a hook
that is not `async` before continuing what triggered it, so the start is
always recorded first. The wait is the time it takes to write one small
file, and never more than the entry point's two seconds. If you registered
an earlier block that had `"async": true` on `SessionStart`, print the block
again and replace it; Codex asks for the review again because the text
changed. A payload that is cut
short, empty, not JSON, not an object, or has no `session_id` records
nothing, and the hook still exits 0 without printing.

Fields that sit beside the three and are skipped unread: `transcript_path`,
`agent_transcript_path`, `prompt`, `last_assistant_message`, `tool_name`,
`tool_use_id`, `tool_input`, `tool_response`, `model`, `turn_id`,
`permission_mode`, `agent_id`, `agent_type`, `stop_hook_active`, `reason`,
`trigger`, and on the model-request events `status`, `error`, `kind`,
`request_id`, `response_id`, `thread_id`, `token_usage` and
`attempt_number`. The gate in `tests/codex.rs` puts a sentinel in every one
of them for every event and finds it nowhere afterwards.

## What Codex gives the box no way to report

Each of these was looked for in the hooks, in the files Codex documents and
in its CLI reference. Where nothing was found, nothing is reported.

| Not reported | Why |
|---|---|
| A turn that failed (`stop_failure`) | No hook runs when a turn ends in an error. `PostModelRequest` can say `failed`, but about one attempt that may be retried, and its `error` is a sentence. |
| A usage limit and when it resets | No hook carries a machine-readable error kind or a reset time. The limit message is prose with the time in words, and is not parsed. |
| Waiting on input other than an approval | Only approval has a hook. There is none for a question to the owner or an MCP elicitation. |
| When the login expires | Codex documents no expiry. The one file that might hold one is the credentials file, which `nucleated` does not read, and the credentials may be in the system keyring. |
| The installed version | No documented file holds it. `version.json` in `CODEX_HOME` is the newest release Codex has seen offered. Asking would mean starting `codex`. |
| A link that opens the session on a phone | No documented address opens a Codex session in the ChatGPT app. No vendor session id is sent beside `session_id`. |

A failed turn, a usage limit and waiting on input do exist as machine-
readable state in Codex's app-server protocol. That is a connection the
daemon would have to hold open to a server Codex labels experimental, and
its stream carries message and tool content as well. It is not used.

What follows from the gaps:

- A turn that fails fires no hook, so its session stays "working" until the
  next prompt or until `SessionEnd`. Codex runs `SessionEnd` when a
  conversation is archived or deleted, when Codex closes normally, or after
  30 minutes idle with no client connected.
- `PermissionRequest` runs before the approval is settled, including when an
  automatic reviewer settles it with nobody waiting. A "needs input" can
  clear a moment later on the next `PostToolUse`.
- A denied approval runs no `PostToolUse`, so the session can stay in "needs
  input" until its next tool call or `Stop`.
- Tool hooks inside a subagent carry the root session's id, so a subagent's
  tool use counts as the session's activity.
- `Interrupt`, `SessionEnd` and the model-request events were read from
  `main`. Whether an older Codex release tolerates a hooks file that names
  an event it does not have was not checked.

## The agent server

`server_alive` is what the environment's `unit` or `check` says, as for any
agent. `contrib/systemd/codex-server@.service` is an example user unit, one
instance per login, that runs `codex remote-control --no-daemon` with that
login's `CODEX_HOME`. Codex's reference labels `remote-control` and the app
server experimental. Codex's own managed daemon (`codex remote-control
start`) is a detached process systemd does not supervise, so a unit cannot
stand for it; use `check` with a command of your own if you run that.

Active means the process is up, not that a phone is paired. OpenAI's
remote-connections page says phone setup starts from the desktop app and
cannot be done from the CLI, while the CLI reference documents `codex
remote-control pair`; whether a headless Linux box can be paired with the
phone app is not established.

## When Codex is absent or broken

Nothing in `nucleated` calls Codex. With Codex not installed, its hooks not
registered or not yet reviewed, no events arrive and the heartbeat still
reports the box and the unit's true state. The hook entry point exits 0
within two seconds and prints nothing whatever it is handed.
