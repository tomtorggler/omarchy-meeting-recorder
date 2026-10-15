use super::*;

struct FakeAgent {
    dir: PathBuf,
    built: Built,
    agent: Agent,
}

impl FakeAgent {
    fn new(id: &str, mode: &str) -> Self {
        use std::os::unix::fs::PermissionsExt;
        // Avoid global PATH/HOME changes: only this child's environment
        // and executable are replaced. All state is synthetic and private.
        let dir = std::env::temp_dir().join(format!(
            "meeting-recorder-fake-codex-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        let mut built = build(id, "synthetic prompt", &dir).unwrap();
        let program = dir.join(id);
        if id == "claude" {
            std::fs::write(
                &program,
                r#"#!/bin/sh
set -e
# Baseline failures must return EFBIG, not produce a core dump.
trap '' XFSZ
case "$FAKE_AGENT_MODE" in
  hang) exec sleep 30 ;;
  descendants) sleep 30 & echo $! > descendant.pid; printf done; exit 0 ;;
esac
cat > prompt.txt
case "$FAKE_AGENT_MODE" in
  persistent) printf appended >> "$FAKE_AGENT_STATE"; printf 'persistent write succeeded' ;;
  flood) head -c 3145728 /dev/zero | tr '\000' x; head -c 3145728 /dev/zero | tr '\000' e >&2 ;;
  failure) echo 'synthetic Claude failure' >&2; exit 17 ;;
  empty) ;;
  *) exit 19 ;;
