# Herdr

`driver = "herdr-core"` or `driver = "herdr-tui"` in an `[[environment]]`
table: herdr-core is the server behind the Herdr desktop and phone apps, and
Herdr TUI is the terminal app with a server of its own. The adapters are
`src/driver/herdr_core.rs` and `src/driver/herdr_tui.rs`; their tests and
their part of the privacy gate are in `tests/herdr.rs`. What holds for every
driver is in the README under Drivers.

```toml
[[environment]]
name = "herdr"
root = "/home/agent/client-a"
driver = "herdr-core"
# Herdr's documentation names no unit, so one of these is required.
check = "test -S /run/user/1001/herdr-core/default.sock"
# driver_poll = "5s"
# driver_session = "default"
# driver_socket = "/run/user/1001/herdr-core/default.sock"
```

One environment holds every terminal of one Herdr server. The two products
share nothing on the wire, so each has its own adapter; a box that runs both
has one environment for each.

The adapters were written against herdr-core 0.1.21 (`schema/README.md`,
`schema/hc.openrpc.json`, `docs/security.md`) and Herdr TUI 0.9.5 (the
`socket-api` page, `src/api/schema/agents.rs`, `src/session.rs`). Nothing
here is taken from anywhere else.

## What nucleated can do to Herdr, and what it does

Both sockets are control sockets. Any program running as the agent user can
connect to one and run a command, type into a terminal or read a screen, and
neither product offers a read-only way in or a way to ask for only some
fields. `nucleated` runs as that user, so it holds the same access as every
other program of that user.

It uses one thing. To herdr-core it writes this line and no other:

```
{"jsonrpc":"2.0","id":1,"method":"terminal.list","params":{}}
```

To the Herdr TUI it writes this line and no other:

```
{"id":"n1","method":"agent.list","params":{}}
```

The two lines are constants in `herdr_core.rs`, and a socket can be asked
only through an enum of the two. Each look opens a connection, writes its
line, reads one line back and closes. Neither call returns screen contents,
and neither marks anything as seen. `nucleated` never runs `hc` or `herdr`:
running `hc` would start a core, and that is acting.

## Where the socket is looked for

`driver_socket`, when the config sets it. Otherwise where the product itself
puts it:

| | Socket |
|---|---|
| herdr-core | `$XDG_RUNTIME_DIR/herdr-core/<session>.sock`; without that variable `/run/user/<uid>/herdr-core/<session>.sock` when that folder exists; else `$XDG_STATE_HOME/herdr-core/<session>/<session>.sock`, with `~/.local/state` for the variable |
| herdr-tui | `$XDG_CONFIG_HOME/herdr/herdr.sock`, with `~/.config` for the variable; for a named session `…/herdr/sessions/<session>/herdr.sock` |

`<session>` is `driver_session`. For herdr-core it is `default` when unset;
for the TUI an unset `driver_session` is the default session's socket.
Herdr's own overrides (`HC_SOCKET`, `HC_SESSION`, the `socket` key of
herdr-core's config file, `HERDR_SOCKET_PATH`, `HERDR_SESSION`) are not read:
a box that uses one says so in `driver_socket` or `driver_session`.

The socket folders are mode 0700, so `nucleated` must run as the user Herdr
runs as.

## What is read from the answer

Each answer is parsed into structs that name these fields and nothing else.
Every other key is skipped unread, and a record that is not a JSON object is
not read at all.

| | Read, and how it is used |
|---|---|
| herdr-core `terminals[]` | `address`, `created_ms` (the session id); `state`, `reported`, `status.state`, `status.kind` (the phase); `turn`; `foreground.cwd`, else `cwd` (box only) |
| herdr-tui `agents[]` | `terminal_id` (the session id); `agent_status` (the phase); `completion_seq`; `pane_id` (box only); `agent_session.kind` and `.value` (box only); `foreground_cwd`, else `cwd` (box only) |

Box only means the value is used on the box and is never stored, logged or
sent. A folder is used to find the repo names and the branch, as a hook's
`cwd` is. A pane id and an agent's own session id are compared with what
hooks have reported (see below).

Never read from herdr-core: `name`, `argv`, `title`, `labels`, `template`,
`first_message`, `identity`, `parent`, `records`, `reason`, `status.app`,
`status.title`, `status.msg`, `status.progress`, `foreground.name`,
`foreground.argv`, and all of `machines`. Never read from the TUI: `name`,
`agent`, `title`, `terminal_title`, `terminal_title_stripped`,
`display_agent`, `state_labels`, `tokens`, `workspace_id`, `tab_id`,
`agent_session.source`, `agent_session.agent`.

Each value read is held to what its product documents: a herdr-core address
is `t`, a digit from 1 to 9, then more of core's own digits; a TUI id is up
to 64 of letters, digits, `_`, `:` and `-`; a state is one of the documented
words. Anything else is treated as not there. A herdr-core terminal on
another machine (`machine/t7`) is never reported: its address would name a
machine other than this box. An `agent_session` whose kind is `path` is
never taken.

An answer is one line of at most 4 MiB, connected to within two seconds and
answered within two more. Anything else is no look at all.

## What a session is

| | Session id | Example |
|---|---|---|
| herdr-core | `hc:<address>:<created_ms>` | `hc:t7:1795140000000` |
| herdr-tui | `herdr:<terminal_id>` | `herdr:term_65f1a2b3c4d5e1` |

The address is what a person types after `hc attach`. A herdr-core terminal
is a session only once its program has reported a status with OSC 7501: core
stores what the program says and guesses nothing, so a program that reports
none is not an agent here. The TUI lists only panes that host an agent.

## What each state becomes

