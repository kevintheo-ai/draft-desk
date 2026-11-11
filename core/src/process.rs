//! Running a configured program: clean environment, input on stdin, capped output, a hard time limit.

use crate::{Error, Result};
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Cap for stdout and stderr each.
pub const MAX_OUTPUT: usize = 1024 * 1024;
/// How long the pipes may stay open after the program itself ended.
const PIPE_GRACE: Duration = Duration::from_secs(2);
/// How long the pipes may stay open after the rest of the group was killed.
const KILL_GRACE: Duration = Duration::from_secs(1);

#[derive(Debug)]
pub struct Output {
    /// `None`: ended by a signal, killed at the time limit, or the wait for it failed.
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
    /// stdout or stderr was longer than [`MAX_OUTPUT`].
    pub overflow: bool,
}

/// Runs `argv` with only `PATH`, `HOME`, `LANG` from this process plus `extra_env`. stdin is written from its own
/// thread (no deadlock with a large input), output is read up to the cap and the rest drained. At the time limit
/// the whole process group is killed. `Err` only when the program did not start.
pub fn run(
    argv: &[String],
    extra_env: &[(&str, String)],
    stdin: Vec<u8>,
    timeout: Duration,
) -> Result<Output> {
    run_with(argv, extra_env, stdin, timeout, Child::try_wait)
}

