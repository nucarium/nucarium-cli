# T3 Code

`driver = "t3code"` in an `[[environment]]` table. One such environment is
one T3 Code server on the box, and each of its top-level threads is one
session. The adapter is `src/driver/t3code.rs`; its tests and its part of the
privacy gate are in `tests/t3code.rs`. What holds for every driver is in the
README under Drivers.

```toml
[[environment]]
name = "t3"
root = "/home/agent/client-a"
driver = "t3code"
# unit = "t3code.service"        # the default
# driver_poll = "15s"
# driver_home = "/home/agent/.t3"
# driver_url = "http://127.0.0.1:3773"
# driver_token_file = "/home/agent/.config/nucleated/t3code.token"
```

Everything below was read from T3 Code's own repository
(`pingdotgg/t3code`, server package 0.1.10, orchestration protocol 2). The
file each fact came from is named in the adapter's source beside the code
that relies on it.

## Setup

T3 Code runs unattended as a systemd user service:

```sh
t3 service install          # installs and starts t3code.service, with lingering
```

`nucleated` watches that unit, as it watches any agent server, to say whether
the environment is alive. Leave T3 Code listening on this machine only, which
is how a new service starts.

Then issue `nucleated` a token that can read and do nothing else, once, as
the agent user:

```sh
umask 077
t3 auth session issue --scope orchestration:read --ttl 30d \
  --label nucarium --token-only > ~/.config/nucleated/t3code.token
```

`--ttl` is the token's life (`30d` is T3 Code's own default); `--scope`
replaces the default permissions with the one named.

`nucleated` never runs `t3` and never issues, renews or revokes a token. A
token file that anyone but its owner can read is not used.

Two things about that token, plainly:

- `orchestration:read` is the narrowest permission T3 Code has for thread
  state, and it also reads threads' contents: the same token would open a
  transcript. `nucleated` never asks for one. Its code has two requests in it
  and no way to make a third.
- A token issued this way does not renew. When it expires, or you revoke it
  with `t3 auth session revoke`, the environment's sessions stop moving and
  `nucleated status` says `driver t3code: rejected`. Issue a new one into the
  same file.

## What is asked

Two GETs, to this machine and nowhere else. No redirect is followed and no
proxy is used.

| Request | When | Sent with it |
|---|---|---|
| `GET /.well-known/t3/environment` | Before the first look, again after any look that failed, when the server's address changes, and when the last look that worked is more than ten minutes old | Nothing |
| `GET /api/orchestration/shell` | Every `driver_poll` | `authorization: Bearer <token>`, `x-t3-orchestration-protocol: 2` |

The shell request carries `omitIdleSubagents=1` and `omitOlderSettled=1` when
the server's descriptor advertises `shellThreadFamilies` and
`shellOlderSettled`, as T3 Code's own client does, and no query otherwise.

What the descriptor said is kept between looks in
`drivers/<environment>.json`, as the environment's id and three numbers: one
made from the server's address, and a yes or no for each of the two
capabilities. A daemon restarted within ten minutes of a look that worked
does not ask again.

The server's address is `driver_url`, or else the `origin` the running server
wrote to `<driver_home>/userdata/server-runtime.json` (`driver_home` defaults
to `~/.t3`). An address that is not `127.0.0.1`, `localhost` or `[::1]` is
never asked: `driver_url` is refused when the config loads, and a runtime
file that names another address reads as `unsupported`. For a server whose
runtime file names a network address, set
`driver_url = "http://127.0.0.1:<port>"`.

Never used: `/mcp`, the WebSocket, the `t3` program, the thread, transcript
and history routes, the database file, any request that is not a GET. A test
asserts the exact requests the adapter's stand-in server received.

## What is read

T3 Code has no route, permission or flag that returns thread state without
thread content: the shell answer carries each thread's title, its latest
message, its error text and its goal beside the state. So the answer does
pass through `nucleated`'s reader. The adapter's structs name the fields
below and nothing else, and the parser reads past every other key without
keeping it. Nothing skipped is held, logged, spooled or sent.

