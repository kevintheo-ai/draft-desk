//! The sidebar: the waiting drafts, newest first, each with a status symbol. The rows live in their own main-thread
//! slot, so AppKit can ask for them while the app itself is busy (a reload inside `with_app`).

use std::cell::RefCell;

use chrono::{DateTime, Local};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSAccessibility, NSBackgroundStyle, NSColor, NSFont, NSImageView, NSScrollView,
    NSTableCellView, NSTableColumn, NSTableView, NSTableViewStyle, NSTextField, NSView,
};

use draftdesk_core::address;
use draftdesk_core::desk::confirmation;
use draftdesk_core::item::{ItemRead, State};

use crate::app::Delegate;
use crate::editor::pin;
use crate::i18n::{state_text, tr};
use crate::ui::{insets, label, ns, stack, symbol};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tint {
    Plain,
    Accent,
    Orange,
    Red,
    Green,
    Gray,
}

/// One line of the sidebar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub id: String,
    pub title: String,
    pub subtitle: String,
    /// An SF Symbol name.
    pub symbol: &'static str,
    pub tint: Tint,
    /// What VoiceOver reads.
    pub a11y: String,
}

/// `HH:MM` today, else the day; empty for an unreadable time.
pub fn when(created_at: &str, now: DateTime<Local>) -> String {
    let Ok(t) = DateTime::parse_from_rfc3339(created_at) else {
        return String::new();
    };
    let t = t.with_timezone(&Local);
    if t.date_naive() == now.date_naive() {
        t.format("%H:%M").to_string()
    } else if crate::i18n::is_german() {
        t.format("%d.%m.").to_string()
    } else {
        t.format("%b %-d").to_string()
    }
}

/// The first recipient by name (or address), and how many more.
fn recipients(to: &[String], more: usize) -> String {
    let first = to
        .first()
        .and_then(|e| address::parse_list(e).ok())
        .and_then(|l| l.into_iter().next())
        .map(|a| if a.name.is_empty() { a.address } else { a.name })
        .unwrap_or_default();
    if more > 0 {
        format!("{first} +{more}")
    } else {
        first
    }
}

pub fn row(read: &ItemRead, now: DateTime<Local>) -> Row {
    let i = match read {
        ItemRead::Valid(i) => i,
        ItemRead::Invalid { item_id, errors } => {
            let first = errors.first().cloned().unwrap_or_default();
            return Row {
                id: item_id.clone(),
                title: item_id.clone(),
                subtitle: first.chars().take(120).collect(),
                symbol: "xmark.octagon",
                tint: Tint::Red,
                a11y: format!("{item_id}, {}", tr("ungültig", "invalid")),
            };
        }
    };
    let title = if i.subject.trim().is_empty() {
        tr("(ohne Betreff)", "(no subject)").to_string()
    } else {
        i.subject.clone()
    };
    let more = i.to.len() + i.cc.len() + i.bcc.len() - usize::from(!i.to.is_empty());
    let subtitle = format!("{} · {}", recipients(&i.to, more), when(&i.created_at, now));
    let (symbol, tint) = match i.state {
        State::Ready if confirmation(i).is_some() => ("exclamationmark.triangle", Tint::Orange),
        State::Ready if i.edits.edited => ("pencil", Tint::Plain),
        State::Ready => ("envelope", Tint::Plain),
        State::SendingScheduled => ("clock", Tint::Orange),
        State::Sending => ("paperplane", Tint::Accent),
        State::Sent => ("checkmark.circle", Tint::Green),
        State::Failed => ("exclamationmark.octagon", Tint::Red),
        State::Discarded => ("trash", Tint::Gray),
    };
    Row {
        id: i.item_id.clone(),
        a11y: format!("{title}, {}, {subtitle}", state_text(i.state)),
        title,
        subtitle,
        symbol,
        tint,
    }
}

thread_local! {
    static ROWS: RefCell<Vec<Row>> = const { RefCell::new(Vec::new()) };
}

pub fn set_rows(rows: Vec<Row>) {
    ROWS.with(|r| *r.borrow_mut() = rows);
}

pub fn row_count() -> usize {
    ROWS.with(|r| r.borrow().len())
}

pub fn row_at(index: usize) -> Option<Row> {
    ROWS.with(|r| r.borrow().get(index).cloned())
}

