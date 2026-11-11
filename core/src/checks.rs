//! The check command (findings for the window) and the fix command (a new body for the editor, never saved).
//!
//! Protocol: stdin is the item view as JSON (no secrets); the check prints
//! `{"verdict":"pass|warn|block","findings":[{"level","title","detail","quote","confirm"}]}`, the fix
//! prints `{"body":"..."}`. Empty or broken output, a non-zero exit or the time limit are errors, never a pass.

use crate::config::CommandCfg;
use crate::item::{
    CheckSource, Checks, Finding, Item, Level, MAX_BODY_CHARS, Verdict, content_digest,
};
use crate::process;
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckOutput {
    verdict: Verdict,
    findings: Vec<Finding>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FixOutput {
    body: String,
}

#[derive(Serialize)]
struct FixInput<'a> {
    item: Value,
    findings: &'a [Finding],
}

/// What a check or fix command sees of an item: no history, no send state, no secrets.
pub fn item_view(item: &Item, thread: Option<&str>) -> Value {
    json!({
        "account": item.account,
        "from": item.from,
        "to": item.to,
        "cc": item.cc,
        "bcc": item.bcc,
        "subject": item.subject,
        "body": item.body,
        "body_sha256": item.body_sha256,
        "reply": item.reply,
        "thread": thread,
        "attachments": item.attachments.iter().map(|a| json!({"name": a.name, "size": a.size})).collect::<Vec<_>>(),
    })
}

/// Runs the command; `what` names the error (`check` -> `check_failed` / `check_timeout`).
fn call(cfg: &CommandCfg, item: &Item, stdin: Vec<u8>, what: &str) -> Result<Vec<u8>> {
    let env = [("DRAFTDESK_ITEM_ID", item.item_id.clone())];
    let out = process::run(&cfg.argv, &env, stdin, Duration::from_secs(cfg.timeout_s))
        .map_err(|e| Error(format!("{what}_failed: {e}")))?;
    if out.timed_out {
        return Err(Error(format!(
            "{what}_timeout: no answer within {} s",
            cfg.timeout_s
        )));
    }
    if out.code != Some(0) {
        let code = out
            .code
            .map_or("a signal".to_string(), |c| format!("exit {c}"));
        return Err(Error(format!("{what}_failed: ended with {code}")));
    }
    if out.overflow || out.stdout.iter().all(u8::is_ascii_whitespace) {
        return Err(Error(format!("{what}_failed: empty or oversized output")));
    }
    Ok(out.stdout)
}

pub fn run_check(cfg: &CommandCfg, item: &Item, thread: Option<&str>) -> Result<Checks> {
    let stdin = serde_json::to_vec(&item_view(item, thread))?;
    let out = call(cfg, item, stdin, "check")?;
    let parsed: CheckOutput = serde_json::from_slice(&out)
        .map_err(|e| Error(format!("check_failed: not the check format: {e}")))?;
    Ok(Checks {
        body_sha256: item.body_sha256.clone(),
        verdict: parsed.verdict,
        findings: parsed.findings.into_iter().map(Finding::cleaned).collect(),
        ran_at: Some(crate::now_utc()),
        source: Some(CheckSource::Command),
        // Of exactly the snapshot the command judged, not of the item when the findings are stored.
        content_digest: Some(content_digest(item)),
    })
}

/// The new body for the editor. The caller never stores or sends it without the person.
pub fn run_fix(cfg: &CommandCfg, item: &Item, findings: &[Finding]) -> Result<String> {
    let stdin = serde_json::to_vec(&FixInput {
        item: item_view(item, None),
        findings,
    })?;
    let out = call(cfg, item, stdin, "fix")?;
    let parsed: FixOutput = serde_json::from_slice(&out)
        .map_err(|e| Error(format!("fix_failed: not the fix format: {e}")))?;
    if parsed.body.trim().is_empty() || parsed.body.chars().count() > MAX_BODY_CHARS {
        return Err(Error(
            "fix_failed: the new body is empty or too long".into(),
        ));
    }
    Ok(parsed.body)
}

/// Made for another body than the current one, or for other content (recipients, subject, attachments).
pub fn is_stale(checks: &Checks, item: &Item) -> bool {
    checks.body_sha256 != item.body_sha256
        || checks
            .content_digest
            .as_ref()
            .is_some_and(|d| *d != content_digest(item))
}

