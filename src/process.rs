use anyhow::{Context, Result};
use std::io::{Read, Write};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::process::CommandExt;

/// A statusLine command must never consume the refresh interval itself.
pub const STATUSLINE_COMMAND_BUDGET: Duration = Duration::from_secs(2);

#[derive(Debug, PartialEq, Eq)]
pub struct CommandOutput {
    pub stdout: Vec<u8>,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
}

/// Run a user-owned shell command with a hard wall-clock budget.
///
/// The child owns a process group so a timeout removes the shell and any
/// helper it started. stdout is drained concurrently, while the caller keeps
/// ownership of the child and can therefore kill and reap it without a pid
/// reuse race.
pub fn run_shell_with_deadline(
    command: &str,
    input: &[u8],
    budget: Duration,
) -> Result<CommandOutput> {
    let mut child = Command::new("sh");
    child
        .args(["-c", command])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    own_process_group(&mut child);

    let mut child = child.spawn().context("run previous statusLine")?;
    let stdin = child
        .stdin
        .take()
        .context("open previous statusLine stdin")?;
    let stdout = child
        .stdout
        .take()
        .context("open previous statusLine stdout")?;
    let input = input.to_vec();
    let writer = thread::spawn(move || {
        let mut stdin = stdin;
        let _ = stdin.write_all(&input);
    });

    let output = collect_with_deadline(child, stdout, budget);
    let _ = writer.join();
    output.context("read previous statusLine output")
}

/// Run a program directly (no shell) with a hard wall-clock budget.
///
/// stdin is `/dev/null`, so a CLI that would otherwise wait for a login or a
/// prompt sees end-of-file at once. stdout and stderr share one pipe because a
/// CLI may report "not signed in" on either. The environment is exactly what
/// the caller set on `command`; nothing is added here.
pub fn run_program_with_deadline(mut command: Command, budget: Duration) -> Result<CommandOutput> {
    let (reader, writer) = std::io::pipe().context("create output pipe")?;
    let error_writer = writer.try_clone().context("share output pipe")?;
    command
        .stdin(Stdio::null())
        .stdout(writer)
        .stderr(error_writer);
    own_process_group(&mut command);
    let child = command.spawn().context("run program")?;
    // `Command` still holds the parent's copies of the pipe's write end. They
    // must close now, or reading would wait for this process instead of the
    // child and never see end-of-file.
    drop(command);
    collect_with_deadline(child, reader, budget).context("read program output")
}

/// Put the child in a process group of its own, so a timeout can remove every
/// helper it started along with it.
fn own_process_group(command: &mut Command) {
    #[cfg(unix)]
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    #[cfg(not(unix))]
    let _ = command;
}

/// Drain `stdout` while a watchdog kills the child's process group once
/// `budget` elapses, then reap the child.
fn collect_with_deadline(
    child: Child,
    mut stdout: impl Read,
    budget: Duration,
) -> Result<CommandOutput> {
    let child = Arc::new(Mutex::new(Some(child)));
    let timed_out = Arc::new(AtomicBool::new(false));
    let (cancel, cancelled) = mpsc::channel();
    let watchdog_child = Arc::clone(&child);
    let watchdog_timed_out = Arc::clone(&timed_out);
    let watchdog = thread::spawn(move || {
        if cancelled.recv_timeout(budget).is_err() {
            watchdog_timed_out.store(true, Ordering::Release);
            let _ = terminate_and_reap(&watchdog_child);
        }
    });

    let mut output = Vec::new();
    let read_result = stdout.read_to_end(&mut output);
    let _ = cancel.send(());
    let status = terminate_and_reap(&child)?;
    let _ = watchdog.join();
    read_result?;

    Ok(CommandOutput {
        stdout: output,
        exit_code: status.and_then(|status| status.code()),
        timed_out: timed_out.load(Ordering::Acquire),
    })
}

fn terminate_and_reap(child: &Mutex<Option<Child>>) -> Result<Option<ExitStatus>> {
    let mut slot = child
        .lock()
        .map_err(|_| anyhow::anyhow!("lock child process"))?;
    let Some(mut child) = slot.take() else {
        return Ok(None);
    };
    #[cfg(unix)]
    unsafe {
        // setpgid in the child setup makes this include shell descendants.
        let _ = libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
    }
    let _ = child.kill();
    child.wait().map(Some).context("reap child process")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn captures_a_completed_command() {
        let result =
            run_shell_with_deadline("printf done", b"ignored", Duration::from_secs(1)).unwrap();
        assert_eq!(result.stdout, b"done");
        assert_eq!(result.exit_code, Some(0));
        assert!(!result.timed_out);
    }

    #[test]
    fn a_program_reports_stdout_and_stderr_in_one_stream_without_a_shell() {
        let mut command = Command::new("sh");
        command.args(["-c", "printf out; printf err >&2; exit 3"]);
        let result = run_program_with_deadline(command, Duration::from_secs(5)).unwrap();
        assert_eq!(result.stdout, b"outerr");
        assert_eq!(result.exit_code, Some(3));
        assert!(!result.timed_out);
    }

    #[test]
    fn a_program_reads_end_of_file_on_stdin_instead_of_waiting() {
        let mut command = Command::new("/bin/cat");
        command.env_clear();
        let started = Instant::now();
        let result = run_program_with_deadline(command, Duration::from_secs(5)).unwrap();
        assert!(!result.timed_out);
        assert!(result.stdout.is_empty());
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn a_program_that_exceeds_its_budget_is_killed() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 20 & wait"]);
        let started = Instant::now();
        let result = run_program_with_deadline(command, Duration::from_millis(100)).unwrap();
        assert!(result.timed_out);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn kills_a_command_that_exceeds_its_budget() {
        let started = Instant::now();
        let result =
            run_shell_with_deadline("sleep 20 & wait", b"", Duration::from_millis(100)).unwrap();
        assert!(result.timed_out);
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
