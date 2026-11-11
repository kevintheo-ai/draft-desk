//! The command line against a temporary root and a file instead of the keychain.

use serde_json::Value;
use std::path::PathBuf;
use std::process::{Command, Output};

struct Env {
    root: PathBuf,
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn env(name: &str) -> Env {
    let root = std::env::temp_dir().join(format!("dd-cli-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("home")).unwrap();
    std::fs::write(
        root.join("home/config.json"),
        r#"{"accounts":[{"name":"work","kind":"command","senders":[{"address":"me@example.org","name":"Me"}],
           "send_command":["/bin/cat"]}]}"#,
    )
    .unwrap();
    // A signing key, as the app would have created it.
    std::fs::write(
        root.join("secrets.json"),
        format!(r#"{{"result-signing-key":"{}"}}"#, "ab".repeat(32)),
    )
    .unwrap();
    Env { root }
}

impl Env {
    fn run(&self, args: &[&str]) -> Output {
        self.run_stdin(args, "")
    }

    /// The command line on this root, with stdout and stderr captured.
    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_draftdesk"));
        c.args(args)
            .env("DRAFTDESK_HOME", self.root.join("home"))
            .env(
                "DRAFTDESK_TEST_SECRETS_FILE",
                self.root.join("secrets.json"),
            )
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        c
    }

