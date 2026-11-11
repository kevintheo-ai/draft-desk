//! `draftdesk`: hand drafts to Draft Desk, follow them, set up accounts. It cannot send: the core's `send`
//! feature is off for this crate, so no sending code is even compiled in.

use draftdesk_core::accounts::{self, AccountSecret};
use draftdesk_core::add::{ReplaceOutcome, add, withdraw};
use draftdesk_core::config::{
    AccountCfg, Config, GmailCfg, ImapCfg, Kind, Security, SenderCfg, SmtpCfg,
};
use draftdesk_core::gmail::{http::UreqClient, oauth};
use draftdesk_core::item::{Draft, ItemRead};
use draftdesk_core::results::{self, ResultRecord, ResultStatus};
use draftdesk_core::secrets::Secrets;
use draftdesk_core::store::Store;
use serde_json::{Value, json};
use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

const USAGE: &str = "draftdesk: hand a mail draft to Draft Desk for review; a person sends it from the app.

  draftdesk add <draft.json> [--replaces <id>]
  draftdesk list [--json]
  draftdesk status <id>
  draftdesk wait <id> [--timeout <seconds>]
  draftdesk discard <id>
  draftdesk claim --session <session> [--limit <n>]
  draftdesk account add-smtp --name N --address A [--sender-name S] --host H --port P --security tls|starttls
                            --username U [--imap-host H --imap-port P] [--save-sent-copy] [--password-stdin]
  draftdesk account add-gmail --name N --address A --client-id ID [--sender-name S] [--secret-stdin]
  draftdesk account add-command --name N --address A [--sender-name S] --send /abs/program
                               [--discard /abs/program] [--timeout <seconds, 30-900, default 180>]
  draftdesk account list
  draftdesk account remove --name N
  draftdesk config check

Root: $DRAFTDESK_HOME, else ~/Library/Application Support/Draft Desk.
Exit codes: 0 ok, 1 error, 2 invalid input or refused, 3 unknown id, 124 time limit.";

/// An exit code with a message for stderr.
struct Fail(u8, String);

type Out = Result<(), Fail>;

fn fail<T>(code: u8, msg: impl Into<String>) -> Result<T, Fail> {
    Err(Fail(code, msg.into()))
}

fn err1(e: draftdesk_core::Error) -> Fail {
    Fail(1, e.0)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Fail(code, msg)) => {
            if !msg.is_empty() {
                eprintln!("draftdesk: {msg}");
            }
            ExitCode::from(code)
        }
    }
}

fn root() -> Result<PathBuf, Fail> {
    draftdesk_core::config::default_root().map_err(err1)
}

/// The keychain; debug builds may use a JSON file instead (tests never touch the real keychain).
fn secrets() -> Arc<dyn Secrets + Send + Sync> {
    draftdesk_core::secrets::system()
}

/// Opens Draft Desk in the background, or wakes it; the app notices the new draft by itself. Only for the standard
/// root: the app knows no other.
#[cfg(target_os = "macos")]
fn wake_app() {
    use std::process::{Command, Stdio};
    if !draftdesk_core::config::is_default_root() {
        return;
    }
    let opened = Command::new("/usr/bin/open")
        .args(["-g", "-b", draftdesk_core::APP_BUNDLE_ID])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !opened {
        eprintln!("draftdesk: the Draft Desk app is not installed; open it to review the draft");
    }
}

fn open_store() -> Result<Store, Fail> {
    let root = root()?;
    let cfg = Config::load(&root).map_err(|e| Fail(2, e.0))?;
    Store::open(&root, cfg).map_err(err1)
}

/// `--flag value` pairs and bare words.
struct Args<'a> {
    words: Vec<&'a str>,
    flags: Vec<(&'a str, Option<&'a str>)>,
}

/// Flags that take no value.
const SWITCHES: &[&str] = &[
    "--json",
    "--save-sent-copy",
    "--password-stdin",
    "--secret-stdin",
];

fn parse(args: &[String]) -> Result<Args<'_>, Fail> {
    let (mut words, mut flags, mut i) = (vec![], vec![], 0);
    while i < args.len() {
        let a = args[i].as_str();
        if a.starts_with("--") {
            if SWITCHES.contains(&a) {
                flags.push((a, None));
            } else {
                let v = args
                    .get(i + 1)
                    .ok_or(Fail(2, format!("{a} needs a value")))?;
                flags.push((a, Some(v.as_str())));
                i += 1;
            }
        } else {
            words.push(a);
        }
        i += 1;
    }
    Ok(Args { words, flags })
}