| herdr-core `status.state` | herdr-tui `agent_status` | Reported as |
|---|---|---|
| `working` | `working` | working |
| `idle`, `done` | `idle`, `done` | ready for its owner (the two differ only in whether a person has looked) |
| `blocked`, kind `permission` | | waiting for permission |
| `blocked`, kind `question`, `auth` or none | `blocked` | waiting for input |
| `error` | | failed, with no error kind and no limit reset time |
| | `unknown` | nothing: the session keeps the state it had |
| any other word | any other word | nothing |

A session ends when herdr-core clears a terminal's status after its program
reported one (the program exited, or its shell came back to the foreground),
when a terminal's `state` is `finished`, or when two looks in a row no
longer list it. A TUI session ends when two looks in a row no longer list
the agent. A new agent started in the same terminal is a new run of the same
session id.

`turn` (core) and `completion_seq` (TUI) show a turn that began and ended
between two looks.

## An agent that has hooks of its own

Agents keep their own hooks inside Herdr, so Claude Code in a Herdr terminal
is already reported by `nucleated hook`, and in more detail. The hook wins,
and the driver does not report that terminal a second time.

A terminal outlives the agent in it. An agent killed without its
`SessionEnd` hook leaves a session that never ends, and another agent, with
no hooks, may be started in the same terminal. So a hook's hold on a
terminal or a pane lapses when the hook has said nothing for two minutes and
Herdr then shows the agent there at work or blocked while the hook's session
is not, for two minutes more with still no word from the hook. What Herdr
shows is reported from then on. A terminal at rest contradicts nothing, and
a hook that speaks again holds it again.

The variables below are ones any program in a terminal can set, so a hook's
hold is good only for what its own session shows. When Herdr shows an agent
blocked, or (herdr-core) failed, and the hook's session does not show the
same within a minute and a half, what Herdr shows is reported, and stays
reported until the hook's session shows it too or the agent moves on. And a
hook holds a terminal or pane only if its session's folder is the one Herdr
gives for it, a folder inside that one, or one above it that is still inside
the environment's `root`: Herdr names the folder of whatever is running in
front, which may be deeper than the agent's own. Set `root` to the folder
your work is under, or a hooked agent whose pane is elsewhere is reported
twice.

- herdr-core gives a terminal's processes `HC_TERMINAL_ID` and
  `HC_TERMINAL_SOCKET`. A hook that runs with an address-shaped
  `HC_TERMINAL_ID` and a `HC_TERMINAL_SOCKET` that is this environment's
  socket claims that terminal.
- The TUI gives a pane's processes `HERDR_ENV=1`, `HERDR_PANE_ID` and
  `HERDR_SOCKET_PATH`. A hook that runs with all three, the last being this
  environment's socket, claims that pane.
- The TUI's own Claude Code integration passes the agent's session id to
  Herdr, which lists it as `agent_session`. An agent whose `agent_session`
  id is a session a hook is reporting is left to the hook, whatever its
  variables say.

A hook works out which socket is this environment's the same way the daemon
does. With `driver_socket` set, both read it from the config. Without it,
the hook uses its own `XDG_RUNTIME_DIR`, `XDG_STATE_HOME` or
`XDG_CONFIG_HOME`, which are the agent's and not the daemon's: if Herdr
starts its agents with any of them changed, the hook finds another path,
claims nothing, and the agent is reported twice. Set `driver_socket` on such
a box.

What the adapters therefore report is the agents Nucarium has no hook
adapter for.

## When Herdr is not there

No socket, a server that refuses, hangs or answers with something else, or a
Herdr that no longer knows the call: the look finds nothing, no event is
written, no session is ended, and whether the environment's server is alive
is still asked only of the `unit` or `check` the config names.
`nucleated status` shows one fixed word:

| Word | Meaning |
|---|---|
| `absent` | no socket at that path |
| `unreachable` | a socket nothing is listening on, or one that hung up |
| `rejected` | the socket may not be opened by this user |
| `timeout` | connected, and no answer in time |
| `bad_reply` | an answer that is not the documented one, or one too long |
| `error` | Herdr answered the call with an error, as a newer or older Herdr that does not know the call does. What the error says is not read |

## What Herdr gives the box no way to report

- **A link.** Neither product documents an address that opens one terminal,
  agent or machine: no URL scheme, no web address. `link_template` is none
  and no `driver_ref` is sent. An alert can name the machine and, from the
  session id, the terminal address.
- **Tool activity.** Both report a state word and nothing about tools, so no
  `activity` beats are sent while an agent stays working. Watch must not
  apply its "no tool activity" rule to these environments.
- **Why a turn failed.** herdr-core gives the reason only as the program's
  free text, which is not read; the TUI has no error state at all, and a
  failed agent there reads as idle. No error kind, usage limit or reset time
  comes from either.
- **What an agent is blocked on, in the TUI.** It says blocked and not on
  what, so every block is reported as waiting for input.

## Limits worth knowing

- The TUI works out most agents' state by reading the bottom of the screen,
  and says of itself that a prompt it does not recognise can show as idle. A
  needs-input alert can be missed for that reason.
- herdr-core knows only what a program reports with OSC 7501. An agent that
  reports nothing is invisible to this adapter.
- herdr-core's turn counter goes up whenever a status leaves `working`, for
  a block as well as for a finish. A block that came and went entirely
  between two looks is therefore reported as a turn that ended and another
  that began.
- A TUI pane moved to another workspace gets a new pane id, while its
  processes keep the old one in `HERDR_PANE_ID`. A hook in such a pane no
  longer claims it, and unless Herdr lists the agent's own session id the
  agent is reported twice.
- An agent started with Herdr's variables scrubbed from its environment is
  likewise reported twice.
- herdr-core is in preview. Its API "only grows", and the adapter says
  nothing about a state or a field it does not know.
