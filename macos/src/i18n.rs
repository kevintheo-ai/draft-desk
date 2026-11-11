//! Interface language: German or English, from the Mac's language, chosen once at start. A German Draft
//! Desk on an English Mac: System Settings > General > Language & Region > Applications. Also the texts for what the
//! core reports (error codes, notes after a restart, the outcome of a send).
//!
//! In the app the choice is one global flag; under `cargo test` it is kept per test thread (as in Lookout), so tests
//! that switch the language cannot race each other.

use draftdesk_core::desk::Note;
use draftdesk_core::item::State;
use draftdesk_core::sender::FireResult;
use objc2_foundation::NSLocale;

/// Reads the first preferred language of this app (macOS applies the per-app choice here).
pub fn set_from_system() {
    let first = NSLocale::preferredLanguages()
        .firstObject()
        .map(|s| s.to_string())
        .unwrap_or_default();
    set_german(first.starts_with("de"));
}

pub fn set_german(german: bool) {
    flag::set(german);
}

pub fn is_german() -> bool {
    flag::get()
}

/// A subject, name or passage in quotation marks: „…“ in German, “…” in English.
pub fn quoted(s: &str) -> String {
    if is_german() {
        format!("„{s}“")
    } else {
        format!("“{s}”")
    }
}

/// Interface text in its language.
pub fn tr<'a>(de: &'a str, en: &'a str) -> &'a str {
    if is_german() { de } else { en }
}

/// Core error prefixes and what to tell the person; `true` keeps the core's detail in brackets.
const ERRORS: &[(&str, &str, &str, bool)] = &[
    (
        "encoding_blocked",
        "Unsichtbare oder Steuerzeichen im Text oder Betreff: bitte zuerst entfernen.",
        "Invisible or control characters in the text or subject: remove them first.",
        false,
    ),
    (
        "needs_confirmation",
        "Offene Befunde müssen vor dem Senden bestätigt werden.",
        "Open findings must be confirmed before sending.",
        false,
    ),
    (
        "content_changed",
        "Der Entwurf wurde inzwischen geändert. Er ist neu geladen, bitte noch einmal prüfen.",
        "The draft changed meanwhile. It is reloaded; please look again.",
        false,
    ),
    (
        "not scheduled",
        "Nicht geplant: der Entwurf hat sich seit der Anzeige geändert.",
        "Not scheduled: the draft changed since it was shown.",
        false,
    ),
    (
        "a send was already attempted",
        "Für diesen Entwurf gab es schon einen Sendeversuch. Er wird nie automatisch wiederholt.",
        "This draft was already sent once or tried. It is never sent again automatically.",
        false,
    ),
    (
        "not ready",
        "Dieser Entwurf kann gerade nicht geändert oder gesendet werden.",
        "This draft cannot be changed or sent right now.",
        false,
    ),
    (
        "state conflict",
        "Dieser Entwurf kann gerade nicht geändert oder gesendet werden.",
        "This draft cannot be changed or sent right now.",
        false,
    ),
    (
        "busy",
        "Für diesen Entwurf läuft gerade eine Prüfung oder Korrektur.",
        "A check or fix runs for this draft.",
        false,
    ),
    (
        "check_timeout",
        "Der Prüfbefehl hat nicht rechtzeitig geantwortet.",
        "The check command did not answer in time.",
        true,
    ),
    (
        "check_failed",
        "Die Prüfung ist fehlgeschlagen.",
        "The check failed.",
        true,
    ),
    (
        "fix_timeout",
        "Der Korrekturbefehl hat nicht rechtzeitig geantwortet.",
        "The fix command did not answer in time.",
        true,
    ),
    (
        "fix_failed",
        "Die Korrektur ist fehlgeschlagen.",
        "The fix failed.",
        true,
    ),
    (
        "no_check_command",
        "Kein Prüfbefehl eingerichtet (Einstellungen).",
        "No check command set up (Settings).",
        false,
    ),
    (
        "no_fix_command",
        "Kein Korrekturbefehl eingerichtet (Einstellungen).",
        "No fix command set up (Settings).",
        false,
    ),
    (
        "no findings to fix",
        "Es gibt keine Befunde zum Korrigieren.",
        "There are no findings to fix.",
        false,
    ),
    // What saving a typed change refuses most often.
    (
        "to[",
        "Eine Empfängeradresse ist ungültig.",
        "A recipient address is invalid.",
        true,
    ),
    (
        "cc[",
        "Eine Empfängeradresse ist ungültig.",
        "A recipient address is invalid.",
        true,
    ),
    (
        "bcc[",
        "Eine Empfängeradresse ist ungültig.",
        "A recipient address is invalid.",
        true,
    ),
    (
        "to: empty",
        "„An“ braucht mindestens einen Empfänger.",
        "“To” needs at least one recipient.",
        false,
    ),
    (
        "subject: empty",
        "Der Betreff fehlt.",
        "The subject is missing.",
        false,
    ),
    (
        "subject:",
        "Der Betreff ist ungültig.",
        "The subject is invalid.",
        true,
    ),
];

