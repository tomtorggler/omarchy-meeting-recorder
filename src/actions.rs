//! Actions: your own scripts, run on a finished meeting from the done page.
//!
//! They live in `config.toml`, each with a name for the menu and a command:
//!
//! ```toml
//! [[action]]
//! name = "Store transcript in Obsidian"
//! command = "~/.local/bin/meeting-to-obsidian"
//! ```
//!
//! The command runs through `sh -c` in the meeting folder, with that folder as
//! `$1` and the meeting described in `MEETING_*` variables. What it prints last
//! is shown when it is done; a link there (web or `obsidian://`) goes behind an
//! Open button instead. An action may change the meeting itself: when it
//! edited the transcript or the meeting file, the done page reads them again.
//!
//! An action with `auto = true` also runs on its own, as soon as a meeting's
//! transcript is done (and its chapters, when an agent writes them).

use std::path::Path;
use std::process::Command;

use crate::meeting::Manifest;

/// How actions work, for people and their agents; the done page links here
/// when there are none yet.
pub const DOCS: &str =
    "https://github.com/jankeesvw/omarchy-meeting-recorder/blob/main/docs/actions.md";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Action {
    pub name: String,
    pub command: String,
    /// Runs by itself when a transcript is done, besides being in the menu.
    pub auto: bool,
}

mod config;
pub use config::{load, read, remove, save};

/// The actions that run on their own when a transcript is done, in the order
/// of the config file.
pub fn automatic() -> Vec<Action> {
    load().into_iter().filter(|a| a.auto).collect()
}

/// How an action ended: the last thing it printed, and a link in it if any.
pub struct Outcome {
    pub message: String,
    pub url: Option<String>,
}

/// Runs `action` on the meeting in `dir`. Blocking; the caller runs it off the
/// main thread.
pub fn run(action: &Action, dir: &Path, manifest: &Manifest) -> Result<Outcome, String> {
    run_cancellable(action, dir, manifest, &crate::transcribe::Abort::default())
}

pub fn run_cancellable(
    action: &Action,
    dir: &Path,
    manifest: &Manifest,
    cancel: &crate::transcribe::Abort,
) -> Result<Outcome, String> {
    let date = gtk::glib::DateTime::from_unix_local(manifest.started_at)
        .and_then(|t| t.format("%Y-%m-%d %H:%M"))
        .map(|s| s.to_string())
        .unwrap_or_default();
    let audio = ["audio.ogg", "mic.ogg", "computer.ogg"]
        .iter()
        .map(|f| dir.join(f))
        .find(|p| p.exists());
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(&action.command)
        .arg("meeting-action")
        .arg(dir)
        .current_dir(dir)
        .env("MEETING_DIR", dir)
        .env("MEETING_TRANSCRIPT", dir.join("transcript.md"))
        .env(
            "MEETING_MANIFEST",
            crate::meeting::find(dir).unwrap_or_default(),
        )
        .env("MEETING_TITLE", &manifest.title)
        .env("MEETING_DATE", &date)
        .env("MEETING_STARTED_AT", manifest.started_at.to_string())
        .env("MEETING_DURATION", manifest.duration_secs.to_string())
        .env("MEETING_LANGUAGE", &manifest.language)
        .env("MEETING_SPEAKERS", manifest.people().join("\n"))
        .env("MEETING_CONTEXT", manifest.context_markdown())
        .env(
            "MEETING_INVITEES_JSON",
            serde_json::to_string(manifest.invitees())
                .map_err(|e| format!("Could not encode meeting invitees: {e}"))?,
        )
        .env("MEETING_AUDIO", audio.unwrap_or_default());
    let output = crate::process::output(
        &mut command,
        cancel,
        std::time::Duration::from_secs(30 * 60),
    )?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let why = last_line(&stderr)
            .or_else(|| last_line(&stdout))
            .unwrap_or_else(|| format!("exited with {}", output.status));
        return Err(why);
    }
    let line = last_line(&stdout).unwrap_or_default();
    let url = find_url(&line);
    // The link goes behind the Open button, so the message is only the words.
    let message = match &url {
        Some(url) => line.replace(url.as_str(), ""),
        None => line,
    };
    let message = message.trim().trim_end_matches(':').trim().to_owned();
    Ok(Outcome {
        message: if message.is_empty() {
            "Done".to_owned()
        } else {
            message
        },
        url,
    })
}