impl<'a> Args<'a> {
    fn get(&self, name: &str) -> Option<&'a str> {
        self.flags
            .iter()
            .find(|(k, _)| *k == name)
            .and_then(|(_, v)| *v)
    }
    fn need(&self, name: &str) -> Result<&'a str, Fail> {
        self.get(name).ok_or(Fail(2, format!("{name} is required")))
    }
    fn has(&self, name: &str) -> bool {
        self.flags.iter().any(|(k, _)| *k == name)
    }
    /// Refuses flags the command does not know.
    fn only(&self, allowed: &[&str]) -> Out {
        match self.flags.iter().find(|(k, _)| !allowed.contains(k)) {
            Some((k, _)) => fail(2, format!("unknown option {k}")),
            None => Ok(()),
        }
    }
}

fn run(args: &[String]) -> Out {
    let a = parse(args)?;
    match a.words.as_slice() {
        ["add", file] => cmd_add(file, &a),
        ["list"] => cmd_list(&a),
        ["status", id] => cmd_status(id, &a),
        ["wait", id] => cmd_wait(id, &a),
        ["discard", id] => cmd_discard(id, &a),
        ["claim"] => cmd_claim(&a),
        ["account", "add-smtp"] => account_add_smtp(&a),
        ["account", "add-gmail"] => account_add_gmail(&a),
        ["account", "add-command"] => account_add_command(&a),
        ["account", "list"] => account_list(&a),
        ["account", "remove"] => account_remove(&a),
        ["config", "check"] => {
            a.only(&[])?;
            Config::load(&root()?).map_err(|e| Fail(2, e.0))?;
            println!("ok");
            Ok(())
        }
        [] | ["help"] => {
            println!("{USAGE}");
            Ok(())
        }
        _ => fail(2, format!("unknown command\n\n{USAGE}")),
    }
}

fn print(v: &Value) {
    println!("{v}");
}

fn replaced_json(r: &Option<ReplaceOutcome>) -> Value {
    match r {
        None => Value::Null,
        Some(ReplaceOutcome::Withdrawn(id)) => json!({"item_id": id, "outcome": "withdrawn"}),
        Some(ReplaceOutcome::BlockedWindowEdited(id)) => {
            json!({"item_id": id, "outcome": "blocked_window_edited"})
        }
        Some(ReplaceOutcome::TooLate(id)) => json!({"item_id": id, "outcome": "too_late"}),
        Some(ReplaceOutcome::NotFound(id)) => json!({"item_id": id, "outcome": "not_found"}),
    }
}

fn cmd_add(file: &str, a: &Args) -> Out {
    a.only(&["--replaces"])?;
    let text = std::fs::read_to_string(file).map_err(|e| Fail(2, format!("{file}: {e}")))?;
    let draft: Draft = serde_json::from_str(&text).map_err(|e| Fail(2, format!("{file}: {e}")))?;
    let store = open_store()?;
    let out =
        add(&store, draft, a.get("--replaces"), chrono::Local::now()).map_err(|e| Fail(2, e.0))?;
    print(&json!({"item_id": out.item_id, "replaced": replaced_json(&out.replaced)}));
    #[cfg(target_os = "macos")]
    wake_app();
    Ok(())
}

fn summary(r: &ItemRead) -> Value {
    match r {
        ItemRead::Valid(i) => json!({
            "item_id": i.item_id, "state": i.state, "account": i.account, "subject": i.subject,
            "to": i.to, "cc": i.cc, "reply": i.reply, "origin": i.origin, "created_at": i.created_at,
            "edited": i.edits.edited,
        }),
        ItemRead::Invalid { item_id, errors } => {
            json!({"item_id": item_id, "state": "invalid", "errors": errors})
        }
    }
}

fn cmd_list(a: &Args) -> Out {
    a.only(&["--json"])?;
    let store = open_store()?;
    let items: Vec<Value> = store.list().iter().map(summary).collect();
    if a.has("--json") {
        print(&Value::Array(items));
        return Ok(());
    }
    for v in items {
        let field = |k: &str| v[k].as_str().unwrap_or("").to_string();
        println!(
            "{}  {:<18} {:<12} {}",
            field("item_id"),
            field("state"),
            field("account"),
            field("subject")
        );
    }
    Ok(())
}

fn status_json(
    store: &Store,
    secrets: &dyn Secrets,
    id: &str,
) -> Result<(Value, Option<ResultRecord>), Fail> {
    let read = store.read_any(id);
    let result = results::read_verified(store, secrets, id).map_err(err1)?;
    let known = matches!(read, ItemRead::Valid(_)) || store.item_dir(id).is_ok_and(|d| d.exists());
    if !known && result.is_none() {
        return fail(3, format!("unknown item {id}"));
    }
    let mut v = summary(&read);
    v["result"] = serde_json::to_value(&result).unwrap_or(Value::Null);
    Ok((v, result))
}

