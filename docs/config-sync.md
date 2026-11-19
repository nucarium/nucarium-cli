# Config sync

`[config_sync]` in the daemon's config. The work is in
`src/config_sync/sync.rs` (with `sync/repo.rs` for what git is asked and
`sync/merge.rs` for the settings merge); its tests and its part of the
privacy gate are in `tests/config_sync.rs`. The config keys, the three
values a heartbeat carries and the six words for how a sync stands are in
the README under Config sync.

```toml
[config_sync]
repository = "git@github.com:your-org/agent-config.git"
branch = "main"
# interval = "hourly"
# allowed_signers = "/home/agent/.config/nucleated/config-signers"
# on_drift = "overwrite"
```

Only Claude Code environments are synced: it is the one agent whose config
folder this page can describe from the vendor's own documentation. An
environment with another `agent`, one watched through a driver, and one with
no `config_dir` are passed over, and nothing is said about their config.

## The repository

```
settings.json                 your settings
CLAUDE.md                     the instructions file
skills/<name>/...             one folder per skill
environments/<env-name>/      optional: the same three, for one environment
```

An environment's own folder is laid over the root:

- its `CLAUDE.md` replaces the root's;
- each `skills/<name>/` it has replaces the root's skill of that name whole;
- its `settings.json` is merged onto the root's by the rule Claude Code
  documents for layered managed settings: a single value is replaced, lists
  are combined with repeats left out, nested objects are merged key by key.

Everything else in the repository is ignored, unread.

## What is synced, and what never is

The only paths read from the repository and written under a config folder:

| In the repository | In the config folder |
|---|---|
| `settings.json` | `settings.json`, merged (see below) |
| `CLAUDE.md` | `CLAUDE.md` |
| `skills/<name>/**` | `skills/<name>/**` |

A skill folder named `synced` (in any capitalisation), one whose name starts
with `anthropic-skills`, and one whose name starts with `.` are Claude Code's
own and are not synced.

Never opened, in the repository or on the box: `.credentials.json`,
`.claude.json`, `plugins/`, `projects/`, `history.jsonl`, and anything else
not in the table. `/etc/claude-code` is never written. MCP servers are kept
by Claude Code in `.claude.json`, beside account details, so they are not
synced as files; name a plugin marketplace and the plugins you want in your
synced `settings.json` (`extraKnownMarketplaces`, `enabledPlugins`) and
Claude Code fetches them itself.

A commit is refused whole (`unsafe_entry`) if a symbolic link or a submodule
sits on a synced path. A config folder is not written (`unsafe_destination`)
if a synced path in it passes through a symbolic link, or if a folder stands
where a file should. Limits: 2,000 files, 1 MB a file, 20 MB in all
(`too_large`).

Files are written whole (beside their place, then moved in), mode 644, or
755 where the repository marks a file executable. A file an earlier sync
wrote that the commit no longer has is removed. A file the sync did not
write is never removed.

## Your settings, under the entries `nucleate` and `nucleated` own

The settings file is merged, never copied. Owned means a hook handler of
type `command` whose program is `nucleated` and whose first argument is
`hook`, or whose program is `nucleate` and whose first argument is
`hook-create` or `hook-remove`. The program may have a folder in front.

1. Anything in your settings that looks owned is taken out. The repository
   can neither forge an owned entry nor replace one.
2. The owned handlers of the file on the box are kept: per event, each
   matcher group cut down to its owned handlers.
3. The result is your settings with, for each event, your groups first and
   the owned groups after. Everything else that was in the file on the box
   is replaced.

Applying twice gives the same file. The sync never installs an owned entry
that is missing; it only keeps the ones that are there. The `nucleate` MCP
registration is in `.claude.json`, which the sync does not open.

Your first sync therefore replaces whatever the box recipe put in
`settings.json` apart from those hooks. Start your repository's
`settings.json` from the file that is on the box, and read the dry run.

Refused: settings that are not a JSON object or whose `hooks` are not the
shape hooks take (`invalid_settings`); settings that set
`env.CLAUDE_CONFIG_DIR`, which would point every login at one folder
(`forbidden_key`). If the file on the box cannot be read as JSON the sync
stops (`settings_unreadable`), because the owned entries could not be
carried over.

Claude Code applies a changed settings file to running sessions without a
restart, so a sync can change hooks and permissions mid-session. Managed
settings, from your organisation or in `/etc/claude-code`, win over the user
settings a sync writes; the commit reported is what is on disk, not what an
organisation's policy lets the agent honour.

## The dry run comes first

Nothing is applied to an environment until you have run

```
nucleated config sync --dry-run [--env NAME]
```

for it. It fetches, checks the commit and prints what would change:

```json
{ "dry_run": true, "signatures": "not_checked", "environments": [ {
  "environment": "client-a", "action": "would_apply",
  "commit": "…", "applied_commit": null, "modified": null, "detail": null,
  "changes": [ { "path": "CLAUDE.md", "change": "add" },
               { "path": "settings.json", "change": "update" } ] } ] }
```