/// `action "<name>" <meeting>`: runs one of your actions on a meeting folder
/// or `.meeting-recorder` file, exactly as the done page does, and prints how
/// it went. For trying an action out, by you or by your agent.
pub fn cli(args: &[String]) -> gtk::glib::ExitCode {
    use gtk::glib::ExitCode;
    let actions = load();
    let [name, meeting] = args else {
        eprintln!(
            "Usage: {} action \"<name>\" <meeting folder>",
            crate::APP_NAME
        );
        eprintln!();
        if actions.is_empty() {
            eprintln!(
                "No actions yet; add them to {}",
                crate::models::config_file().display()
            );
            eprintln!("See {DOCS}");
        } else {
            eprintln!("Actions in {}:", crate::models::config_file().display());
            for action in &actions {
                let auto = if action.auto {
                    "  (runs on its own when a transcript is done)"
                } else {
                    ""
                };
                eprintln!("  {}{auto}", action.name);
            }
        }
        return ExitCode::from(2);
    };
    let Some(action) = actions.iter().find(|a| &a.name == name) else {
        eprintln!(
            "No action called \"{name}\" in {}",
            crate::models::config_file().display()
        );
        return ExitCode::FAILURE;
    };
    let Some((dir, manifest)) = crate::meeting::open(Path::new(meeting)) else {
        eprintln!("No meeting in {meeting}");
        return ExitCode::FAILURE;
    };
    match run(action, &dir, &manifest) {
        Ok(outcome) => {
            println!("{}", outcome.message);
            ExitCode::SUCCESS
        }
        Err(why) => {
            eprintln!("{} failed: {why}", action.name);
            ExitCode::FAILURE
        }
    }
}

/// When the transcript and the meeting file of `dir` last changed, and their
/// sizes: compared before and after an action to see whether it edited them.
pub fn fingerprint(dir: &Path) -> Vec<Option<(std::time::SystemTime, u64)>> {
    [Some(dir.join("transcript.md")), crate::meeting::find(dir)]
        .into_iter()
        .map(|path| {
            let meta = std::fs::metadata(path?).ok()?;
            Some((meta.modified().ok()?, meta.len()))
        })
        .collect()
}

fn last_line(text: &str) -> Option<String> {
    text.lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.chars().take(200).collect())
}