/// Sending needs an explicit confirmation: a blocking verdict or finding, or one marked `confirm`, even when stale.
pub fn needs_confirmation(checks: Option<&Checks>) -> bool {
    checks.is_some_and(|c| {
        c.verdict == Verdict::Block
            || c.findings
                .iter()
                .any(|f| f.level == Level::Block || f.confirm)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::tests::item;
    use crate::sha256_hex;

    fn script(name: &str, timeout_s: u64) -> CommandCfg {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/bin")
            .join(name);
        CommandCfg {
            argv: vec![p.to_string_lossy().into_owned()],
            timeout_s,
        }
    }

    #[test]
    fn pass_output_parsed() {
        let it = item();
        let c = run_check(&script("check-warn.sh", 10), &it, Some("earlier mail")).unwrap();
        assert_eq!(c.verdict, Verdict::Warn);
        assert_eq!(c.body_sha256, it.body_sha256);
        assert_eq!(c.source, Some(CheckSource::Command));
        assert!(c.ran_at.is_some());
        assert_eq!(c.findings.len(), 1);
        assert_eq!(c.findings[0].title, "subject Re: Offer");
        assert!(c.findings[0].confirm);
    }

    #[test]
    fn findings_lose_control_and_bidi_characters() {
        // The window shows these texts, also in the question before sending: nothing may reorder or hide them.
        let c = run_check(&script("check-bidi.sh", 10), &item(), None).unwrap();
        let f = &c.findings[0];
        assert_eq!(
            (f.title.as_str(), f.detail.as_str(), f.quote.as_str()),
            ("fineevil", "ab\nnext", "xy")
        );
    }

    #[test]
    fn check_sees_item_view_and_env() {
        let c = run_check(&script("check-echo.sh", 10), &item(), Some("earlier mail")).unwrap();
        let seen: Value = serde_json::from_str(&c.findings[0].detail).unwrap();
        assert_eq!(seen["subject"], "Re: Offer");
        assert_eq!(seen["thread"], "earlier mail");
        assert!(seen.get("send").is_none() && seen.get("history").is_none());
        assert_eq!(c.findings[0].title, item().item_id, "DRAFTDESK_ITEM_ID");
    }

    fn err(name: &str, timeout_s: u64) -> String {
        run_check(&script(name, timeout_s), &item(), None)
            .unwrap_err()
            .0
    }

    #[test]
    fn empty_output_is_error() {
        assert!(err("empty.sh", 10).contains("check_failed"));
    }

    #[test]
    fn garbage_is_error() {
        assert!(err("garbage.sh", 10).contains("check_failed"));
    }

    #[test]
    fn unknown_field_is_error() {
        assert!(err("check-unknown-field.sh", 10).contains("check_failed"));
    }

    #[test]
    fn nonzero_exit_is_error() {
        let e = err("check-exit3.sh", 10);
        assert!(e.contains("check_failed") && e.contains("exit 3"), "{e}");
    }

    #[test]
    fn timeout_kills_child() {
        let t = std::time::Instant::now();
        assert!(err("sleep.sh", 1).contains("check_timeout"));
        assert!(t.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn large_stdin_no_deadlock() {
        let mut it = item();
        it.body = "x".repeat(1024 * 1024);
        it.body_sha256 = sha256_hex(it.body.as_bytes());
        let c = run_check(&script("check-count.sh", 30), &it, None).unwrap();
        let n: usize = c.findings[0].title.trim().parse().unwrap();
        assert!(n > 1024 * 1024, "{n}");
    }

    #[test]
    fn stale_after_body_change() {
        let mut it = item();
        let c = run_check(&script("check-warn.sh", 10), &it, None).unwrap();
        assert!(!is_stale(&c, &it));
        it.body.push('!');
        it.body_sha256 = sha256_hex(it.body.as_bytes());
        assert!(is_stale(&c, &it));
    }

    #[test]
    fn stale_after_a_change_of_what_the_check_judged() {
        let it = item();
        let c = run_check(&script("check-warn.sh", 10), &it, None).unwrap();
        let changes: Vec<fn(&mut Item)> = vec![
            |i| i.to.push("bob@example.org".into()),
            |i| i.cc.push("bob@example.org".into()),
            |i| i.subject.push('!'),
            |i| {
                i.attachments.push(crate::item::Attachment {
                    name: "a.pdf".into(),
                    size: 1,
                    sha256: "a".repeat(64),
                })
            },
        ];
        for (k, f) in changes.into_iter().enumerate() {
            let mut other = it.clone();
            f(&mut other);
            assert!(is_stale(&c, &other), "change {k}");
        }
        // Checks stored before they were bound to the whole content stay bound to the body only.
        let mut old = serde_json::to_value(&c).unwrap();
        old.as_object_mut().unwrap().remove("content_digest");
        let old: Checks = serde_json::from_value(old).unwrap();
        let mut other = it.clone();
        other.subject.push('!');
        assert!(!is_stale(&old, &other));
    }

    #[test]
    fn confirm_or_block_needs_confirmation() {
        let f = |level, confirm| Finding {
            level,
            title: "t".into(),
            detail: String::new(),
            quote: String::new(),
            confirm,
        };
        let c = |verdict, findings| Checks {
            body_sha256: "a".repeat(64),
            verdict,
            findings,
            ran_at: None,
            source: None,
            content_digest: None,
        };
        assert!(!needs_confirmation(None));
        assert!(!needs_confirmation(Some(&c(Verdict::Pass, vec![]))));
        assert!(!needs_confirmation(Some(&c(
            Verdict::Warn,
            vec![f(Level::Warn, false), f(Level::Info, false)]
        ))));
        assert!(needs_confirmation(Some(&c(
            Verdict::Warn,
            vec![f(Level::Warn, true)]
        ))));
        assert!(needs_confirmation(Some(&c(
            Verdict::Warn,
            vec![f(Level::Block, false)]
        ))));
        assert!(needs_confirmation(Some(&c(Verdict::Block, vec![]))));
    }

    #[test]
    fn fix_returns_body_only() {
        let it = item();
        let findings = run_check(&script("check-warn.sh", 10), &it, None)
            .unwrap()
            .findings;
        let body = run_fix(&script("fix.sh", 10), &it, &findings).unwrap();
        assert_eq!(body, "Fixed body for: subject Re: Offer");
        assert!(
            run_fix(&script("check-warn.sh", 10), &it, &findings)
                .unwrap_err()
                .0
                .contains("fix_failed")
        );
        assert!(
            run_fix(&script("empty.sh", 10), &it, &findings)
                .unwrap_err()
                .0
                .contains("fix_failed")
        );
    }
}