| From | Fields read |
|---|---|
| `server-runtime.json` | `version`, `origin` |
| The descriptor | `environmentId`, `orchestrationProtocolVersion`, `capabilities.shellThreadFamilies`, `capabilities.shellOlderSettled` |
| Each thread in `threads` | `id`, `projectId`, `status`, `activityRunStatus`, `pendingRuntimeRequest.kind`, `pendingRuntimeRequest.userOnly`, `lastErrorClass`, `usageLimitResetAt`, `lineage.parentThreadId`, `lineage.relationshipToParent`, `lineage.sideChat`, `creationSource`, `settledOverride`, `archivedAt` and `deletedAt` (whether set, not when), `itemCount`, `pendingBackgroundTasks[].kind`, `pendingBackgroundTasks[].awaited`, `worktreePath` |
| Each project in `projects` | `id`, `workspaceRoot` |

`worktreePath` and `workspaceRoot` are used on the box, exactly as a hook's
working folder is, to find the repo names and the branch from git. They are
never stored and never sent.

T3 Code can give a thread a worktree of its own (`worktreePath`), in a folder
T3 Code chooses. When the project is a nucleus session's checkout, that
worktree is registered in the nucleus's bare repository. It is not the nucleus's own: housekeeping does not snapshot
or remove it, and neither housekeeping nor `nucleate` prunes the worktree
records of a repository that holds it. Its unsaved work is counted with that
nucleus's, which only reads. A thread worktree of a project outside any
nucleus is not counted anywhere.

A thread's `title` is read only on a box whose config sets
`session_names = true`, and sent as the session's name (see Session names in
the README). Otherwise it is skipped unread.

Never named, and so never read: `latestVisibleMessage`, `lastError`,
`lastErrorCode`, `goal`, `branch`, `tags`, `linkOrigin`, `modelSelection`,
pull requests and issues, a background task's `description`, `command` and
`taskId`, every thread in `archivedThreads`, a project's `title`, `scripts`,
`repositoryIdentity` and `faviconPath`, and the descriptor's `label`.

Every state word is checked against the list T3 Code's contracts give for
it. A word that is not on the list, or a sentence, a list or an object where
a word belongs, is no opinion: the thread keeps the state it had.

