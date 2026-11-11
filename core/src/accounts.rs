//! Setting up accounts and the check/fix commands, shared by the command line and the app. The keychain changes
//! first, the configuration last: a refused keychain (the person clicks "Deny") leaves the configuration as it was,
//! so the step can simply be repeated. A secret never goes into a file.

use crate::config::{AccountCfg, CommandCfg, Config, Kind};
use crate::gmail::oauth;
use crate::secrets::Secrets;
use crate::{Result, bail};
use std::path::Path;

/// What an account keeps in the keychain.
pub enum AccountSecret<'a> {
    None,
    /// SMTP, and IMAP with the same password when the account has IMAP.
    Smtp {
        password: &'a str,
    },
    Gmail {
        client_secret: &'a str,
        refresh_token: &'a str,
    },
}

/// The command a slot of the configuration holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandSlot {
    Check,
    Fix,
}

/// The account on its own, before any secret is asked for.
pub fn validate(acc: &AccountCfg) -> Result<()> {
    let errors = Config {
        accounts: vec![acc.clone()],
        ..Default::default()
    }
    .validate();
    if errors.is_empty() {
        Ok(())
    } else {
        bail!("{}", errors.join("; "))
    }
}

/// Before any secret is asked for or a Google sign-in starts: the account on its own, and its name still free.
pub fn check_new(root: &Path, acc: &AccountCfg) -> Result<()> {
    validate(acc)?;
    if Config::load(root)?.account(&acc.name).is_some() {
        bail!("account {} exists; remove it first", acc.name);
    }
    Ok(())
}

/// Stores the secrets, then adds the account (validated, never over an existing name).
// ponytail: a secret stored before a later failure stays in the keychain; the next add of the name overwrites it and
// `remove` deletes it.
pub fn add(
    root: &Path,
    secrets: &dyn Secrets,
    acc: AccountCfg,
    secret: AccountSecret,
) -> Result<()> {
    validate(&acc)?;
    let pairs: Vec<(String, &str)> = match (acc.kind, &secret) {
        (Kind::Smtp, AccountSecret::Smtp { password }) => {
            let mut v = vec![(format!("{}:smtp", acc.name), *password)];
            if acc.imap.is_some() {
                v.push((format!("{}:imap", acc.name), *password));
            }
            v
        }
        (
            Kind::Gmail,
            AccountSecret::Gmail {
                client_secret,
                refresh_token,
            },
        ) => vec![
            (oauth::secret_key(&acc.name), *client_secret),
            (oauth::refresh_key(&acc.name), *refresh_token),
        ],
        (Kind::Command, AccountSecret::None) => vec![],
        _ => bail!("{}: the secret does not fit the account kind", acc.name),
    };
    if pairs.iter().any(|(_, v)| v.is_empty()) {
        bail!("{}: empty secret", acc.name);
    }
    let mut cfg = Config::load(root)?;
    if cfg.account(&acc.name).is_some() {
        bail!("account {} exists; remove it first", acc.name);
    }
    for (key, value) in pairs {
        secrets.set(&key, value)?;
    }
    cfg.accounts.push(acc);
    cfg.save(root)
}

/// Deletes every secret an account of that name could have, then removes the account. `false`: no such account.
pub fn remove(root: &Path, secrets: &dyn Secrets, name: &str) -> Result<bool> {
    let mut cfg = Config::load(root)?;
    let before = cfg.accounts.len();
    cfg.accounts.retain(|a| a.name != name);
    if cfg.accounts.len() == before {
        return Ok(false);
    }
    for key in [
        format!("{name}:smtp"),
        format!("{name}:imap"),
        oauth::secret_key(name),
        oauth::refresh_key(name),
    ] {
        secrets.delete(&key)?;
    }
    cfg.save(root)?;
    Ok(true)
}