/// A core error for the person: a sentence for the known codes, the core's text otherwise.
pub fn error_text(err: &str) -> String {
    for (prefix, de, en, detail) in ERRORS {
        if err.starts_with(prefix) {
            return if *detail {
                format!("{} ({err})", tr(de, en))
            } else {
                tr(de, en).to_string()
            };
        }
    }
    format!("{} {err}", tr("Fehler:", "Error:"))
}

/// A note of the start-up repair or the poll; `label` names the draft (its subject, else its id).
pub fn note_text(n: &Note, label: &str) -> String {
    match n {
        Note::Unscheduled(_) => format!(
            "{}: {}",
            quoted(label),
            tr(
                "Der geplante Versand wurde beim Neustart abgebrochen. Nichts wurde gesendet.",
                "The scheduled send was cancelled by the restart. Nothing was sent."
            )
        ),
        Note::Interrupted(_) => format!(
            "{}: {}",
            quoted(label),
            tr(
                "Die App wurde während des Versands beendet. Unklar, ob gesendet: bitte im Gesendet-Ordner nachsehen.",
                "The app stopped while sending. It may or may not have gone out: please check the sent folder."
            )
        ),
        Note::Finalized(_) => format!(
            "{}: {}",
            quoted(label),
            tr(
                "Wurde gesendet; der Abschluss ist nachgeholt.",
                "Was sent; the follow-up steps are done now."
            )
        ),
        Note::CleanedUp(_) => format!(
            "{}: {}",
            quoted(label),
            tr(
                "Postfach-Entwurf nach dem Verwerfen aufgeräumt.",
                "Mailbox draft cleaned up after the discard."
            )
        ),
        Note::ConfigInvalid(detail) => format!(
            "{} ({detail})",
            tr(
                "config.json ist ungültig; die bisherige Einstellung gilt weiter.",
                "config.json is invalid; the previous settings stay."
            )
        ),
        Note::NoSigningKey(detail) => format!(
            "{} ({detail})",
            tr(
                "Kein Signierschlüssel im Schlüsselbund: Ergebnisse erreichen die Agenten nicht.",
                "No signing key in the keychain: results do not reach the agents."
            )
        ),
        Note::Problem(_, detail) if label.is_empty() => {
            format!("{} {detail}", tr("Problem:", "Problem:"))
        }
        Note::Problem(_, detail) => format!("{}: {detail}", quoted(label)),
    }
}

/// The outcome of a send. `None`: nothing to tell (an old countdown after an undo).
pub fn fire_text(r: &FireResult, rcpts: usize) -> Option<String> {
    Some(match r {
        FireResult::Sent => {
            if is_german() {
                format!("Gesendet an {rcpts} Empfänger.")
            } else if rcpts == 1 {
                "Sent to 1 recipient.".to_string()
            } else {
                format!("Sent to {rcpts} recipients.")
            }
        }
        FireResult::NotSent(reason) if reason.contains("no approval") => return None,
        FireResult::NotSent(reason) if reason.contains("expired") => tr(
            "Der Countdown kam zu spät an (Ruhezustand?). Nichts wurde gesendet; bitte erneut auf Senden klicken.",
            "The countdown ended too late (sleep?). Nothing was sent; please click Send again.",
        )
        .to_string(),
        FireResult::NotSent(reason) => format!("{} ({reason})", tr("Nicht gesendet.", "Not sent.")),
        FireResult::Failed(code) => format!(
            "{} ({code})",
            tr("Nicht gesendet: Fehler vor dem Versand.", "Not sent: an error before sending.")
        ),
        FireResult::Rejected(code) => format!(
            "{} ({code})",
            tr(
                "Vom Anbieter abgelehnt. Nichts wurde gesendet.",
                "Refused by the provider. Nothing was sent."
            )
        ),
        FireResult::Unknown(reason) => format!(
            "{} ({reason})",
            tr(
                "Unklar, ob gesendet. Bitte im Gesendet-Ordner nachsehen; es wird nie automatisch wiederholt.",
                "It may or may not have gone out. Please check the sent folder; it is never resent automatically."
            )
        ),
    })
}

