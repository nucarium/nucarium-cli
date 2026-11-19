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

/// Why an owner's settings cannot be applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// Not a JSON object, or its `hooks` are not the shape hooks take.
    Invalid,
    /// It sets the variable that names a login's config folder, which would
    /// point every login at one folder.
    ForbiddenKey,
}

/// A hook handler `nucleated` or `nucleate` owns: a command handler whose
/// program is `nucleated` and whose first argument is `hook`, or whose
/// program is `nucleate` and whose first argument is one of its two hooks.
/// The program may be written with a folder in front, as the recipe writes
/// it, and in quotes.
pub fn owned(handler: &Value) -> bool {
    if handler["type"] != "command" {
        return false;
    }
    let Some(command) = handler["command"].as_str() else {
        return false;
    };
    let mut words = command.split_whitespace();
    let program = words
        .next()
        .unwrap_or_default()
        .trim_matches(|quote| quote == '"' || quote == '\'');
    let program = program.rsplit('/').next().unwrap_or_default();
    matches!(
        (program, words.next()),
        ("nucleated", Some("hook")) | ("nucleate", Some("hook-create" | "hook-remove"))
    )
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
pub fn without_owned(settings: &mut Map<String, Value>) {
    let Some(Value::Object(hooks)) = settings.get_mut("hooks") else {
        return;
    };
    for groups in hooks.values_mut() {
        let Value::Array(groups) = groups else {
            continue;
        };
        for group in groups.iter_mut() {
            if let Some(Value::Array(handlers)) = group.get_mut("hooks") {
                handlers.retain(|handler| !owned(handler));
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

/// The owned handlers of the settings on the box: per event, each matcher
/// group cut down to its owned handlers, with its matcher and in its order.
fn owned_groups(current: &Value) -> Vec<(String, Vec<Value>)> {
    let Some(hooks) = current["hooks"].as_object() else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for (event, groups) in hooks {
        let kept: Vec<Value> = groups
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|group| {
                let mut group = group.as_object()?.clone();
                let handlers: Vec<Value> = group
                    .get("hooks")?
                    .as_array()?
                    .iter()
                    .filter(|handler| owned(handler))
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
pub fn owners(settings: Value, config_dir_var: &str) -> Result<Value, Refused> {
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
    without_owned(&mut settings);
    Ok(Value::Object(settings))
}

/// What a sync writes: the owner's settings, as [`owners`] left them, with
/// the owned handlers of the file on the box added after the owner's own for
/// each event. Applying it again to its own result changes nothing.
pub fn merged(owner: &Value, current: Option<&Value>) -> Value {
    let mut result = owner.as_object().cloned().unwrap_or_default();
    let kept = current.map(owned_groups).unwrap_or_default();
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
pub fn same_but_for_owned(owner: &Value, current: &Value) -> bool {
    let Some(mut current) = current.as_object().cloned() else {
        return false;
    };
    without_owned(&mut current);
    let mut owner = owner.as_object().cloned().unwrap_or_default();
    without_owned(&mut owner);
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

    fn handler(command: &str) -> Value {
        json!({ "type": "command", "command": command, "timeout": 5 })
    }

    #[test]
    fn a_handler_is_owned_by_the_shape_of_its_command() {
        for command in [
            "nucleated hook Stop",
            "$HOME/.local/bin/nucleated hook Stop --env client-a",
            "\"/home/agent/.local/bin/nucleated\" hook SessionEnd",
            "nucleate hook-create",
            "/usr/local/bin/nucleate hook-remove",
        ] {
            assert!(owned(&handler(command)), "{command}");
        }
        for command in [
            "nucleated status",
            "nucleate status",
            "echo nucleated hook Stop",
            "/opt/not-nucleated hook Stop",
            "$HOME/.claude/hooks/session-env.sh",
            "",
        ] {
            assert!(!owned(&handler(command)), "{command}");
        }
        // Only a command handler runs a program.
        assert!(!owned(
            &json!({ "type": "prompt", "command": "nucleated hook Stop" })
        ));
        assert!(!owned(&json!("nucleated hook Stop")));
    }

    #[test]
    fn the_owners_settings_replace_the_file_and_the_owned_handlers_stay() {
        let owner = owners(
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
                "Notification": [{ "matcher": "permission_prompt", "hooks": [handler("nucleated hook Notification --env a")] }],
                "PreToolUse": [{ "matcher": "Bash", "hooks": [handler("old-guard.sh")] }],
            },
        });
        let result = merged(&owner, Some(&on_the_box));
        assert_eq!(
            result,
            json!({
                "permissions": { "deny": ["Read(./.env)"] },
                "hooks": {
                    "Stop": [
                        { "hooks": [handler("say done")] },
                        { "hooks": [handler("nucleated hook Stop --env a")] },
                    ],
                    "Notification": [{ "matcher": "permission_prompt", "hooks": [handler("nucleated hook Notification --env a")] }],
                },
            })
        );
        // Applied to its own result, it changes nothing.
        assert_eq!(merged(&owner, Some(&result)), result);
        assert_eq!(text(&merged(&owner, Some(&result))), text(&result));
        // With no file on the box there is nothing owned to keep.
        assert_eq!(merged(&owner, None), owner);
    }

    #[test]
    fn the_repository_can_neither_forge_nor_replace_an_owned_handler() {
        let owner = owners(
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
            json!({ "hooks": { "Stop": [{ "hooks": [handler("nucleated hook Stop")] }] } }),
            VAR,
        )
        .unwrap();
        assert_eq!(only_forged, json!({}));
    }

    #[test]
    fn settings_that_are_not_settings_are_refused() {
        assert_eq!(owners(json!([1, 2]), VAR), Err(Refused::Invalid));
        assert_eq!(owners(json!("settings"), VAR), Err(Refused::Invalid));
        assert_eq!(owners(json!({ "hooks": [] }), VAR), Err(Refused::Invalid));
        assert_eq!(
            owners(json!({ "hooks": { "Stop": "notify.sh" } }), VAR),
            Err(Refused::Invalid)
        );
        assert_eq!(
            owners(
                json!({ "env": { "CLAUDE_CONFIG_DIR": "/home/agent/.claude" } }),
                VAR
            ),
            Err(Refused::ForbiddenKey)
        );
        assert!(owners(json!({ "env": { "EDITOR": "vi" } }), VAR).is_ok());
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
        let owner = owners(json!({ "model": "a" }), VAR).unwrap();
        let registered = json!({
            "model": "a",
            "hooks": { "Stop": [{ "hooks": [handler("nucleated hook Stop")] }] },
        });
        assert!(same_but_for_owned(&owner, &registered));
        assert!(same_but_for_owned(&owner, &json!({ "model": "a" })));
        assert!(!same_but_for_owned(&owner, &json!({ "model": "b" })));
        assert!(!same_but_for_owned(
            &owner,
            &json!({ "model": "a", "hooks": { "Stop": [{ "hooks": [handler("mine.sh")] }] } })
        ));
        assert!(!same_but_for_owned(&owner, &json!([])));
    }
}