/// Sets or clears the check or fix command.
pub fn set_command(root: &Path, slot: CommandSlot, cmd: Option<CommandCfg>) -> Result<()> {
    let mut cfg = Config::load(root)?;
    match slot {
        CommandSlot::Check => cfg.check_command = cmd,
        CommandSlot::Fix => cfg.fix_command = cmd,
    }
    cfg.save(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{GmailCfg, ImapCfg, Security, SenderCfg, SmtpCfg};
    use crate::secrets::MemorySecrets;
    use crate::store::tests::temp_root;

    fn account(name: &str, kind: Kind) -> AccountCfg {
        AccountCfg {
            name: name.into(),
            kind,
            senders: vec![SenderCfg {
                address: "me@example.org".into(),
                name: Some("Me".into()),
                fixed_name: false,
            }],
            smtp: None,
            imap: None,
            gmail: None,
            send_command: None,
            discard_command: None,
            timeout_s: None,
            save_sent_copy: false,
        }
    }

    fn smtp(name: &str, with_imap: bool) -> AccountCfg {
        let mut a = account(name, Kind::Smtp);
        a.smtp = Some(SmtpCfg {
            host: "smtp.example.org".into(),
            port: 465,
            security: Security::Tls,
            username: "me@example.org".into(),
        });
        if with_imap {
            a.imap = Some(ImapCfg {
                host: "imap.example.org".into(),
                port: 993,
                username: "me@example.org".into(),
            });
        }
        a
    }

    fn command(name: &str, program: &str) -> AccountCfg {
        let mut a = account(name, Kind::Command);
        a.send_command = Some(vec![program.into()]);
        a
    }

    fn names(root: &Path) -> Vec<String> {
        Config::load(root)
            .unwrap()
            .accounts
            .into_iter()
            .map(|a| a.name)
            .collect()
    }

    #[test]
    fn add_smtp_writes_config_and_both_passwords() {
        let r = temp_root("acc1");
        let s = MemorySecrets::default();
        add(
            &r.0,
            &s,
            smtp("mail", true),
            AccountSecret::Smtp { password: "pw" },
        )
        .unwrap();
        assert_eq!(names(&r.0), vec!["mail"]);
        assert_eq!(s.get("mail:smtp").unwrap().as_deref(), Some("pw"));
        assert_eq!(s.get("mail:imap").unwrap().as_deref(), Some("pw"));
        let file = std::fs::read_to_string(r.0.join("config.json")).unwrap();
        assert!(!file.contains("\"pw\""), "no secret in the configuration");
        let r2 = temp_root("acc1b");
        add(
            &r2.0,
            &s,
            smtp("solo", false),
            AccountSecret::Smtp { password: "x" },
        )
        .unwrap();
        assert_eq!(s.get("solo:imap").unwrap(), None);
    }

    #[test]
    fn add_gmail_writes_secret_and_refresh() {
        let r = temp_root("acc2");
        let s = MemorySecrets::default();
        let mut a = account("g", Kind::Gmail);
        a.gmail = Some(GmailCfg {
            client_id: "id.apps.googleusercontent.com".into(),
        });
        let secret = AccountSecret::Gmail {
            client_secret: "cs",
            refresh_token: "rt",
        };
        add(&r.0, &s, a, secret).unwrap();
        assert_eq!(
            s.get(&oauth::secret_key("g")).unwrap().as_deref(),
            Some("cs")
        );
        assert_eq!(
            s.get(&oauth::refresh_key("g")).unwrap().as_deref(),
            Some("rt")
        );
    }

    #[test]
    fn add_existing_name_writes_nothing() {
        let r = temp_root("acc3");
        let s = MemorySecrets::default();
        add(
            &r.0,
            &s,
            command("ops", "/usr/bin/true"),
            AccountSecret::None,
        )
        .unwrap();
        let err = add(
            &r.0,
            &s,
            smtp("ops", false),
            AccountSecret::Smtp { password: "pw" },
        );
        assert!(err.unwrap_err().0.contains("exists"));
        assert_eq!(s.get("ops:smtp").unwrap(), None);
        assert_eq!(Config::load(&r.0).unwrap().accounts[0].kind, Kind::Command);
    }

    #[test]
    fn add_invalid_account_writes_nothing() {
        let r = temp_root("acc4");
        let s = MemorySecrets::default();
        assert!(add(&r.0, &s, command("rel", "relative"), AccountSecret::None).is_err());
        assert!(
            add(
                &r.0,
                &s,
                smtp("Bad Name", false),
                AccountSecret::Smtp { password: "pw" }
            )
            .is_err()
        );
        assert!(!r.0.join("config.json").exists());
        assert_eq!(s.get("Bad Name:smtp").unwrap(), None);
    }

    #[test]
    fn secret_kind_mismatch_or_empty_writes_nothing() {
        let r = temp_root("acc5");
        let s = MemorySecrets::default();
        assert!(add(&r.0, &s, smtp("m", false), AccountSecret::None).is_err());
        assert!(
            add(
                &r.0,
                &s,
                command("c", "/usr/bin/true"),
                AccountSecret::Smtp { password: "pw" }
            )
            .is_err()
        );
        assert!(
            add(
                &r.0,
                &s,
                smtp("m", false),
                AccountSecret::Smtp { password: "" }
            )
            .is_err()
        );
        assert!(!r.0.join("config.json").exists());
        assert_eq!(s.get("c:smtp").unwrap(), None);
    }

    #[test]
    fn remove_deletes_entry_and_keys() {
        let r = temp_root("acc6");
        let s = MemorySecrets::default();
        add(
            &r.0,
            &s,
            smtp("mail", true),
            AccountSecret::Smtp { password: "pw" },
        )
        .unwrap();
        add(
            &r.0,
            &s,
            command("ops", "/usr/bin/true"),
            AccountSecret::None,
        )
        .unwrap();
        assert!(remove(&r.0, &s, "mail").unwrap());
        assert_eq!(names(&r.0), vec!["ops"]);
        assert_eq!(s.get("mail:smtp").unwrap(), None);
        assert_eq!(s.get("mail:imap").unwrap(), None);
    }

    #[test]
    fn remove_unknown_is_false() {
        let r = temp_root("acc7");
        let s = MemorySecrets::default();
        assert!(!remove(&r.0, &s, "nobody").unwrap());
    }

    /// A keychain that refuses every change (the person clicked "Deny").
    struct Refusing;

    impl Secrets for Refusing {
        fn get(&self, _key: &str) -> Result<Option<String>> {
            Ok(None)
        }
        fn set(&self, _key: &str, _value: &str) -> Result<()> {
            bail!("denied")
        }
        fn delete(&self, _key: &str) -> Result<()> {
            bail!("denied")
        }
    }

    #[test]
    fn refused_keychain_leaves_the_configuration_as_it_was() {
        let r = temp_root("acc9");
        // Adding: no account without its password.
        let pw = AccountSecret::Smtp { password: "pw" };
        assert!(add(&r.0, &Refusing, smtp("mail", true), pw).is_err());
        assert!(!r.0.join("config.json").exists());
        // Removing: the account stays until its secrets are gone, so removing it again works.
        let s = MemorySecrets::default();
        let pw = AccountSecret::Smtp { password: "pw" };
        add(&r.0, &s, smtp("mail", true), pw).unwrap();
        assert!(remove(&r.0, &Refusing, "mail").is_err());
        assert_eq!(names(&r.0), vec!["mail"]);
        assert!(remove(&r.0, &s, "mail").unwrap());
        assert!(names(&r.0).is_empty());
        assert_eq!(s.get("mail:smtp").unwrap(), None);
    }

    #[test]
    fn check_new_refuses_a_taken_name_before_any_secret() {
        let r = temp_root("acc10");
        let s = MemorySecrets::default();
        check_new(&r.0, &command("ops", "/usr/bin/true")).unwrap();
        add(
            &r.0,
            &s,
            command("ops", "/usr/bin/true"),
            AccountSecret::None,
        )
        .unwrap();
        let err = check_new(&r.0, &smtp("ops", false)).unwrap_err();
        assert!(err.0.contains("exists"), "{}", err.0);
        assert!(check_new(&r.0, &command("rel", "relative")).is_err());
    }

    #[test]
    fn set_command_round_trip_and_clear() {
        let r = temp_root("acc8");
        let cmd = CommandCfg {
            argv: vec!["/usr/bin/true".into()],
            timeout_s: 60,
        };
        set_command(&r.0, CommandSlot::Check, Some(cmd.clone())).unwrap();
        set_command(&r.0, CommandSlot::Fix, Some(cmd.clone())).unwrap();
        let cfg = Config::load(&r.0).unwrap();
        assert_eq!(cfg.check_command, Some(cmd.clone()));
        assert_eq!(cfg.fix_command, Some(cmd));
        set_command(&r.0, CommandSlot::Check, None).unwrap();
        assert_eq!(Config::load(&r.0).unwrap().check_command, None);
        let bad = CommandCfg {
            argv: vec!["relative".into()],
            timeout_s: 60,
        };
        assert!(set_command(&r.0, CommandSlot::Fix, Some(bad)).is_err());
    }
}
