//! Live state for the bar widget.
//!
//! The app listens on a Unix socket in $XDG_RUNTIME_DIR and writes one JSON
//! line per tick to every connected client: 20 times a second while recording,
//! once a second otherwise. `omarchy-meeting-recorder watch` connects to it and
//! copies those lines to stdout, printing `{"state":"off"}` while the app is not
//! running, so the widget only has to read NDJSON from a process.
//!
//! A line looks like:
//! {"state":"recording","elapsed":754,"title":"Weekly","mic":0.62,"computer":0.31,"progress":0.0}
//! with `mic` and `computer` as meter levels from 0 to 1, `progress` the
//! transcription progress from 0 to 1 while the state is "transcribing", and
//! `folder` the meeting's folder once it has one ("" before).
//!
//! `omarchy-meeting-recorder wait` reads the same lines to wait for a meeting
//! to be done and print its folder, for scripts and agents.

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gtk::glib;

use crate::APP_NAME;
use crate::audio::{Source, to_meter};

const MAX_LINE: usize = 4096;

#[derive(Clone, Default)]
pub struct Status {
    /// idle, recording, paused, stopping, transcribing or done
    pub state: &'static str,
    pub started_at: i64,
    /// Seconds spent paused so far, and when the current pause began (0: not paused).
    pub paused_secs: i64,
    pub pause_began: i64,
    pub title: String,
    pub progress: f64,
    /// The meeting's folder, once it has one.
    pub folder: String,
    pub stage: String,
}

pub type SharedStatus = Arc<Mutex<Status>>;
/// One status per open window; the socket reports the busiest.
pub type Statuses = Arc<Mutex<Vec<SharedStatus>>>;

/// How much a window's state matters to the bar: the one recording first.
fn rank(state: &str) -> u8 {
    match state {
        "recording" | "paused" => 4,
        "stopping" => 3,
        "transcribing" => 2,
        "done" => 1,
        _ => 0,
    }
}

/// The status the bar widget shows: the busiest window's.
pub fn busiest(statuses: &Statuses) -> Status {
    statuses
        .lock()
        .unwrap()
        .iter()
        .map(|s| s.lock().unwrap().clone())
        .max_by_key(|s| rank(s.state))
        .unwrap_or(Status {
            state: "idle",
            ..Default::default()
        })
}

fn socket_path() -> PathBuf {
    glib::user_runtime_dir().join(format!("{APP_NAME}.sock"))
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Commands a client may send, one per line. `start` may be followed by a
/// space and the meeting's name, `notes` by the path of a notes file, and
/// `name` by a name for the meeting about to be recorded.
pub const COMMANDS: [&str; 7] = [
    "name",
    "start",
    "stop",
    "compact",
    "pause",
    "new-window",
    "notes",
];

/// A command from a client, and what came with it: the name for `start`, the
/// notes file's path for `notes` ("" without).
pub type Command = (&'static str, String);

/// Starts the socket server. Called once, from the primary instance. Clients
/// get the state lines of the busiest window; a line a client writes that
/// names one of `COMMANDS` is passed on to `commands`.
pub fn serve(
    statuses: Statuses,
    mic: Source,
    system: Source,
    commands: async_channel::Sender<Command>,
) {
    let path = socket_path();
    // A socket file left behind by a crash refuses new binds; nobody answers on it.
    if UnixStream::connect(&path).is_err() {
        let _ = std::fs::remove_file(&path);
    }
    let Ok(listener) = UnixListener::bind(&path) else {
        eprintln!("{APP_NAME}: could not listen on {}", path.display());
        return;
    };
    let clients: Arc<Mutex<Vec<UnixStream>>> = Arc::default();

    let accepted = clients.clone();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            // A write timeout rather than non-blocking mode: the flag would be
            // shared with the reading clone below.
            if stream
                .set_write_timeout(Some(Duration::from_millis(20)))
                .is_err()
            {
                continue;
            }
            if let Ok(reader) = stream.try_clone() {
                let commands = commands.clone();
                thread::spawn(move || read_commands(reader, &commands));
            }
            accepted.lock().unwrap().push(stream);
        }
    });

    thread::spawn(move || {
        loop {
            let snapshot = busiest(&statuses);
            let recording = snapshot.state == "recording";
            let taking = recording || snapshot.state == "paused";
            let until = if snapshot.pause_began > 0 {
                snapshot.pause_began
            } else {
                now()
            };
            let busy = recording || snapshot.state == "transcribing";
            let line = serde_json::json!({
                "state": snapshot.state,
                "elapsed": if taking { (until - snapshot.started_at - snapshot.paused_secs).max(0) } else { 0 },
                "title": snapshot.title,
                "mic": round(to_meter(mic.recent_peak(3))),
                "computer": round(to_meter(system.recent_peak(3))),
                "progress": round(snapshot.progress),
                "folder": snapshot.folder,
                "warning": if taking { mic.health().map(|s| format!("Microphone: {s}"))
                    .or_else(|| system.health().map(|s| format!("Computer: {s}"))) } else { None },
            })
            .to_string()
                + "\n";
            // A client that cannot keep up is dropped rather than waited for.
            clients
                .lock()
                .unwrap()
                .retain_mut(|client| match client.write_all(line.as_bytes()) {
                    Ok(()) => true,
                    Err(e) => e.kind() == ErrorKind::Interrupted,
                });
            thread::sleep(Duration::from_millis(if recording {
                50
            } else if busy {
                250
            } else {
                1000
            }));
        }
    });
}

