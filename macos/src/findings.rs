//! The findings above the text: a header (verdict, time, source, stale) and one row per finding with a symbol by
//! level; findings on the same passage share one row. At most 180 pt high; more scrolls.

use chrono::Local;
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_app_kit::{
    NSAccessibility, NSFont, NSFontDescriptorSymbolicTraits, NSImageView, NSLayoutConstraint,
    NSScrollView, NSStackView, NSTextField, NSView,
};
use objc2_foundation::NSArray;

use draftdesk_core::item::{CheckSource, Checks, Finding, Level, Verdict};

use crate::i18n::{quoted, tr};
use crate::sidebar::{Tint, color, when};
use crate::ui::{label, ns, stack, symbol};

/// The SF Symbol and colour of a level.
pub fn level_symbol(l: Level) -> (&'static str, Tint) {
    match l {
        Level::Info => ("info.circle", Tint::Gray),
        Level::Warn => ("exclamationmark.triangle", Tint::Orange),
        Level::Block => ("xmark.octagon", Tint::Red),
    }
}

fn verdict_symbol(v: Verdict) -> (&'static str, Tint) {
    match v {
        Verdict::Pass => ("checkmark.seal", Tint::Green),
        Verdict::Warn => ("exclamationmark.triangle", Tint::Orange),
        Verdict::Block => ("xmark.octagon", Tint::Red),
    }
}

/// "Check: warnings · 12:34 · check command · stale"; `None`: nothing to show.
pub fn header(checks: Option<&Checks>, stale: bool, running: bool) -> Option<String> {
    if running {
        return Some(tr("Prüfung läuft …", "Checking…").to_string());
    }
    let c = checks?;
    let verdict = match c.verdict {
        Verdict::Pass => tr("bestanden", "passed"),
        Verdict::Warn => tr("Hinweise", "warnings"),
        Verdict::Block => tr("blockiert", "blocked"),
    };
    let mut parts = vec![format!("{} {verdict}", tr("Prüfung:", "Check:"))];
    if let Some(t) = c.ran_at.as_deref().map(|t| when(t, Local::now())) {
        parts.push(t);
    }
    match c.source {
        Some(CheckSource::Agent) => parts.push(tr("vom Agenten", "by the agent").into()),
        Some(CheckSource::Command) => parts.push(tr("Prüfbefehl", "check command").into()),
        None => {}
    }
    if stale {
        parts.push(tr("veraltet", "stale").into());
    }
    parts.retain(|p| !p.is_empty());
    Some(parts.join(" · "))
}

/// The findings bundled by the passage of `body` they quote: findings whose quotes overlap share one bundle, named
/// by the whole passage they cover together (no sentence guessing, so "Dr." or "1." split nothing and no part of a
/// quote drops out of view). Bundles keep the order of their first finding; a finding whose quote is empty or not in
/// `body` stays alone.
/// ponytail: the first occurrence of a quote is its place; a short quote that also stands earlier binds there.
pub fn bundles<'a>(findings: &'a [Finding], body: &str) -> Vec<(Option<String>, Vec<&'a Finding>)> {
    /// A bundle: the byte range its quotes cover (none: a finding without a place) and its findings.
    type Bundle<'a> = (Option<(usize, usize)>, Vec<&'a Finding>);
    let mut out: Vec<Bundle<'a>> = Vec::new();
    for f in findings {
        let Some((mut s, mut e)) = Some(f.quote.as_str())
            .filter(|q| !q.trim().is_empty())
            .and_then(|q| body.find(q).map(|p| (p, p + q.len())))
        else {
            out.push((None, vec![f]));
            continue;
        };
        // The bundles are disjoint, so one pass finds every one this quote reaches (it can join two).
        let (mut joined, mut at) = (Vec::new(), None);
        let mut i = 0;
        while i < out.len() {
            match out[i].0 {
                Some((bs, be)) if bs < e && s < be => {
                    let (_, fs) = out.remove(i);
                    (s, e) = (s.min(bs), e.max(be));
                    joined.extend(fs);
                    at.get_or_insert(i);
                }
                _ => i += 1,
            }
        }
        joined.push(f);
        out.insert(at.unwrap_or(out.len()), (Some((s, e)), joined));
    }
    out.into_iter()
        .map(|(r, fs)| (r.map(|(s, e)| body[s..e].trim().to_string()), fs))
        .collect()
}

