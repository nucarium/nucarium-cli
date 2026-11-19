# Orca

`driver = "orca"` in an `[[environment]]` table. The adapter is
`src/driver/orca.rs`; its tests and its part of the privacy gate are in
`tests/orca.rs`. What holds for every driver is in the README under Drivers.

```toml
[[environment]]
name = "orca"
# Orca's workspace directory. Orca makes and removes the worktrees in it, so
# it may not be, hold or sit inside a nucleus, and housekeeping never touches
# anything under it.
root = "/home/agent/orca/workspaces"
driver = "orca"
# unit = "orca-serve.service"    # the default
# driver_poll = "15s"
# driver_cli = "/opt/orca-server/bin/orca-ide"
# driver_home = "/home/agent/.config/orca"
# driver_dirs = ["/srv/more-orca-worktrees"]
```

Everything below is from Orca's own documentation and source
(`stablyai/orca`, read at version 1.4.214). Nothing was learned by running
Orca: the adapter is tested against a stand-in that prints the answer Orca's
source describes.

## Setting it up

Orca runs on a headless Linux box as `orca-ide serve`, and
`orca-ide serve service install` writes the systemd user unit
`orca-serve.service`. That unit is the environment's `unit` unless the
config names another. Whether the environment is alive is asked of systemd,
as for every environment, and never of Orca.

- `root` is Orca's workspace directory. Orca's default is
  `~/orca/workspaces`; if you changed that setting in Orca, name the same
  folder here. `nucleated` does not read Orca's settings.
- `driver_cli` is Orca's command-line program. Left out, it is `orca-ide` on
  `PATH` (`orca` on a machine that is not Linux).
- `driver_home` is Orca's data folder, for a box that runs Orca from a
  folder other than its default. It is passed to Orca's program as
  `ORCA_USER_DATA_PATH`.
- `driver_dirs` lists any other folders Orca keeps worktrees in.
- `nucleated` must run as the same Unix user as Orca. Orca's program reaches
  the server over a socket that is private to that user.

## The one command

Every `driver_poll`, on the driver thread, `nucleated` runs

```
<driver_cli> worktree ps --json
```

and nothing else: Orca documents it as the way for another tool to read a
running Orca. It is never given `--agent-wait`, which Orca's source calls too
costly for a poller. No other Orca command is ever run, and `nucleated` reads
none of Orca's own files: not its runtime file, its token or its database.

The command is given nothing on standard input and five seconds. Every
`ORCA_` variable the daemon itself holds is taken out of the command's
environment first. Orca's program reads some of them as "ask a paired server
on another machine" and others as "this call comes from one of Orca's own
agents", and the daemon asks the Orca on its own box, as nobody. A test
asserts the arguments and the environment of every run.

## What is read from the answer

Orca has no way to ask for state alone. The answer to `worktree ps` also
carries each agent's prompt, title, last reply and tool input, and each
worktree's terminal preview, comment and linked pull request. The answer is
parsed as it streams, into structs that name these fields and no others;
every other key is skipped without being read, and the program's own
complaints are discarded.

| Field | Used for |
|---|---|
| `ok` | Whether this is an answer or an error reply |
| `result.truncated` | Whether the list is whole |
| `worktrees[].path` | On the box only: the repo name and the branch, from git's own files there |
| `worktrees[].workspaceKind` | A `folder-workspace` has no git, so no repo is looked for |
| `agents[].orcaSessionId` | The session's id |
| `agents[].state`, `agents[].mainAgent.state` | What the agent is doing |
| `agents[].mainAgent.outcome` | How a finished turn ended |
| `agents[].updatedAt` | That a working agent is still doing things |
| `agents[].restoredUnconfirmed` | Orca itself is not sure of this row |
| `agents[].agentType` | On the box only: whether the agent's own hook could report it |

A value of the wrong kind in one of these fields is nothing. The session id
must be a UUID, which is what Orca mints; a row with anything else there, or
with no id yet, is not reported. The state and the outcome must be one of
Orca's own words. The worktree's id is never read, because it contains a
path, and the repo and branch strings Orca sends are not read either.

## What each state becomes

The main agent's own state is used where the row carries one, so an agent
whose subagents are still running reads as its own turn does.

| Orca says | Session event |
|---|---|
| first seen | `session_start`, then the event for its state |
| `working` | `user_prompt_submit`; while it stays working and `updatedAt` moves, `activity` once per beat |
| `blocked`, `waiting` | `input_prompt` |
| `done`, outcome `failure` or `interruption` | `stop_failure`, with that word as `error_kind` |
| `done`, any other outcome or none | `stop` |
| no longer listed, in two whole lists in a row | `session_end` |
| `restoredUnconfirmed`, or a word the adapter does not know | nothing: the session stays as it was |

An agent that was already idle when Orca is first seen is history and is
remembered without an event. If it is gone before it does anything, no
`session_end` is sent for it either.

## Agents with their own hooks