fn read_commands(stream: UnixStream, commands: &async_channel::Sender<Command>) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.by_ref().take(MAX_LINE as u64).read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                if let Some(command) = parse_command(&line) {
                    let _ = commands.send_blocking(command);
                }
            }
        }
    }
}

fn parse_command(line: &str) -> Option<Command> {
    let line = line.trim();
    let (name, title) = match line.split_once(' ') {
        Some(("start", title)) => ("start", title.trim()),
        Some(("notes", path)) => ("notes", path.trim()),
        Some(("name", title)) => ("name", title.trim()),
        _ => (line, ""),
    };
    COMMANDS
        .iter()
        .find(|c| **c == name)
        .map(|c| (*c, title.to_owned()))
}

/// `omarchy-meeting-recorder start "Weekly"`: the line that starts a recording
/// with that name. Whitespace is collapsed, so a name can never add a line.
pub fn start_line(title: &[String]) -> String {
    let title = title
        .iter()
        .flat_map(|word| word.split_whitespace())
        .collect::<Vec<_>>()
        .join(" ");
    if title.is_empty() {
        "start".to_owned()
    } else {
        format!("start {title}")
    }
}

/// `--name <name>`: the line that names the meeting about to be
/// recorded, without starting it. None without a name.
pub fn name_line(title: &[String]) -> Option<String> {
    let title = title
        .iter()
        .flat_map(|word| word.split_whitespace())
        .collect::<Vec<_>>()
        .join(" ");
    (!title.is_empty()).then(|| format!("name {title}"))
}

/// `omarchy-meeting-recorder --notes <file>`: the line that opens those notes.
/// None for a path that would add a line.
pub fn notes_line(path: &std::path::Path) -> Option<String> {
    let path = path.to_str()?;
    (!path.contains('\n') && !path.trim().is_empty()).then(|| format!("notes {path}"))
}

/// `omarchy-meeting-recorder stop`: ask the running app to stop recording.
pub fn send(command: &str) -> bool {
    match UnixStream::connect(socket_path()) {
        Ok(mut stream) => stream.write_all(format!("{command}\n").as_bytes()).is_ok(),
        Err(_) => false,
    }
}

/// `omarchy-meeting-recorder wait [--audio]`: waits until the meeting that
/// is being recorded is done and prints its folder. With `audio_only` it is
/// back as soon as the audio is saved, before the transcript. Started before
/// the recorder, it waits for it. False when the recorder closes before the
/// meeting is done.
pub fn wait(audio_only: bool) -> bool {
    let mut busy = false;
    loop {
        let Ok(stream) = UnixStream::connect(socket_path()) else {
            if busy {
                return false;
            }
            thread::sleep(Duration::from_millis(500));
            continue;
        };
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.by_ref().take(MAX_LINE as u64).read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let Ok(status) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            let state = status["state"].as_str().unwrap_or_default();
            let folder = status["folder"].as_str().unwrap_or_default();
            if let Some(done) = finished(state, folder, busy, audio_only) {
                println!("{done}");
                return true;
            }
            busy |= matches!(state, "recording" | "paused" | "stopping" | "transcribing");
        }
    }
}