/// "ABSICHT ×2 (vor dem Senden bestätigen) · WIDERSPRUCH": each title once, in order, with its count.
pub fn tags(findings: &[&Finding]) -> String {
    let mut seen: Vec<(&str, usize, bool)> = Vec::new();
    for f in findings {
        match seen.iter_mut().find(|(t, _, _)| *t == f.title) {
            Some(e) => {
                e.1 += 1;
                e.2 |= f.confirm;
            }
            None => seen.push((f.title.as_str(), 1, f.confirm)),
        }
    }
    seen.iter()
        .map(|&(title, n, confirm)| {
            let mut t = title.to_string();
            if n > 1 {
                t.push_str(&format!(" ×{n}"));
            }
            if confirm {
                t.push(' ');
                t.push_str(tr(
                    "(vor dem Senden bestätigen)",
                    "(confirm before sending)",
                ));
            }
            t
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

fn rank(l: Level) -> u8 {
    match l {
        Level::Info => 0,
        Level::Warn => 1,
        Level::Block => 2,
    }
}

fn level_word(l: Level) -> &'static str {
    match l {
        Level::Info => tr("Hinweis", "Note"),
        Level::Warn => tr("Warnung", "Warning"),
        Level::Block => tr("Blockiert", "Blocking"),
    }
}

/// The findings section.
pub struct Findings {
    pub view: Retained<NSStackView>,
    icon: Retained<NSImageView>,
    header: Retained<NSTextField>,
    list: Retained<NSStackView>,
}

impl Findings {
    pub fn new(mtm: MainThreadMarker) -> Findings {
        let icon = NSImageView::new(mtm);
        let header = label(mtm, "");
        header.setFont(Some(&NSFont::boldSystemFontOfSize(12.0)));
        let top = stack(mtm, &[&icon, &header], false, 6.0);
        let list = stack(mtm, &[], true, 8.0);
        let scroll = NSScrollView::new(mtm);
        scroll.setHasVerticalScroller(true);
        scroll.setAutohidesScrollers(true);
        scroll.setDrawsBackground(false);
        scroll.setDocumentView(Some(&list));
        list.setTranslatesAutoresizingMaskIntoConstraints(false);
        let clip = scroll.contentView();
        let grow = scroll
            .heightAnchor()
            .constraintEqualToAnchor(&list.heightAnchor());
        // Wants to be as high as the rows, but at most 180 pt; above that it scrolls.
        grow.setPriority(250.0);
        NSLayoutConstraint::activateConstraints(&NSArray::from_retained_slice(&[
            list.leadingAnchor()
                .constraintEqualToAnchor(&clip.leadingAnchor()),
            list.trailingAnchor()
                .constraintEqualToAnchor(&clip.trailingAnchor()),
            list.topAnchor().constraintEqualToAnchor(&clip.topAnchor()),
            scroll
                .heightAnchor()
                .constraintLessThanOrEqualToConstant(180.0),
            grow,
        ]));
        let view = stack(mtm, &[&top, &scroll], true, 6.0);
        scroll.setTranslatesAutoresizingMaskIntoConstraints(false);
        scroll
            .widthAnchor()
            .constraintEqualToAnchor(&view.widthAnchor())
            .setActive(true);
        view.setAccessibilityLabel(Some(&ns(tr("Befunde", "Findings"))));
        view.setHidden(true);
        Findings {
            view,
            icon,
            header,
            list,
        }
    }

    /// Shows the checks of the draft with the text `body`; `running`: a check runs now.
    pub fn show(
        &self,
        mtm: MainThreadMarker,
        checks: Option<&Checks>,
        body: &str,
        stale: bool,
        running: bool,
    ) {
        for v in self.list.views().iter() {
            self.list.removeView(&v);
        }
        let Some(text) = header(checks, stale, running) else {
            self.view.setHidden(true);
            return;
        };
        self.header.setStringValue(&ns(&text));
        let (name, tint) = match checks {
            Some(c) if !running => verdict_symbol(c.verdict),
            _ => ("hourglass", Tint::Gray),
        };
        self.icon.setImage(symbol(name, &text).as_deref());
        self.icon.setContentTintColor(Some(&color(tint)));
        if !running && let Some(c) = checks {
            for (passage, fs) in bundles(&c.findings, if stale { "" } else { body }) {
                let line = match passage {
                    Some(s) if fs.len() > 1 => bundle_row(mtm, &s, &fs),
                    _ => row(mtm, fs[0]),
                };
                self.list.addArrangedSubview(&line);
            }
        }
        // Findings of an earlier text stay readable but step back, one row each: their quotes would bind to
        // whatever the new text holds.
        self.list.setAlphaValue(if stale { 0.5 } else { 1.0 });
        self.view.setHidden(false);
    }

    /// The header and every row's text, for tests and VoiceOver checks.
    pub fn texts(&self) -> Vec<String> {
        let mut out = vec![self.header.stringValue().to_string()];
        out.extend(
            self.list
                .views()
                .iter()
                .filter_map(|v| v.accessibilityLabel())
                .map(|s| s.to_string()),
        );
        out
    }
}

/// Several findings on one passage: the worst level's symbol, the passage in bold, the titles once each below it;
/// the details as tooltip and for VoiceOver.
fn bundle_row(mtm: MainThreadMarker, passage: &str, fs: &[&Finding]) -> Retained<NSView> {
    let worst = fs
        .iter()
        .map(|f| f.level)
        .max_by_key(|&l| rank(l))
        .unwrap_or(Level::Info);
    let s = NSTextField::wrappingLabelWithString(&ns(&quoted(passage)), mtm);
    s.setFont(Some(&NSFont::boldSystemFontOfSize(12.0)));
    let names = tags(fs);
    let t = NSTextField::wrappingLabelWithString(&ns(&names), mtm);
    t.setFont(Some(&NSFont::systemFontOfSize(12.0)));
    let details: Vec<String> = fs
        .iter()
        .filter(|f| !f.detail.is_empty())
        .map(|f| format!("{}: {}", f.title, f.detail))
        .collect();
    let mut a11y = format!("{}: {} {names}", level_word(worst), quoted(passage));
    if !details.is_empty() {
        a11y.push_str(&format!(" ({})", details.join("; ")));
    }
    let line = with_symbol(mtm, worst, &a11y, &stack(mtm, &[&s, &t], true, 2.0));
    line.setToolTip(Some(&ns(&details.join("\n"))));
    Retained::into_super(line)
}

/// The level's symbol left of `text`; `a11y` labels the row for VoiceOver.
fn with_symbol(
    mtm: MainThreadMarker,
    level: Level,
    a11y: &str,
    text: &NSStackView,
) -> Retained<NSStackView> {
    let (name, tint) = level_symbol(level);
    let line = match symbol(name, a11y) {
        Some(img) => {
            let iv = NSImageView::imageViewWithImage(&img, mtm);
            iv.setContentTintColor(Some(&color(tint)));
            stack(mtm, &[&iv, text], false, 8.0)
        }
        None => stack(mtm, &[text], false, 8.0),
    };
    line.setAccessibilityLabel(Some(&ns(a11y)));
    line
}

/// One finding: the level's symbol, the title in bold, the detail, the quote in italics (unless the detail already
/// says it), and the note when it must be confirmed before sending.
fn row(mtm: MainThreadMarker, f: &Finding) -> Retained<NSView> {
    let quote = if f.detail.contains(&f.quote) {
        ""
    } else {
        f.quote.as_str()
    };
    let mut title = f.title.clone();
    if f.confirm {
        title = format!(
            "{title} {}",
            tr("(vor dem Senden bestätigen)", "(confirm before sending)")
        );
    }
    let t = NSTextField::wrappingLabelWithString(&ns(&title), mtm);
    t.setFont(Some(&NSFont::boldSystemFontOfSize(12.0)));
    let mut lines: Vec<Retained<NSTextField>> = vec![t];
    if !f.detail.is_empty() {
        let d = NSTextField::wrappingLabelWithString(&ns(&f.detail), mtm);
        d.setFont(Some(&NSFont::systemFontOfSize(12.0)));
        lines.push(d);
    }
    if !quote.is_empty() {
        let q = NSTextField::wrappingLabelWithString(&ns(&quoted(quote)), mtm);
        let base = NSFont::systemFontOfSize(12.0);
        let italic = base
            .fontDescriptor()
            .fontDescriptorWithSymbolicTraits(NSFontDescriptorSymbolicTraits::TraitItalic);
        q.setFont(NSFont::fontWithDescriptor_size(&italic, 12.0).as_deref());
        lines.push(q);
    }
    let refs: Vec<&NSView> = lines.iter().map(|l| &***l as &NSView).collect();
    let a11y = format!(
        "{}: {}",
        level_word(f.level),
        [title.as_str(), f.detail.as_str(), quote]
            .iter()
            .filter(|s| !s.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join(" ")
    );
    Retained::into_super(with_symbol(
        mtm,
        f.level,
        &a11y,
        &stack(mtm, &refs, true, 2.0),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checks(verdict: Verdict, source: Option<CheckSource>) -> Checks {
        Checks {
            body_sha256: String::new(),
            verdict,
            findings: vec![],
            ran_at: None,
            source,
            content_digest: None,
        }
    }

    #[test]
    fn header_names_verdict_source_and_stale() {
        crate::i18n::set_german(true);
        assert_eq!(header(None, false, false), None);
        assert_eq!(header(None, false, true).unwrap(), "Prüfung läuft …");
        assert_eq!(
            header(
                Some(&checks(Verdict::Warn, Some(CheckSource::Command))),
                true,
                false
            )
            .unwrap(),
            "Prüfung: Hinweise · Prüfbefehl · veraltet"
        );
        crate::i18n::set_german(false);
        assert_eq!(
            header(
                Some(&checks(Verdict::Pass, Some(CheckSource::Agent))),
                false,
                false
            )
            .unwrap(),
            "Check: passed · by the agent"
        );
        assert_eq!(level_symbol(Level::Block), ("xmark.octagon", Tint::Red));
        assert_eq!(level_symbol(Level::Info), ("info.circle", Tint::Gray));
    }

    fn finding(level: Level, title: &str, quote: &str, confirm: bool) -> Finding {
        Finding {
            level,
            title: title.into(),
            detail: format!("about {quote}"),
            quote: quote.into(),
            confirm,
        }
    }

    #[test]
    fn bundles_findings_per_quoted_passage_and_names_titles_once() {
        crate::i18n::set_german(true);
        let body =
            "Hallo,\n\nWie besprochen schicke ich dir morgen die Unterlagen. Viele Grüsse\nMax\n";
        let fs = vec![
            finding(
                Level::Warn,
                "ABSICHT",
                "schicke ich dir morgen die Unterlagen",
                true,
            ),
            finding(Level::Warn, "ABSICHT", "morgen", true),
            finding(
                Level::Block,
                "WIDERSPRUCH",
                "Wie besprochen schicke ich dir morgen die Unterlagen.",
                false,
            ),
            finding(Level::Block, "ANRED-1", "", false),
            finding(Level::Warn, "UNBELEGT", "Wie besprochen", false),
            finding(Level::Info, "ABSICHT", "Viele Grüsse", false),
            finding(Level::Warn, "UNBELEGT", "steht nicht im Text", false),
            finding(Level::Warn, "LEER", "  ", false),
        ];
        let shape = |b: &[(Option<String>, Vec<&Finding>)]| -> Vec<(Option<String>, usize)> {
            b.iter().map(|(s, v)| (s.clone(), v.len())).collect()
        };
        let b = bundles(&fs, body);
        let whole = "Wie besprochen schicke ich dir morgen die Unterlagen.";
        assert_eq!(
            shape(&b),
            vec![
                (Some(whole.into()), 4),
                (None, 1),
                (Some("Viele Grüsse".into()), 1),
                (None, 1),
                (None, 1),
            ]
        );
        assert_eq!(
            tags(&b[0].1),
            "ABSICHT ×2 (vor dem Senden bestätigen) · WIDERSPRUCH · UNBELEGT"
        );
        // A quote reaching over both passages joins them, the heading names all of it (nothing drops from view).
        let mut more = fs.clone();
        more.push(finding(
            Level::Warn,
            "ZUSAGE",
            "Unterlagen. Viele Grüsse",
            false,
        ));
        assert_eq!(
            shape(&bundles(&more, body)),
            vec![
                (Some(format!("{whole} Viele Grüsse")), 6),
                (None, 1),
                (None, 1),
                (None, 1),
            ]
        );
        // An abbreviation splits nothing: "Dr." is no end of anything here.
        let dr = "Ich schicke Herrn Dr. Müller morgen die Unterlagen.";
        let two = [
            finding(Level::Warn, "ABSICHT", "schicke Herrn Dr. Müller", true),
            finding(Level::Warn, "UNBELEGT", "Müller morgen", false),
        ];
        assert_eq!(
            shape(&bundles(&two, dr)),
            vec![(Some("schicke Herrn Dr. Müller morgen".into()), 2)]
        );
        // Without the text (an earlier text's findings) nothing bundles.
        assert!(
            bundles(&fs, "")
                .iter()
                .all(|(s, v)| s.is_none() && v.len() == 1)
        );
    }
}