A Claude Code or Codex agent started inside Orca still runs `nucleated hook`
when its config folder has the hooks block. The hook says more than Orca
does, so the hook wins: once a Claude Code hook has spoken from inside one
of Orca's worktrees, the Claude Code agents Orca lists in that worktree are
not reported from Orca, until the hook's session ends. A hook that has gone
quiet does not hold that for ever: when it has said nothing for two minutes,
and Orca then shows such an agent at work or waiting on you while the hook's
session is not, for two minutes more, Orca's view is reported again. A hook
never keeps from you an agent that waits on you or has failed: when Orca
shows either and no hook's session in that worktree shows the same a minute
and a half on, Orca's view of that agent is reported, until a hook's session
shows it or the agent moves on. Codex has a hook for an approval and none
for any other wait, so a hooked Codex that Orca shows `blocked` or `waiting`
on anything else is reported from Orca too while it waits. A Codex
hook does the same for the Codex agents there. A Codex beside a hooked Claude Code, with
no hooks of its own, is still reported from Orca, and so is every other
agent Orca runs.

The mark that a worktree's agents of one kind are spoken for is a number
made from the worktree's path, and the agent's config name. The folder's
name is not written to the state folder.

## Unsaved work

Orca's worktrees are ordinary git worktrees, so the count needs nothing from
Orca and works while Orca is stopped. The reporter looks in `root` and each
`driver_dirs` folder for folders that hold a `.git`, one level down or two
(`<dir>/<name>`, and `<dir>/<repo>/<name>` with Orca's nested layout on), and
asks each the same question it asks a nucleus worktree. Links are not
followed. A folder with no `.git` is a folder workspace and has nothing to
count. If one of the folders cannot be read, the count is not known and no
number is sent.

Orca can keep a worktree outside its workspace directory, and its answer to
`worktree ps` says where each one is. At every look those paths are compared,
on the box, with the folders the count is made from; only how many are left
out is kept, never a path. While Orca lists a linked worktree that is in none
of those folders, the count would leave its work out, so no number is sent
for the environment or for the box, and `nucleated status` shows
`N worktree(s) outside its folders, unsaved work not counted` beside the
driver. Name the folder that holds it in `driver_dirs` and the count comes
back at the next one. A project's own checkout is not such a worktree, and
neither is a nucleus session opened in Orca, which its nucleus counts.

The heartbeat reports the count for the environment and adds it to the
machine's totals. It never reports a snapshot time for an Orca environment:
snapshots write to a repository, and nothing here writes to Orca's.
Housekeeping skips the environment, refuses a job asked for it by hand, and
passes by any worktree under its folders that it meets through a repository
a nucleus shares. An Orca worktree outside those folders is passed by too:
housekeeping, and `nucleate` itself, change only the worktrees under a
nucleus's own `.worktrees/`, and never prune the worktree records of a
repository that holds any other.

## What `nucleated status` says

`driver orca: <word> since HH:MM`, where the word is one of:

| Word | Meaning |
|---|---|
| `ok` | Orca answered |
| `absent` | There is no program at `driver_cli` that this user can run |
| `error` | The program ran and said it failed, or printed an error reply. With no Orca running, this is what its program does. What the error says is not read |
| `unreachable` | The program ran, said it succeeded, and printed nothing |
| `timeout` | No answer in five seconds; the program was stopped. Also when the program exited but left something running that still holds its output open: the look ends at five seconds all the same |
| `bad_reply` | It printed something that is not the documented answer, or more than any real answer holds |

None of these ends a session, writes an event or changes whether the
environment is alive. A look that fails is asked again less often, up to
once a minute.

## What Orca gives no way to report

- **A link.** Orca's `orca://` links pair a device and install plugins,
  skills and bots. None opens a worktree, an agent or a session, in the
  desktop app or the phone app, so an alert for an Orca environment carries
  no link.
- **Permission or question.** `blocked` and `waiting` do not say which, so
  both are reported as waiting for input.
- **An error kind beyond the two words above, and when a usage limit
  resets.** These reach Watch for an Orca agent only through the agent's own
  hook.
- **The agent vendor's own session id.** The session is known by Orca's id.
- **Outcomes and pull requests.** Not reported from Orca.

## Limits to know about

- Waiting for input is seen at the next look, up to `driver_poll` late.
- A session ends when Orca stops listing it: closed, exited, or put to sleep
  mid-turn. An agent that comes back later is a new session.
- If Orca cuts its list short, no session is ended for being missing from it.
- An agent is placed by its repo only when the repo is on GitHub or the
  folder is the project's own checkout. A worktree's folder is named after
  the task, not the repo, so for a repo hosted elsewhere nothing is said
  from Orca of where the agent works. A hook that speaks from inside such a
  worktree names the folder of the repository the worktree was added from,
  never the worktree's own.
- The hook rule works per worktree and kind of agent. Two Claude Code agents
  in one worktree, one with the hooks block and one without, are both left
  to the one hook.
- Unpushed commits are counted against what Orca last fetched. `nucleated`
  never fetches in Orca's repositories.
- The projects Orca was pointed at are their owner's, not Orca's, and work
  in them is not counted.
- Orca's answer is described by its source, not by a published schema. If a
  release renames a field, the adapter falls silent; `nucleated status` shows
  when Orca last answered.
