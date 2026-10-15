//! Own a child process group until its leader has been signalled and reaped.

pub(crate) struct ChildGroup {
    pub(crate) child: std::process::Child,
    pub(crate) stopped: bool,
}

impl ChildGroup {
    pub(crate) fn exited(&mut self) -> Result<bool, String> {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // Observe without reaping: the leader's PID remains reserved until
        // stop() has signalled its group, so a reused PGID cannot be targeted.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                self.child.id(),
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result == -1 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                return Ok(false);
            }
            if error.raw_os_error() == Some(libc::ECHILD) {
                // If something else reaped it, numeric signalling is unsafe.
                self.stopped = true;
            }
            return Err(error.to_string());
        }
        Ok(unsafe { info.si_pid() } != 0)
    }

    pub(crate) fn stop(&mut self) -> std::io::Result<std::process::ExitStatus> {
        if !self.stopped {
            let pid = self.child.id() as libc::pid_t;
            // Never call try_wait/kill (which can internally reap) before
            // signalling the group. The direct kill covers a failed setsid.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
                libc::kill(pid, libc::SIGKILL);
            }
            // Disarm before the reaping wait, including its error paths.
            self.stopped = true;
        }
        self.child.wait()
    }
}

impl Drop for ChildGroup {
    fn drop(&mut self) {
        if !self.stopped {
            let _ = self.stop();
        }
    }
}

/// Run a command with bounded output and a cancellable deadline. Nonblocking
/// pipes keep descendants or noisy commands from trapping the supervisor.
pub(crate) fn output(
    command: &mut std::process::Command,
    cancel: &std::sync::atomic::AtomicBool,
    timeout: std::time::Duration,
) -> Result<std::process::Output, String> {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    use std::sync::atomic::Ordering;
    if cancel.load(Ordering::Relaxed) {
        return Err("Cancelled".into());
    }
    command
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut group = ChildGroup {
        child: command.spawn().map_err(|e| e.to_string())?,
        stopped: false,
    };
    let mut stdout = Tail::new(group.child.stdout.take().unwrap())?;
    let mut stderr = Tail::new(group.child.stderr.take().unwrap())?;
    let started = std::time::Instant::now();
    loop {
        stdout.drain()?;
        stderr.drain()?;
        if group.exited()? {
            break;
        }
        if cancel.load(Ordering::Relaxed) {
            return Err("Cancelled".into());
        }
        if started.elapsed() >= timeout {
            return Err(format!("Timed out after {} seconds", timeout.as_secs()));
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let status = group.stop().map_err(|e| e.to_string())?;
    stdout.drain()?;
    stderr.drain()?;
    Ok(std::process::Output {
        status,
        stdout: stdout.bytes,
        stderr: stderr.bytes,
    })
}

struct Tail<T> {
    stream: T,
    bytes: Vec<u8>,
}

impl<T: std::io::Read + std::os::fd::AsRawFd> Tail<T> {
    fn new(stream: T) -> Result<Self, String> {
        let fd = stream.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(Self {
            stream,
            bytes: Vec::new(),
        })
    }

    fn drain(&mut self) -> Result<(), String> {
        let mut chunk = [0; 8192];
        // Limit both memory and work per tick so cancellation stays responsive.
        for _ in 0..32 {
            match self.stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    self.bytes.extend_from_slice(&chunk[..n]);
                    if self.bytes.len() > 32768 {
                        self.bytes.drain(..self.bytes.len() - 32768);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.to_string()),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        process::Command,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::{Duration, Instant},
    };

    #[test]
    fn noisy_output_is_bounded_and_descendants_cannot_hold_pipes_open() {
        let started = Instant::now();
        let result = output(
            Command::new("sh").args(["-c", "head -c 100000 /dev/zero; echo last; sleep 30 &"]),
            &AtomicBool::new(false),
            Duration::from_secs(3),
        )
        .unwrap();
        assert!(result.status.success());
        assert_eq!(result.stdout.len(), 32768);
        assert!(result.stdout.ends_with(b"last\n"));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn cancel_and_timeout_stop_a_hung_command() {
        let cancel = Arc::new(AtomicBool::new(false));
        let worker = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            worker.store(true, Ordering::Relaxed);
        });
        assert_eq!(
            output(
                Command::new("sh").args(["-c", "sleep 30"]),
                &cancel,
                Duration::from_secs(3)
            )
            .unwrap_err(),
            "Cancelled"
        );
        assert!(
            output(
                Command::new("sh").args(["-c", "sleep 30"]),
                &AtomicBool::new(false),
                Duration::from_millis(50)
            )
            .unwrap_err()
            .contains("Timed out")
        );
    }
}