pub fn index_of(id: &str) -> Option<usize> {
    ROWS.with(|r| r.borrow().iter().position(|x| x.id == id))
}

pub fn color(t: Tint) -> Retained<NSColor> {
    match t {
        Tint::Plain => NSColor::secondaryLabelColor(),
        Tint::Accent => NSColor::controlAccentColor(),
        Tint::Orange => NSColor::systemOrangeColor(),
        Tint::Red => NSColor::systemRedColor(),
        Tint::Green => NSColor::systemGreenColor(),
        Tint::Gray => NSColor::tertiaryLabelColor(),
    }
}

define_class!(
    /// One row of the sidebar. Its labels follow the selection like those of Mail's list (white on the accent colour):
    /// the table tells the cell its background style, the cell tells every label.
    // SAFETY: NSTableCellView has no subclassing requirements, and `RowCell` implements no `Drop`.
    #[unsafe(super(NSTableCellView))]
    #[thread_kind = MainThreadOnly]
    #[name = "DraftDeskRowCell"]
    pub struct RowCell;

    impl RowCell {
        #[unsafe(method(setBackgroundStyle:))]
        fn set_background_style(&self, style: NSBackgroundStyle) {
            // SAFETY: the superclass's implementation with the same argument.
            let () = unsafe { msg_send![super(self), setBackgroundStyle: style] };
            let mut labels = Vec::new();
            text_fields(self, &mut labels);
            for l in labels {
                if let Some(cell) = l.cell() {
                    cell.setBackgroundStyle(style);
                }
            }
        }
    }
);

/// Every text field below `v`.
fn text_fields(v: &NSView, out: &mut Vec<Retained<NSTextField>>) {
    for s in v.subviews().iter() {
        if let Ok(t) = s.clone().downcast::<NSTextField>() {
            out.push(t);
        }
        text_fields(&s, out);
    }
}

/// The view of one row: symbol, title in bold, the second line in grey.
pub fn cell(mtm: MainThreadMarker, row: &Row) -> Retained<NSView> {
    let title = label(mtm, &row.title);
    title.setFont(Some(&NSFont::boldSystemFontOfSize(13.0)));
    let sub = label(mtm, &row.subtitle);
    sub.setFont(Some(&NSFont::systemFontOfSize(11.0)));
    sub.setTextColor(Some(&NSColor::secondaryLabelColor()));
    let text = stack(mtm, &[&title, &sub], true, 2.0);
    let image = symbol(row.symbol, &row.a11y).map(|img| {
        let iv = NSImageView::imageViewWithImage(&img, mtm);
        iv.setContentTintColor(Some(&color(row.tint)));
        iv
    });
    let line = match &image {
        Some(iv) => stack(mtm, &[iv, &text], false, 8.0),
        None => stack(mtm, &[&text], false, 8.0),
    };
    line.setEdgeInsets(insets(4.0, 6.0, 4.0, 6.0));
    let this = RowCell::alloc(mtm).set_ivars(());
    // SAFETY: NSView's initializer.
    let cell: Retained<RowCell> = unsafe { msg_send![super(this), init] };
    cell.addSubview(&line);
    pin(&line, &cell);
    // SAFETY: a weak outlet; the title lives as long as the cell, inside it.
    unsafe { cell.setTextField(Some(&title)) };
    cell.setAccessibilityLabel(Some(&ns(&row.a11y)));
    Retained::into_super(Retained::into_super(cell))
}