    fn run_stdin(&self, args: &[&str], stdin: &str) -> Output {
        use std::io::Write;
        let mut child = self
            .command(args)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn draft(&self, extra: &str) -> PathBuf {
        let p = self.root.join(format!("draft-{}.json", extra.len()));
        std::fs::write(
            &p,
            format!(
                r#"{{"schema":"draftdesk/1","account":"work","from":{{"address":"me@example.org"}},
                   "to":["Ann <ann@example.org>"],"subject":"Re: Offer","body":"Hello Ann\n",
                   "origin":{{"agent":"test","session":"s1"}}{extra}}}"#
            ),
        )
        .unwrap();
        p
    }

    fn add(&self) -> String {
        let out = self.run(&["add", self.draft("").to_str().unwrap()]);
        assert_eq!(
            out.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        json(&out)["item_id"].as_str().unwrap().to_string()
    }

    fn queue(&self) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(self.root.join("home/state/queue"))
            .map(|d| {
                d.map(|e| e.unwrap().file_name().into_string().unwrap())
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v
    }

    fn set_state(&self, id: &str, edit: impl FnOnce(&mut Value)) {
        let p = self
            .root
            .join("home/state/queue")
            .join(id)
            .join("item.json");
        let mut v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        edit(&mut v);
        std::fs::write(&p, v.to_string()).unwrap();
    }
}

fn json(out: &Output) -> Value {
    let text = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(text.lines().next().unwrap_or("null")).unwrap()
}

fn lines(out: &Output) -> Vec<Value> {
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn add_then_status_then_list() {
    let e = env("a");
    let id = e.add();
    let status = e.run(&["status", &id]);
    assert_eq!(status.status.code(), Some(0));
    let v = json(&status);
    assert_eq!(
        (v["state"].as_str(), v["result"].is_null()),
        (Some("ready"), true)
    );
    let list = e.run(&["list", "--json"]);
    let items = json(&list);
    assert_eq!(items[0]["item_id"], id.as_str());
    let plain = e.run(&["list"]);
    assert!(String::from_utf8_lossy(&plain.stdout).contains("Re: Offer"));
    assert_eq!(
        e.run(&["status", "20261106-223000-99999999"]).status.code(),
        Some(3)
    );
}

#[test]
fn add_invalid_exit2_no_files() {
    let e = env("b");
    let bad = e.root.join("bad.json");
    std::fs::write(&bad, r#"{"schema":"draftdesk/1","account":"work","from":{"address":"me@example.org"},"to":[],"subject":"x","body":"y"}"#).unwrap();
    assert_eq!(
        e.run(&["add", bad.to_str().unwrap()]).status.code(),
        Some(2)
    );
    std::fs::write(&bad, r#"{"schema":"draftdesk/1","surprise":1}"#).unwrap();
    assert_eq!(
        e.run(&["add", bad.to_str().unwrap()]).status.code(),
        Some(2)
    );
    assert_eq!(e.run(&["add", "/nonexistent.json"]).status.code(), Some(2));
    assert!(e.queue().is_empty());
    assert_eq!(e.run(&["add"]).status.code(), Some(2));
    assert_eq!(e.run(&["list", "--bogus"]).status.code(), Some(2));
}

#[test]
fn replace_reports_withdrawn() {
    let e = env("c");
    let old = e.add();
    let out = e.run(&[
        "add",
        e.draft(r#","cc":[]"#).to_str().unwrap(),
        "--replaces",
        &old,
    ]);
    assert_eq!(out.status.code(), Some(0));
    let v = json(&out);
    assert_eq!(v["replaced"]["outcome"], "withdrawn");
    assert_eq!(v["replaced"]["item_id"], old.as_str());
    assert_eq!(json(&e.run(&["status", &old]))["state"], "discarded");
}

#[test]
fn add_twice_prints_same_id() {
    let e = env("corr");
    let p = e.root.join("corr.json");
    let write = |body: &str| {
        std::fs::write(
            &p,
            format!(
                r#"{{"schema":"draftdesk/1","account":"work","from":{{"address":"me@example.org"}},
                   "to":["Ann <ann@example.org>"],"subject":"Invoice","body":"{body}",
                   "origin":{{"agent":"inbox/invoice","correlation":"c1"}}}}"#
            ),
        )
        .unwrap();
    };
    write("Hello Ann\\n");
    let ids: Vec<String> = (0..2)
        .map(|_| {
            let out = e.run(&["add", p.to_str().unwrap()]);
            assert_eq!(
                out.status.code(),
                Some(0),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            json(&out)["item_id"].as_str().unwrap().to_string()
        })
        .collect();
    assert_eq!(ids[0], ids[1]);
    assert_eq!(e.queue(), vec![ids[0].clone()]);
    write("Other text\\n");
    let out = e.run(&["add", p.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("correlation conflict"));
    assert_eq!(e.queue(), vec![ids[0].clone()]);
}

#[test]
fn add_command_takes_a_time_limit() {
    let e = env("tmo");
    let args = |name: &str, timeout: &str| -> Vec<String> {
        [
            "account",
            "add-command",
            "--name",
            name,
            "--address",
            "b@example.org",
            "--send",
            "/bin/cat",
            "--timeout",
            timeout,
        ]
        .map(String::from)
        .to_vec()
    };
    let run = |a: Vec<String>| e.run(&a.iter().map(String::as_str).collect::<Vec<_>>());
    let ok = run(args("bridge", "300"));
    assert_eq!(
        ok.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&ok.stderr)
    );
    let cfg: Value =
        serde_json::from_str(&std::fs::read_to_string(e.root.join("home/config.json")).unwrap())
            .unwrap();
    let saved = cfg["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == "bridge")
        .unwrap();
    assert_eq!(saved["timeout_s"], 300);
    for (name, bad) in [("short", "5"), ("long", "901"), ("word", "abc")] {
        assert_eq!(run(args(name, bad)).status.code(), Some(2), "{bad}");
    }
}

#[test]
fn list_json_has_origin_reply_cc() {
    let e = env("lst");
    let p = e.draft(
        r#","cc":["Carl <carl@example.org>"],
           "reply":{"in_reply_to":"<a@example.org>","references":["<a@example.org>"],"gmail_thread_id":"t1"}"#,
    );
    let out = e.run(&["add", p.to_str().unwrap()]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let id = json(&out)["item_id"].as_str().unwrap().to_string();
    let listed = json(&e.run(&["list", "--json"]))[0].clone();
    let status = json(&e.run(&["status", &id]));
    for v in [&listed, &status] {
        assert_eq!(
            v["origin"],
            serde_json::json!({"agent": "test", "session": "s1", "correlation": null})
        );
        assert_eq!(v["cc"], serde_json::json!(["Carl <carl@example.org>"]));
        assert_eq!(v["reply"]["in_reply_to"], "<a@example.org>");
        assert_eq!(
            v["reply"]["references"],
            serde_json::json!(["<a@example.org>"])
        );
        assert_eq!(v["reply"]["gmail_thread_id"], "t1");
    }
}

#[test]
fn wait_times_out_124() {
    let e = env("d");
    let id = e.add();
    let t = std::time::Instant::now();
    let out = e.run(&["wait", &id, "--timeout", "1"]);
    assert_eq!(out.status.code(), Some(124));
    assert!(t.elapsed() < std::time::Duration::from_secs(5));
}

#[test]
fn claim_prints_and_renames() {
    let e = env("e");
    let id = e.add();
    assert_eq!(e.run(&["discard", &id]).status.code(), Some(0));
    // The discard published a signed result; wait sees it at once.
    let waited = e.run(&["wait", &id, "--timeout", "5"]);
    assert_eq!(waited.status.code(), Some(0));
    assert_eq!(json(&waited)["result"]["status"], "discarded");
    assert!(e.run(&["claim", "--session", "other"]).stdout.is_empty());
    let claimed = lines(&e.run(&["claim", "--session", "s1"]));
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0]["item_id"], id.as_str());
    assert!(e.run(&["claim", "--session", "s1"]).stdout.is_empty());
    let results = e.root.join("home/state/results");
    assert!(
        results
            .join(format!("{id}.json.discarded-delivered"))
            .exists()
    );
}

#[test]
fn claim_limit_leaves_the_rest_for_the_next_claim() {
    let e = env("cl");
    let (a, b) = (e.add(), e.add());
    for id in [&a, &b] {
        assert_eq!(e.run(&["discard", id]).status.code(), Some(0));
    }
    let first = lines(&e.run(&["claim", "--session", "s1", "--limit", "1"]));
    assert_eq!(first.len(), 1);
    let second = lines(&e.run(&["claim", "--session", "s1", "--limit", "1"]));
    assert_eq!(second.len(), 1);
    assert_ne!(first[0]["item_id"], second[0]["item_id"]);
    assert!(e.run(&["claim", "--session", "s1"]).stdout.is_empty());
    for bad in ["0", "-1", "x"] {
        assert_eq!(
            e.run(&["claim", "--session", "s1", "--limit", bad])
                .status
                .code(),
            Some(2)
        );
    }
}

#[test]
fn discard_rules() {
    let e = env("f");
    let edited = e.add();
    e.set_state(&edited, |v| v["edits"]["edited"] = true.into());
    assert_eq!(e.run(&["discard", &edited]).status.code(), Some(2));
    let sending = e.add();
    e.set_state(&sending, |v| v["state"] = "sending".into());
    assert_eq!(e.run(&["discard", &sending]).status.code(), Some(2));
    let ready = e.add();
    assert_eq!(e.run(&["discard", &ready]).status.code(), Some(0));
    let v = json(&e.run(&["status", &ready]));
    assert_eq!(v["state"], "discarded");
    assert_eq!(
        e.run(&["discard", "20261106-223000-99999999"])
            .status
            .code(),
        Some(3)
    );
}

#[test]
fn accounts_add_list_remove() {
    let e = env("g");
    let out = e.run_stdin(
        &[
            "account",
            "add-smtp",
            "--name",
            "mail",
            "--address",
            "box@example.org",
            "--host",
            "smtp.example.org",
            "--port",
            "465",
            "--security",
            "tls",
            "--username",
            "box@example.org",
            "--imap-host",
            "imap.example.org",
            "--imap-port",
            "993",
            "--password-stdin",
        ],
        "s3cret\n",
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let secrets = std::fs::read_to_string(e.root.join("secrets.json")).unwrap();
    assert!(
        secrets.contains(r#""mail:smtp":"s3cret""#) && secrets.contains(r#""mail:imap":"s3cret""#)
    );
    let cfg = std::fs::read_to_string(e.root.join("home/config.json")).unwrap();
    assert!(!cfg.contains("s3cret"), "no secret in the configuration");
    let out = e.run(&[
        "account",
        "add-command",
        "--name",
        "ops",
        "--address",
        "ops@example.org",
        "--send",
        "/usr/bin/true",
    ]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        e.run(&[
            "account",
            "add-command",
            "--name",
            "ops",
            "--address",
            "x@example.org",
            "--send",
            "/bin/x"
        ])
        .status
        .code(),
        Some(2)
    );
    assert_eq!(
        e.run(&[
            "account",
            "add-command",
            "--name",
            "rel",
            "--address",
            "x@example.org",
            "--send",
            "rel"
        ])
        .status
        .code(),
        Some(2)
    );
    let names: Vec<String> = lines(&e.run(&["account", "list"]))
        .iter()
        .map(|v| v["account"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, vec!["work", "mail", "ops"]);
    assert_eq!(
        e.run(&["account", "remove", "--name", "mail"])
            .status
            .code(),
        Some(0)
    );
    assert!(
        !std::fs::read_to_string(e.root.join("secrets.json"))
            .unwrap()
            .contains("mail:smtp")
    );
    assert_eq!(
        e.run(&["account", "remove", "--name", "mail"])
            .status
            .code(),
        Some(3)
    );
    assert_eq!(e.run(&["config", "check"]).status.code(), Some(0));
}

#[test]
fn cli_has_no_send_command() {
    let e = env("h");
    let id = e.add();
    for cmd in ["send", "fire", "schedule"] {
        assert_eq!(e.run(&[cmd, &id]).status.code(), Some(2), "{cmd}");
    }
    assert_eq!(json(&e.run(&["status", &id]))["state"], "ready");
}

#[test]
fn discard_tells_when_the_result_cannot_be_published() {
    let e = env("pub");
    let id = e.add();
    // A folder where the result file goes: writing the result fails.
    std::fs::create_dir_all(e.root.join(format!("home/state/results/{id}.json"))).unwrap();
    let out = e.run(&["discard", &id]);
    assert_eq!(out.status.code(), Some(0), "discarded all the same");
    assert_eq!(json(&out)["state"], "discarded");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("could not be published"), "{err}");
    // No signing key yet (the app has not started once): nothing to publish, and nothing to tell.
    std::fs::write(e.root.join("secrets.json"), "{}").unwrap();
    let other = e.add();
    let out = e.run(&["discard", &other]);
    assert_eq!(out.status.code(), Some(0));
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `account add-smtp` for the account `mail`, plus `more`.
fn add_smtp<'a>(more: &[&'a str]) -> Vec<&'a str> {
    let mut args = vec![
        "account",
        "add-smtp",
        "--name",
        "mail",
        "--address",
        "box@example.org",
        "--host",
        "smtp.example.org",
        "--port",
        "465",
        "--security",
        "tls",
        "--username",
        "box@example.org",
    ];
    args.extend_from_slice(more);
    args
}

#[test]
fn imap_port_without_imap_host_is_refused() {
    let e = env("imp");
    let out = e.run_stdin(
        &add_smtp(&["--imap-port", "993", "--password-stdin"]),
        "s3cret\n",
    );
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("--imap-port needs --imap-host"), "{err}");
    let cfg = std::fs::read_to_string(e.root.join("home/config.json")).unwrap();
    assert!(!cfg.contains("box@example.org"), "nothing was added");
}

/// A pseudo terminal: (the side a person types into, the terminal the program reads).
fn pty() -> (std::fs::File, std::fs::File) {
    use std::os::fd::FromRawFd;
    let (mut keyboard, mut terminal) = (0, 0);
    // SAFETY: openpty only fills the two descriptors; name, settings and window size are not asked for.
    let r = unsafe {
        libc::openpty(
            &mut keyboard,
            &mut terminal,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(r, 0, "openpty: {}", std::io::Error::last_os_error());
    // SAFETY: two fresh descriptors that nothing else owns.
    unsafe {
        (
            std::fs::File::from_raw_fd(keyboard),
            std::fs::File::from_raw_fd(terminal),
        )
    }
}

/// Whether the terminal shows what is typed.
fn echoes(terminal: &std::fs::File) -> bool {
    use std::os::fd::AsRawFd;
    // SAFETY: tcgetattr fills the struct for an open terminal descriptor.
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        assert_eq!(libc::tcgetattr(terminal.as_raw_fd(), &mut t), 0);
        t.c_lflag & libc::ECHO != 0
    }
}

#[test]
fn password_stdin_at_a_terminal_is_not_shown() {
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    let e = env("tty");
    let (mut keyboard, terminal) = pty();
    assert!(echoes(&terminal), "a new terminal shows what is typed");
    let child = e
        .command(&add_smtp(&["--password-stdin"]))
        .stdin(terminal.try_clone().unwrap())
        .spawn()
        .unwrap();
    // The terminal shows a key when it is typed: type only once the echo is off (or after a while, to see it fail).
    let start = std::time::Instant::now();
    while echoes(&terminal) && start.elapsed() < std::time::Duration::from_secs(10) {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let hidden = !echoes(&terminal);
    keyboard.write_all(b"s3cret\n").unwrap();
    let out = child.wait_with_output().unwrap();
    // SAFETY: a flag change on a descriptor this test owns.
    unsafe { libc::fcntl(keyboard.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) };
    let mut shown = [0u8; 256];
    let n = keyboard.read(&mut shown).unwrap_or(0);
    let shown = String::from_utf8_lossy(&shown[..n]);
    assert!(
        hidden && !shown.contains("s3cret"),
        "the terminal showed {shown:?}"
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("Password: "));
    let secrets = std::fs::read_to_string(e.root.join("secrets.json")).unwrap();
    assert!(secrets.contains(r#""mail:smtp":"s3cret""#), "{secrets}");
}
