# nucleate

Multi-repo session workspaces for coding agents, on your own machine.

Each agent session gets its own folder. The session starts empty and adds the
repositories it needs as git worktrees of shared bare clones, all on one
session branch. When the session ends, its folder is removed, unless something
in it exists nowhere else, in which case nothing is touched.

It is built for [Claude Code](https://code.claude.com/docs) (worktree hooks and
an MCP server), but the command line works on its own.

This repository builds two binaries:

- **`nucleate`**, the workspace CLI described first. It also signs a box in to
  [Nucarium Watch](https://nucarium.com) (`nucleate login`).
- **`nucleated`**, the box daemon: a heartbeat, session lifecycle events,
  housekeeping, and a local status view. See [nucleated](#nucleated).

Both are useful with no account. Watch is the hosted half: it tells you the
moment an agent needs you or a box goes dark.

> Nucarium Watch sees which agent is doing what, where and since when — never
> what it was told or what it wrote.

## Install

Download a release archive for Linux from the
[releases page](https://github.com/nucarium/nucarium-cli/releases), or build
from source with `cargo install --path .`. Either way you get both binaries.
They need `git` on the `PATH`.

An archive is named `nucleate-v<version>-<target>.tar.gz`, for
`x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl`, with
`<archive>.sha256` beside it. It holds `nucleate` and `nucleated` at its top
level, both static.

## Layout

Everything lives under one workspace root:

```text
<root>/
  .repos/<repo>.git        bare clones; a repo is available when its clone exists
  .worktrees/<session>/    one folder per session
    .workspace.json        the session's manifest
    AGENTS.md              a note for the agent, listing the session's repos
    <repo>/                a linked worktree on branch workspace/<session>
  .seeds/<repo>/           untracked files to copy into new worktrees
```

Create a nucleus and make a repository available:

```bash
nucleate init --root /home/agent/work
nucleate clone https://github.com/your-org/web.git
```

`nucleate init` creates the three folders under the root (default: the current
folder) and writes `root` to the config file if the file does not name one yet.
It prints `{ "root", "config", "created" }`; running it again changes nothing
and prints `"created": false`.

`nucleate clone <url> [--name NAME]` makes the bare clone at
`.repos/<name>.git` (the name comes from the URL unless given), fetches into
remote-tracking refs so that fetching never rewrites a branch a session has
checked out, and records the remote's default branch. It prints one entry in
the shape `nucleate available` uses: `{ "repo", "label", "bare" }`.

## Commands

```bash
nucleate init                       # create a nucleus here
nucleate clone <url>                # make a repo available
nucleate available                  # repos that can be added
nucleate create-session fix-login   # an empty session
cd <root>/.worktrees/fix-login
nucleate add demo                   # add a repo; the session is the folder you're in
nucleate add demo some-branch       # ...or on a named branch, resuming it if it exists
nucleate status                     # what's in the session and what's unsaved
nucleate remove demo                # remove one repo (--force to discard unsaved work)
nucleate remove-session             # remove the whole session
nucleate refresh                    # fetch every repo and drop branches deleted upstream
nucleate login                      # sign this box in to Nucarium Watch
nucleate whoami                     # which machine and account, and is the token good
nucleate logout                     # revoke the token and forget it
```

`--session <name-or-path>` names a session from anywhere. Every command prints
JSON on standard output; anything meant for a person goes to standard error.

## What counts as unsaved

`nucleate` refuses to remove a worktree that holds anything not stored
elsewhere:

- uncommitted or untracked changes;
- commits that are ahead of the branch's upstream;
- with no upstream, a `HEAD` that is on no remote branch **and** is not the head
  of a merged pull request.

The last check asks GitHub, because a squash merge rewrites the commits and the
branch is usually deleted afterwards. If GitHub can't be reached, the answer is
treated as "not merged". `remove-session` is all or nothing: one unsaved repo
keeps the whole session.

## Configuration

The workspace root is taken from, in order: `--root`, `NUCLEATE_ROOT`, the
config file, or the nearest folder above the current one that contains `.repos`.

`~/.config/nucleate/config.toml` (or the file named by `NUCLEATE_CONFIG`):

```toml
root = "/home/agent/work"
# A file holding a GitHub token, for the merged-pull-request check on private
# repositories. GH_TOKEN or GITHUB_TOKEN in the environment take precedence.
github_token_file = "/run/agent-tokens/github"
# Session branches are named <branch_prefix>/<session>. Default: "workspace".
branch_prefix = "workspace"
```

## Seed files

A custom worktree hook replaces Claude Code's own `.worktreeinclude` handling,
and a bare repository has no working files to copy from. So `nucleate` copies
files matching the repo's committed `.worktreeinclude` from
`<root>/.seeds/<repo>/`, which only you populate (an `.env`, say). No seed, no
copy, no error.

## With Claude Code

Run the Remote Control server in the workspace root with
`claude remote-control --spawn worktree`, and register the two hooks there, in
`<root>/.claude/settings.json`:

```json
{
  "hooks": {
    "WorktreeCreate": [
      { "hooks": [{ "type": "command", "command": "nucleate hook-create", "timeout": 120 }] }
    ],
    "WorktreeRemove": [
      { "hooks": [{ "type": "command", "command": "nucleate hook-remove", "timeout": 120 }] }
    ]
  }
}
```

`nucleate hook-create demo` starts every session with `demo` already added.

Register the MCP server for the user, not the folder, because a session's own
folder is `.worktrees/<session>/` and project-level MCP configuration there
would not be found:

```bash
claude mcp add --scope user nucleate -- nucleate mcp
```

Agents then have four tools: `list_repos`, `add_repo`, `remove_repo` and
`workspace_status`.

## Signing a box in to Watch

A box has no browser, so signing in happens on your phone or laptop. On the
box:

```bash
nucleate login
```

It prints a short code and an address, then waits:

```text
  Sign this box in to Nucarium Watch

  On your phone or laptop, open

      https://app.nucarium.com/activate

  and enter this code:

      █   █ ████    ███ ████        █   █   ███ █   █ █████
      █   █ █   █     █ █   █       ██ ██     █ █   █   █
      █ █ █ █   █     █ ████  █████ █ █ █     █ █████   █
      ██ ██ █   █ █   █ █   █       █   █ █   █ █   █   █
      █   █ ████   ███  ████        █   █  ███  █   █   █

      WDJB-MJHT

  Or open this link, which already carries the code:

      https://app.nucarium.com/activate?user_code=WDJB-MJHT

  Waiting for you to approve. The code lasts 15 minutes. Ctrl-C cancels.
```

Approve it in the console, choosing an existing machine or naming a new one.
The box then says what was just granted, in one line, and prints JSON:

```text
agentbox can now report to Your Org. It can read nothing.
```

```json
{
  "account_name": "Your Org",
  "api_url": "https://api.nucarium.com",
  "logged_in": true,
  "machine_id": "mac_7h2k9m4q1x8z5b6n",
  "machine_name": "agentbox",
  "replaced": false,
  "scope": "ingest",
  "token_file": "/home/agent/.config/nucleated/token"
}
```

The token is a machine token: write-only, bound to this one machine, listed and
revocable in the console. It does not expire. To rotate it, log in again: the
new token is stored, the one it replaces is revoked, and the output says
`"replaced": true`. The token is never printed, by any command.

| Command | What it does |
|---|---|
| `nucleate login` | Runs the sign-in above (the OAuth 2.0 device authorization grant, RFC 8628, client `nucleate`, scope `ingest`) and stores the token. |
| `nucleate login --token-stdin` | Stores a token minted in the console, read from standard input, without the sign-in. For provisioning a box with nobody present. It does not contact Watch and never asks anything. Run where a token is already stored, it replaces it and says `"replaced": true`; storing the same token again is harmless. |
| `nucleate whoami` | Prints the machine, the account, the API, and whether Watch still accepts the token (`"token": "valid"`, `"rejected"` or `"unknown"` when Watch cannot be reached). It changes nothing in Watch, never asks anything, and gives up on a Watch that does not answer within 20 seconds. |
| `nucleate logout` | Revokes the token at Watch (RFC 7009), then deletes it from the box. If Watch cannot be reached the token is still deleted, the output says `"revoked": false`, and a line on standard error says to revoke it in the console. |

Flags for `nucleate login`:

| Flag | Meaning |
|---|---|
| `--api-url <url>` | The Watch API to sign in to. Default `https://api.nucarium.com`. |
| `--name <name>` | The machine name to suggest. Default: `machine` from the daemon's config, else the hostname. You can change it when you approve. |
| `--token-stdin` | Read a token from standard input instead of signing in. |

The API address is taken from, in order: `--api-url`, `NUCARIUM_API_URL`,
`url` in the `[watch]` table of the daemon's config, then the default. Anything
but `https` is refused unless the host is this machine (`localhost`,
`127.0.0.1`, `[::1]`), which is how a local Watch API is used in development.
`login` records the address it signed in to beside the token, and `whoami`,
`logout` and the daemon use that one from then on.

Exit codes, for all three commands:

| Code | Meaning |
|---|---|
| 0 | Done. For `logout` this includes "the token is gone from the box but Watch could not be told", and "there was nothing to log out of". |
| 1 | Something failed: Watch could not be reached, answered unexpectedly, or a file could not be written. |
| 2 | The command line was wrong. |
| 3 | `login`: the request was denied in the console. |
| 4 | `login`: the code expired before anyone approved it. |
| 5 | `whoami`: this box is not signed in. |
| 6 | `whoami`: Watch rejected the stored token. |
| 130 | `login`: cancelled with Ctrl-C. Nothing was stored. |

Where the credential lives:

`nucleate login` writes the credential; `nucleated` reads it.

| File | Mode | Holds |
|---|---|---|
| `~/.config/nucleated/token` | 600 | The machine token and nothing else. |
| `~/.config/nucleated/login.json` | 600 | No secret: the API address, machine id and name, account name, scope and when the box signed in. |

`XDG_CONFIG_HOME` moves both. `token_file` in the `[watch]` table moves the
token, and `login.json` goes beside it. Neither `nucleate` nor `nucleated` uses
a token file that its group or anyone else can read: they say so and behave as
if the box were not signed in.

## nucleated

The daemon runs as the agent user, never as root. It does five things:

| Job | What it does |
|---|---|
| Heartbeat | Every 60 seconds: disk used, load, uptime, memory pressure, and which configured agent servers are alive. |
| Event forwarding | `nucleated hook <event>` writes the allowlisted fields of an agent hook to a local spool. The daemon sends routine events in batches and "needs input" at once. |
| Environment discovery | It reads the agent servers on the box from its config and reports each as an environment. |
| Housekeeping | Fetch and prune, remove sessions that are finished and hold nothing unsaved, and snapshot uncommitted work to a hidden ref. These call the same code as `nucleate`. |
| Local status | `nucleated status` prints what Watch would show, for a box with no account. |

Its target is hosted Watch, or none. Signed in, it reports to Watch. Not signed
in, it sends nothing anywhere and everything else still works.

### Commands

| Command | What it does |
|---|---|
| `nucleated run` | The daemon, in the foreground. This is what the systemd unit starts. Exits 0 on SIGTERM or SIGINT, 1 if its config cannot be read. |
| `nucleated hook <event> [--env NAME]` | The hook entry point. `<event>` is the agent's own event name, unchanged (`SessionStart`, `Notification`, and so on). Reads the hook's JSON on standard input, prints nothing, and exits 0 within two seconds whatever happens. |
| `nucleated status [--json] [--watch]` | The local view, as text. `--json` prints the same data as JSON. `--watch` redraws it every five seconds. |
| `nucleated check` | Validates the config, the token file's permissions, and that each environment's nucleus exists and its agent server can be asked for; when signed in, sends one heartbeat. Prints `{ "ok", "checks": [{ "name", "ok", "detail" }] }`, exits 0 when everything passes and 1 when anything fails. |
| `nucleated init [--env NAME] [--root PATH] [--unit UNIT] [--config-dir DIR]` | Writes a first config file with one environment. Asks nothing. Prints `{ "config", "created" }` and leaves an existing file alone. |
| `nucleated hooks print [--env NAME]` | Prints the `hooks` block to merge into that login's `settings.json`. |
| `nucleated housekeeping run <job> [--env NAME] [--dry-run]` | Runs one job now: `refresh`, `cleanup` or `snapshot`. Prints JSON. `--dry-run` reports what `cleanup` or `snapshot` would do. |
| `nucleated unit` | Prints the systemd user unit below. |

### Configuration

`~/.config/nucleated/config.toml`, or the file named by `NUCLEATED_CONFIG`.
Every key is optional. With no file at all the daemon runs with no
environments and no target.

```toml
# The name suggested for this box at `nucleate login`. Default: the hostname.
machine = "agentbox"

# "hosted" or "none". Left out, it is "hosted" once the box is signed in and
# "none" until then. "none" keeps a signed-in box from sending anything.
target = "hosted"

# Where the daemon keeps its spool and what it knows about sessions.
# state_dir = "/home/agent/.local/state/nucleated"

[watch]
# Only for a Watch API on this machine, in development.
# url = "http://localhost:8787"
# token_file = "/home/agent/.config/nucleated/token"

[heartbeat]
# Watch names the interval its account expects; the daemon uses the longer of
# that and this.
interval = "60s"

# One table per agent server on the box.
[[environment]]
# The environment's name on the box and its first label in Watch. Lower-case
# letters, digits, `.`, `_` and `-`; at most 64 characters.
name = "client-a"
# Its nucleus.
root = "/home/agent/client-a"
# The systemd user unit that runs its agent server. The server is alive while
# the unit is active. Default: agent-server@<name>.service.
unit = "agent-server@client-a.service"
# Instead of `unit`: a command that exits 0 while the agent server is alive.
# check = "tmux has-session -t client-a"
# Which agent runs here. Default and only value today: "claude-code".
agent = "claude-code"
# That login's agent config folder, so a hook can tell which environment it
# belongs to (see Hooks).
config_dir = "/home/agent/.claude-client-a"
# Per-environment overrides of nucleate's settings. Defaults: nucleate's own.
# github_token_file = "/run/agent-tokens/client-a"
# branch_prefix = "workspace"

[housekeeping]
# Each takes "hourly", "daily", "weekly" or "off".
refresh = "hourly"
cleanup = "weekly"
snapshot = "hourly"
# A session younger than this is never cleaned up.
cleanup_min_age = "7d"

[spool]
# dir = "/home/agent/.local/state/nucleated/spool"
max_age = "7d"
max_events = 10000
```

Environment variables: `NUCLEATED_CONFIG` (the config file),
`NUCARIUM_API_URL` (the Watch API, for `nucleate login`),
`XDG_CONFIG_HOME` and `XDG_STATE_HOME`.

Files the daemon keeps, all under `~/.local/state/nucleated/` (or `state_dir`)
and all mode 600:

| Path | Holds |
|---|---|
| `spool/<event_id>.json` | One event waiting to be sent. Written to a temporary name and renamed. Deleted once Watch has it. |
| `spool/rejected/` | Events Watch refused. Kept for `max_age`, never retried. |
| `sessions/<environment>/<session_id>.json` | What the box knows about each session: status, since when, repos, branch. `nucleated status` reads these. |
| `daemon.json` | The daemon's last heartbeat and what it has sent. |
| `housekeeping.json` | When each housekeeping job last ran, and what it did. |
| `snapshots/` | The tree last pushed for each branch, so the same work is not pushed twice. |

### The systemd user unit

`~/.config/systemd/user/nucleated.service`, with the binary at
`~/.local/bin/nucleated`:

```ini
[Unit]
Description=nucleated: heartbeat, session events and housekeeping for this box
Documentation=https://nucarium.dev

[Service]
Type=simple
ExecStart=%h/.local/bin/nucleated run
Restart=always
RestartSec=5
NoNewPrivileges=yes

[Install]
WantedBy=default.target
```

```bash
nucleated unit > ~/.config/systemd/user/nucleated.service
systemctl --user daemon-reload
systemctl --user enable --now nucleated.service
```

The agent user needs lingering (`loginctl enable-linger agent`) for the unit to
start at boot. The daemon picks up a new token or a changed config file by
itself; neither needs a restart. It runs `git` as the agent user, with that
user's git credentials, for housekeeping.

### Hooks

The hook entry point is one command per event, registered in that login's
Claude Code user settings (`~/.claude/settings.json`, or `settings.json` in the
folder `CLAUDE_CONFIG_DIR` names). `nucleated hooks print` prints this block:

```json
{
  "hooks": {
    "SessionStart": [
      { "hooks": [{ "type": "command", "command": "nucleated hook SessionStart", "async": true, "timeout": 5 }] }
    ],
    "UserPromptSubmit": [
      { "hooks": [{ "type": "command", "command": "nucleated hook UserPromptSubmit", "async": true, "timeout": 5 }] }
    ],
    "Notification": [
      {
        "matcher": "permission_prompt|idle_prompt|agent_needs_input|elicitation_dialog|elicitation_url_dialog|elicitation_complete|elicitation_response",
        "hooks": [{ "type": "command", "command": "nucleated hook Notification", "async": true, "timeout": 5 }]
      }
    ],
    "PostToolUse": [
      { "hooks": [{ "type": "command", "command": "nucleated hook PostToolUse", "async": true, "timeout": 5 }] }
    ],
    "Stop": [
      { "hooks": [{ "type": "command", "command": "nucleated hook Stop", "async": true, "timeout": 5 }] }
    ],
    "StopFailure": [
      { "hooks": [{ "type": "command", "command": "nucleated hook StopFailure", "async": true, "timeout": 5 }] }
    ],
    "SessionEnd": [
      { "hooks": [{ "type": "command", "command": "nucleated hook SessionEnd", "timeout": 5 }] }
    ]
  }
}
```

With `--env NAME` each command becomes `nucleated hook <event> --env NAME`.
Without it, the environment is the one whose `config_dir` matches
`CLAUDE_CONFIG_DIR`, else the one whose `root` holds the session's folder, else
the only one configured. A hook that matches no environment does nothing. Two
logins that share a nucleus cannot be told apart by folder: give each its
`config_dir`, or register its hooks with `--env`.

What each event becomes:

| Claude Code event | Sent as | The session becomes |
|---|---|---|
| `SessionStart` | `session_start`; nothing when the session only compacted | idle |
| `UserPromptSubmit` | `user_prompt_submit` | working |
| `Notification` `permission_prompt` | `permission_prompt` | needs input |
| `Notification` `idle_prompt`, `agent_needs_input` | `input_prompt` | needs input |
| `Notification` `elicitation_dialog`, `elicitation_url_dialog` | `elicitation_prompt` | needs input |
| `Notification` `elicitation_complete`, `elicitation_response` | `activity` | working |
| `PostToolUse` | `activity`, only when the session was not already working | working |
| `Stop` | `stop` | idle |
| `StopFailure` | `stop_failure`, with the error kind | failed |
| `SessionEnd` | `session_end` | ended |

`PostToolUse` is how a session leaves "needs input" after you grant a
permission: the tool ran, so the agent is working again. It fires on every tool
call; the daemon drops it unless it changes the session's status, so a busy
session sends nothing extra.

The hook never blocks a session. It prints nothing, exits 0, gives up after two
seconds, and needs neither the daemon nor the network: it only writes a file.

### What leaves the box

Every payload is built from named fields. This is the whole list.

| Sent | From |
|---|---|
| `event_id` | Made on the box: a UUIDv7. |
| `env_id` | The environment's `name` in the config. |
| `session_id` | The hook's `session_id`. |
| `bridge_session_id` | `CLAUDE_CODE_BRIDGE_SESSION_ID`, when the agent sets it. |
| `type` | The table above. |
| `repos` | The session's repos, as `owner/name` from each remote, else the folder name. |
| `branch` | The session's branch. |
| `error_kind` | The hook's `error`, for `StopFailure` only, and only when it is a short kind such as `rate_limit`. |
| `client_ts` | The box's clock. |
| Heartbeat | Disk used %, 1-minute load, uptime, memory pressure %, each environment's name and whether its agent server is alive, the daemon's version, the box's clock. |

Each value is checked against the shape Watch accepts before it is written
down. One that does not fit is left out.

Never read from the hook, never written to disk, never sent: prompts, messages,
session titles, tool inputs and outputs, transcript paths and contents, the
last assistant message, compaction summaries and instructions, error details,
file contents, file paths beyond a repo's name, environment variables, user
names and tokens. The hook's `cwd` is read to find the session's repos and
branch, and goes no further.

`tests/privacy.rs` is the release gate for this. It feeds every hook event a
payload with a sentinel string in every content-bearing field, in the working
directory's path and in the environment, then proves that no sentinel is in the
spool, in the local state, or in any request the daemon makes.

### Spool and replay

The hook writes one file per event. The daemon sends "needs input" events as
soon as it sees them, and everything else every ten seconds or as soon as 50
are waiting, at most 100 to a request. A failed send is retried after one
second, doubling to a ceiling of five minutes, with jitter; after an outage the
spool is replayed in order. Watch deduplicates on `event_id`, so a batch can be
sent twice safely.

An event Watch refuses (a 4xx other than 401, 408 and 429, or an entry in the
response's `rejected` list) moves to `spool/rejected/` and is not retried. If
Watch rejects the token (401), the daemon keeps the spool, stops sending, and
`nucleated status` says to run `nucleate login`. Past `max_events` the oldest
routine events are dropped first and an unsent "needs input" last; events older
than `max_age` are dropped.

With no target, events update the local state and their spool files are deleted
once applied.

### Housekeeping

| Job | Default | What it does |
|---|---|---|
| `refresh` | hourly | `nucleate refresh` for each environment: fetch every bare repo and drop branches deleted upstream. |
| `cleanup` | weekly | Removes a whole session, never one repo out of it, and only when all of these hold: its agent session has ended; no process has a working directory inside it; its last commit and its last agent event are older than `cleanup_min_age`; and every repo in it passes `nucleate`'s unsaved-work rule. Also prunes stale worktree records. |
| `snapshot` | hourly | For each worktree with uncommitted work: commits its state, without touching the index or the branch, and pushes that commit to `refs/wip/<branch>` on the remote, with slashes in the branch name replaced by hyphens. Ignored files are not captured. A worktree with a file changed in the last two minutes is skipped, and so is one whose state was already pushed. |

Whether a process is inside a session is read from `/proc`, so `cleanup`
removes nothing on a system without it. Where two logins share one nucleus,
each job runs over it once, and a session is live while an agent session of
either login is working in it.

## Assumptions

Choices this repository makes where the product spec leaves room:

- **One sign-in, two binaries.** `nucleate login` writes the credential;
  `nucleated` reads it. Both live under `~/.config/nucleated/`, with
  `nucleate`'s own settings staying in `~/.config/nucleate/config.toml`.
- **A credential remembers its API.** The address used at `login` is stored
  beside the token and used from then on, so a token is only ever sent to the
  API that issued it.
- **Logging in again rotates.** The new token is stored first, then the old one
  is revoked.
- **`whoami` checks the token without reporting anything.** It sends Watch an
  empty batch of events: an accepted token is answered with a validation error,
  a rejected one with 401. No heartbeat is sent, so a box whose daemon has
  stopped still looks stopped.
- **A bad token file disables sending, not the daemon.** Housekeeping and local
  status keep running, and `nucleated status` and `nucleated check` say why
  nothing is being sent.
- **An agent server is alive while its systemd user unit is active.** The
  daemon asks; it does not start, stop or restart agent servers. `check` is the
  alternative where there is no unit to ask.
- **The daemon, not the hook, decides what is worth sending.** The hook writes
  every event it is given; the daemon applies them in order to the local state
  and forwards the ones that change a session's status or its "needs input".
- **Hosted Watch or nothing.** There is no setting for another Watch. `url` in
  `[watch]` exists for developing against an API on the same machine.

## Development

```bash
cargo test                 # builds real repositories in temporary folders
cargo test -- --ignored    # also runs the one test that calls the GitHub API
cargo clippy --all-targets -- -D warnings
```

Nothing in the suite talks to Watch. `tests/common` starts a stand-in for its
API on a local port; `tests/login.rs` drives the sign-in against it through
every device-flow error, a slow API and a dead one, and `tests/daemon.rs`
drives the daemon's loop through batching, backoff and replay after an outage.
`tests/privacy.rs` is the release gate described under
[What leaves the box](#what-leaves-the-box): if it fails, do not release.

To run both binaries against a Watch API on this machine, sign in with
`nucleate login --api-url http://localhost:8787`.

Pushing a tag like `v0.1.0` (matching the version in `Cargo.toml`) publishes a
release with one archive per target and its checksum.

## Licence

Licensed under either of the [Apache License, Version 2.0](LICENSE-APACHE) or
the [MIT licence](LICENSE-MIT), at your option. Unless you state otherwise, any
contribution you submit for inclusion in this project is licensed the same way,
with no additional terms.
