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
  unsaved work and what each session left behind, housekeeping, and a local
  status view. See [nucleated](#nucleated).

Both are useful with no account. Watch is the hosted half: it tells you the
moment an agent needs you or a box goes dark.

> Nucarium Watch sees which agent is doing what, where and since when — never
> what it was told or what it wrote.

## Install

On a Mac, with [Homebrew](https://brew.sh):

```bash
brew install nucarium/tap/nucleate
```

On Linux, download a release archive from the
[releases page](https://github.com/nucarium/nucarium-cli/releases). Or build
from source anywhere with `cargo install --path .`. Every route gives you both
binaries. They need `git` on the `PATH`.

An archive is named `nucleate-v<version>-<target>.tar.gz`, with
`<archive>.sha256` beside it, and holds `nucleate` and `nucleated` at its top
level. The targets are `x86_64-unknown-linux-musl` and
`aarch64-unknown-linux-musl` (static, for boxes), and `aarch64-apple-darwin`
and `x86_64-apple-darwin` (for trying it on a Mac; the Homebrew formula
installs these).

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

`nucleate` removes only what it made. Its own worktrees are the ones under the
nucleus's `.worktrees/`. A worktree anywhere else that was added from one of
the nucleus's bare repos, by another tool or by hand, is never removed by
`remove`, `remove-session` or `hook-remove`, and while a repo holds such a
worktree its stale worktree records are not pruned, since `git worktree prune`
would drop that worktree's record too if its folder were away at the time.

`nucleated` counts by this same rule, so what Watch shows as unsaved is what
`nucleate` would refuse to delete: the worktrees with uncommitted changes, and
the commits that are on no remote. A new session branch tracks the
repository's default branch until it is pushed with `git push -u`. Commits
pushed without `-u` are still ahead of that upstream, so they still count.

## Configuration

The workspace root is taken from, in order: `--root`, `NUCLEATE_ROOT`, the
config file, or the nearest folder above the current one that contains `.repos`.

`~/.config/nucleate/config.toml` (or the file named by `NUCLEATE_CONFIG`):

```toml
root = "/home/agent/work"
# A file holding a GitHub token. Every git call nucleate makes signs in to
# https://github.com with it, as x-access-token, in place of the user's own
# credential helper for GitHub; the file is read each time git asks. The
# merged-pull-request check on private repositories uses it too, where
# GH_TOKEN or GITHUB_TOKEN in the environment take precedence.
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

The daemon runs as the agent user, never as root. It does twelve things:

| Job | What it does |
|---|---|
| Heartbeat | Every 60 seconds: disk used, load, uptime, memory pressure, whether the box is on battery and whether it needs a reboot, which configured agent servers are alive and how often each has been restarted, the last count of unsaved work, and how often the daemon passes on a working session's tool activity. |
| Event forwarding | `nucleated hook <event>` writes the allowlisted fields of an agent hook to a local spool. The daemon sends routine events in batches and "needs input" at once. |
| Environment discovery | It reads the agent servers on the box from its config and reports each as an environment. |
| Drivers | For an environment watched through a driver, a tool you run and steer agents with, it asks the driver which agents it has and what each is doing, and reports them as sessions. It reads state and sends the driver no command. See [Drivers](#drivers). |
| Config sync | With a repository of your own named in the config, it keeps each login's agent settings, instructions and skills at one pinned branch, and each heartbeat says which commit an environment is at. Off by default. See [Config sync](#config-sync). |
| Environment setup | It reads what the box recipe set each environment up with, its repositories, network access, counts of variables and secrets, and how its setup script last ran, and each heartbeat says so. It writes nothing there and runs no script. See [Environment setup](#environment-setup). |
| Plan usage | For each Claude Code login, every five minutes it asks the agent vendor how much of the plan is used, and each heartbeat says so. See [Plan usage](#plan-usage). |
| Unsaved work and outcomes | It counts what exists only on the box, on a schedule, and when a session stops or ends it reports what the session left behind: commits made, work not saved, and the addresses of its open pull requests. See [Unsaved work and outcomes](#unsaved-work-and-outcomes). |
| Housekeeping | Fetch and prune, remove sessions that are finished and hold nothing unsaved, and snapshot uncommitted work to a hidden ref. These call the same code as `nucleate`. |
| Maintenance pause | `nucleated pause 2h` tells Watch to hold its alerts for this box going dark while you work on it. See [The maintenance pause](#the-maintenance-pause). |
| Fallback notifier | With an address of your own in the config, the box tells you itself when it cannot reach Watch. Off by default. See [When Watch cannot be reached](#when-watch-cannot-be-reached). |
| Local status | `nucleated status` prints what Watch would show, for a box with no account. |

Its target is hosted Watch, or none. Signed in, it reports to Watch. Not signed
in, it sends nothing anywhere and everything else still works.

The daemon observes. It reports and it tidies, and it never starts, stops,
pauses or resumes an agent or an agent server. Every probe it runs only asks.

### Commands

| Command | What it does |
|---|---|
| `nucleated run` | The daemon, in the foreground. This is what the systemd unit starts. Exits 0 on SIGTERM or SIGINT, 1 if its config cannot be read. |
| `nucleated hook <event> [--env NAME] [--agent NAME]` | The hook entry point. `<event>` is the agent's own event name, unchanged (`SessionStart`, `Notification`, and so on). `--agent` says whose hook it is; without it, the hook is the named environment's agent's, else Claude Code's. Reads the hook's JSON on standard input, prints nothing, and exits 0 within two seconds whatever happens. |
| `nucleated status [--json] [--watch]` | The local view, as text. `--json` prints the same data as JSON. `--watch` redraws it every five seconds. |
| `nucleated check` | Validates the config, the token file's permissions, and that each environment's nucleus (for a driver, its folder) exists and its agent server can be asked for; when signed in, sends one heartbeat. Prints `{ "ok", "checks": [{ "name", "ok", "detail" }] }`, exits 0 when everything passes and 1 when anything fails. |
| `nucleated init [--env NAME] [--root PATH] [--unit UNIT] [--config-dir DIR]` | Writes a first config file with one environment. Asks nothing. Prints `{ "config", "created" }` and leaves an existing file alone. |
| `nucleated hooks print [--env NAME] [--agent NAME]` | Prints the `hooks` block to merge into that login's agent settings: for `--agent`, else for the named environment's agent, else for Claude Code. |
| `nucleated config sync [--env NAME] [--dry-run]` | Syncs agent config now, for one environment or all of them. `--dry-run` prints what would change and changes nothing. Exits 1 when `[config_sync]` is not set. See [Config sync](#config-sync). |
| `nucleated housekeeping run <job> [--env NAME] [--dry-run]` | Runs one job now: `refresh`, `cleanup` or `snapshot`. Prints JSON. `--dry-run` reports what `cleanup` or `snapshot` would do. |
| `nucleated unit` | Prints the systemd user unit below. |
| `nucleated pause <duration>` | Announces maintenance for `<duration>`, at most `24h`: `30m`, `2h`. Prints `{ "paused": true, "until" }`. Exits 2, and pauses nothing, for no time at all, more than a day, or anything that is not a duration. Pausing again replaces the pause. |
| `nucleated resume` | Ends the pause now. Prints `{ "paused": false, "was_paused" }`. Harmless when nothing is paused. |

### Configuration

`~/.config/nucleated/config.toml`, or the file named by `NUCLEATED_CONFIG`.
Every key is optional. With no file at all the daemon runs with no
environments and no target.

A key no table knows is an error that names it, and never a setting quietly
left at its default: `allowed-signers` for `allowed_signers` would otherwise
be a sync that checks no signature. A daemon that is running keeps the config
it had until the file is fixed, and one that is starting does not start.

```toml
# The name suggested for this box at `nucleate login`. Default: the hostname.
machine = "agentbox"

# "hosted" or "none". Left out, it is "hosted" once the box is signed in and
# "none" until then. "none" keeps a signed-in box from sending anything.
target = "hosted"

# Where the daemon keeps its spool and what it knows about sessions.
# state_dir = "/home/agent/.local/state/nucleated"
# Where it keeps what may hold content, which the state folder never does:
# the clone of your config repository and the one copy of what a sync
# replaced. It may not be inside state_dir.
# cache_dir = "/home/agent/.cache/nucleated"
# Where the box recipe writes what each environment is set up with (see
# Environment setup). Read, never written.
# setup_dir = "/var/lib/nucarium/environments"

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
# Which agent runs here: "claude-code" or "codex". Default: "claude-code".
agent = "claude-code"
# That login's agent config folder, so a hook can tell which environment it
# belongs to (see Hooks).
config_dir = "/home/agent/.claude-client-a"
# Per-environment overrides of nucleate's settings. Defaults: nucleate's own.
# github_token_file = "/run/agent-tokens/client-a"
# branch_prefix = "workspace"

# An environment watched through a driver (see Drivers). One table per
# driver's server on the box.
# [[environment]]
# name = "t3"
# The driver: "t3code", "herdr-core", "herdr-tui" or "orca".
# driver = "t3code"
# The folder its agents work under. For a driver that makes its own
# worktrees (Orca) this is the folder it keeps them in, and it may not be,
# hold or sit inside a nucleus. For the others it may be a nucleus.
# root = "/home/agent/client-a"
# The unit that runs the driver's server. Default: the unit the driver's own
# documentation names, where it names one (t3code.service, orca-serve.service).
# A driver that names none needs `unit` or `check` here.
# unit = "t3code.service"
# How often the driver is asked. Default: 15s (5s for Herdr). "off" keeps the
# environment and asks nothing.
# driver_poll = "15s"
# Each driver reads only the keys its page lists; any other is refused.
# driver_home = "/home/agent/.t3"                 # the driver's own data folder
# driver_url = "http://127.0.0.1:3773"            # its server, on this machine only
# driver_token_file = "/home/agent/.config/nucleated/t3code.token"  # read-only, mode 600
# driver_socket = "/run/user/1001/herdr-core/default.sock"
# driver_session = "default"
# driver_cli = "/opt/orca-server/bin/orca-ide"
# driver_dirs = ["/srv/more-orca-worktrees"]

[housekeeping]
# Each takes "hourly", "daily", "weekly" or "off".
refresh = "hourly"
cleanup = "weekly"
snapshot = "hourly"
# A session younger than this is never cleaned up.
cleanup_min_age = "7d"

[report]
# How often unsaved work is counted, once per nucleus: "1m" or longer, or
# "off". A heartbeat reads the last count and never runs git itself.
unsaved = "10m"

[git]
# What the daemon does with a repo whose git config or pointers it will not
# run git in (see Git in session repos): "enforce" refuses it; "report" logs
# and shows the refusal and runs git in it anyway.
vetting = "enforce"

# Only on a box with a UPS that NUT watches. The daemon asks NUT whether the
# box is running on battery. Left out, nothing is said about a battery.
# [ups]
# name = "ups@localhost"

# Only to have the box tell you itself when it cannot reach Watch. Left out,
# the box posts nowhere.
# [notify]
# An address of your own, shaped as ntfy takes a post. https, or http on this
# machine.
# url = "https://ntfy.sh/your-private-topic"
# A file holding a bearer token for that address, mode 600.
# token_file = "/home/agent/.config/nucleated/notify-token"
# How long Watch must have been unreachable before the box says so. At least "2m".
# after = "10m"

[spool]
# dir = "/home/agent/.local/state/nucleated/spool"
max_age = "7d"
max_events = 10000

# Only to keep agent config current from a repository of your own (see
# Config sync). Left out, nothing is fetched and nothing is written.
# [config_sync]
# The repository, as git takes it. It is handed to git on the box and goes
# nowhere else: not to a log, not to the state folder, not to Watch.
# repository = "git@github.com:your-org/agent-config.git"
# The one branch to follow. Required.
# branch = "main"
# How often it is fetched: "hourly", "daily", "weekly" or "off".
# interval = "hourly"
# An OpenSSH allowed-signers file you keep outside that repository. Naming
# one turns require_signed on: a commit not signed by a signer in it is
# refused, and so is one that is not a descendant of the last one applied.
# allowed_signers = "/home/agent/.config/nucleated/config-signers"
# require_signed = true
# What a file changed by hand means when a new commit arrives: "overwrite"
# replaces it, after keeping one copy on the box; "keep" leaves that
# environment alone until the change is undone.
# on_drift = "overwrite"
```

Environment variables: `NUCLEATED_CONFIG` (the config file),
`NUCARIUM_API_URL` (the Watch API, for `nucleate login`),
`XDG_CONFIG_HOME`, `XDG_STATE_HOME` and `XDG_CACHE_HOME`.

Files the daemon keeps, all under `~/.local/state/nucleated/` (or `state_dir`)
and all mode 600:

| Path | Holds |
|---|---|
| `spool/<event_id>.json` | One event waiting to be sent. Written to a temporary name and renamed. Deleted once Watch has it. |
| `spool/rejected/` | Events Watch refused. Kept for `max_age`, never retried. |
| `sessions/<environment>/<session_id>.json` | What the box knows about each session: status, since when, repos, branch, and when an event for it was last sent. `nucleated status` reads these. |
| `daemon.json` | The daemon's last heartbeat and what it has sent, and the names of the fields it knows and has not sent because Watch has not said it takes them. |
| `housekeeping.json` | When each housekeeping job last ran, what it did, and when it last finished with nothing failed. |
| `snapshots/` | The tree last pushed for each branch, so the same work is not pushed twice. |
| `unsaved.json` | The last count of unsaved work, per nucleus: two numbers and two times, and the name of each repo the daemon would not run git in, with one word for why. The heartbeat reads the numbers and times, never the names. |
| `outcomes/<environment>/<session_id>.json` | A note that a session stopped or ended and its repos are to be looked at. Removed when they have been. |
| `merged.json` | GitHub's answers to "was this commit merged?", so the count asks about a commit once an hour at most and never again once it was merged. |
| `pause.json` | The time a maintenance pause ends. Only while one is set. |
| `notify.json` | What the fallback notifier has posted in the outage in progress. Only during one. |
| `drivers/<environment>.json` | What the last look at an environment's driver found: one fixed word for whether the driver answered, since when, each session's id, phase and counters, the few things an adapter learned of its driver once and need not ask again, each a name the adapter chose and a number, for a claim in doubt, its key and since when, and, for a session a hook speaks for that waits or has failed while the hook's session does not show it, its id and since when. No title, message or path. |
| `claims/<environment>/<key>/<session_id>.json` | A note that an agent whose own hook reports it is running inside one of that driver's sessions, so the driver's view of it is not reported a second time. The key is the driver's own id for the session, or a number made from a folder's path; never a name a person chose. Inside are the hook's environment and, for where its session works, numbers made from that folder's path and the paths of the folders it is in; never a path. The hook renews it as it speaks. Removed when the hook's session ends, or when the hook has gone quiet and the driver sees the session doing what the hook's session is not (see Drivers). |
| `usage.json` | Per Claude Code environment, its last good plan usage reading: when it was read, and per limit its window, the model it is for, a percentage and when it resets. No token. |
| `config-sync.json` | Per environment: the commit its config was applied from, whether a synced file was changed by hand, one fixed word for how the sync stands, and two times. No address, branch, path or name. Only with `[config_sync]` set. |

One folder is kept outside it, because it holds content: `~/.cache/nucleated/`
(or `cache_dir`), with the clone of your config repository and the one copy
of what a sync replaced. Nothing in it is ever sent.

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
user's git credentials, for housekeeping and for the count of unsaved work. In
an environment whose `github_token_file` names a file, its git signs in to
GitHub with that file's token instead, so one box can host nuclei whose
repositories belong to different GitHub accounts.

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

Whose hook it is comes before where it lands, and nothing is guessed about
it. A hook is the agent's that its command names with `--agent`, else the
agent of the environment `--env` names, else Claude Code's. Another agent's
block, printed with `nucleated hooks print --agent NAME`, carries `--agent
NAME` in every command, and its payload is read only by that agent's adapter.
The environment is then looked for among those that run that agent, by that
agent's own config-folder variable or by folder. An environment watched
through a driver takes any agent's hooks that are run from inside its folder.

Each agent has one adapter, in a file of its own under `src/agent/`:

| `agent` | Adapter | Its page |
|---|---|---|
| `claude-code` | `src/agent/claude_code.rs` | This section. |
| `codex` | `src/agent/codex.rs` | [docs/codex.md](docs/codex.md) |

Codex keeps its hooks in a `hooks.json` of their own, and `nucleated hooks
print --agent codex` prints the whole of that file, not a block to merge.
There are two places for it. Per login, it goes in that login's
`$CODEX_HOME/hooks.json`, printed with `--env NAME`; Codex runs nothing from
it until that login has opened Codex, run `/hooks` and approved it, and asks
again whenever a command in it changes. Box-wide, it goes in
`/etc/codex/hooks.json` as root, printed without `--env`; each hook is then
placed by its login's `CODEX_HOME`, so every Codex environment needs its
`config_dir`. `nucleated` writes neither file.
`contrib/systemd/codex-server@.service` is an example user unit for one
Codex login's agent server, for the environment's `unit` to name; nothing
installs or starts it. [docs/codex.md](docs/codex.md) has both routes, the
events read and what Codex gives the box no way to report.

What each event becomes:

| Claude Code event | Sent as | The session becomes |
|---|---|---|
| `SessionStart` | `session_start`; nothing when the session only compacted | idle |
| `UserPromptSubmit` | `user_prompt_submit` | working |
| `Notification` `permission_prompt` | `permission_prompt` | needs input |
| `Notification` `idle_prompt`, `agent_needs_input` | `input_prompt` | needs input |
| `Notification` `elicitation_dialog`, `elicitation_url_dialog` | `elicitation_prompt` | needs input |
| `Notification` `elicitation_complete`, `elicitation_response` | `activity` | working |
| `PostToolUse` | `activity`, when the session was not already working, and once in five minutes while it is | working |
| `Stop` | `stop` | idle |
| `StopFailure` | `stop_failure`, with the error kind | failed |
| `SessionEnd` | `session_end` | ended |

One more event comes from no hook. Ten seconds after a `Stop`, a `StopFailure`
or a `SessionEnd` in a nucleus session, the daemon sends an `outcome`: what the
session left behind. It moves the session to no other status. See
[Unsaved work and outcomes](#unsaved-work-and-outcomes).

`PostToolUse` is how a session leaves "needs input" after you grant a
permission: the tool ran, so the agent is working again. It fires on every tool
call. The daemon sends the one that changes the session's status, and after
that one in every five minutes for as long as the tools keep running. Watch
calls a session stuck when it has been working with no tool activity for the
time you set, so it has to hear that there is some; a busy session costs one
small event every five minutes and no more. The five minutes are counted on the
hooks' own clock, from the last event sent for that session, and carry over a
restart of the daemon. Each heartbeat tells Watch the interval as
`activity_beat_s`, and Watch never judges a session on less than twice it.

A failed stop carries the agent's own short kind for the failure. Watch reads
it: `rate_limit` shows as paused on a usage limit, `authentication_failed` as
an expired login, and any other kind as failed. Those lists are Watch's. The
box does not know them, sends whatever short kind the agent gives, and
`nucleated status` shows the session as failed with its kind.

Three more facts have a place in what the box sends, and Claude Code gives the
box none of them to read, so today the daemon sends none: when a usage limit
resets (`limit_resets_at` on a failed stop), when an environment's login
expires (`login_expires_at` on the heartbeat) and the agent's version
(`agent_version` on the heartbeat). Each is read by the agent's adapter under
`src/agent/`, and an adapter answers only from a field or a file the agent
documents. The daemon does not guess: Watch says "paused on a usage limit"
without a time, and warns of no expiry it was not told about.

The hook never blocks a session. It prints nothing, exits 0, gives up after two
seconds, and needs neither the daemon nor the network: it only writes a file.

### Drivers

A driver is a tool you run and steer agents with on the box: T3 Code, Herdr,
Orca. It calls no hook, so the daemon asks it, on a thread of its own that a
heartbeat never waits for. An environment with `driver` set is watched this
way. Each agent the driver lists becomes a session of that environment, under
the driver's own id for it, and each change in what the agent is doing becomes
the same lifecycle event a hook would have recorded.

| `driver` | Adapter | Its page |
|---|---|---|
| `t3code` | `src/driver/t3code.rs` | [docs/t3code.md](docs/t3code.md) |
| `herdr-core`, `herdr-tui` | `src/driver/herdr_core.rs`, `src/driver/herdr_tui.rs` | [docs/herdr.md](docs/herdr.md) |
| `orca` | `src/driver/orca.rs` | [docs/orca.md](docs/orca.md) |

What holds for every driver, whatever its adapter:

- **State only.** No driver offers its agents' state without their content
  beside it: titles, messages, prompts, terminal text. An adapter reads a
  driver's answer into a list of named fields and skips every other field
  unread. Nothing skipped is held, logged, written down or sent. What comes
  out of an adapter is an id, what the agent is doing, a short kind for a
  failure, a time, a counter, and a folder that is used on the box to find a
  repo's name and a branch and goes no further.
- **Observe only.** An adapter sends its driver the read calls its page names
  and no other. Nothing in the daemon starts, stops or steers an agent, and
  nothing sends a driver a command.
- **A driver that is not there costs nothing.** Absent, slow, refusing or
  speaking a newer protocol, it is no look at all: no event is made, no
  session is ended, and whether the environment's server is alive is still
  asked of its supervisor (`unit` or `check`) and of nothing else. `nucleated
  status` shows one fixed word for why, and since when: `absent`,
  `unreachable`, `timeout`, `rejected`, `no_token`, `unsupported`, `error` or
  `bad_reply`. `error` is a driver that answered the call with an error of
  its own, or whose program ran and said it failed; what the error says is
  never read. The word is shown on the box and is not in the heartbeat: to
  Watch, a driver that has stopped answering is a live server whose sessions
  have stopped moving. A driver that keeps not answering is asked ever less
  often, up to once a minute.
- **The hook wins.** An agent that runs inside a driver and also has
  `nucleated`'s hooks is seen twice. The hook notes which of the driver's
  sessions it is running in, and for as long as the hook's session lives the
  driver's view of that agent is not reported. A hook that has stopped
  speaking does not win for ever: an agent killed without its `SessionEnd`
  leaves a session that never ends, and must not hide whatever runs in its
  place. So when the hook has said nothing for two minutes and the driver
  then sees the session at work or waiting on its owner while the hook's
  session is not, for two minutes more with still no word from the hook, the
  note is dropped and the driver's view is reported. A session at rest
  contradicts nothing, and a hook that speaks again wins again.

  A hook is believed for what its own session shows, and for nothing more.
  Its note is a file any program on the box could write, by running
  `nucleated hook` with a driver's variables set. So it never keeps from you
  a session that waits on you or has failed: when the driver sees either,
  and no hook that speaks for the session shows the same within a minute and
  a half, the driver's view is reported, and stays reported until a hook's
  own session shows it too or the session moves on. A session the driver
  had already reported as waiting or failed is not taken back at all on such
  a note. An agent whose hooks have no word for what the driver sees (Codex
  waiting on an answer, say) is therefore reported from both for as long as
  that lasts. And where the driver says which folder a session works in, a
  note counts only if its hook's session works in that folder or inside it,
  or above it and still inside the environment's `root`; a note that names
  no folder, or another, is some other session's. What a note can still do
  is stand in for a session that is at work or at rest.

  How a hook knows where it is
  running is each driver's own, from the variables the driver documents or
  from the folder, and is on its page. For Herdr it holds only when the
  socket the hook's terminal names is the environment's own; for Orca it is
  per worktree and kind of agent, so a Claude Code hook stands in for the
  Claude Code agents in its worktree and not for a Codex beside them.
- **What is idle when a driver is first seen is history.** It is remembered
  and nothing is said of it until it does something. If it goes away without
  ever doing anything, nothing is said of that either: a session Watch never
  heard of is not reported as ended. A session that turns up later has just
  started, and is reported as started.
- **A session is ended when the driver says so, or stops listing it** in two
  looks in a row. One look can catch a driver between two states.
- **Housekeeping leaves a driver's own worktrees alone.** For a driver that
  makes and removes its own worktrees (Orca), no job runs for its
  environment, by schedule or by hand, and no job over a nucleus beside it
  snapshots, removes or prunes anything under its folders. Its unsaved work
  is counted by the same rule as a nucleus's, which only reads.
- **And any other worktree it did not make.** That does not rest on the
  config naming a folder. A nucleus's own worktrees are the ones under its
  `.worktrees/`; a worktree a driver made anywhere else from one of the
  nucleus's repos (a T3 Code thread's, an Orca worktree outside its named
  folders) is not snapshotted and not removed, and its repo's worktree
  records are not pruned. Its unsaved work is still counted with the
  nucleus's, which only reads.
- **A driver's unsaved count is all of its worktrees or nothing.** While a
  driver that owns its worktrees lists one that is in none of the folders
  the count is made from, no count is sent for its environment, and so none
  for the box. `nucleated status` says how many are outside.

The heartbeat names the driver an environment is watched through, and, where
the driver's own apps take a link to one session, the driver's id for the
installation that the link needs, once Watch has said it takes those two
fields (see [Fields Watch has to ask for](#fields-watch-has-to-ask-for)).
Watch builds the link; the box sends no address. A link's template has `{driver_ref}` and `{session_id}` in it, each
one segment of a path. Whoever fills it in percent-encodes each value as a
whole segment, leaving only letters, digits, `-` and `_` as they are: `.` and
`:`, which both values may hold, are encoded too.

### Config sync

With `[config_sync]` set, the daemon keeps each login's agent config at one
branch of a git repository you name. It fetches on its interval, checks the
newest commit, and writes that commit's settings, instructions and skills
into the config folder of every environment that has a `config_dir` and runs
Claude Code, the one agent whose config folder the sync knows the layout of.
An environment of another agent is passed over, and an environment watched
through a driver is not one login and is not synced. A `config_dir` that is
not a folder is not created: that environment's sync stands at `failed`.
[docs/config-sync.md](docs/config-sync.md) says what is synced, how your
settings are laid under the entries `nucleate` and `nucleated` own, and what
is never touched.

- **Pulled by the box, never pushed by Watch.** Watch has no way to send a
  box its config. It is told which commit an environment is at, and nothing
  about what the commit holds.
- **Nothing is applied for the first time until you have looked.** `nucleated
  config sync --dry-run` prints what would change in an environment. Until
  that has been run for it, a new environment's sync stands at `pending`.
- **One copy of what a sync replaces is kept on the box**, in the cache
  folder.
- **A change made by hand is put back by the next commit**, or by `nucleated
  config sync` run by hand, and by nothing else: the scheduled pass notices
  it and reports it, and writes nothing while the branch stands still.
- **Only the hook handlers `nucleated` and `nucleate` write are kept from
  the file on the box**, and a handler is one of those only if it is,
  whole, a form they write: what `nucleated hooks print` prints, or one of
  the two worktree hooks above, with the program as the bare name or as
  `.local/bin/<name>` under the agent user's home. A handler that only
  starts like one, or carries anything else along, is a change made by hand:
  it is reported as one and the next sync writes the file without it.
- **Git's own words for an error are shown nowhere**: not in the log, not in
  the state folder, and not by `nucleated config sync` at a terminal. They
  can quote the repository's address. The command gives one fixed word, and
  the same fetch run with git gives the rest.
- **Credentials are never read and never written.**

Each heartbeat says three things about an environment's config and no more:
`config_commit`, the commit its config folder was last applied from;
`config_modified`, whether a file the sync wrote has been changed by hand
since; and `config_state`, one fixed word for how the sync stands. They go to
a Watch that has said it takes them (see
[Fields Watch has to ask for](#fields-watch-has-to-ask-for)); `nucleated
status` shows them either way.

| `config_state` | Means |
|---|---|
| `ok` | The config folder is at the newest commit the box has fetched. |
| `pending` | A commit is there to apply and waits for your dry run. |
| `held` | A newer commit is held back by a change made by hand, with `on_drift = "keep"`. |
| `unreachable` | The repository could not be fetched. |
| `rejected` | The newest commit was refused: not signed by a named signer, not a descendant of the last, or holding something the sync will not write. |
| `failed` | The commit was accepted and could not be written on the box. |

Why exactly, in one more fixed word, is in `nucleated status` and stays on
the box: for `unreachable`, whether git refused the fetch, the fetch ran over
its minute, or there is no git to fetch with. A dry run changes nothing,
`config-sync.json` included, so what `nucleated status` says of an
environment's config is from the daemon's last pass or the last run without
`--dry-run`. What a check last found is passed on for ten minutes and no longer:
a sync that has stopped checking says nothing rather than something old.

### Environment setup

The box recipe defines each environment's repositories, network access,
network secrets, setup script and environment variables, and applies them as
root. It writes what it applied into one folder, `/var/lib/nucarium/environments/`
(or `setup_dir`), two files per environment:

| File | Written by | Holds |
|---|---|---|
| `<name>.json` | The recipe | `network` (`none`, `limited` or `full`), `allowed_hosts`, `repos`, `variables` and `secrets` (counts), and `setup_script`, whether there is one. |
| `<name>.setup.json` | The setup script's runner | The last run: `state` (`running`, `ok` or `failed`), when it started and ended as Unix milliseconds, its exit code, and the hash of the script that ran. Absent until the script has run once. |

The daemon reads both afresh for every heartbeat, and writes neither. Each
heartbeat then carries, per environment, a `setup` object: the network word,
the three counts, the repositories' names, and of the script's last run its
state, when it started, when it ended and, for a failed run, its exit code.
There is no `script` when the environment has no setup script or it has not
run yet. The hash of the script, its text, its output, the variables' names
and values, and the files' paths stay on the box.

What the daemon cannot vouch for it leaves out, and the heartbeat is never
the worse for it:

- No `<name>.json`, one that cannot be read, or one that is not what the
  recipe writes: no `setup`.
- A count out of range (`allowed_hosts` over 10000, `variables` or `secrets`
  over 1000), or more than 32 repositories: no `setup`.
- A repository's name that is not an `owner/name` or a `name`: that name
  alone is left out.
- With a setup script, a `<name>.setup.json` that is there and cannot be
  read or is not what the runner writes, or a run that ended with no end
  time or, as written, before it started: no `setup`. A `setup` with no
  `script` would tell Watch the script was removed, and close what it knows
  of a failed run; no `setup` at all leaves Watch as it was. A
  `<name>.setup.json` that is not there yet means the script has not run,
  and `setup` is sent with no `script`.
- An exit code that is not 0 to 255 is left out.
- A time ahead of the box's clock is sent as now.

Each of these is logged once, in fixed words, when it first appears, and not
at every heartbeat. `nucleated status` shows the setup either way, and names
in `setup_problems` why anything was left out. `setup` goes only to a Watch
that has said it takes it (see
[Fields Watch has to ask for](#fields-watch-has-to-ask-for)).

### Plan usage

Each environment whose agent is Claude Code, with `config_dir` set and no
driver, is one login on a Claude subscription. Every five minutes, the first
time a little after the daemon starts and spread so the logins on a box are
not all asked at once, the daemon reads that login's access token from
`<config_dir>/.credentials.json` and asks
`https://api.anthropic.com/api/oauth/usage` how much of each limit is used:
the five-hour window, the weekly one, and any weekly limit on one model.

This is an endpoint the vendor does not document, and it may change. The
daemon reads the few fields it knows and passes over the rest; an answer it
cannot read counts as a failed read. The token is used for that one request
and nothing else: it is never refreshed, never logged and never written
down, and nothing is ever written into the config folder. A token that has
expired is left for Claude Code to refresh, and that round is skipped.

A good reading is kept in `usage.json`, and survives a restart. A failed
read changes nothing: the heartbeat goes on carrying the last good reading,
with the time it was read, and the log says once, with the environment's
name and a fixed word or an HTTP status, that the read failed. An
environment that has never had a good reading says nothing.

Each heartbeat then carries, per environment, a `usage` object: `read_at`,
and `limits`, each with a `window` (`five_hour` or `seven_day`), a `scope`
for a limit on one model, `used_pct` from 0 to 100, and `resets_at` once the
window has started. `usage` goes only to a Watch that has said it takes it
(see [Fields Watch has to ask for](#fields-watch-has-to-ask-for)).

### Fields Watch has to ask for

Seven fields of a heartbeat are newer than the contract every Watch holds:
`driver`, `driver_ref`, `config_commit`, `config_modified`, `config_state`,
`setup` and `usage`. Watch refuses a heartbeat that carries a field it does not
name, and a refused heartbeat is a box gone dark, with every environment on
it looking down and no event sent. So the daemon sends none of the seven on a
guess.

- Every heartbeat carries what every Watch takes. Watch's answer to a
  heartbeat may name, in `accepts`, the newer fields it takes; the daemon
  then sends those, starting at the next turn of its loop and not a whole
  interval later. A Watch that names none is sent none.
- What the box knows and has not sent stays on the box. `nucleated status`
  shows the values, and names the fields held back; `nucleated check` says
  the same in its `heartbeat` line.
- If Watch refuses a heartbeat that carried a field it had named, the daemon
  sends the same heartbeat without the newer fields in the same breath, and
  sends none of them for an hour. The box stays in sight throughout.
- Signing in again starts from nothing named.

Until the Watch you report to names them, an environment watched through a
driver is reported like any other: its server, its sessions and their
events, with no link into the driver, config sync runs on the box with
nothing said of it to Watch, and an environment's setup and plan usage are
shown only by `nucleated status`.

### What leaves the box

Every payload is built from named fields. This is the whole list.

| Sent | From |
|---|---|
| `event_id` | Made on the box: a UUIDv7. |
| `env_id` | The environment's `name` in the config. |
| `session_id` | The hook's `session_id`. For a session a driver lists, the driver's own id for it. |
| `bridge_session_id` | `CLAUDE_CODE_BRIDGE_SESSION_ID`, when the agent sets it. |
| `type` | The table above. |
| `repos` | The session's repos, as `owner/name` from each remote, else the name of the folder the repository lives in. For a linked worktree outside a nucleus that is the folder of the repository it was added from, never the worktree's own, which a tool that makes a worktree per task names after the task. |
| `branch` | The session's branch. |
| `error_kind` | The hook's `error`, for `StopFailure` only, and only when it is a short kind such as `rate_limit`. |
| `limit_resets_at` | When the agent says a usage limit resets, for `StopFailure` only: a timestamp, kept only from a minute before to eight days after the event. Claude Code's hook gives none, so none is sent. |
| `client_ts` | The box's clock. |
| `commits`, with an `outcome` only | How many commits the session made: those on its branch since the commit each worktree started from. A number. |
| `uncommitted_repos`, `unpushed_commits`, with an `outcome` only | How many of the session's repos hold uncommitted changes, and how many of its commits are on no remote. Numbers. |
| `pull_requests`, with an `outcome` only | The addresses of the open pull requests whose head is the session's commit, from GitHub. Each is `https://github.com/<owner>/<name>/pull/<number>` on a repo the same event names in `repos`, and nothing else: no title, no query, no fragment. |
| Heartbeat | Disk used %, 1-minute load, uptime, memory pressure %, each environment's name and whether its agent server is alive, the daemon's version, `activity_beat_s` (the number 300), the box's clock. |
| Heartbeat, about the box | `on_battery`, yes or no, from NUT, only with `[ups]` set. `reboot_required`, yes or no, on Debian and Ubuntu, where `/var/run/reboot-required` says so. `uncommitted_repos`, `unpushed_commits` and `last_snapshot_at`: the box's unsaved work with each nucleus counted once, and the time since which every nucleus has had a good snapshot. `paused_until`, a timestamp, only while a maintenance pause is set. |
| Heartbeat, per environment | `server_restarts`, systemd's running count of restarts of the agent server's unit. `unsaved`: `uncommitted_repos`, `unpushed_commits` and `counted_at`, two numbers and a time. `last_snapshot_at`, when the snapshot job last finished for that nucleus with nothing failed. |
| Heartbeat, per environment, when the agent's adapter can read it | `agent_version`, a short version string, and `login_expires_at`, a timestamp. For Claude Code neither is sent. |
| Heartbeat, per environment watched through a driver, to a Watch that names the field in `accepts` | `driver`, the driver's name from the config: `t3code`, `herdr-core`, `herdr-tui` or `orca`. `driver_ref`, the driver's own id for its installation, only for a driver whose links need one and only when it is an id (letters, digits, `.`, `_`, `:` and `-`). |
| Heartbeat, per Claude Code environment with a good plan usage reading, to a Watch that names the field in `accepts` | `usage`: `read_at`, a timestamp, and `limits`, at most 16, each a `window`, one of two fixed words, a `scope`, a model's name of at most 32 letters, digits, spaces, `.`, `+` and `-`, `used_pct`, a number from 0 to 100, and `resets_at`, a timestamp. |
| Heartbeat, per environment, with `[config_sync]` set, to a Watch that names the field in `accepts` | `config_commit`, a git commit id in hex. `config_modified`, yes or no. `config_state`, one of six fixed words. |
| Heartbeat, per environment the box recipe set up, to a Watch that names the field in `accepts` | `setup`: `network`, one of three fixed words; `allowed_hosts`, `variables` and `secrets`, numbers; `repos`, each an `owner/name` or a `name`; and `script`, the setup script's last run: `state`, one of three fixed words, `started_at` and `finished_at`, timestamps, and `exit_code`, a number. |

Each value is checked against the shape Watch accepts before it is written
down or sent. One that does not fit is left out. A time is built from a time
and never from the agent's words for one, a count is a number, and a pull
request's address is held to its pattern, so there is nowhere in any of them
for a sentence to go. A value the box could not find out is left out too, which
is not the same as zero: a probe that gets no answer, a count not yet made and
a GitHub that does not reply each leave their field off.

Never read from the hook, never written to disk, never sent: prompts, messages,
session titles, tool inputs and outputs, transcript paths and contents, the
last assistant message, compaction summaries and instructions, error details,
file contents, file paths beyond a repo's name, environment variables, user
names and tokens. The hook's `cwd` is read to find the session's repos and
branch, and goes no further.

Never sent from what the box finds out itself: commit subjects and messages,
file names, diffs, pull request titles, descriptions, branches and authors.
The daemon never asks git for a commit's subject or a file's contents. `git
status` names the files that changed, and that answer is read for whether it is
empty and then dropped. From GitHub's list of pull requests it reads four
fields: the address, the state, whether it was merged and the head commit.

Never read from a driver, never written to disk, never sent: thread and
session titles, messages, prompts, goals, terminal contents and previews, tool
names and input, error text, command lines, labels, and the names of other
machines. A driver hands these over beside the state, and the adapter's parser
skips them unread. A folder a driver names is read to find the session's repos
and branch, and goes no further.

Never sent from a config sync: the repository's address, its branch, a
commit's subject, author or signer, any file's name or content, and git's own
words for an error.

Never sent from an environment's setup: the setup script, its hash and its
output, the names and values of its variables and secrets, the hosts it may
reach, and the paths of the recipe's files.

Three things leave the box for somewhere other than Watch, and none goes to
Nucarium. The hourly snapshot pushes uncommitted work, which is code, to
`refs/wip/<branch>` on the repository's own remote: your Git host, under your
credentials. The plan usage request carries a Claude Code login's own access
token to the agent vendor, and nothing else. And with `[notify]` set, the box
posts a machine name, an environment name and a time to the address you gave
it, while it cannot reach Watch.

`tests/privacy.rs` is the release gate for this. It feeds every hook event a
payload with a sentinel string in every content-bearing field, in the working
directory's path and in the environment, then proves that no sentinel is in the
spool, in the local state, or in any request the daemon makes. It does the same
for what the box finds out itself: a session's repository gets a commit subject,
file names and file contents full of the sentinel, a stand-in for GitHub
describes its pull request with the sentinel in the title, the body, the branch
and the author, and what leaves is counts, times and one address. It lists
every field an event and a heartbeat may carry, and fails if one is sent that
is not on the list.

Every adapter for a third party adds its own part of the gate, in its own
test file, built from the same pieces (`tests/common/gate.rs`): the sentinel,
the one list of fields, the scanner, the stand-in for Watch, and a way to run
the daemon itself as a process with a `PATH` the test chooses, for proving it
starts no third party's program. The scanner looks at what every file under
the state folder holds and at what every file and folder there is called. An
adapter's fixture is its third party's own answer with the sentinel in every
field that carries content, in fields where a short word belongs, and in
fields that do not exist yet. `tests/codex.rs`, `tests/t3code.rs`, `tests/herdr.rs`,
`tests/orca.rs`, `tests/config_sync.rs` and `tests/setup.rs` are each part of
the gate: if one fails, do not release.

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

### Unsaved work and outcomes

A box that dies takes with it whatever was only on the box. Watch shows how
much that is before it happens, and says it when a session ends holding some
or a machine goes dark.

The **count** is per environment: how many worktrees in its nucleus hold
uncommitted changes, and how many commits are on no remote, by
[the rule `nucleate` deletes by](#what-counts-as-unsaved). A third thread, the
reporter, makes it every ten minutes (`unsaved` in `[report]`), once per
nucleus, and writes two numbers and a time to `unsaved.json`. The heartbeat
reads that file. It runs no git, however many worktrees there are, so a
heartbeat costs the same on a box with one session as with a hundred. Two
logins that share a nucleus report the same counts, and the box's own totals
count that nucleus once. The count covers every worktree of the nucleus's
repos, including one another tool added outside `.worktrees/`, except those
in the folders of a driver that owns its worktrees, which are counted under
that driver's environment. A worktree git cannot read makes its nucleus's count
unknown, and nothing is sent in place of it; so does a repo the daemon refuses
(see [Git in session repos](#git-in-session-repos)). `"off"` stops the count and
removes the last one.

With the count goes `last_snapshot_at`: when the snapshot job last finished for
that nucleus with nothing failed. A run whose push was refused does not move
it.

The **outcome** is per session. When a session in a nucleus stops, fails or
ends, the daemon's loop leaves a note and the reporter looks at that session's
repos ten seconds later, so a push that was still running has landed. It looks
at most once a minute for one session, however often the session stops, and
sends:

- `commits`: the commits on the session's branch since each worktree started;
- `uncommitted_repos` and `unpushed_commits`: what the session holds that is
  nowhere else;
- `pull_requests`: the addresses of the open pull requests whose head is the
  session's commit. GitHub is asked only for a repo whose remote is on GitHub,
  where the session has made a commit, and whose `HEAD` is on a remote branch;
  it is the one call `nucleate` already makes to recognise a merged branch.

What the box could not find out it leaves off, and Watch keeps what it had: no
`pull_requests` when GitHub did not answer or the session's commit is on no
remote branch, no `commits` when a worktree's starting commit cannot be read.
An empty `pull_requests` says there are none: the session made no commit, or
GitHub listed no open pull request with its commit as the head.
When the session's folder is already gone, the outcome says only that nothing
unsaved is left. A session outside a nucleus has no outcome: `nucleate` made no
folder for it and keeps no record of where it started.

The reporter asks about a worktree with `git --no-optional-locks status`, so it
never leaves the index locked under an agent that is committing.

### The maintenance pause

Reboot a box, or pull its power to move it, and Watch tells you it has gone
dark, which you knew. Say so first:

```bash
nucleated pause 2h
```

```json
{
  "paused": true,
  "until": "2026-11-20T03:10:00.000Z"
}
```

The running daemon sends a heartbeat at once, carrying the time the pause ends.
Until then Watch holds its alerts for this machine and its agent servers going
dark, and shows the machine as paused for maintenance. If the box is still dark
when the pause ends, the alert held is sent then. If it came back first, nothing
is sent.

`nucleated resume` ends the pause now. Otherwise it ends by itself: a pause is
at most a day, and one whose time has passed is over without anyone ending it.

A pause holds those alerts and does nothing else. No agent, agent server or
housekeeping job is touched, sessions go on being reported, and every other
kind of alert is sent as ever. A pause set on a box that is not signed in is
kept and tells nobody.

### When Watch cannot be reached

Watch tells you when a box goes dark. It cannot tell you that it has stopped
hearing from every box at once, or that this box's route to it is down while
its agents work on. With an address of your own in the config, the box says so
itself:

```toml
[notify]
url = "https://ntfy.sh/your-private-topic"
```

It posts three kinds of message, each once:

| When | The post |
|---|---|
| No heartbeat has been accepted for `after` (ten minutes) | `agentbox cannot reach Nucarium Watch`: "No heartbeat has been accepted since 13:40 UTC. This box tells you here when a session waits on you." |
| While that lasts, each time a session starts waiting on you | `An agent on client-a is waiting for you`: "A session on client-a (agentbox) has waited since 13:52 UTC. Watch cannot be reached, so this comes from the box." |
| A heartbeat is accepted again | `agentbox can reach Nucarium Watch again`: "Heartbeats are accepted again after 25 minutes." |

A post has the shape Watch's own alerts have: the title in a `Title` header, the
sentence as a plain-text body, `Priority`, `Tags`, and `X-Nucarium-Alert-Kind`
set to `box_watch_unreachable`, `box_session_waiting` or `box_watch_reachable`.
It carries the machine's name, the environment's name and a time in UTC. It
goes to that one address, with the token in `token_file` if you set one, and
never to Nucarium. The machine token is Watch's and is sent to nothing else.

A session that started waiting before the box had spoken is told of right
after the first post. A session that is answered and waits again is told of
again; the same wait never is. `notify.json` records what was posted, so
restarting the daemon in the middle of an outage repeats nothing. A post that
fails is tried again after a second, doubling to five minutes. If Watch comes
back before the first post was ever delivered, neither it nor the "again" post
is sent.

A refused token and a request to slow down are answers from Watch, not an
outage. The notifier runs only on a box that reports to Watch: with no login,
or with `target = "none"`, it posts nothing.

This covers Watch being unreachable from a box that still has a network. A box
with no network cannot post either. That case is the one Watch itself covers:
it tells you the machine has gone dark.

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

No job runs for an environment whose driver owns its worktrees, and no job
reaches into that driver's folders from a nucleus beside them: a worktree
there is not snapshotted, not removed, and its record in a shared repository
is not pruned. The same holds for every worktree of a nucleus's repos that is
not under the nucleus's `.worktrees/`, whoever made it and wherever it is:
`snapshot` reports it as skipped, and `cleanup` does not prune the worktree
records of a repo that holds one. See [Drivers](#drivers).

### Git in session repos

`nucleate` and the daemon run git on the host, as the agent user, in bare
repos and worktrees that agent sessions can write: `nucleate` from Claude
Code's worktree hooks, its MCP server and the commands you run, the daemon on
its own schedule. A repo's own config can name a program for git to run
(`core.fsmonitor`, `core.sshCommand`, a filter's `clean`, a
`credential.helper`, an `include.path` that brings in more of the same), and a
worktree's `.git` file or a `worktrees/*/commondir` can send git to another
repo whose config does. So both check a repo before every git call they make
there:

- It reads the repo's `config`, and a worktree's `config.worktree`, with git's
  own parser and no includes, and refuses the repo unless every key is one of
  these: `core.repositoryformatversion`, `core.bare`, `core.filemode`,
  `core.logallrefupdates`, `core.ignorecase`, `core.precomposeunicode`,
  `core.hooksPath`, `remote.<name>.url`, `remote.<name>.fetch`,
  `branch.<name>.remote`, `branch.<name>.merge`,
  `branch.<name>.vscode-merge-base`, `extensions.worktreeConfig`, and any
  `gc.<key>` set to a number. These are what `git clone`, `git worktree add`
  and `nucleate` write, and the branch keys `nucleate hook-create` adds for
  each session. A repo's `credential.*` and `url.*.insteadOf` are not on the
  list: a fetch uses the agent user's own credential helper and rewrites, from
  the global config below, and never a repo's. Where a `github_token_file` is
  set, the helper for `https://github.com` is replaced on the command line by
  one that reads that file; only the file's path is on the command line.
- It refuses a worktree with no `.git` of its own, or one whose `.git`, or
  whose `commondir`, leads outside the nucleus's `.repos/`, or nowhere. It
  then hands git a worktree's folders as it checked them, so a worktree's
  pointer changed after the check is not followed.
  The worktrees of a driver that owns them belong to repos of the owner's
  choosing: their config is checked, and where they point is not.
- It refuses a worktree whose index names a submodule, checked out or not:
  `git add` and `git status` start git again inside one that is checked out,
  where that repository's own config would be read and not checked. A repo
  that uses submodules is therefore refused; report-only mode shows which.
- It runs git with no system config or attributes (`GIT_CONFIG_NOSYSTEM=1`,
  `GIT_ATTR_NOSYSTEM=1`), with its own global config (the one its environment
  names in `GIT_CONFIG_GLOBAL`, else `~/.gitconfig`), and with
  `core.hooksPath=/dev/null`, `core.fsmonitor=false`,
  `core.attributesFile=/dev/null` and `diff.ignoreSubmodules=all` on the
  command line, where they win over anything in a repo. That is why `core.hooksPath` may be set: it does nothing.

`nucleate` always enforces the check; `vetting` in the daemon's config does
not change it. A command that would run git in a refused repo fails with
`refused repo <name>: <why>` and changes nothing: `create-session`, `add` and
the MCP server's `add_repo` make no worktree, `hook-create` fails so that
Claude Code shows the session why and starts no worktree, and `hook-remove`,
`remove`, `remove-session` and `status` leave the session as it is. `refresh`
reports the repo as `refused`, as the daemon's job does, and exits 1.

A refused repo is skipped by the daemon. It logs it once, by name, in one fixed
sentence that quotes nothing from its config, and logs it again only if it is
refused again after passing. `refresh` and `snapshot` report it as `refused`
with one word for why (`config`, `pointer`, `submodule` or `unreadable`), which counts as a
run that did not finish clean. `cleanup` keeps any session that holds it. Its
nucleus's unsaved-work count is unknown until it passes again, so Watch is
sent no count rather than one that leaves it out. `nucleated status` names it
on its environment's line, as in `git refused: web (config)`, from the last
count; with the count off, only the log has it. The heartbeat itself is never
held up by it.

The check runs just before each call, so a session that rewrites a repo's
config, or adds a `commondir` to a bare repo, between the check and git's own
read of it could still slip past it. The overrides on the command line hold
either way.

**Checking a box before enforcing.** A repo set up by hand or by another tool
may hold a key that is not on the list (`submodule.*`, `pull.rebase`, `lfs.*`,
a `core.hooksPath` set by husky alongside something else). To find out without
the daemon refusing anything (`nucleate` refuses such a repo either way):

1. Set `vetting = "report"` in `[git]` and restart the daemon:
   `systemctl --user restart nucleated`.
2. Run `nucleated housekeeping run refresh`, which logs each repo enforcing
   would refuse, and wait for the next count (ten minutes, or `unsaved` in
   `[report]`).
3. Read `nucleated status`: each repo that enforcing would refuse is named
   after `git refused:`. `journalctl --user -u nucleated | grep refused` shows
   the same, once per repo.
4. For each, read its config yourself (`git config --file
   <nucleus>/.repos/<repo>.git/config --no-includes --list`), and remove what
   should not be there, or decide the repo is not one for this box.
5. When `nucleated status` names none, set `vetting = "enforce"` (or remove
   the line) and restart the daemon.

In report-only mode git runs in a refused repo as it did before this check,
with the overrides above: hooks, the file-system monitor and an attributes
file stay off, and any other program its config, or a submodule's, names can
run.

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
  and forwards the ones that change a session's status or its "needs input",
  and one tool call in five minutes from a session that stays working.
- **The box does not sort failures or call a session stuck.** It reports a
  failed stop with the agent's kind and leaves it to Watch to say which kinds
  are a usage limit or an expired login. Stuck is decided on Watch's clock,
  from deadlines the account sets. `nucleated status` shows `failed` with the
  kind, and `working`.
- **Hosted Watch or nothing.** There is no setting for another Watch. `url` in
  `[watch]` exists for developing against an API on the same machine.
- **One rule for unsaved work.** The counts Watch shows come from the same
  function `nucleate` refuses a deletion by, so the two never disagree.
- **Unknown is not zero.** A probe that gets no answer, a count not yet made
  and a GitHub that does not reply leave their field out of what is sent.
  Nothing is put in its place.
- **Restarts are systemd's to count.** `server_restarts` is the unit's own
  `NRestarts`. An environment watched by a `check` command has no supervisor
  the daemon can ask, so it sends no count. Watch decides what a restart loop
  is; the box only reports the number.
- **A reboot is required where the system can say so.** On Debian and Ubuntu
  `/var/run/reboot-required` is the convention, and its absence means no. On
  another system its absence would mean nothing, so nothing is sent.
- **Pull requests are GitHub's.** Their addresses come from GitHub's list of
  the pull requests for a commit, which `nucleate` already reads to recognise a
  merged branch. A remote on another host has none the box can know of.
- **The notifier speaks only when Watch cannot.** It tells you of a waiting
  session only when the event that would have told Watch is still unsent.

## Development

```bash
cargo test                 # builds real repositories in temporary folders
cargo test -- --ignored    # also runs the one test that calls the GitHub API
cargo clippy --all-targets -- -D warnings
```

The suite needs `git` and OpenSSH's `ssh-keygen` on `PATH`. The tests of the
config sync's signature check make their own keys, and fail on a machine that
cannot make one: a check that did not run is not reported as passed.

Nothing in the suite talks to Watch or to GitHub. `tests/common` starts a
stand-in for the Watch API on a local port; `tests/login.rs` drives the sign-in
against it through every device-flow error, a slow API and a dead one, and
`tests/daemon.rs` drives the daemon's loop through batching, backoff and replay
after an outage. `tests/reports.rs` covers what the box reports about itself:
the unsaved count and its schedule, outcomes, the supervisor's signals, the
pause and the notifier. It moves the clock and waits for nothing. Where a test
needs GitHub, `NUCLEATE_GITHUB_API_URL` or the reporter's own seam points
`nucleate` at a stand-in, and only an `http` address on this machine is
accepted there, so nothing else can take GitHub's place.
The stand-in holds the two ingest routes to the Watch contract as the API
does (`contract` in `tests/common`): a field the contract does not name, or a
value outside its pattern or range, is answered with `validation_failed`.
When the contract changes, that module changes with it.
`tests/privacy.rs` is the release gate described under
[What leaves the box](#what-leaves-the-box): if it fails, do not release.
`tests/seam.rs` covers what holds for every agent and every driver whichever
adapter is asked: whose hook a hook is, where it lands, and that housekeeping
never touches what a driver owns. Each adapter's own tests, and its part of
the gate, are in a file named for it.

To run both binaries against a Watch API on this machine, sign in with
`nucleate login --api-url http://localhost:8787`.

### Releases

Nobody tags a release by hand. Three workflows do it:

| Workflow       | Runs on                       | Does                                                                                                   |
| -------------- | ----------------------------- | ------------------------------------------------------------------------------------------------------ |
| `build.yml`    | Every pull request, and called by `release.yml` | fmt, clippy and the tests on Linux; the tests and both macOS builds on macOS              |
| `release.yml`  | Every push to `main`          | Runs `build.yml` on the pushed commit; only if all of it passes, creates the GitHub Release and its tag on that commit with semantic-release, from the Conventional Commits since the last tag |
| `binaries.yml` | A published release           | Builds the four archives with the tag as their version, attaches them to the release, tells the Homebrew tap and the box recipe |

A test gates every release. The whole suite runs on the pushed commit before
semantic-release does, the privacy gate in `tests/privacy.rs` and the setup
gate in `tests/setup.rs` among it. If any test fails, there is no tag, no
GitHub Release, no archives and no new pin in the tap or the recipe; the next
push to `main` that passes releases everything since the last tag.

A `fix:` commit makes a patch release and a `feat:` commit a minor one; a
push with neither makes no release. The version in `Cargo.toml` is a
placeholder (`0.0.0-dev`): a release build takes its version from the tag, so
a build from source reports `0.0.0-dev`.

The last step sends the version and the four checksums to the two
repositories that pin a release. `nucarium/homebrew-tap` rewrites its formula.
`nucarium/nucarium-ansible` moves the box recipe's pin and releases the
recipe, and that release moves the pin in the installer at `nucarium.sh`. So
one release here reaches Homebrew, the recipe and the installer with nobody
copying a checksum. That needs the Robot App (secrets `GH_APP_ID` and
`GH_PRIVATE_KEY`) installed on all of them.

## Licence

Licensed under either of the [Apache License, Version 2.0](LICENSE-APACHE) or
the [MIT licence](LICENSE-MIT), at your option. Unless you state otherwise, any
contribution you submit for inclusion in this project is licensed the same way,
with no additional terms.