What leaves the box because of this driver: `driver` (the word `t3code`) and
`driver_ref` (T3 Code's own id for the installation) in the heartbeat, once
Watch has said it takes those two fields, and the same lifecycle events a
hook produces, with the thread's id as the session id.

## What each state becomes

The order is T3 Code's own, the one it uses to tell its phone app what a
thread is doing. Only top-level threads are sessions: subagents and side
chats live inside their parent.

| The thread | Is |
|---|---|
| Deleted or archived | Ended |
| Holds a pending request of kind `user_input` | Needs input |
| Holds one of kind `mcp-elicitation` | Needs an answer to a tool's question |
| Holds one of kind `command`, `file-read`, `file-change`, `permission` or `dynamic_tool_call` | Needs permission |
| Has a live subagent below it holding a request the owner must answer | Needs what the subagent needs |
| Run status `preparing`, `starting`, `running` or `waiting` | Working |
| `completed`, with background work the agent waits on, a subagent or a background task | Working |
| `completed`, `idle`, `interrupted`, `cancelled` or `rolled_back` | Idle |
| `failed` | Failed, with `lastErrorClass` as the error kind |
| `queued`, or a word not on T3 Code's list | No opinion |
| Settled, and idle or failed by the rows above | Ended |

The run status is `activityRunStatus` when there is one and `status`
otherwise. An `auth_refresh` request is not the owner's to answer and is
passed over. A settled thread that is still working or waiting stays what it
is, as it does in T3 Code. A failure whose class is `usage_limit` carries
`usageLimitResetAt` as the time the limit resets.

A subagent's request counts when the subagent is one T3 Code made itself, is
still running, and asks for something only the owner can give: any request
but a token refresh, and a `user_input` only when it is marked `userOnly`. It
is the top-level thread above it that waits.

From there the daemon does what it does for every driver: a change of state
becomes the event a hook would have sent, a thread missing from two looks in
a row has ended, what is idle at the first look is history (and is never
reported as ended if it goes away before doing anything), and a thread
still working whose `itemCount` has grown is heard from once every five
minutes.

## The link that opens a thread

```
https://app.t3.codes/threads/{driver_ref}/{session_id}
```

This is the address T3 Code's own "Copy thread" copies. On a phone with T3
Code installed it opens the thread in the app; elsewhere it asks where to
open it. Each part is percent-encoded, including `.` and `:`. It opens only
for someone whose T3 Code is already connected to the machine that runs the
thread.

## An agent that is seen twice

T3 Code runs Claude Code with the login's own configuration, so a login with
`nucleated`'s hooks installed may report a session that T3 Code also lists
as a thread. T3 Code sets `T3CODE_THREAD_ID` on every agent it launches. A
hook that finds it says which thread it is running in, and the driver stands
aside for that thread: the hook wins, and the session is reported once. A
hook that has gone quiet while T3 Code shows the thread at work or waiting
on you, and the hook's session is not, loses that after two minutes of each:
see "The hook wins" in the README.

`T3CODE_THREAD_ID` is a variable, and any program can set one. So the hook
stands in for a thread only so far as its own session shows what the thread
is doing. A thread that waits on you or has failed, with no hook's session
showing the same a minute and a half on, is reported from T3 Code as well,
until a hook's session shows it or the thread moves on. And where T3 Code
gives the thread's folder (`worktreePath`, else `workspaceRoot`), the hook
counts only if its session works in that folder or inside it, or above it
and inside the environment's `root`.

## When T3 Code is absent or unhappy

A look that fails writes no event, ends no session and changes nothing about
whether the environment is alive, which is its systemd unit's to say. It
changes one word, shown by `nucleated status` and logged once per change:

| Word | Means |
|---|---|
| `ok` | The last look worked |
| `absent` | No runtime file, or nothing listening at the address |
| `unreachable` | The connection failed some other way |
| `timeout` | No whole answer inside four seconds |
| `no_token` | The token file is missing, empty, or readable by others |
| `rejected` | T3 Code answered 401 or 403: the token expired, was revoked, or lacks `orchestration:read` |
| `unsupported` | The server does not say it speaks orchestration protocol 2, its runtime file is of another version, or the file names an address that is not this machine |
| `bad_reply` | The answer was not the shell: an error, a page, a cut-off body |

After failures the daemon asks less often, up to once a minute, and goes back
to `driver_poll` at the first look that works.

## What T3 Code gives the box no way to report

- **State without content.** There is no state-only source, so the promise
  rests on the parser, not on the transport. The gate test puts a sentinel
  in every content field of a whole shell answer and proves none of it
  reaches the wire, the state folder, the status view or the log.
- **Push.** A thread that starts waiting is reported at the next look, up to
  `driver_poll` later. A hook reports at once.
- **A stable protocol.** T3 Code promises third parties nothing about this
  route. After a protocol change the environment reads `unsupported` and its
  sessions stand still until `nucleated` is updated.
- **Per-login health.** One T3 Code server can run several provider logins.
  They are one environment here, with no agent version and no login expiry.
- **Threads whose id cannot be sent.** A thread id with characters outside
  `A-Z a-z 0-9 . _ : -` is not reported.
- **Which of several waiting subagents asked last.** T3 Code announces the
  newest. The adapter reads no times, so a thread with several subagents
  waiting at once reads as needing input if any of them does, then a tool's
  answer, then permission.
- **A monitor that keeps a finished run open.** T3 Code tells one kind of
  monitor from another by its task id, which is not read. A thread whose
  latest run was woken by a monitor reads as idle between wakes.
- **The agent's own session id.** The shell does not carry it, so a driver
  session has no vendor session id, and the hook is recognised by
  `T3CODE_THREAD_ID` alone. A `claude` started by hand in a T3 Code terminal
  carries the variable too: its hook reports it, and the driver then stands
  aside for the thread it was started in.