esac
"#,
            )
            .unwrap();
        } else {
            std::fs::write(
                &program,
                "#!/bin/sh\nulimit -c 0\nwhile [ $# -gt 0 ]; do\n if [ \"$1\" = -o ]; then shift; export FAKE_AGENT_ANSWER=$1; fi\n shift\ndone\nexec \"$FAKE_AGENT_TEST_EXE\" --exact agent::piped::tests::fake_codex_child --ignored --nocapture\n",
            ).unwrap();
        }
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        built.program = program.into();
        built.env.push((
            "FAKE_AGENT_TEST_EXE",
            std::env::current_exe().unwrap().into(),
        ));
        built.env.push(("FAKE_AGENT_MODE", mode.into()));
        built
            .env
            .push(("FAKE_AGENT_STATE", dir.join("persistent.bin").into()));
        Self {
            dir,
            built,
            agent: Agent {
                id: id.into(),
                name: label(id),
            },
        }
    }

    fn run(&mut self, prompt: &str, timeout: Duration) -> Result<String, String> {
        let built = std::mem::replace(
            &mut self.built,
            build(&self.agent.id, "", &self.dir).unwrap(),
        );
        run(
            &self.agent,
            prompt,
            &self.dir,
            built,
            timeout,
            Duration::from_millis(50),
        )
    }

    fn assert_descendant_stopped(&self) {
        let pid = std::fs::read_to_string(self.dir.join("descendant.pid")).unwrap();
        // SIGKILL is delivered asynchronously: the runner has returned once
        // it is queued, and a busy machine may run the target a little
        // later. The descendant never exits on its own before 10 seconds.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid.trim()));
            if stat.is_err() || stat.unwrap().split_whitespace().nth(2) == Some("Z") {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "descendant {} survived",
                pid.trim()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for FakeAgent {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A real child process of the runner, using the same fs::write as Codex
/// 0.159.0's last-message writer, without a model or the user's Codex home.
#[test]
#[ignore = "subprocess fixture; invoked only by the runner tests"]
fn fake_codex_child() {
    use std::os::unix::fs::FileExt;
    let Ok(mode) = std::env::var("FAKE_AGENT_MODE") else {
        return;
    };
    // A baseline RLIMIT_FSIZE failure should return EFBIG, never request
    // a core dump (piped system core handlers can ignore ulimit -c).
    unsafe {
        libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
    }
    let answer = PathBuf::from(std::env::var_os("FAKE_AGENT_ANSWER").unwrap());
    let state = PathBuf::from(std::env::var_os("FAKE_AGENT_STATE").unwrap());
    if mode == "escaped-reader" {
        assert!(unsafe { libc::setsid() } > 0);
        // Keep every channel open, including unread stdin, outside the
        // runner's owned session. Only this test fixture will clean it up.
        let _output = std::fs::OpenOptions::new()
            .write(true)
            .open(&answer)
            .unwrap();
        std::fs::write(
            state.with_file_name("descendant.ready"),
            std::fs::read_to_string("/proc/self/stat").unwrap(),
        )
        .unwrap();
        std::thread::sleep(Duration::from_secs(5));
        std::process::exit(0);
    }
    if mode == "descendant" {
        // Retain stdin/stdout/stderr and the final-answer FIFO. Ignore
        // TERM so the timeout's KILL and normal-exit cleanup are exercised.
        unsafe {
            libc::signal(libc::SIGTERM, libc::SIG_IGN);
        }
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .open(&answer)
            .unwrap();
        std::fs::write(state.with_file_name("descendant.ready"), "ready").unwrap();
        // Keep going when the runner closes its ends (EPIPE), so only its
        // KILL ends this process early; a runner that forgot it leaks
        // nothing past the deadline.
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(10) {
            let failed = output.write_all(&[b'n'; 8192]).is_err()
                | std::io::stdout().write_all(&[b'o'; 8192]).is_err()
                | std::io::stderr().write_all(&[b'e'; 8192]).is_err();
            if failed {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        std::process::exit(0);
    }
    if matches!(
        mode.as_str(),
        "descendants" | "early" | "timeout" | "escaped-exit" | "escaped-timeout"
    ) {
        if matches!(mode.as_str(), "descendants" | "escaped-exit") {
            std::fs::write(&answer, "done").unwrap();
        }
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "agent::piped::tests::fake_codex_child",
                "--ignored",
                "--nocapture",
            ])
            .env(
                "FAKE_AGENT_MODE",
                if mode.starts_with("escaped-") {
                    "escaped-reader"
                } else {
                    "descendant"
                },
            )
            .stdin(Stdio::inherit())
            .spawn()
            .unwrap();
        std::fs::write(
            state.with_file_name("descendant.pid"),
            child.id().to_string(),
        )
        .unwrap();
        while !state.with_file_name("descendant.ready").exists() {
            std::thread::sleep(Duration::from_millis(1));
        }
        if matches!(mode.as_str(), "timeout" | "escaped-timeout") {
            unsafe {
                libc::signal(libc::SIGTERM, libc::SIG_IGN);
            }
            std::thread::sleep(Duration::from_secs(60));
        }
        std::process::exit(0);
    }
    let mut prompt = String::new();
    std::io::stdin().read_to_string(&mut prompt).unwrap();
    assert_eq!(prompt, "synthetic prompt");
    match mode.as_str() {
        "persistent" => {
            let file = std::fs::OpenOptions::new()
                .write(true)
                .open(&state)
                .unwrap();
            // Real writes beyond the old 1 MiB ceiling, including the
            // offset/size seen in the failing SQLite pwrite64 syscall.
            if let Err(e) = file.write_all_at(&vec![b'p'; 2 * 1024 * 1024], 0) {
                eprintln!("persistent write failed: {e}");
                std::process::exit(17);
            }
            file.write_all_at(&[b'q'; 4096], 3_715_072).unwrap();
            std::fs::write(&answer, "persistent write succeeded").unwrap();
        }
        "flood" => {
            std::io::stdout()
                .write_all(&vec![b'o'; 3 * 1024 * 1024])
                .unwrap();
            std::io::stderr()
                .write_all(&vec![b'e'; 3 * 1024 * 1024])
                .unwrap();
            std::fs::write(&answer, "€".repeat(MAX_ANSWER_BYTES)).unwrap();
        }
        "failure" => {
            eprintln!("synthetic failure");
            std::process::exit(17);
        }
        "empty" => {}
        _ => panic!("unknown fixture mode"),
    }
}

#[test]
fn codex_persistent_writes_can_exceed_the_output_file_limit() {
    let mut fake = FakeAgent::new("codex", "persistent");
    std::fs::write(fake.dir.join("persistent.bin"), vec![0; 2 * 1024 * 1024]).unwrap();
    assert_eq!(
        fake.run("synthetic prompt", Duration::from_secs(10))
            .unwrap(),
        "persistent write succeeded"
    );
    assert_eq!(
        std::fs::metadata(fake.dir.join("persistent.bin"))
            .unwrap()
            .len(),
        3_719_168
    );
}

#[test]
fn codex_capture_bounds_all_streams_and_preserves_utf8() {
    use std::os::unix::fs::FileTypeExt;
    let mut fake = FakeAgent::new("codex", "flood");
    let answer = fake
        .run("synthetic prompt", Duration::from_secs(10))
        .unwrap();
    assert_eq!(answer.len(), MAX_ANSWER_BYTES - MAX_ANSWER_BYTES % 3);
    assert!(answer.chars().all(|c| c == '€'));
    assert!(
        std::fs::metadata(fake.dir.join("answer.txt"))
            .unwrap()
            .file_type()
            .is_fifo()
    );
    assert!(!fake.dir.join("stdout.txt").exists());
    assert!(!fake.dir.join("stderr.txt").exists());
    // The same capture used for stdout/stderr keeps only its prefix.
    use std::os::fd::FromRawFd;
    let mut fds = [0; 2];
    assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
    let receiver = unsafe { std::fs::File::from_raw_fd(fds[0]) };
    let mut sender = unsafe { std::fs::File::from_raw_fd(fds[1]) };
    let mut capture = Capture::new(receiver, 3).unwrap();
    sender.write_all(b"abcdef").unwrap();
    capture.drain().unwrap();
    assert_eq!(capture.bytes, b"abc");
}

#[test]
fn codex_exit_without_an_answer_or_with_failure_does_not_wait_for_fifo() {
    for (mode, message) in [
        ("empty", "Codex returned nothing"),
        ("failure", "synthetic failure"),
    ] {
        let mut fake = FakeAgent::new("codex", mode);
        let started = Instant::now();
        assert_eq!(
            fake.run("synthetic prompt", Duration::from_secs(10))
                .unwrap_err(),
            message
        );
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}

#[test]
fn codex_exit_and_timeout_clean_up_descendants_retaining_stdio() {
    for mode in ["descendants", "early", "timeout"] {
        let mut fake = FakeAgent::new("codex", mode);
        let started = Instant::now();
        // Exceeds a pipe's capacity, and neither the parent nor its
        // descendant reads stdin. Sending the prompt must not block.
        let result = fake.run(&"x".repeat(MAX_REQUEST_BYTES), Duration::from_millis(500));
        if mode == "timeout" {
            assert!(result.unwrap_err().contains("did not answer within"));
        } else {
            assert!(
                result
                    .unwrap()
                    .starts_with(if mode == "descendants" { "done" } else { "n" })
            );
        }
        assert!(started.elapsed() < Duration::from_secs(3));
        fake.assert_descendant_stopped();
    }
}

#[test]
fn codex_escaped_stdin_reader_cannot_extend_exit_or_timeout() {
    for mode in ["escaped-exit", "escaped-timeout"] {
        let mut fake = FakeAgent::new("codex", mode);
        let started = Instant::now();
        let result = fake.run(&"x".repeat(MAX_REQUEST_BYTES), Duration::from_millis(500));
        let elapsed = started.elapsed();
        let pid: libc::pid_t = std::fs::read_to_string(fake.dir.join("descendant.pid"))
            .unwrap()
            .parse()
            .unwrap();
        // The runner must return without killing the escaped session.
        // Clean up our synthetic sleeper before making assertions, even
        // when running this regression against the earlier implementation.
        // Use a stable handle for test-only escaped-session cleanup too.
        // Verify its start time so the failing five-second baseline never
        // targets a recycled PID after its synthetic sleeper has exited.
        use std::os::fd::{AsRawFd, FromRawFd};
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
        let current = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        let original = std::fs::read_to_string(fake.dir.join("descendant.ready")).unwrap();
        let matches = current.split_whitespace().nth(21) == original.split_whitespace().nth(21);
        let still_alive = matches && current.split_whitespace().nth(2).is_some_and(|s| s != "Z");
        if fd >= 0 {
            let handle = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
            if matches {
                unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        handle.as_raw_fd(),
                        libc::SIGKILL,
                        std::ptr::null::<libc::siginfo_t>(),
                        0,
                    );
                }
            }
        }
        assert!(elapsed < Duration::from_secs(2), "{mode} took {elapsed:?}");
        assert!(still_alive);
        if mode == "escaped-exit" {
            assert_eq!(result.unwrap(), "done");
        } else {
            assert!(result.unwrap_err().contains("did not answer within"));
        }
    }
}

#[test]
fn codex_group_observation_keeps_leader_reserved_until_cleanup() {
    let child = Command::new("setsid")
        .args(["sh", "-c", "exit 17"])
        .spawn()
        .unwrap();
    let mut group = crate::process::ChildGroup {
        child,
        stopped: false,
    };
    let pid = group.child.id();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !group.exited().unwrap() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    // Observation is repeatable and the zombie still reserves its PID.
    assert!(group.exited().unwrap());
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    assert_eq!(stat.split_whitespace().nth(2), Some("Z"));
    assert_eq!(group.stop().unwrap().code(), Some(17));
    assert!(group.stopped);
    assert!(!Path::new(&format!("/proc/{pid}")).exists());
    // A repeated cleanup uses Child's cached status, never signals again.
    assert_eq!(group.stop().unwrap().code(), Some(17));
}

#[test]
fn claude_can_append_to_existing_state_above_the_old_file_limit() {
    let mut fake = FakeAgent::new("claude", "persistent");
    let state = fake.dir.join("persistent.bin");
    std::fs::write(&state, vec![b'p'; 4 * 1024 * 1024]).unwrap();
    // Reproduce the old wrapper with the same synthetic executable and limit.
    let old = Command::new("sh")
        .args(["-c", r#"ulimit -f "$1" && shift && exec "$@""#, "sh"])
        .arg(FILE_LIMIT_BLOCKS.to_string())
        .arg(&fake.built.program)
        .args(&fake.built.args)
        .envs(fake.built.env.iter().map(|(k, v)| (k, v)))
        .current_dir(&fake.dir)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        !old.status.success(),
        "the old file limit must reject this write"
    );
    assert_eq!(std::fs::metadata(&state).unwrap().len(), 4 * 1024 * 1024);
    assert_eq!(fake.built.capture, CaptureMode::Stdout);
    assert_eq!(
        fake.run("synthetic prompt", Duration::from_secs(10))
            .unwrap(),
        "persistent write succeeded"
    );
    assert_eq!(
        std::fs::metadata(&state).unwrap().len(),
        4 * 1024 * 1024 + 8
    );
    assert_eq!(
        std::fs::read_to_string(fake.dir.join("prompt.txt")).unwrap(),
        "synthetic prompt"
    );
    for name in ["stdout.txt", "stderr.txt", "answer.txt"] {
        assert!(!fake.dir.join(name).exists());
    }
}

#[test]
fn claude_output_is_bounded_without_a_process_file_limit() {
    let mut fake = FakeAgent::new("claude", "flood");
    let answer = fake
        .run("synthetic prompt", Duration::from_secs(10))
        .unwrap();
    assert_eq!(answer.len(), MAX_ANSWER_BYTES);
    assert!(answer.chars().all(|c| c == 'x'));
}

#[test]
fn claude_failure_and_empty_output_are_not_successful_answers() {
    for (mode, expected) in [
        ("failure", "synthetic Claude failure"),
        ("empty", "Claude Code returned nothing"),
    ] {
        let mut fake = FakeAgent::new("claude", mode);
        assert_eq!(
            fake.run("synthetic prompt", Duration::from_secs(10))
                .unwrap_err(),
            expected
        );
    }
}

#[test]
fn claude_deadlines_and_descendant_cleanup_still_hold() {
    let mut fake = FakeAgent::new("claude", "hang");
    let start = Instant::now();
    assert!(
        fake.run(&"x".repeat(MAX_REQUEST_BYTES), Duration::from_millis(100))
            .unwrap_err()
            .contains("did not answer within")
    );
    assert!(start.elapsed() < Duration::from_secs(3));
    let mut fake = FakeAgent::new("claude", "descendants");
    let start = Instant::now();
    assert_eq!(
        fake.run(&"x".repeat(MAX_REQUEST_BYTES), Duration::from_secs(10))
            .unwrap(),
        "done"
    );
    assert!(start.elapsed() < Duration::from_secs(3));
    fake.assert_descendant_stopped();
}
