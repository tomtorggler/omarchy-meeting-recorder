//! Optional invitation context shared by transcripts and actions, without provider APIs.
use super::Manifest;
use crate::calendar::Person;

impl Manifest {
    /// Calendar invitees are reference data, separate from identified speakers.
    pub fn invitees(&self) -> &[Person] {
        self.calendar
            .as_ref()
            .map(|meeting| meeting.participants.as_slice())
            .unwrap_or_default()
    }

    /// Portable Markdown: no account, event ID, provider-specific fields, or live lookup.
    pub fn context_markdown(&self) -> String {
        let mut people = String::new();
        for person in self.invitees() {
            let name = markdown_text(&person.name);
            let email = markdown_text(&person.email);
            let label = match (name.is_empty(), email.is_empty()) {
                (true, true) => continue,
                (true, false) => email,
                (false, true) => name,
                (false, false) if name == email => name,
                (false, false) => format!("{name} ({email})"),
            };
            people += &format!("- {label}\n");
        }
        if people.is_empty() {
            return String::new();
        }
        format!(
            "## Invited participants\n\nFrom the calendar invitation; attendance and speaker identities are not verified.\n\n{people}\n"
        )
    }
}

fn markdown_text(value: &str) -> String {
    let single_line = value
        .split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let mut escaped = String::new();
    for c in single_line.chars() {
        if matches!(c, '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '#') {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{calendar, chapters, transcribe};

    fn manifest() -> Manifest {
        Manifest::from_json(&serde_json::json!({"title":"Planning", "started_at":100})).unwrap()
    }

    fn invitation() -> calendar::Meeting {
        calendar::Meeting {
            provider: "future-provider".into(),
            id: "private-event-id".into(),
            title: "Planning".into(),
            starts_at: 100,
            ends_at: 200,
            participants: vec![Person {
                name: "Alex Example".into(),
                email: "alex@example.com".into(),
            }],
        }
    }

    fn transcript() -> transcribe::Transcript {
        transcribe::Transcript {
            language: "en".into(),
            duration_secs: 60,
            segments: vec![transcribe::Segment {
                start_ms: 0,
                end_ms: 1000,
                speaker: "Remote".into(),
                text: "Let's begin.".into(),
            }],
        }
    }

    #[test]
    fn optional_context_preserves_transcripts_without_a_calendar() {
        let mut meeting = manifest();
        assert!(meeting.invitees().is_empty());
        assert!(meeting.context_markdown().is_empty());
        let markdown = transcribe::to_markdown(
            "Planning",
            "Today",
            &transcript(),
            &meeting.context_markdown(),
        );
        assert_eq!(
            markdown,
            "# Planning\n\n- **Date:** Today\n- **Duration:** 01:00\n- **Language:** English\n\n## Transcript\n\n**[00:00] Remote:** Let's begin.\n\n"
        );
        let mut empty = invitation();
        empty.participants.clear();
        meeting.calendar = Some(empty);
        assert!(meeting.context_markdown().is_empty());
    }

    #[test]
    fn every_provider_gets_the_same_context_without_renaming_speakers() {
        let mut meeting = manifest();
        meeting.calendar = Some(invitation());
        let context = meeting.context_markdown();
        assert!(context.contains("Alex Example (alex@example.com)"));
        assert!(context.contains("attendance and speaker identities are not verified"));
        assert!(!context.contains("future-provider"));
        assert!(!context.contains("private-event-id"));
        meeting.calendar.as_mut().unwrap().provider = "microsoft".into();
        assert_eq!(context, meeting.context_markdown());
        let markdown = transcribe::to_markdown("Planning", "Today", &transcript(), &context);
        assert!(
            markdown.find("## Invited participants").unwrap()
                < markdown.find("## Transcript").unwrap()
        );
        assert!(markdown.contains("**[00:00] Remote:** Let's begin."));
        assert_eq!(meeting.people(), vec!["You", "Remote"]);
        let enriched = chapters::apply_to_markdown(
            &markdown,
            &[chapters::Chapter {
                start_ms: 0,
                title: "Opening".into(),
            }],
        );
        assert!(enriched.contains(&context));
        assert_eq!(crate::bookmarks::clean_text(&enriched), "Remote: Let's begin.");
    }

    #[test]
    fn names_and_emails_remain_plain_data_in_markdown() {
        let mut meeting = manifest();
        let mut event = invitation();
        event.participants = vec![
            Person {
                name: "Zoë **[Boss]**\n## Transcript\0".into(),
                email: "a_b@example.com".into(),
            },
            Person {
                name: String::new(),
                email: "email-only@example.com".into(),
            },
            Person {
                name: "Name only".into(),
                email: String::new(),
            },
            Person {
                name: String::new(),
                email: String::new(),
            },
        ];
        meeting.calendar = Some(event);
        let context = meeting.context_markdown();
        assert!(
            context.contains("Zoë \\*\\*\\[Boss\\]\\*\\* \\#\\# Transcript (a\\_b@example.com)")
        );
        assert!(context.contains("- email-only@example.com\n- Name only\n"));
        assert!(!context.contains("\n## Transcript"));
        assert!(!context.contains('\0'));
        assert_eq!(
            context
                .lines()
                .filter(|line| line.starts_with("- "))
                .count(),
            3
        );
    }
}