/// [`run`] with the wait step passed in: a test makes it fail after the start.
fn run_with(
    argv: &[String],
    extra_env: &[(&str, String)],
    stdin: Vec<u8>,
    timeout: Duration,
    try_wait: fn(&mut Child) -> std::io::Result<Option<ExitStatus>>,
) -> Result<Output> {
    let Some((program, args)) = argv.split_first() else {
        return Err(Error("empty command".into()));
    };
    let mut cmd = Command::new(program);
    cmd.args(args).env_clear();
    for key in ["PATH", "HOME", "LANG"] {
        if let Ok(v) = std::env::var(key) {
            cmd.env(key, v);
        }
    }
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|e| Error(format!("cannot start {program:?}: {e}")))?;
    let mut input = child.stdin.take().expect("piped stdin");
    let writer = thread::spawn(move || {
        // A program that does not read its input is fine; a broken pipe only ends the writing.
        let _ = input.write_all(&stdin);
    });
    let capped = |mut r: Box<dyn Read + Send>| {
        thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = r.by_ref().take(MAX_OUTPUT as u64 + 1).read_to_end(&mut buf);
            let _ = std::io::copy(&mut r, &mut std::io::sink());
            buf
        })
    };
    let out = capped(Box::new(child.stdout.take().expect("piped stdout")));
    let err = capped(Box::new(child.stderr.take().expect("piped stderr")));
    let pgid = child.id() as i32;
    // SAFETY: plain kill(2) on the group the child leads (created with process_group(0)).
    let kill_group = || unsafe {
        libc::kill(-pgid, libc::SIGKILL);
    };
    let start = Instant::now();
    // ponytail: polling every 50 ms instead of signal handling; fine for commands that run for seconds.
    let (status, timed_out) = loop {
        match try_wait(&mut child) {
            Ok(Some(s)) => break (Some(s), false),
            Ok(None) => {}
            // The program started and may have done its work: never an `Err` from here on, which reads as "never
            // started". It ends like a run without an exit status.
            Err(_) => {
                kill_group();
                let _ = child.wait();
                break (None, false);
            }
        }
        if start.elapsed() >= timeout {
            kill_group();
            let _ = child.wait();
            break (None, true);
        }
        thread::sleep(Duration::from_millis(50));
    };
    // The program ended, but a process it left behind may still hold a pipe: give the pipes a short grace, then
    // kill the rest of the group (it still exists while a member holds a pipe, so its id cannot be reused). One that
    // left the group (setsid) survives the kill: after a second grace its pipes are given up.
    let grace = Instant::now();
    let mut killed = false;
    while !(writer.is_finished() && out.is_finished() && err.is_finished())
        && grace.elapsed() < PIPE_GRACE + KILL_GRACE
    {
        if !killed && grace.elapsed() >= PIPE_GRACE {
            kill_group();
            killed = true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    // ponytail: a reader still waiting stays behind, blocked until the last holder of its pipe ends; that output is
    // lost (empty), so a send command without its confirmation line counts as unknown. A reader that shares its
    // buffer could hand over a part.
    let collect = |h: thread::JoinHandle<Vec<u8>>| {
        if h.is_finished() {
            h.join().unwrap_or_default()
        } else {
            Vec::new()
        }
    };
    let (mut stdout, mut stderr) = (collect(out), collect(err));
    let overflow = stdout.len() > MAX_OUTPUT || stderr.len() > MAX_OUTPUT;
    stdout.truncate(MAX_OUTPUT);
    stderr.truncate(MAX_OUTPUT);
    Ok(Output {
        code: status.and_then(|s| s.code()),
        stdout,
        stderr,
        timed_out,
        overflow,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Vec<String> {
        vec!["/bin/sh".into(), "-c".into(), script.into()]
    }

    #[test]
    fn exit_code_and_output() {
        let o = run(
            &sh("cat; echo err >&2; exit 3"),
            &[],
            b"in".to_vec(),
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(
            (o.code, o.stdout, o.stderr, o.timed_out),
            (Some(3), b"in".to_vec(), b"err\n".to_vec(), false)
        );
    }

    #[test]
    fn environment_is_an_allowlist() {
        // cargo sets CARGO_PKG_NAME for test runs; the child must not see it.
        assert!(std::env::var("CARGO_PKG_NAME").is_ok());
        let o = run(
            &sh("env"),
            &[("X_EXTRA", "1".into())],
            vec![],
            Duration::from_secs(5),
        )
        .unwrap();
        let env = String::from_utf8(o.stdout).unwrap();
        assert!(
            !env.contains("CARGO_PKG_NAME") && env.contains("X_EXTRA=1"),
            "{env}"
        );
    }

    #[test]
    fn timeout_kills_the_group() {
        let t = Instant::now();
        let o = run(
            &sh("sleep 30 & sleep 30"),
            &[],
            vec![],
            Duration::from_millis(300),
        )
        .unwrap();
        assert!(o.timed_out && o.code.is_none());
        assert!(t.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn leftover_child_holding_the_pipe_does_not_hang() {
        let t = Instant::now();
        let o = run(
            &sh("sleep 30 & echo done"),
            &[],
            vec![],
            Duration::from_secs(60),
        )
        .unwrap();
        assert_eq!(
            (o.code, o.stdout.as_slice(), o.timed_out),
            (Some(0), b"done\n".as_slice(), false)
        );
        assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    }

    #[test]
    fn output_is_capped() {
        let o = run(
            &sh("head -c 3000000 /dev/zero"),
            &[],
            vec![],
            Duration::from_secs(10),
        )
        .unwrap();
        assert!(o.overflow);
        assert_eq!(o.stdout.len(), MAX_OUTPUT);
        assert_eq!(o.code, Some(0));
    }

    #[test]
    fn missing_program_is_an_error() {
        assert!(
            run(
                &["/nonexistent/x".into()],
                &[],
                vec![],
                Duration::from_secs(1)
            )
            .is_err()
        );
        assert!(run(&[], &[], vec![], Duration::from_secs(1)).is_err());
    }

    #[test]
    fn an_error_while_waiting_is_no_start_error() {
        // The program runs and may have done its work: an `Err` would read as "never started" (a send is retried).
        let t = Instant::now();
        let o = run_with(
            &sh("sleep 30"),
            &[],
            vec![],
            Duration::from_secs(60),
            |_| Err(std::io::Error::from_raw_os_error(libc::ECHILD)),
        )
        .unwrap();
        assert!(o.code.is_none() && !o.timed_out, "{o:?}");
        assert!(t.elapsed() < Duration::from_secs(5), "the group was killed");
    }

    #[test]
    fn a_process_that_left_the_group_cannot_hold_the_run() {
        let dir = std::env::temp_dir().join(format!(
            "dd-proc-{}-{}",
            std::process::id(),
            crate::random_hex(4)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let pidfile = dir.join("pid");
        // perl starts its own session (no setsid program on the Mac) and keeps stdout and stderr open.
        let script = format!(
            "perl -e 'use POSIX; setsid(); sleep 20' & echo $! > '{}'; echo done",
            pidfile.display()
        );
        let t = Instant::now();
        let o = run(&sh(&script), &[], vec![], Duration::from_secs(60)).unwrap();
        let elapsed = t.elapsed();
        let pid: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // SAFETY: kill(2) on the process this test started.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(elapsed < Duration::from_secs(10), "{elapsed:?}");
        assert_eq!(o.code, Some(0));
        assert!(
            o.stdout.is_empty(),
            "the output still held open is given up"
        );
    }
}