pub fn state_text(s: State) -> &'static str {
    match s {
        State::Ready => tr("Bereit", "Ready"),
        State::SendingScheduled => tr("Versand geplant", "Send scheduled"),
        State::Sending => tr("Wird gesendet", "Sending"),
        State::Sent => tr("Gesendet", "Sent"),
        State::Failed => tr("Fehlgeschlagen", "Failed"),
        State::Discarded => tr("Verworfen", "Discarded"),
    }
}

#[cfg(not(test))]
mod flag {
    use std::sync::atomic::{AtomicBool, Ordering};

    static GERMAN: AtomicBool = AtomicBool::new(false);

    pub fn set(german: bool) {
        GERMAN.store(german, Ordering::Relaxed);
    }

    pub fn get() -> bool {
        GERMAN.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod flag {
    use std::cell::Cell;

    thread_local! {
        static GERMAN: Cell<bool> = const { Cell::new(true) };
    }

    pub fn set(german: bool) {
        GERMAN.with(|g| g.set(german));
    }

    pub fn get() -> bool {
        GERMAN.with(Cell::get)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_codes_translate_both_languages() {
        set_german(true);
        assert_eq!(
            error_text("encoding_blocked: remove the invisible or control characters first"),
            "Unsichtbare oder Steuerzeichen im Text oder Betreff: bitte zuerst entfernen."
        );
        assert_eq!(
            error_text("check_failed: ended with exit 3"),
            "Die Prüfung ist fehlgeschlagen. (check_failed: ended with exit 3)"
        );
        set_german(false);
        assert_eq!(
            error_text("busy: a check or fix runs for this draft"),
            "A check or fix runs for this draft."
        );
        assert!(error_text("content_changed: x").starts_with("The draft changed"));
    }

    #[test]
    fn unknown_code_keeps_original() {
        set_german(true);
        assert_eq!(error_text("from.name: x"), "Fehler: from.name: x");
        set_german(false);
        assert_eq!(error_text("weird"), "Error: weird");
    }

    #[test]
    fn typing_mistakes_have_sentences() {
        set_german(true);
        assert_eq!(
            error_text("cc[1]: no address"),
            "Eine Empfängeradresse ist ungültig. (cc[1]: no address)"
        );
        assert_eq!(error_text("subject: empty"), "Der Betreff fehlt.");
        assert!(
            error_text("subject: contains control/bidi characters")
                .starts_with("Der Betreff ist ungültig.")
        );
        set_german(false);
        assert_eq!(
            error_text("to: empty"),
            "“To” needs at least one recipient."
        );
    }

    #[test]
    fn fire_texts() {
        set_german(false);
        assert_eq!(
            fire_text(&FireResult::Sent, 1).unwrap(),
            "Sent to 1 recipient."
        );
        assert_eq!(
            fire_text(&FireResult::Sent, 3).unwrap(),
            "Sent to 3 recipients."
        );
        assert_eq!(
            fire_text(
                &FireResult::NotSent("no approval in this process".into()),
                1
            ),
            None
        );
        assert!(
            fire_text(&FireResult::NotSent("the approval expired".into()), 1)
                .unwrap()
                .contains("click Send again")
        );
        assert!(
            fire_text(&FireResult::Unknown("drop".into()), 1)
                .unwrap()
                .contains("never resent")
        );
        set_german(true);
        assert_eq!(
            fire_text(&FireResult::Sent, 2).unwrap(),
            "Gesendet an 2 Empfänger."
        );
        assert!(
            fire_text(&FireResult::Rejected("smtp_5xx".into()), 1)
                .unwrap()
                .ends_with("(smtp_5xx)")
        );
    }

    #[test]
    fn note_texts() {
        set_german(true);
        let n = Note::Unscheduled("20261107-120000-00000001".into());
        assert!(note_text(&n, "Re: Angebot").starts_with("„Re: Angebot“: Der geplante Versand"));
        assert_eq!(
            note_text(&Note::Problem(String::new(), "disk full".into()), ""),
            "Problem: disk full"
        );
        set_german(false);
        assert!(note_text(&Note::ConfigInvalid("json".into()), "").ends_with("(json)"));
        assert_eq!(state_text(State::SendingScheduled), "Send scheduled");
        assert!(note_text(&n, "Re: Offer").starts_with("“Re: Offer”: The scheduled send"));
    }

    #[test]
    fn quotes_follow_the_language() {
        set_german(true);
        assert_eq!(quoted("for Friday"), "„for Friday“");
        set_german(false);
        assert_eq!(quoted("for Friday"), "“for Friday”");
    }
}
