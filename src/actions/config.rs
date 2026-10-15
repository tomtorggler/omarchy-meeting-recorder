//! Lossless editing of the shared action configuration. A stale editor never
//! overwrites an action changed in another window or by an external editor.
use super::Action;
use std::path::Path;
use toml_edit::{DocumentMut, Item, Table, value};

pub fn load() -> Vec<Action> {
    read().unwrap_or_default()
}

pub fn read() -> Result<Vec<Action>, String> {
    parse(&read_text(&crate::models::config_file())?)
}

fn read_text(path: &Path) -> Result<String, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(format!("Could not read actions: {e}")),
    }
}

fn document(text: &str) -> Result<DocumentMut, String> {
    text.parse().map_err(|e| format!("Check config.toml: {e}"))
}

fn from_table(table: &Table) -> Option<Action> {
    let name = table.get("name")?.as_str()?.to_owned();
    let command = table.get("command")?.as_str()?.to_owned();
    if name.trim().is_empty() || command.trim().is_empty() {
        return None;
    }
    Some(Action {
        name,
        command,
        // Only a TOML boolean opts in; strings such as "true" do not.
        auto: table
            .get("auto")
            .and_then(Item::as_bool)
            .unwrap_or(false),
    })
}

pub(super) fn parse(text: &str) -> Result<Vec<Action>, String> {
    let doc = document(text)?;
    Ok(doc
        .get("action")
        .and_then(Item::as_array_of_tables)
        .into_iter()
        .flatten()
        .filter_map(from_table)
        .collect())
}

pub fn save(previous: Option<&Action>, action: &Action) -> Result<(), String> {
    edit_file(&crate::models::config_file(), previous, Some(action))
}

pub fn remove(action: &Action) -> Result<(), String> {
    edit_file(&crate::models::config_file(), Some(action), None)
}

fn edit_file(
    path: &Path,
    previous: Option<&Action>,
    action: Option<&Action>,
) -> Result<(), String> {
    let text = read_text(path)?;
    let next = edit(&text, previous, action)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    crate::bookmarks::atomic_write(path, next.as_bytes()).map_err(|e| e.to_string())
}

fn edit(text: &str, previous: Option<&Action>, action: Option<&Action>) -> Result<String, String> {
    let mut doc = document(text)?;
    if doc.get("action").is_none() {
        doc["action"] = Item::ArrayOfTables(toml_edit::ArrayOfTables::new());
    }
    let tables = doc["action"]
        .as_array_of_tables_mut()
        .ok_or("Actions must use [[action]] tables in config.toml")?;
    let index = match previous {
        Some(old) => Some(
            tables
                .iter()
                .position(|t| from_table(t).as_ref() == Some(old))
                .ok_or("This action changed elsewhere. Reopen the action manager and try again.")?,
        ),
        None => None,
    };
    if let Some(action) = action {
        let name = action.name.trim();
        let command = action.command.trim();
        if name.is_empty() || command.is_empty() {
            return Err("Enter a name and command".into());
        }
        if tables
            .iter()
            .enumerate()
            .any(|(i, t)| Some(i) != index && t.get("name").and_then(Item::as_str) == Some(name))
        {
            return Err("An action with this name already exists".into());
        }
        let mut table = index
            .map(|i| tables.get(i).unwrap().clone())
            .unwrap_or_default();
        table["name"] = value(name);
        table["command"] = value(command);
        table["auto"] = value(action.auto);
        if let Some(i) = index {
            *tables.get_mut(i).unwrap() = table;
        } else {
            tables.push(table);
        }
    } else if let Some(i) = index {
        tables.remove(i);
    }
    Ok(doc.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crud_preserves_other_settings_comments_and_unknown_action_fields() {
        let original = "# keep me\nmodel = 'tiny'\n\n[[action]]\nname='Old'\ncommand='echo old'\nextra=42\n\n[calendar]\nclient='abc' # keep too\n";
        let old = parse(original).unwrap().remove(0);
        assert!(!old.auto);
        let next = Action {
            name: "New".into(),
            command: "echo \"$1\"\n# hi".into(),
            auto: true,
        };
        let edited = edit(original, Some(&old), Some(&next)).unwrap();
        assert_eq!(parse(&edited).unwrap(), std::slice::from_ref(&next));
        for kept in [
            "# keep me",
            "model = 'tiny'",
            "extra=42",
            "client='abc' # keep too",
        ] {
            assert!(edited.contains(kept), "lost {kept}");
        }
        assert!(edit(&edited, Some(&old), None).is_err());
        assert!(edit(&edited, None, Some(&next)).is_err());
        let deleted = edit(&edited, Some(&next), None).unwrap();
        assert!(parse(&deleted).unwrap().is_empty());
        assert!(deleted.contains("[calendar]"));
        assert_eq!(
            parse(&edit(&deleted, None, Some(&next)).unwrap()).unwrap(),
            [next]
        );
    }

    #[test]
    fn malformed_config_is_not_overwritten_and_auto_requires_boolean() {
        let action = Action {
            name: "Test".into(),
            command: "echo yes".into(),
            auto: false,
        };
        assert!(edit("broken = [", None, Some(&action)).is_err());
        assert!(edit("action = 'wrong shape'", None, Some(&action)).is_err());
        assert!(
            !parse("[[action]]\nname='Test'\ncommand='echo yes'\nauto='true'").unwrap()[0]
                .auto
        );
    }
}