/// A link to open: a web page, or a note in Obsidian.
fn find_url(text: &str) -> Option<String> {
    text.split_whitespace()
        .find(|w| {
            ["https://", "http://", "obsidian://"]
                .iter()
                .any(|s| w.starts_with(s))
        })
        .map(|w| w.trim_end_matches(['.', ',', ')', '"', '\'']).to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions_get_optional_provider_neutral_context_for_existing_transcripts() {
        let root = std::env::temp_dir().join(format!("mr-action-context-{}", std::process::id()));
        let dir = root.join("202610091000 Planning");
        std::fs::create_dir_all(&dir).unwrap();
        let original = "# Planning\n\n## Transcript\n\n**[00:00] Remote:** Hi.\n";
        std::fs::write(dir.join("transcript.md"), original).unwrap();
        let (_, mut manifest) = crate::meeting::open(&dir).unwrap();
        let action = Action {
            name: "Capture context".into(),
            command: r#"printf '%s' "$MEETING_CONTEXT" > context.md
printf '%s' "$MEETING_INVITEES_JSON" > invitees.json
printf '%s' "$MEETING_SPEAKERS" > speakers.txt"#
                .into(),
            auto: false,
        };
        for provider in [Some("microsoft"), Some("future-provider"), None] {
            manifest.calendar = provider.map(|provider| crate::calendar::Meeting {
                provider: provider.into(),
                id: "local-snapshot".into(),
                title: "Planning".into(),
                starts_at: 100,
                ends_at: 200,
                participants: vec![crate::calendar::Person {
                    name: "Zoë \"Example\" $(touch injected)".into(),
                    email: "zoe@example.com".into(),
                }],
            });
            crate::meeting::write(&dir, &manifest).unwrap();
            // Exercise the same disk snapshot path used by manual and automatic runs.
            let (_, saved) = crate::meeting::open(&dir).unwrap();
            run(&action, &dir, &saved).unwrap();
            let invitees: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(dir.join("invitees.json")).unwrap())
                    .unwrap();
            assert_eq!(invitees, serde_json::json!(manifest.invitees()));
            assert_eq!(
                std::fs::read_to_string(dir.join("context.md")).unwrap(),
                manifest.context_markdown()
            );
            assert_eq!(
                std::fs::read_to_string(dir.join("speakers.txt")).unwrap(),
                "You\nRemote"
            );
            assert_eq!(
                std::fs::read_to_string(dir.join("transcript.md")).unwrap(),
                original
            );
            assert!(!dir.join("injected").exists());
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reads_actions_and_skips_the_rest() {
        let config = r#"
model = "large-v3-turbo"  # not an action

[[action]]
name = "Copy to Obsidian"
command = "~/.local/bin/meeting-to-obsidian"

[[action]]
name = 'Publish'   # a comment
command = 'publish "$1" --secret'

[[action]]
name = "Store in my notes"
command = "~/bin/store-meeting"
auto = true   # runs on its own when the transcript is done

[[action]]
name = "No command, ignored"

[other]
name = "not an action"
"#;
        assert_eq!(
            config::parse(config).unwrap(),
            [
                Action {
                    name: "Copy to Obsidian".into(),
                    command: "~/.local/bin/meeting-to-obsidian".into(),
                    auto: false,
                },
                Action {
                    name: "Publish".into(),
                    command: "publish \"$1\" --secret".into(),
                    auto: false,
                },
                Action {
                    name: "Store in my notes".into(),
                    command: "~/bin/store-meeting".into(),
                    auto: true,
                },
            ]
        );
        let parsed = config::parse(config).unwrap();
        let automatic: Vec<&str> = parsed
            .iter()
            .filter(|a| a.auto)
            .map(|a| a.name.as_str())
            .collect();
        assert_eq!(automatic, ["Store in my notes"]);
    }

    #[test]
    fn a_link_goes_behind_the_button_and_out_of_the_message() {
        let dir = std::env::temp_dir().join(format!("mr-action-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = Manifest {
            calendar: None,
            title: "Weekly".into(),
            started_at: 0,
            duration_secs: 60,
            format: crate::export::Format::Mono,
            language: "en".into(),
            speakers: vec!["Maya".into(), "Tom".into()],
            labels: Vec::new(),
            imported: None,
            speaker_count: None,
            model: None,
            chapters: Vec::new(),
            chapters_by: None,
        };
        let action = |command: &str| Action {
            name: "Test".into(),
            command: command.into(),
            auto: false,
        };
        let saved = run(
            &action("echo busy; echo Saved obsidian://open?file=Weekly"),
            &dir,
            &manifest,
        )
        .unwrap();
        assert_eq!(saved.message, "Saved");
        assert_eq!(saved.url.as_deref(), Some("obsidian://open?file=Weekly"));
        let named = run(
            &action(r#"echo "$MEETING_TITLE by $(echo "$MEETING_SPEAKERS" | head -1)""#),
            &dir,
            &manifest,
        )
        .unwrap();
        assert_eq!(named.message, "Weekly by Maya");
        assert!(named.url.is_none());
        let failed = run(&action("echo nope >&2; exit 3"), &dir, &manifest);
        assert_eq!(failed.err().as_deref(), Some("nope"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_link_in_the_output_is_found() {
        assert_eq!(
            find_url("Published: https://gist.github.com/abc123."),
            Some("https://gist.github.com/abc123".into())
        );
        assert_eq!(
            find_url("Saved obsidian://open?vault=Writing&file=Meetings%2FWeekly"),
            Some("obsidian://open?vault=Writing&file=Meetings%2FWeekly".into())
        );
        assert_eq!(find_url("Copied to the vault"), None);
    }
}
