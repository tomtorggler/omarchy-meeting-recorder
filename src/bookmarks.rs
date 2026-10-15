//! Bookmarks, portable transcript exports and atomic writes, independent of the UI.
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
pub struct Bookmark {
    pub at_ms: i64,
    pub label: String,
}

pub fn bookmarks(dir: &Path) -> Vec<Bookmark> {
    std::fs::read(dir.join("bookmarks.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub fn save_bookmarks(dir: &Path, bookmarks: &[Bookmark]) -> std::io::Result<()> {
    atomic_write(
        &dir.join("bookmarks.json"),
        &serde_json::to_vec_pretty(bookmarks)?,
    )
}

/// Replace only after a complete, flushed write. Unique temps also handle multiple windows.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    let temp = path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        SERIAL.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temp);
    }
    result
}

pub fn clean_text(markdown: &str) -> String {
    markdown
        .lines()
        .filter_map(crate::meeting::parse_segment)
        .map(|(_, speaker, text)| format!("{speaker}: {text}"))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Save a fresh copy; never overwrite an earlier export or another meeting.
pub fn export(dir: &Path, destination: &Path) -> std::io::Result<PathBuf> {
    let markdown = std::fs::read(dir.join("transcript.md"))?;
    std::fs::create_dir_all(destination)?;
    let name = dir.file_name().unwrap_or_default().to_string_lossy();
    for n in 1..10_000 {
        let suffix = if n == 1 {
            String::new()
        } else {
            format!(" ({n})")
        };
        let path = destination.join(format!("{name}{suffix}.md"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                if let Err(error) = file.write_all(&markdown).and_then(|_| file.sync_all()) {
                    let _ = std::fs::remove_file(&path);
                    return Err(error);
                }
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("too many copies of this meeting"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_exports_preserve_previous_copy() {
        let root = std::env::temp_dir().join(format!("mr-notes-{}", std::process::id()));
        let meeting = root.join("Meeting");
        std::fs::create_dir_all(&meeting).unwrap();
        std::fs::write(meeting.join("transcript.md"), "original").unwrap();
        let first = export(&meeting, &root.join("notes")).unwrap();
        std::fs::write(meeting.join("transcript.md"), "edited").unwrap();
        let second = export(&meeting, &root.join("notes")).unwrap();
        assert_ne!(first, second);
        assert_eq!(std::fs::read_to_string(first).unwrap(), "original");
        assert_eq!(std::fs::read_to_string(second).unwrap(), "edited");
        save_bookmarks(
            &meeting,
            &[Bookmark {
                at_ms: 1200,
                label: "Decision".into(),
            }],
        )
        .unwrap();
        assert_eq!(bookmarks(&meeting)[0].at_ms, 1200);
        std::fs::remove_dir_all(root).unwrap();
    }
}