/// The folder to print once the meeting `wait` waits for is far enough along.
fn finished<'a>(state: &str, folder: &'a str, busy: bool, audio_only: bool) -> Option<&'a str> {
    let far_enough = match state {
        "done" => busy,
        "transcribing" => audio_only,
        _ => false,
    };
    (far_enough && !folder.is_empty()).then_some(folder)
}

fn round(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// `omarchy-meeting-recorder watch`: relay the app's state lines to stdout.
pub fn watch() {
    let mut stdout = std::io::stdout();
    loop {
        if let Ok(stream) = UnixStream::connect(socket_path()) {
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.by_ref().take(MAX_LINE as u64).read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) if !line.ends_with('\n') => break, // over-long line: not ours
                    Ok(_) => {
                        if stdout
                            .write_all(line.as_bytes())
                            .and_then(|_| stdout.flush())
                            .is_err()
                        {
                            return; // the widget went away
                        }
                    }
                }
            }
        }
        if writeln!(stdout, r#"{{"state":"off"}}"#)
            .and_then(|_| stdout.flush())
            .is_err()
        {
            return;
        }
        thread::sleep(Duration::from_secs(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(state: &'static str, title: &str) -> SharedStatus {
        Arc::new(Mutex::new(Status {
            state,
            title: title.into(),
            ..Default::default()
        }))
    }

    #[test]
    fn start_may_carry_a_name() {
        assert_eq!(parse_command("start\n"), Some(("start", String::new())));
        assert_eq!(
            parse_command("start  Product Review \n"),
            Some(("start", "Product Review".into()))
        );
        assert_eq!(parse_command("stop\n"), Some(("stop", String::new())));
        // Only start takes a name.
        assert_eq!(parse_command("stop now\n"), None);
        assert_eq!(parse_command("starting\n"), None);
    }

    #[test]
    fn notes_can_name_the_meeting() {
        let args = ["Voice-over\nstop".to_owned(), " prioriteit ".to_owned()];
        assert_eq!(name_line(&args).as_deref(), Some("name Voice-over stop prioriteit"));
        assert_eq!(name_line(&[]), None);
        assert_eq!(
            parse_command("name Voice-over prioriteit\n"),
            Some(("name", "Voice-over prioriteit".into()))
        );
    }

    #[test]
    fn notes_take_a_path() {
        assert_eq!(
            parse_command("notes /tmp/a script.md\n"),
            Some(("notes", "/tmp/a script.md".into()))
        );
        let path = std::path::Path::new("/tmp/a script.md");
        assert_eq!(
            notes_line(path).as_deref(),
            Some("notes /tmp/a script.md")
        );
        assert_eq!(notes_line(std::path::Path::new("/tmp/a\nstop")), None);
    }

    #[test]
    fn wait_prints_the_folder_once_the_meeting_is_done() {
        let folder = "/home/me/Documents/Meetings/202610131549 Voice-over";
        assert_eq!(finished("done", folder, true, false), Some(folder));
        assert_eq!(finished("transcribing", folder, true, false), None);
        assert_eq!(finished("transcribing", folder, true, true), Some(folder));
        // A meeting that was already done before waiting began is not the one.
        assert_eq!(finished("done", folder, false, false), None);
        assert_eq!(finished("done", "", true, false), None);
    }

    #[test]
    fn a_name_cannot_add_a_command() {
        let args = ["Weekly\nstop".to_owned(), " sync ".to_owned()];
        assert_eq!(start_line(&args), "start Weekly stop sync");
        assert_eq!(start_line(&[]), "start");
        assert_eq!(start_line(&[" ".to_owned()]), "start");
    }

    #[test]
    fn the_bar_follows_the_busiest_window() {
        let statuses = Statuses::default();
        assert_eq!(busiest(&statuses).state, "idle");
        statuses.lock().unwrap().extend([
            status("done", "Old meeting"),
            status("transcribing", "Weekly"),
            status("idle", ""),
        ]);
        assert_eq!(busiest(&statuses).title, "Weekly");
        statuses
            .lock()
            .unwrap()
            .push(status("recording", "Standup"));
        assert_eq!(busiest(&statuses).title, "Standup");
    }
}