`signatures` is `required` when a commit has to be signed by a signer you
name before it is applied, and `not_checked` when it does not. Read it before
you let a first apply go ahead: after the dry run, whoever can push to the
branch can change what runs on the box. A misspelled key cannot turn the
check off behind your back, because a key the config does not know stops the
sync with an error that names it.

Until then the environment stands at `pending`. A dry run of a commit that
is refused shows no changes and does not count. Naming another repository
or another branch asks for a dry run again. After the dry run the daemon
applies at its next pass, and each newer commit after that by itself.

`nucleated config sync` without `--dry-run` fetches and applies now, and
puts back files changed by hand. `action` is one of `applied`,
`up_to_date`, `would_apply`, `skipped`, or the state word a sync stopped at.

A dry run changes nothing, and that includes `config-sync.json`: with no
daemon running, `nucleated status` says nothing new about config after one.
The daemon's next pass, or a run without `--dry-run`, writes it. A file
changed by hand is put back by the next commit or by `nucleated config
sync`, and not by the scheduled pass while the branch stands still.

## Verifying a commit

With `allowed_signers` set, the newest commit must carry an SSH signature
by a key that file names, checked with `git verify-commit` and
`gpg.ssh.allowedSignersFile`, with your own git config and the system's
left out of the check. Keep the file outside the synced repository. Only
the newest commit is checked: it commits to its whole tree. OpenPGP and
X.509 signatures are not accepted. A commit that is not a descendant of the
one applied is refused too (`not_descendant`), so an older signed commit
cannot be replayed.

Signing protects against someone who can push to the repository. It does
not protect against the box: the agent user owns the daemon's config and
the signers file. A merge made in a Git host's web page may be signed by
the host and not by you, and is then refused.

The fetch uses whatever access to the repository the agent user's git
already has. The sync adds no credential and stores none. Files are read
straight from the commit's tree; nothing is checked out, so no hook, filter
or attribute of the repository runs.

## Changed by hand

Each pass (every 30 seconds) looks at each file the sync wrote, once, and
reads one only when its size, time or mode has moved. `settings.json` is
compared by meaning: it counts as changed only if your own settings in it
differ from the commit's. Reformatting it, or registering `nucleated`'s
hooks again, is not a change. Claude Code's own `/config` and `/plugin`
write this file, and that does count: keep those keys in the repository.

A file added by hand beside the synced ones is not noticed. The flag clears
when the files match again or a commit is applied.

With `on_drift = "overwrite"` (the default) the next commit replaces a
changed file. With `on_drift = "keep"` the environment stands at `held`
until the change is undone.

## What is kept on the box

In the cache folder (`~/.cache/nucleated/config-sync`, or under
`cache_dir`):

| Path | Holds |
|---|---|
| `repo.git/` | The bare clone. |
| `fetch.json` | When the last fetch was, and the commit it found. |
| `env/<name>/sync.json` | The files the sync wrote there, by name, and that your dry run was run. |
| `env/<name>/owner-settings.json` | Your settings as last applied. |
| `env/<name>/replaced/` | One copy of every file the last sync replaced or removed, at its path. A later sync that replaces anything replaces this copy. |

The state folder holds only `config-sync.json`: commit ids, yes-or-noes,
times and the fixed words below. The repository's address and branch, file
names and contents, and git's own words for an error are never logged, put
in the state folder or sent. They are not printed by `nucleated config sync`
at a terminal either: git's words can quote the repository's address, with
whatever is in it, and the command's output is one that gets pasted
elsewhere. To see why git refused something, run the git command yourself.

## The finer word in `nucleated status`

| Word | State | Means |
|---|---|---|
| `awaiting_dry_run` | `pending` | No dry run yet for this environment and repository. |
| `hand_change` | `held` | `on_drift = "keep"` and a synced file was changed. |
| `fetch_failed` | `unreachable` | Git ran and the fetch was refused: the repository is not there, will not let this box in, or has no such branch. Tried again in five minutes. |
| `fetch_timeout` | `unreachable` | The fetch ran over a minute and was stopped. Tried again in five minutes. |
| `no_git` | `unreachable` | There is no `git` on the daemon's `PATH`. Tried again in five minutes. |
| `no_signers` | `rejected` | The signers file is not there. |
| `unverified` | `rejected` | No SSH signature by a named signer. |
| `not_descendant` | `rejected` | The branch no longer contains the applied commit. |
| `invalid_settings` | `rejected` | See above. |
| `forbidden_key` | `rejected` | See above. |
| `unsafe_entry` | `rejected` | A link or submodule on a synced path, or a name that is not a plain one. |
| `too_large` | `rejected` | Over a limit. |
| `read_failed` | `failed` | Git could not read the commit from the clone. |
| `no_config_dir` | `failed` | The environment's `config_dir` is not a folder. The sync does not create it. |
| `settings_unreadable` | `failed` | See above. |
| `unsafe_destination` | `failed` | See above. |
| `write_failed` | `failed` | A file, or the copy of what it replaces, could not be written. |

A refused commit is not checked again until the branch moves or the signers
file changes. A failed write is tried again after five minutes. Whatever
stops a sync, the config that was there stays, the heartbeat goes on, and
no session is touched.
