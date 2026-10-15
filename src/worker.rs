//! Isolated, cancellable transcription. Native model failures must never kill capture.
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Mutex, atomic::Ordering};
use std::time::Duration;

use gtk::glib;
use serde::{Deserialize, Serialize};

use crate::transcribe::{self, Abort, CANCELLED, Event, Events, Transcript};

// Only one model allocation at a time, even with several imported meetings.
static SLOT: Mutex<()> = Mutex::new(());

#[derive(Serialize, Deserialize)]
pub enum Tracks {
    Raw(PathBuf),
    Kept(PathBuf),
    Single(PathBuf, Option<usize>),
}

#[derive(Serialize, Deserialize)]
struct Request {
    tracks: Tracks,
    language: String,
    model: String,
}

#[derive(Serialize, Deserialize)]
enum Message {
    Stage(String),
    Progress(f64),
    Segment(String),
    Complete(Result<Transcript, String>),
}

pub fn transcribe(
    tracks: Tracks,
    language: &str,
    model: &str,
    events: &Events,
    abort: &Abort,
) -> Result<Transcript, String> {
    let request = Request {
        tracks,
        language: language.into(),
        model: model.to_owned(),
    };
    let _ = events.send_blocking(Event::Stage("Waiting for transcription".into()));
    let _slot = loop {
        if abort.load(Ordering::Relaxed) {
            return Err(CANCELLED.into());
        }
        if let Ok(slot) = SLOT.try_lock() {
            break slot;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let mut command = reexec_command();
    command.arg("__transcribe-worker");
    execute(command, &request, events, abort)
}

fn reexec_command() -> Command {
    // Keep the running version, even after an update replaces its executable.
    // On Linux, current_exe() would then return an unusable " (deleted)" path.
    Command::new("/proc/self/exe")
}

fn execute(
    mut command: Command,
    request: &Request,
    events: &Events,
    abort: &Abort,
) -> Result<Transcript, String> {
    command.process_group(0);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let parent = std::process::id() as libc::pid_t;
    // SAFETY: only async-signal-safe system calls in the child before exec.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() != parent {
                libc::_exit(1);
            }
            // Native assertions must not leave multi-GB model core dumps behind.
            let limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            libc::setrlimit(libc::RLIMIT_CORE, &limit);
            Ok(())
        });
    }
    let child = command
        .spawn()
        .map_err(|e| {
            format!(
                "could not start the transcription worker. Restart the app and choose Transcribe again. Worker startup error: {e}"
            )
        })?;
    let mut group = crate::process::ChildGroup {
        child,
        stopped: false,
    };
    let stdout = group.child.stdout.take().expect("piped stdout");
    let stderr = group.child.stderr.take().expect("piped stderr");
    let events = events.clone();
    let reader = std::thread::spawn(move || read_messages(stdout, &events));
    let diagnostics = std::thread::spawn(move || stderr_tail(stderr));
    let input = serde_json::to_writer(group.child.stdin.take().expect("piped stdin"), request);
    let observed = loop {
        if input.is_err() || abort.load(Ordering::Relaxed) {
            break Ok(());
        }
        match group.exited() {
            Ok(true) => break Ok(()),
            Ok(false) => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => break Err(e),
        }
    };
    // Signal before reaping, while the leader PID still reserves the process group.
    // This also closes pipes inherited by an unfinished decoder child.
    let status = group.stop();
    let result = reader
        .join()
        .unwrap_or_else(|_| Err("could not read transcription".into()));
    let diagnostic = diagnostics.join().unwrap_or_default();
    if abort.load(Ordering::Relaxed) {
        return Err(CANCELLED.into());
    }
    observed?;
    let status = status.map_err(|e| e.to_string())?;
    if !status.success() {
        return Err(format!(
            "transcription worker stopped ({status}). Audio is kept; try again. {diagnostic}"
        ));
    }
    input.map_err(|e| format!("could not send transcription request: {e}"))?;
    result
}

fn read_messages(input: impl Read, events: &Events) -> Result<Transcript, String> {
    let mut result = Err("transcription stopped before returning a result".into());
    for line in BufReader::new(input).lines() {
        let line = line.map_err(|e| e.to_string())?;
        let message: Message = serde_json::from_str(&line)
            .map_err(|e| format!("invalid transcription response: {e}"))?;
        let event = match message {
            Message::Stage(s) => Some(Event::Stage(s)),
            Message::Progress(p) => Some(Event::Progress(p)),
            Message::Segment(s) => Some(Event::Segment(s)),
            Message::Complete(value) => {
                result = value;
                None
            }
        };
        if let Some(event) = event {
            let _ = events.send_blocking(event);
        }
    }
    result
}

fn stderr_tail(mut input: impl Read) -> String {
    let mut tail = Vec::new();
    let mut bytes = [0; 4096];
    while let Ok(n) = input.read(&mut bytes) {
        if n == 0 {
            break;
        }
        tail.extend_from_slice(&bytes[..n]);
        if tail.len() > 4096 {
            tail.drain(..tail.len() - 4096);
        }
    }
    String::from_utf8_lossy(&tail)
        .lines()
        .last()
        .unwrap_or("")
        .chars()
        .take(300)
        .collect()
}