fn cmd_status(id: &str, a: &Args) -> Out {
    a.only(&[])?;
    let store = open_store()?;
    let (v, _) = status_json(&store, secrets().as_ref(), id)?;
    print(&v);
    Ok(())
}

fn cmd_wait(id: &str, a: &Args) -> Out {
    a.only(&["--timeout"])?;
    let limit = match a.get("--timeout") {
        Some(s) => Duration::from_secs(
            s.parse()
                .map_err(|_| Fail(2, "--timeout needs whole seconds".into()))?,
        ),
        None => Duration::from_secs(24 * 3600),
    };
    let store = open_store()?;
    let secrets = secrets();
    let start = Instant::now();
    loop {
        let (v, result) = status_json(&store, secrets.as_ref(), id)?;
        if result.is_some() {
            print(&v);
            return Ok(());
        }
        if start.elapsed() >= limit {
            print(&v);
            return fail(124, "");
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn cmd_discard(id: &str, a: &Args) -> Out {
    a.only(&[])?;
    let store = open_store()?;
    // Never touches the mailbox: the app cleans up the provider draft (history `discarded_by_cli`).
    match withdraw(
        &store,
        id,
        None,
        "discarded_by_cli",
        "the app cleans up the mailbox draft",
    )
    .map_err(err1)?
    {
        ReplaceOutcome::Withdrawn(_) => {
            if let ItemRead::Valid(item) = store.read(id) {
                let record = ResultRecord::for_item(&item, ResultStatus::Discarded);
                // `Ok(false)` (no signing key yet) is no error: the app creates the key at its start and publishes
                // then. A failed write is one: `wait` and `claim` would never see the discard.
                if let Err(e) = results::publish(&store, secrets().as_ref(), &record) {
                    eprintln!(
                        "draftdesk: discarded, but the agent's result could not be published: {}",
                        e.0
                    );
                }
            }
            print(&json!({"item_id": id, "state": "discarded"}));
            Ok(())
        }
        ReplaceOutcome::BlockedWindowEdited(_) => fail(2, "edited in the window: discard it there"),
        ReplaceOutcome::TooLate(_) => fail(
            2,
            "too late: it is being sent, was sent or may have been sent",
        ),
        ReplaceOutcome::NotFound(_) => fail(3, format!("unknown item {id}")),
    }
}

fn cmd_claim(a: &Args) -> Out {
    a.only(&["--session", "--limit"])?;
    let session = a.need("--session")?;
    let limit = match a.get("--limit") {
        None => None,
        Some(v) => Some(
            v.parse::<usize>()
                .ok()
                .filter(|n| *n > 0)
                .ok_or(Fail(2, "--limit must be a positive whole number".into()))?,
        ),
    };
    let store = open_store()?;
    for r in results::claim_session(&store, secrets().as_ref(), session, limit).map_err(err1)? {
        print(&serde_json::to_value(&r).unwrap_or(Value::Null));
    }
    Ok(())
}

/// A secret: read as it comes from a pipe, typed without echo at a terminal. At a terminal also with `--…-stdin`: a
/// plain read would show the secret and leave it in the scrollback.
fn read_secret(prompt: &str) -> Result<String, Fail> {
    let stdin = std::io::stdin();
    let mut line = String::new();
    if !stdin.is_terminal() {
        stdin
            .lock()
            .read_line(&mut line)
            .map_err(|e| Fail(1, e.to_string()))?;
    } else {
        line = read_hidden(prompt).map_err(|e| Fail(1, e.to_string()))?;
    }
    let s = line.trim_end_matches(['\n', '\r']).to_string();
    if s.is_empty() {
        return fail(2, "empty secret");
    }
    Ok(s)
}

/// Reads one line from the terminal with echo switched off. The prompt comes only once echo is off, and a terminal
/// that does not switch it off is an error, never a visible secret.
fn read_hidden(prompt: &str) -> std::io::Result<String> {
    // SAFETY: termios calls on stdin with a struct that tcgetattr fills before use.
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(0, &mut t) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let saved = t;
        t.c_lflag &= !libc::ECHO;
        if libc::tcsetattr(0, libc::TCSANOW, &t) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        eprint!("{prompt}: ");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        let r = std::io::stdin().lock().read_line(&mut line);
        libc::tcsetattr(0, libc::TCSANOW, &saved);
        eprintln!();
        r.map(|_| line)
    }
}

fn new_account(a: &Args, kind: Kind) -> Result<AccountCfg, Fail> {
    Ok(AccountCfg {
        name: a.need("--name")?.into(),
        kind,
        senders: vec![SenderCfg {
            address: a.need("--address")?.into(),
            name: a.get("--sender-name").map(String::from),
            fixed_name: false,
        }],
        smtp: None,
        imap: None,
        gmail: None,
        send_command: None,
        discard_command: None,
        timeout_s: None,
        save_sent_copy: false,
    })
}

/// Before the secret is asked for (and before a Google sign-in): valid, and the name still free.
fn check_account(acc: &AccountCfg) -> Out {
    accounts::check_new(&root()?, acc).map_err(|e| Fail(2, e.0))
}

/// Adds the account (validated, never over an existing name), then its secrets.
fn save_account(acc: AccountCfg, secret: AccountSecret) -> Out {
    accounts::add(&root()?, secrets().as_ref(), acc, secret).map_err(|e| Fail(2, e.0))
}

fn port(a: &Args, flag: &str) -> Result<u16, Fail> {
    a.need(flag)?
        .parse()
        .map_err(|_| Fail(2, format!("{flag} must be a port number")))
}

fn account_add_smtp(a: &Args) -> Out {
    a.only(&[
        "--name",
        "--address",
        "--sender-name",
        "--host",
        "--port",
        "--security",
        "--username",
        "--imap-host",
        "--imap-port",
        "--save-sent-copy",
        "--password-stdin",
    ])?;
    let mut acc = new_account(a, Kind::Smtp)?;
    let security = match a.need("--security")? {
        "tls" => Security::Tls,
        "starttls" => Security::Starttls,
        other => {
            return fail(
                2,
                format!("--security must be tls or starttls, not {other}"),
            );
        }
    };
    let username = a.need("--username")?.to_string();
    acc.smtp = Some(SmtpCfg {
        host: a.need("--host")?.into(),
        port: port(a, "--port")?,
        security,
        username: username.clone(),
    });
    if let Some(h) = a.get("--imap-host") {
        acc.imap = Some(ImapCfg {
            host: h.into(),
            port: port(a, "--imap-port")?,
            username,
        });
    } else if a.has("--imap-port") {
        return fail(2, "--imap-port needs --imap-host");
    }
    acc.save_sent_copy = a.has("--save-sent-copy");
    check_account(&acc)?;
    let password = read_secret("Password")?;
    let name = acc.name.clone();
    save_account(
        acc,
        AccountSecret::Smtp {
            password: &password,
        },
    )?;
    print(&json!({"account": name, "kind": "smtp"}));
    Ok(())
}

fn account_add_gmail(a: &Args) -> Out {
    a.only(&[
        "--name",
        "--address",
        "--sender-name",
        "--client-id",
        "--secret-stdin",
    ])?;
    let mut acc = new_account(a, Kind::Gmail)?;
    let client_id = a.need("--client-id")?.to_string();
    acc.gmail = Some(GmailCfg {
        client_id: client_id.clone(),
    });
    check_account(&acc)?;
    let secret = read_secret("OAuth client secret")?;
    let open = |url: &str| {
        eprintln!("Sign in with Google in your browser. If it does not open, visit:\n{url}");
        let _ = std::process::Command::new("/usr/bin/open")
            .arg(url)
            .status();
    };
    let tokens = oauth::authorize(&UreqClient::new(), &client_id, &secret, &open).map_err(err1)?;
    let name = acc.name.clone();
    save_account(
        acc,
        AccountSecret::Gmail {
            client_secret: &secret,
            refresh_token: &tokens.refresh_token,
        },
    )?;
    print(&json!({"account": name, "kind": "gmail"}));
    Ok(())
}

fn account_add_command(a: &Args) -> Out {
    a.only(&[
        "--name",
        "--address",
        "--sender-name",
        "--send",
        "--discard",
        "--timeout",
    ])?;
    let mut acc = new_account(a, Kind::Command)?;
    acc.send_command = Some(vec![a.need("--send")?.into()]);
    acc.discard_command = a.get("--discard").map(|d| vec![d.to_string()]);
    acc.timeout_s = a
        .get("--timeout")
        .map(|t| {
            t.parse()
                .map_err(|_| Fail(2, "--timeout needs whole seconds".into()))
        })
        .transpose()?;
    let name = acc.name.clone();
    save_account(acc, AccountSecret::None)?;
    print(&json!({"account": name, "kind": "command"}));
    Ok(())
}

fn account_list(a: &Args) -> Out {
    a.only(&[])?;
    let cfg = Config::load(&root()?).map_err(|e| Fail(2, e.0))?;
    for acc in &cfg.accounts {
        let senders: Vec<&str> = acc.senders.iter().map(|s| s.address.as_str()).collect();
        print(&json!({"account": acc.name, "kind": acc.kind, "senders": senders}));
    }
    Ok(())
}

fn account_remove(a: &Args) -> Out {
    a.only(&["--name"])?;
    let name = a.need("--name")?;
    if !accounts::remove(&root()?, secrets().as_ref(), name).map_err(err1)? {
        return fail(3, format!("unknown account {name}"));
    }
    print(&json!({"account": name, "removed": true}));
    Ok(())
}
