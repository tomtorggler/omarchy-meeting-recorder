//! Bounded pipes for agents whose own files must not inherit an output-file limit.
//! Claude answers on stdout; Codex keeps its final answer separate from progress.
use super::*;

#[cfg(test)]
mod tests;

pub(super) fn run(
    agent: &Agent,
    prompt: &str,
    dir: &Path,
    built: Built,
    timeout: Duration,
    grace: Duration,
) -> Result<String, String> {
    use std::os::unix::process::ExitStatusExt;

    let mut answer = if built.capture == CaptureMode::CodexAnswer {
        Some(answer_pipe(dir)?)
    } else {
        None
    };
    let child = Command::new("setsid")
        .args(["timeout", "-k"])
        .arg(grace.as_secs_f64().to_string())
        .arg(timeout.as_secs_f64().to_string())
        .arg(&built.program)
        .args(&built.args)
        .envs(built.env.iter().map(|(k, v)| (k, v)))
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Could not start {}: {e}", agent.name))?;
    let mut group = crate::process::ChildGroup {
        child,
        stopped: false,
    };
    let mut stdout = Capture::new(
        group.child.stdout.take().unwrap(),
        MAX_STREAM_BYTES as usize,
    )?;
    let mut stderr = Capture::new(
        group.child.stderr.take().unwrap(),
        MAX_STREAM_BYTES as usize,
    )?;
    let mut stdin = group.child.stdin.take();
    set_nonblocking(stdin.as_ref().unwrap())?;
    let mut sent = 0;

    let started = Instant::now();
    let completed = loop {
        // Each drain is bounded in work as well as memory: continuously noisy
        // streams must not prevent checking the deadline or the other streams.
        stdout.drain()?;
        stderr.drain()?;
        if let Some(answer) = &mut answer {
            answer.drain()?;
        }
        if group.exited()? {
            break true;
        }
        if started.elapsed() > timeout + grace * 2 {
            break false;
        }
        if let Some(input) = &mut stdin {
            // A descendant can leave the owned group and retain unread stdin.
            // No blocking writer or join may extend this supervisor's deadline.
            let end = (sent + 64 * 1024).min(prompt.len());
            match input.write(&prompt.as_bytes()[sent..end]) {
                Ok(0) => stdin = None,
                Ok(n) => {
                    sent += n;
                    if sent == prompt.len() {
                        stdin = None;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => stdin = None,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(e) => return Err(e.to_string()),
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    // Even a successful agent may leave descendants holding stdin or any of
    // the three output channels. Close stdin and kill the owned group, and
    // never wait for pipe EOF. Drop also performs cleanup on every error path.
    drop(stdin);
    let status = group.stop().map_err(|e| e.to_string())?;
    stdout.drain()?;
    stderr.drain()?;
    if let Some(answer) = &mut answer {
        answer.drain()?;
    }
    let answer_bytes = answer
        .as_ref()
        .map_or(stdout.bytes.as_slice(), |answer| answer.bytes.as_slice());
    let stdout = String::from_utf8_lossy(&stdout.bytes);
    let stderr = String::from_utf8_lossy(&stderr.bytes);
    if !completed {
        return Err(format!(
            "{} did not answer within {} seconds",
            agent.name,
            timeout.as_secs()
        ));
    }
    // timeout's KILL can kill timeout itself as the session's group leader,
    // giving us a signal status rather than a shell's numeric 137.
    if matches!(status.code(), Some(124 | 137))
        || (status.signal() == Some(libc::SIGKILL) && started.elapsed() >= timeout)
    {
        return Err(format!(
            "{} did not answer within {} seconds",
            agent.name,
            timeout.as_secs()
        ));
    }
    if !status.success() {
        return Err(first_line(&stderr)
            .or_else(|| first_line(&stdout))
            .unwrap_or_else(|| format!("{} exited with status {status}", agent.name)));
    }
    let answer = tidy(&String::from_utf8_lossy(answer_bytes));
    let answer = truncate(&answer, MAX_ANSWER_BYTES);
    if answer.trim().is_empty() {
        return Err(
            first_line(&stderr).unwrap_or_else(|| format!("{} returned nothing", agent.name))
        );
    }
    Ok(answer.to_owned())
}

/// Capture a prefix and discard the rest, matching the file runner's bounded
/// reads without allowing the temporary output files to grow on disk.
struct Capture<R> {
    reader: R,
    bytes: Vec<u8>,
    max: usize,
}

impl<R: Read + std::os::fd::AsRawFd> Capture<R> {
    fn new(reader: R, max: usize) -> Result<Self, String> {
        set_nonblocking(&reader)?;
        Ok(Self {
            reader,
            bytes: Vec::new(),
            max,
        })
    }

    fn drain(&mut self) -> Result<(), String> {
        let mut buffer = [0; 8192];
        for _ in 0..8 {
            match self.reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    let keep = n.min(self.max - self.bytes.len());
                    self.bytes.extend_from_slice(&buffer[..keep]);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.to_string()),
            }
        }
        Ok(())
    }
}

fn set_nonblocking(stream: &impl std::os::fd::AsRawFd) -> Result<(), String> {
    let fd = stream.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(())
}

fn answer_pipe(dir: &Path) -> Result<Capture<std::fs::File>, String> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;

    let answer_path = dir.join("answer.txt");
    let path =
        std::ffi::CString::new(answer_path.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
    // Codex's -o uses File::create/write_all. An existing FIFO keeps that
    // final-answer channel separate from the progress printed on stdout.
    if unsafe { libc::mkfifo(path.as_ptr(), 0o600) } != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    // Holding both ends avoids EOF before Codex opens -o, and avoids blocking
    // on open if it exits without writing an answer. The descriptor is CLOEXEC.
    let answer_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(answer_path)
        .map_err(|e| e.to_string())?;
    Capture::new(answer_file, MAX_ANSWER_BYTES)
}