fn write_message(message: &Message) {
    let mut out = std::io::stdout().lock();
    let _ = serde_json::to_writer(&mut out, message);
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

/// Private CLI protocol; no GTK initialization and no access to live capture.
pub fn run() -> glib::ExitCode {
    let request: Request = match serde_json::from_reader(std::io::stdin().lock()) {
        Ok(request) => request,
        Err(_) => return glib::ExitCode::FAILURE,
    };
    crate::models::set_override(&request.model);
    let (tx, rx) = async_channel::unbounded();
    let reporter = std::thread::spawn(move || {
        while let Ok(event) = rx.recv_blocking() {
            let message = match event {
                Event::Stage(s) => Message::Stage(s),
                Event::Progress(p) => Message::Progress(p),
                Event::Segment(s) => Message::Segment(s),
                Event::Finished => break,
            };
            write_message(&message);
        }
    });
    let abort = Abort::default();
    let result = match request.tracks {
        Tracks::Single(path, speakers) => transcribe::load_track(&path).and_then(|audio| {
            transcribe::transcribe_single(&audio, &request.language, speakers, &tx, &abort)
        }),
        tracks => {
            let (mic, computer) = match tracks {
                Tracks::Raw(dir) => (dir.join("mic.raw"), dir.join("system.raw")),
                Tracks::Kept(dir) => crate::export::tracks(&dir),
                Tracks::Single(..) => unreachable!(),
            };
            transcribe::load_track(&mic).and_then(|mic| {
                let computer = transcribe::load_track(&computer)?;
                transcribe::transcribe(&mic, &computer, &request.language, &tx, &abort)
            })
        }
    };
    let _ = tx.send_blocking(Event::Finished);
    let _ = reporter.join();
    write_message(&Message::Complete(result));
    glib::ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> Request {
        Request {
            tracks: Tracks::Raw(PathBuf::from("unused")),
            language: "en".into(),
            model: "tiny".into(),
        }
    }

    #[test]
    fn worker_starts_after_app_binary_is_replaced() {
        const FIXTURE: &str = "MEETING_RECORDER_REEXEC_FIXTURE";
        const TEST: &str = "worker::tests::worker_starts_after_app_binary_is_replaced";
        if let Some(path) = std::env::var_os(FIXTURE) {
            let path = PathBuf::from(path);
            assert_eq!(std::env::current_exe().unwrap(), path);
            assert_eq!(path.file_name().unwrap(), "worker-test-copy");
            std::fs::remove_file(&path).unwrap();
            std::fs::write(&path, "replacement executable").unwrap();
            let output = reexec_command()
                .arg("--list")
                .env_remove(FIXTURE)
                .output()
                .unwrap();
            assert!(output.status.success());
            assert!(String::from_utf8_lossy(&output.stdout).contains(TEST));
            return;
        }

        let dir = std::env::temp_dir().join(format!("mr-reexec-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let copy = dir.join("worker-test-copy");
        std::fs::copy(std::env::current_exe().unwrap(), &copy).unwrap();
        let output = Command::new(&copy)
            .args(["--exact", TEST, "--nocapture"])
            .env(FIXTURE, &copy)
            .output()
            .unwrap();
        std::fs::remove_dir_all(dir).unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn launch_failure_names_the_worker_and_explains_retry() {
        let (tx, _) = async_channel::unbounded();
        let error = execute(
            Command::new("/proc/self/exe/not-an-executable"),
            &request(),
            &tx,
            &Abort::default(),
        )
        .err()
        .unwrap();
        assert!(error.contains("could not start the transcription worker"));
        assert!(error.contains("Transcribe again"));
    }

    #[test]
    fn native_failure_does_not_kill_parent() {
        let mut command = Command::new("sh");
        command.args(["-c", "cat >/dev/null; kill -ABRT $$"]);
        let (tx, _) = async_channel::unbounded();
        let result = execute(command, &request(), &tx, &Abort::default());
        assert!(result.err().unwrap().contains("worker stopped"));
    }

    #[test]
    fn cancellation_kills_a_busy_worker() {
        let mut command = Command::new("sh");
        command.args(["-c", "cat >/dev/null; exec sleep 30"]);
        let abort = Abort::default();
        let cancel = abort.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            cancel.store(true, Ordering::Relaxed);
        });
        let (tx, _) = async_channel::unbounded();
        let before = std::time::Instant::now();
        assert_eq!(
            execute(command, &request(), &tx, &abort).err().unwrap(),
            CANCELLED
        );
        assert!(before.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn exited_worker_cannot_leave_descendants_holding_protocol_pipes() {
        let mut command = Command::new("sh");
        command.args(["-c", "cat >/dev/null; sleep 30 & exit 0"]);
        let (tx, _) = async_channel::unbounded();
        let before = std::time::Instant::now();
        assert!(execute(command, &request(), &tx, &Abort::default()).is_err());
        assert!(before.elapsed() < Duration::from_secs(3));
    }
}