/// The table in its scroll view; the delegate is data source and delegate.
pub fn table(
    mtm: MainThreadMarker,
    delegate: &Delegate,
) -> (Retained<NSScrollView>, Retained<NSTableView>) {
    let table = NSTableView::new(mtm);
    let column = NSTableColumn::initWithIdentifier(NSTableColumn::alloc(mtm), &ns("draft"));
    table.addTableColumn(&column);
    table.setHeaderView(None);
    table.setStyle(NSTableViewStyle::SourceList);
    table.setRowHeight(44.0);
    // SAFETY: the delegate lives as long as the app.
    unsafe {
        table.setDataSource(Some(ProtocolObject::from_ref(delegate)));
        table.setDelegate(Some(ProtocolObject::from_ref(delegate)));
    }
    table.setAccessibilityLabel(Some(&ns(tr("Entwürfe", "Drafts"))));
    let scroll = NSScrollView::new(mtm);
    scroll.setDocumentView(Some(&table));
    scroll.setHasVerticalScroller(true);
    scroll.setDrawsBackground(false);
    (scroll, table)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use draftdesk_core::item::Item;

    fn item(state: &str, subject: &str, to: &[&str]) -> Item {
        serde_json::from_value(serde_json::json!({
            "schema": "draftdesk-item/1", "item_id": "20261107-091500-0a1b2c3d",
            "created_at": "2026-11-07T09:15:00+01:00", "state": state, "state_revision": 1,
            "content_revision": 1, "account": "work", "from": {"address": "me@example.org", "name": "Me"},
            "to": to, "cc": [], "bcc": [], "subject": subject, "body": "Hi\n",
            "body_sha256": draftdesk_core::sha256_hex(b"Hi\n"), "reply": null, "mailbox_draft": null,
            "origin": null, "attachments": [], "checks": null, "edits": {"edited": false, "fields_changed": []},
            "send": {}, "history": []
        }))
        .unwrap()
    }

    /// A day of this machine's calendar: the tests then hold in every time zone.
    fn local(day: u32, hour: u32, minute: u32) -> DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 11, day, hour, minute, 0)
            .unwrap()
    }

    fn now() -> DateTime<Local> {
        local(7, 18, 0)
    }

    #[test]
    fn row_model_states() {
        crate::i18n::set_german(true);
        let r = row(
            &ItemRead::Valid(Box::new(item(
                "ready",
                "Re: Offer",
                &["Ann <ann@example.org>", "bob@example.org"],
            ))),
            now(),
        );
        assert_eq!(
            (r.title.as_str(), r.symbol, r.tint),
            ("Re: Offer", "envelope", Tint::Plain)
        );
        assert!(r.subtitle.starts_with("Ann +1 · "), "{}", r.subtitle);
        assert!(r.a11y.contains("Bereit"));
        let mut edited = item("ready", "", &["bob@example.org"]);
        edited.edits.edited = true;
        let r = row(&ItemRead::Valid(Box::new(edited)), now());
        assert_eq!((r.title.as_str(), r.symbol), ("(ohne Betreff)", "pencil"));
        assert!(r.subtitle.starts_with("bob@example.org · "));
        for (state, symbol, tint) in [
            ("sending_scheduled", "clock", Tint::Orange),
            ("sending", "paperplane", Tint::Accent),
            ("failed", "exclamationmark.octagon", Tint::Red),
            ("discarded", "trash", Tint::Gray),
        ] {
            let r = row(
                &ItemRead::Valid(Box::new(item(state, "S", &["a@example.org"]))),
                now(),
            );
            assert_eq!((r.symbol, r.tint), (symbol, tint), "{state}");
        }
        let bad = ItemRead::Invalid {
            item_id: "20261107-091500-deadbeef".into(),
            errors: vec!["account: unknown account \"old\"".into()],
        };
        let r = row(&bad, now());
        assert_eq!((r.symbol, r.tint), ("xmark.octagon", Tint::Red));
        assert!(r.subtitle.starts_with("account: unknown"));
    }

    #[test]
    fn needs_confirmation_shows_a_warning_symbol() {
        let mut it = item("ready", "S", &["a@example.org"]);
        it.checks = Some(
            serde_json::from_value(serde_json::json!({
                "body_sha256": it.body_sha256, "verdict": "block", "findings": []
            }))
            .unwrap(),
        );
        let r = row(&ItemRead::Valid(Box::new(it)), now());
        assert_eq!(
            (r.symbol, r.tint),
            ("exclamationmark.triangle", Tint::Orange)
        );
    }

    #[test]
    fn time_is_hours_today_and_a_day_before() {
        crate::i18n::set_german(true);
        let today = local(7, 9, 15).to_rfc3339();
        let before = local(5, 9, 15).to_rfc3339();
        assert_eq!(when(&today, now()), "09:15");
        assert_eq!(when(&before, now()), "05.11.");
        crate::i18n::set_german(false);
        assert_eq!(when(&before, now()), "Nov 5");
        assert_eq!(when("garbage", now()), "");
    }
}
