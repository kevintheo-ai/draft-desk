//! `config.json` under the Draft Desk root: accounts, senders and the check/fix commands. Secrets never live here.

use crate::address;
use crate::{Error, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

pub const CONFIG_FILE: &str = "config.json";
/// How long a command account's send or discard program may run, unless the account sets `timeout_s`.
pub const SEND_TIMEOUT_S: u64 = 180;
/// The time limits a command account may set, in seconds.
pub const SEND_TIMEOUT_RANGE_S: RangeInclusive<u64> = 30..=900;

/// `$DRAFTDESK_HOME` when it is set and not empty.
fn home_override() -> Option<OsString> {
    std::env::var_os("DRAFTDESK_HOME").filter(|v| !v.is_empty())
}

/// The Draft Desk root: `$DRAFTDESK_HOME`, else `~/Library/Application Support/Draft Desk`.
pub fn default_root() -> Result<PathBuf> {
    root_from(home_override(), std::env::var_os("HOME")).map(followed)
}

/// A root that is itself a symlink (a data folder moved elsewhere and linked back) is followed once: the store
/// refuses symlinks inside, never the place the person chose. A root not there yet stays as it is.
fn followed(root: PathBuf) -> PathBuf {
    match std::fs::symlink_metadata(&root) {
        Ok(m) if m.file_type().is_symlink() => std::fs::canonicalize(&root).unwrap_or(root),
        _ => root,
    }
}

/// True without `$DRAFTDESK_HOME`: the app only knows the standard root.
pub fn is_default_root() -> bool {
    home_override().is_none()
}

fn root_from(draftdesk_home: Option<OsString>, home: Option<OsString>) -> Result<PathBuf> {
    if let Some(h) = draftdesk_home.filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(h));
    }
    let home = home
        .filter(|v| !v.is_empty())
        .ok_or_else(|| Error("HOME is not set".into()))?;
    Ok(PathBuf::from(home).join("Library/Application Support/Draft Desk"))
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub accounts: Vec<AccountCfg>,
    #[serde(default)]
    pub check_command: Option<CommandCfg>,
    #[serde(default)]
    pub fix_command: Option<CommandCfg>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Smtp,
    Gmail,
    Command,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountCfg {
    pub name: String,
    pub kind: Kind,
    pub senders: Vec<SenderCfg>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smtp: Option<SmtpCfg>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imap: Option<ImapCfg>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gmail: Option<GmailCfg>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send_command: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discard_command: Option<Vec<String>>,
    /// Only for kind command: the time limit of the send and discard programs (default [`SEND_TIMEOUT_S`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_s: Option<u64>,
    #[serde(default)]
    pub save_sent_copy: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SenderCfg {
    pub address: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The display name must stay exactly as configured.
    #[serde(default)]
    pub fixed_name: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Security {
    /// Implicit TLS, usually port 465.
    Tls,
    /// STARTTLS, usually port 587.
    Starttls,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SmtpCfg {
    pub host: String,
    pub port: u16,
    pub security: Security,
    pub username: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImapCfg {
    pub host: String,
    pub port: u16,
    pub username: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GmailCfg {
    pub client_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandCfg {
    pub argv: Vec<String>,
    #[serde(default = "default_timeout")]
    pub timeout_s: u64,
}

fn default_timeout() -> u64 {
    300
}

static ACCOUNT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z0-9][a-z0-9_-]{0,31}$").unwrap());

impl Config {
    /// Reads and validates `<root>/config.json`; a missing file is an empty configuration.
    pub fn load(root: &Path) -> Result<Config> {
        let path = root.join(CONFIG_FILE);
        if std::fs::symlink_metadata(&path).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
        {
            return Ok(Config::default());
        }
        let bytes = crate::store::read_regular(&path, 1024 * 1024)?;
        Config::parse(std::str::from_utf8(&bytes).map_err(|_| Error("config: not UTF-8".into()))?)
    }

    /// Validates, then writes `<root>/config.json` atomically (0600).
    pub fn save(&self, root: &Path) -> Result<()> {
        let errors = self.validate();
        if !errors.is_empty() {
            return Err(Error(format!("config: {}", errors.join("; "))));
        }
        crate::store::mkdir_private(root)?;
        let mut json = serde_json::to_string_pretty(self)?;
        json.push('\n');
        crate::store::write_atomic(root, CONFIG_FILE, json.as_bytes())
    }

    pub fn parse(json: &str) -> Result<Config> {
        let cfg: Config = serde_json::from_str(json)?;
        let errors = cfg.validate();
        if errors.is_empty() {
            Ok(cfg)
        } else {
            Err(Error(format!("config: {}", errors.join("; "))))
        }
    }

    /// Every rule violation, in a stable order.
    pub fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();
        let mut seen = BTreeSet::new();
        for a in &self.accounts {
            let n = &a.name;
            if !ACCOUNT_RE.is_match(n) {
                errors.push(format!(
                    "account name {n:?}: must match [a-z0-9][a-z0-9_-]{{0,31}}"
                ));
            }
            if !seen.insert(n.as_str()) {
                errors.push(format!("duplicate account {n:?}"));
            }
            if a.senders.is_empty() {
                errors.push(format!("{n}: no senders"));
            }
            for s in &a.senders {
                let single = matches!(address::parse_list(&s.address).as_deref(),
                    Ok([one]) if one.name.is_empty() && one.address == s.address);
                if !single {
                    errors.push(format!(
                        "{n}: sender address {:?} must be one bare address",
                        s.address
                    ));
                }
                if let Some(name) = &s.name
                    && (name.trim().is_empty()
                        || crate::text::has_linebreak(name)
                        || crate::text::has_forbidden(name))
                {
                    errors.push(format!(
                        "{n}: sender name {name:?} is empty or has control characters"
                    ));
                }
            }
            match a.kind {
                Kind::Smtp if a.smtp.is_none() => errors.push(format!("{n}: kind smtp needs smtp")),
                Kind::Gmail if a.gmail.is_none() => {
                    errors.push(format!("{n}: kind gmail needs gmail"))
                }
                Kind::Command if a.send_command.as_ref().is_none_or(|c| c.is_empty()) => {
                    errors.push(format!("{n}: kind command needs a send_command"))
                }
                _ => {}
            }
            if let Some(s) = &a.smtp {
                check_server(&mut errors, n, "smtp", &s.host, s.port, &s.username);
            }
            if let Some(i) = &a.imap {
                check_server(&mut errors, n, "imap", &i.host, i.port, &i.username);
            }
            if let Some(g) = &a.gmail
                && (g.client_id.trim().is_empty() || g.client_id.contains(char::is_whitespace))
            {
                errors.push(format!("{n}: gmail client_id is empty or has spaces"));
            }
            if a.save_sent_copy && a.imap.is_none() {
                errors.push(format!("{n}: save_sent_copy needs imap"));
            }
            for (what, argv) in [
                ("send_command", &a.send_command),
                ("discard_command", &a.discard_command),
            ] {
                if let Some(argv) = argv {
                    check_argv(&mut errors, &format!("{n}: {what}"), argv);
                }
            }
            match a.timeout_s {
                Some(_) if a.kind != Kind::Command => {
                    errors.push(format!("{n}: timeout_s is only for kind command"))
                }
                Some(t) if !SEND_TIMEOUT_RANGE_S.contains(&t) => errors.push(format!(
                    "{n}: timeout_s must be {}-{} seconds",
                    SEND_TIMEOUT_RANGE_S.start(),
                    SEND_TIMEOUT_RANGE_S.end()
                )),
                _ => {}
            }
        }
        for (what, cmd) in [
            ("check_command", &self.check_command),
            ("fix_command", &self.fix_command),
        ] {
            if let Some(c) = cmd {
                if c.argv.is_empty() {
                    errors.push(format!("{what}: argv is empty"));
                } else {
                    check_argv(&mut errors, what, &c.argv);
                }
                if c.timeout_s == 0 {
                    errors.push(format!("{what}: timeout_s must be at least 1"));
                }
            }
        }
        errors
    }

    pub fn account(&self, name: &str) -> Option<&AccountCfg> {
        self.accounts.iter().find(|a| a.name == name)
    }
}

fn check_server(
    errors: &mut Vec<String>,
    account: &str,
    what: &str,
    host: &str,
    port: u16,
    username: &str,
) {
    if host.trim().is_empty() || host.contains(char::is_whitespace) {
        errors.push(format!("{account}: {what} host is empty or has spaces"));
    }
    if port == 0 {
        errors.push(format!("{account}: {what} port must be 1-65535"));
    }
    if username.trim().is_empty() {
        errors.push(format!("{account}: {what} username is empty"));
    }
}

/// An empty list is allowed only where the caller checks it; the program itself must be an absolute path.
fn check_argv(errors: &mut Vec<String>, what: &str, argv: &[String]) {
    match argv.first() {
        None => errors.push(format!("{what}: is empty")),
        Some(p) if !Path::new(p).is_absolute() => {
            errors.push(format!("{what}: program {p:?} must be an absolute path"))
        }
        _ => {}
    }
}

impl AccountCfg {
    pub fn sender(&self, address: &str) -> Option<&SenderCfg> {
        self.senders.iter().find(|s| s.address == address)
    }

    /// The time limit of a command account's send and discard programs.
    pub fn send_timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_s.unwrap_or(SEND_TIMEOUT_S))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    const GOOD: &str = r#"{
      "accounts": [
        { "name": "work", "kind": "smtp",
          "senders": [ { "address": "me@example.org", "name": "Me", "fixed_name": false } ],
          "smtp": { "host": "smtp.example.org", "port": 465, "security": "tls", "username": "me@example.org" },
          "imap": { "host": "imap.example.org", "port": 993, "username": "me@example.org" },
          "save_sent_copy": true },
        { "name": "gmail", "kind": "gmail", "senders": [ { "address": "me@gmail.example" } ],
          "gmail": { "client_id": "x.apps.googleusercontent.com" } },
        { "name": "custom", "kind": "command", "senders": [ { "address": "ops@example.org" } ],
          "send_command": ["/usr/local/bin/my-send", "--flag"], "discard_command": null }
      ],
      "check_command": { "argv": ["/path/check"], "timeout_s": 300 },
      "fix_command": null
    }"#;

    fn with(edit: impl FnOnce(&mut Value)) -> String {
        let mut v: Value = serde_json::from_str(GOOD).unwrap();
        edit(&mut v);
        v.to_string()
    }

    fn errors(json: &str) -> String {
        match Config::parse(json) {
            Ok(_) => String::new(),
            Err(e) => e.0,
        }
    }

    #[test]
    fn a_root_that_is_a_symlink_is_followed() {
        // A data folder moved elsewhere and linked back: the store refuses symlinks inside, not the root.
        let base = std::env::temp_dir().join(format!("dd-root-{}", crate::random_hex(4)));
        let real = base.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = base.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let root = followed(link.clone());
        assert!(
            std::fs::symlink_metadata(&root).unwrap().is_dir(),
            "{root:?}"
        );
        assert_eq!(root, std::fs::canonicalize(&real).unwrap());
        let missing = base.join("missing");
        assert_eq!(
            followed(missing.clone()),
            missing,
            "not there yet: unchanged"
        );
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn root_env_wins_home_falls_back_and_empty_counts_as_unset() {
        let os = |s: &str| Some(std::ffi::OsString::from(s));
        assert_eq!(
            root_from(os("/tmp/dd"), os("/h")).unwrap(),
            PathBuf::from("/tmp/dd")
        );
        assert_eq!(
            root_from(None, os("/h")).unwrap(),
            PathBuf::from("/h/Library/Application Support/Draft Desk")
        );
        assert_eq!(
            root_from(os(""), os("/h")).unwrap(),
            PathBuf::from("/h/Library/Application Support/Draft Desk")
        );
        assert!(root_from(None, None).is_err());
        assert!(root_from(None, os("")).is_err());
    }

    #[test]
    fn good_config_parses() {
        let cfg = Config::parse(GOOD).unwrap();
        assert_eq!(cfg.accounts.len(), 3);
        assert_eq!(cfg.check_command.unwrap().timeout_s, 300);
        assert!(cfg.accounts[0].sender("me@example.org").is_some());
    }

    #[test]
    fn missing_file_is_empty_config() {
        let dir = std::env::temp_dir().join(format!("dd-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(Config::load(&dir).unwrap(), Config::default());
        std::fs::write(dir.join(CONFIG_FILE), GOOD).unwrap();
        assert_eq!(Config::load(&dir).unwrap().accounts.len(), 3);
        std::fs::write(
            dir.join(CONFIG_FILE),
            with(|v| v["accounts"][0]["name"] = "BAD".into()),
        )
        .unwrap();
        assert!(Config::load(&dir).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn save_roundtrip_and_refuses_invalid() {
        let dir = std::env::temp_dir().join(format!("dd-cfg-save-{}", std::process::id()));
        let cfg = Config::parse(GOOD).unwrap();
        cfg.save(&dir).unwrap();
        assert_eq!(Config::load(&dir).unwrap(), cfg);
        let mut bad = cfg.clone();
        bad.accounts[0].name = "BAD".into();
        assert!(bad.save(&dir).is_err());
        assert_eq!(Config::load(&dir).unwrap(), cfg);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unknown_field_rejected() {
        assert!(errors(&with(|v| v["extra"] = 1.into())).contains("unknown field"));
        assert!(
            errors(&with(|v| v["accounts"][0]["smtp"]["x"] = 1.into())).contains("unknown field")
        );
    }

    #[test]
    fn account_name_rules() {
        assert!(
            errors(&with(|v| v["accounts"][0]["name"] = "Work".into())).contains("account name")
        );
        assert!(errors(&with(|v| v["accounts"][0]["name"] = "-x".into())).contains("account name"));
        assert!(
            errors(&with(|v| v["accounts"][0]["name"] = "a".repeat(33).into()))
                .contains("account name")
        );
        assert!(errors(&with(|v| v["accounts"][0]["name"] = "a".repeat(32).into())).is_empty());
        assert!(
            errors(&with(|v| v["accounts"][1]["name"] = "work".into()))
                .contains("duplicate account")
        );
    }

    #[test]
    fn sender_rules() {
        assert!(errors(&with(|v| v["accounts"][1]["senders"] = json!([]))).contains("no senders"));
        for bad in ["Me <me@x.example>", "nope", "a@x.example, b@x.example"] {
            assert!(
                errors(&with(
                    |v| v["accounts"][1]["senders"][0]["address"] = bad.into()
                ))
                .contains("sender address"),
                "{bad}"
            );
        }
        assert!(
            errors(&with(
                |v| v["accounts"][1]["senders"][0]["name"] = "Bad\u{202E}".into()
            ))
            .contains("sender name")
        );
        assert!(
            errors(&with(
                |v| v["accounts"][1]["senders"][0]["name"] = "Two\nLines".into()
            ))
            .contains("sender name")
        );
    }

    #[test]
    fn security_and_port_rules() {
        assert!(
            errors(&with(
                |v| v["accounts"][0]["smtp"]["security"] = "none".into()
            ))
            .contains("unknown variant")
        );
        assert!(errors(&with(|v| v["accounts"][0]["smtp"]["port"] = 0.into())).contains("port"));
        assert!(
            errors(&with(|v| v["accounts"][0]["smtp"]["port"] = 70000.into())).contains("json")
        );
        assert!(errors(&with(|v| v["accounts"][0]["imap"]["port"] = 0.into())).contains("port"));
    }

    #[test]
    fn kind_needs_its_section() {
        assert!(errors(&with(|v| v["accounts"][0]["smtp"] = Value::Null)).contains("needs smtp"));
        assert!(errors(&with(|v| v["accounts"][1]["gmail"] = Value::Null)).contains("needs gmail"));
        assert!(
            errors(&with(|v| v["accounts"][0]["imap"] = Value::Null))
                .contains("save_sent_copy needs imap")
        );
    }

    #[test]
    fn gmail_client_id_rules() {
        for bad in ["", "   ", "x .apps.googleusercontent.com", "x\n"] {
            assert!(
                errors(&with(
                    |v| v["accounts"][1]["gmail"]["client_id"] = bad.into()
                ))
                .contains("client_id"),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn command_rules() {
        assert!(
            errors(&with(|v| v["accounts"][2]["send_command"] = Value::Null))
                .contains("send_command")
        );
        assert!(
            errors(&with(|v| v["accounts"][2]["send_command"] = json!([])))
                .contains("send_command")
        );
        assert!(
            errors(&with(
                |v| v["accounts"][2]["send_command"] = json!(["my-send"])
            ))
            .contains("absolute")
        );
        assert!(
            errors(&with(
                |v| v["accounts"][2]["discard_command"] = json!(["rel"])
            ))
            .contains("absolute")
        );
        assert!(
            errors(&with(|v| v["check_command"]["argv"] = json!(["check"]))).contains("absolute")
        );
        assert!(
            errors(&with(|v| v["check_command"]["argv"] = json!([]))).contains("check_command")
        );
        assert!(errors(&with(|v| v["check_command"]["timeout_s"] = 0.into())).contains("timeout"));
    }

    #[test]
    fn timeout_out_of_range_invalid() {
        for t in [0, 29, 901] {
            assert!(
                errors(&with(|v| v["accounts"][2]["timeout_s"] = t.into())).contains("timeout_s"),
                "{t}"
            );
        }
        for t in [30, 300, 900] {
            assert!(
                errors(&with(|v| v["accounts"][2]["timeout_s"] = t.into())).is_empty(),
                "{t}"
            );
        }
        assert!(
            errors(&with(|v| v["accounts"][0]["timeout_s"] = 300.into()))
                .contains("only for kind command")
        );
        let cfg = Config::parse(&with(|v| v["accounts"][2]["timeout_s"] = 300.into())).unwrap();
        assert_eq!(cfg.accounts[2].send_timeout(), Duration::from_secs(300));
        let cfg = Config::parse(GOOD).unwrap();
        assert_eq!(cfg.accounts[2].timeout_s, None);
        assert_eq!(cfg.accounts[2].send_timeout(), Duration::from_secs(180));
        assert!(
            !serde_json::to_string(&cfg)
                .unwrap()
                .contains("timeout_s\":null"),
            "an unset limit stays out of config.json"
        );
    }

    #[test]
    fn all_errors_collected() {
        let json = with(|v| {
            v["accounts"][0]["name"] = "BAD".into();
            v["accounts"][2]["send_command"] = json!([]);
        });
        let e = errors(&json);
        assert!(
            e.contains("account name") && e.contains("send_command"),
            "{e}"
        );
    }
}
