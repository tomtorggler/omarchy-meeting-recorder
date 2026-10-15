//! A disposable in-memory index of ordinary meeting folders. Disk remains canonical.
use std::path::{Path, PathBuf};

use crate::meeting::{self, Manifest};

#[derive(Clone)]
pub struct Entry {
    pub dir: PathBuf,
    pub manifest: Manifest,
    pub transcript: String,
    pub problem: Option<String>,
}

#[derive(Clone)]
pub struct Hit {
    pub at_ms: Option<i64>,
    pub text: String,
}

pub fn root() -> PathBuf {
    crate::settings::meetings_folder()
}

/// Reserve a fresh directory before exporting; back-to-back calls can share a title.
pub fn reserve_folder(preferred: &Path) -> std::io::Result<PathBuf> {
    if let Some(parent) = preferred.parent() {
        std::fs::create_dir_all(parent)?;
    }
    for n in 1..10_000 {
        let path = if n == 1 {
            preferred.to_path_buf()
        } else {
            preferred.with_file_name(format!(
                "{} {n}",
                preferred.file_name().unwrap_or_default().to_string_lossy()
            ))
        };
        match std::fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("too many meetings with this name"))
}

pub fn scan(root: &Path) -> std::io::Result<Vec<Entry>> {
    let folders = match std::fs::read_dir(root) {
        Ok(folders) => folders,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut entries: Vec<_> = folders
        .flatten()
        .filter_map(|folder| {
            let path = folder.path();
            if !path.is_dir() {
                return None;
            }
            let (dir, manifest) = meeting::open(&path)?;
            let transcript = std::fs::read_to_string(dir.join("transcript.md")).unwrap_or_default();
            let problem = problem(&dir);
            Some(Entry {
                dir,
                manifest,
                transcript,
                problem,
            })
        })
        .collect();
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.manifest.started_at));
    Ok(entries)
}

impl Entry {
    /// Title and participant matches open the meeting; text matches seek to the quote.
    pub fn search(&self, query: &str) -> Vec<Hit> {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return vec![Hit {
                at_ms: None,
                text: String::new(),
            }];
        }
        let mut hits = Vec::new();
        if self.manifest.calendar.as_ref().is_some_and(|m| {
            m.participants.iter().any(|p| {
                p.name.to_lowercase().contains(&query) || p.email.to_lowercase().contains(&query)
            })
        }) || self.manifest.title.to_lowercase().contains(&query)
            || self
                .manifest
                .speakers
                .iter()
                .any(|s| s.to_lowercase().contains(&query))
        {
            hits.push(Hit {
                at_ms: None,
                text: String::new(),
            });
        }
        for line in self.transcript.lines() {
            if let Some((time, speaker, text)) = meeting::parse_segment(line)
                && text.to_lowercase().contains(&query)
            {
                hits.push(Hit {
                    at_ms: Some(meeting::clock_to_ms(time)),
                    text: format!("{time} · {speaker}: {text}"),
                });
            }
        }
        hits
    }
}

pub fn processing(dir: &Path) -> std::io::Result<()> {
    crate::bookmarks::atomic_write(
        &dir.join(".processing"),
        b"Transcription was interrupted. Try transcribing again.",
    )
}

pub fn completed(dir: &Path, problem: Option<&str>) -> std::io::Result<()> {
    if let Some(problem) = problem {
        crate::bookmarks::atomic_write(&dir.join(".processing"), problem.as_bytes())
    } else {
        match std::fs::remove_file(dir.join(".processing")) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

pub fn problem(dir: &Path) -> Option<String> {
    let problem = std::fs::read_to_string(dir.join(".processing")).ok()?;
    // Older versions made a missing worker executable sound like missing audio.
    if let Some(detail) =
        problem.strip_prefix("Transcription failed: could not start transcription: ")
    {
        return Some(format!(
            "The previous transcription attempt could not start the transcription worker. Choose Transcribe again to retry. Worker startup error: {detail}"
        ));
    }
    Some(problem)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clarifies_saved_worker_launch_errors_without_rewriting_them() {
        let dir = std::env::temp_dir().join(format!("mr-worker-error-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let saved = "Transcription failed: could not start transcription: No such file or directory (os error 2).";
        completed(&dir, Some(saved)).unwrap();
        let displayed = problem(&dir).unwrap();
        assert!(displayed.contains("previous transcription attempt"));
        assert!(displayed.contains("transcription worker"));
        assert!(displayed.contains("Transcribe again"));
        assert!(displayed.contains("os error 2"));
        assert_eq!(
            std::fs::read_to_string(dir.join(".processing")).unwrap(),
            saved
        );
        let other = "Transcription failed: audio decoder failed.";
        completed(&dir, Some(other)).unwrap();
        assert_eq!(problem(&dir).as_deref(), Some(other));
        completed(&dir, None).unwrap();
        assert!(problem(&dir).is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn reserves_distinct_folders_without_touching_existing_meetings() {
        let root = std::env::temp_dir().join(format!("mr-reserve-{}", std::process::id()));
        let preferred = root.join("202610071000 Standup");
        let first = reserve_folder(&preferred).unwrap();
        std::fs::write(first.join("transcript.md"), "keep me").unwrap();
        let second = reserve_folder(&preferred).unwrap();
        assert_ne!(first, second);
        assert!(second.is_dir());
        assert_eq!(
            std::fs::read_to_string(first.join("transcript.md")).unwrap(),
            "keep me"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn indexes_legacy_folders_and_finds_timestamped_unicode_text() {
        let root = std::env::temp_dir().join(format!("mr-library-{}", std::process::id()));
        let older = root.join("202609241400 Planning");
        let newer = root.join("202610071000 Review");
        std::fs::create_dir_all(&older).unwrap();
        std::fs::create_dir_all(&newer).unwrap();
        std::fs::write(
            older.join("transcript.md"),
            "## Transcript\n\n**[01:02] You:** Über migration.\n",
        )
        .unwrap();
        processing(&older).unwrap();
        let entries = scan(&root).unwrap();
        assert_eq!(entries[0].manifest.title, "Review");
        assert!(entries[1].problem.is_some());
        let hits = entries[1].search("ÜBER");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].at_ms, Some(62_000));
        assert!(entries[0].search("migration").is_empty());
        completed(&older, None).unwrap();
        assert!(problem(&older).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }
}
