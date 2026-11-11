//! The editor on the right: a hint bar, the header (From, To, Cc, Bcc, Subject) and the text. Apple-Mail style: a
//! `ready` draft is editable in place; the app saves it (`App::flush`).

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{MainThreadMarker, MainThreadOnly, sel};
use objc2_app_kit::{
    NSAccessibility, NSBackgroundColorAttributeName, NSBezelStyle, NSBox, NSBoxType, NSButton,
    NSButtonType, NSColor, NSControlStateValueOff, NSControlStateValueOn, NSFont,
    NSGridCellPlacement, NSGridView, NSLayoutConstraint, NSLayoutConstraintOrientation,
    NSPasteboardTypeString, NSPopUpButton, NSProgressIndicator, NSScrollView,
    NSSegmentSwitchTracking, NSSegmentedControl, NSStackView, NSTextField, NSTextView,
    NSTitlePosition, NSTokenField, NSView,
};
use objc2_foundation::{NSArray, NSCharacterSet, NSPoint, NSRange, NSRect, NSSize, NSString};
use objc2_web_kit::WKWebView;

use draftdesk_core::config::Config;
use draftdesk_core::edit::Form;
use draftdesk_core::item::Item;
use draftdesk_core::text::{forbidden_utf16, strip_forbidden};

use crate::app::Delegate;
use crate::attachments::Strip;
use crate::findings::Findings;
use crate::i18n::tr;
use crate::preview;
use crate::ui::{insets, label, ns, stack};

/// A coloured bar around `content`, hidden at first. The content is tied to the bar's edges, so the bar is as high
/// as its content and the content as wide as the bar (a long hint wraps instead of widening the window).
fn boxed(mtm: MainThreadMarker, content: &NSView, fill: &NSColor) -> Retained<NSBox> {
    let view = NSBox::new(mtm);
    view.setBoxType(NSBoxType::Custom);
    view.setTitlePosition(NSTitlePosition::NoTitle);
    view.setBorderWidth(0.0);
    view.setCornerRadius(6.0);
    view.setFillColor(fill);
    view.setContentView(Some(content));
    pin(content, &view);
    view.setHidden(true);
    view
}

/// A hint above the header: a sentence, an optional button, a close button.
pub struct Banner {
    pub view: Retained<NSBox>,
    text: Retained<NSTextField>,
    pub action: Retained<NSButton>,
}

impl Banner {
    fn new(mtm: MainThreadMarker, delegate: &Delegate) -> Banner {
        let text = NSTextField::wrappingLabelWithString(&ns(""), mtm);
        text.setContentCompressionResistancePriority_forOrientation(
            250.0,
            NSLayoutConstraintOrientation::Horizontal,
        );
        let target: &AnyObject = delegate.as_ref();
        // SAFETY: the target is the app delegate, which lives as long as the app.
        let action = unsafe {
            NSButton::buttonWithTitle_target_action(
                &ns(""),
                Some(target),
                Some(sel!(bannerAction:)),
                mtm,
            )
        };
        // SAFETY: as above.
        let close = unsafe {
            NSButton::buttonWithTitle_target_action(
                &ns("×"),
                Some(target),
                Some(sel!(dismissBanner:)),
                mtm,
            )
        };
        close.setAccessibilityLabel(Some(&ns(tr("Hinweis schließen", "Close the hint"))));
        let row = stack(mtm, &[&text, &action, &close], false, 8.0);
        row.setEdgeInsets(insets(6.0, 10.0, 6.0, 10.0));
        let view = boxed(
            mtm,
            &row,
            &NSColor::systemYellowColor().colorWithAlphaComponent(0.25),
        );
        Banner { view, text, action }
    }

    /// Shows `text`; `action` is the title of the button (none without).
    pub fn show(&self, text: &str, action: Option<&str>) {
        self.text.setStringValue(&ns(text));
        self.action.setTitle(&ns(action.unwrap_or("")));
        self.action.setHidden(action.is_none());
        self.view.setHidden(false);
    }

    pub fn hide(&self) {
        self.view.setHidden(true);
    }

    /// The sentence shown, `None` when hidden.
    pub fn text(&self) -> Option<String> {
        (!self.view.isHidden()).then(|| self.text.stringValue().to_string())
    }
}

/// The countdown of a scheduled send: "„Subject“ is sent in N s", a progress bar and Undo. Escape undoes only while
/// the bar is shown.
pub struct UndoBar {
    pub view: Retained<NSBox>,
    text: Retained<NSTextField>,
    progress: Retained<NSProgressIndicator>,
    pub button: Retained<NSButton>,
}

impl UndoBar {
    fn new(mtm: MainThreadMarker, delegate: &Delegate) -> UndoBar {
        let text = label(mtm, "");
        let target: &AnyObject = delegate.as_ref();
        // SAFETY: the target is the app delegate, which lives as long as the app.
        let button = unsafe {
            NSButton::buttonWithTitle_target_action(
                &ns(tr("Rückgängig", "Undo")),
                Some(target),
                Some(sel!(undoSend:)),
                mtm,
            )
        };
        let progress = NSProgressIndicator::new(mtm);
        progress.setIndeterminate(false);
        progress.setMinValue(0.0);
        progress.setMaxValue(1.0);
        progress.setAccessibilityLabel(Some(&ns(tr("Zeit bis zum Senden", "Time until sending"))));
        let row = stack(mtm, &[&text, &button], false, 8.0);
        let column = stack(mtm, &[&row, &progress], true, 4.0);
        column.setEdgeInsets(insets(6.0, 10.0, 6.0, 10.0));
        progress.setTranslatesAutoresizingMaskIntoConstraints(false);
        progress
            .widthAnchor()
            .constraintEqualToAnchor_constant(&column.widthAnchor(), -20.0)
            .setActive(true);
        let view = boxed(
            mtm,
            &column,
            &NSColor::controlAccentColor().colorWithAlphaComponent(0.15),
        );
        UndoBar {
            view,
            text,
            progress,
            button,
        }
    }

    /// `left`: the part of the countdown still to run, 1.0 to 0.0.
    pub fn show(&self, text: &str, left: f64) {
        self.text.setStringValue(&ns(text));
        self.progress.setDoubleValue(left);
        if self.view.isHidden() {
            self.button.setKeyEquivalent(&ns("\u{1b}"));
            self.view.setHidden(false);
        }
    }

    pub fn hide(&self) {
        self.button.setKeyEquivalent(&ns(""));
        self.view.setHidden(true);
    }

    pub fn text(&self) -> Option<String> {
        (!self.view.isHidden()).then(|| self.text.stringValue().to_string())
    }
}

/// One sender of the draft's account: address, configured name, whether that name is fixed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SenderChoice {
    address: String,
    name: String,
    fixed: bool,
}

/// The conversation the agent handed over, under the text: folded away under a disclosure button, read only.
pub struct Thread {
    pub row: Retained<NSStackView>,
    disclosure: Retained<NSButton>,
    pub scroll: Retained<NSScrollView>,
    pub text: Retained<NSTextView>,
}

impl Thread {
    fn new(mtm: MainThreadMarker, delegate: &Delegate) -> Thread {
        let target: &AnyObject = delegate.as_ref();
        // SAFETY: the target is the app delegate, which lives as long as the app.
        let disclosure = unsafe {
            NSButton::buttonWithTitle_target_action(
                &ns(""),
                Some(target),
                Some(sel!(toggleThread:)),
                mtm,
            )
        };
        disclosure.setBezelStyle(NSBezelStyle::Disclosure);
        disclosure.setButtonType(NSButtonType::PushOnPushOff);
        disclosure.setTitle(&ns(""));
        let title = tr("Gesprächsverlauf", "Conversation");
        disclosure.setAccessibilityLabel(Some(&ns(title)));
        let caption = label(mtm, title);
        caption.setTextColor(Some(&NSColor::secondaryLabelColor()));
        let row = stack(mtm, &[&disclosure, &caption], false, 4.0);
        let scroll = NSTextView::scrollableTextView(mtm);
        let text = scroll
            .documentView()
            .and_then(|v| v.downcast::<NSTextView>().ok())
            .expect("a scrollable text view holds a text view");
        text.setEditable(false);
        text.setRichText(false);
        text.setFont(Some(&NSFont::systemFontOfSize(12.0)));
        text.setTextColor(Some(&NSColor::secondaryLabelColor()));
        text.setTextContainerInset(NSSize::new(6.0, 6.0));
        text.setAccessibilityLabel(Some(&ns(title)));
        scroll
            .heightAnchor()
            .constraintEqualToConstant(180.0)
            .setActive(true);
        row.setHidden(true);
        scroll.setHidden(true);
        Thread {
            row,
            disclosure,
            scroll,
            text,
        }
    }
}

pub struct Editor {
    pub root: Retained<NSStackView>,
    /// From, To, Cc, Bcc, Subject.
    grid: Retained<NSGridView>,
    pub undo: UndoBar,
    pub banner: Banner,
    pub from: Retained<NSPopUpButton>,
    pub from_name: Retained<NSTextField>,
    pub to: Retained<NSTokenField>,
    pub cc: Retained<NSTokenField>,
    pub bcc: Retained<NSTokenField>,
    pub subject: Retained<NSTextField>,
    pub strip: Strip,
    pub findings: Findings,
    /// "Text | Preview" above the text.
    pub toggle: Retained<NSSegmentedControl>,
    pub body_scroll: Retained<NSScrollView>,
    pub body: Retained<NSTextView>,
    pub preview: Retained<WKWebView>,
    pub thread: Thread,
    /// Shown when no draft is selected.
    pub empty: Retained<NSTextField>,
    senders: Vec<SenderChoice>,
}

fn token_field(mtm: MainThreadMarker, delegate: &Delegate, a11y: &str) -> Retained<NSTokenField> {
    let f = NSTokenField::new(mtm);
    f.setTokenizingCharacterSet(Some(&NSCharacterSet::characterSetWithCharactersInString(
        &ns(",;\n"),
    )));
    // SAFETY: the delegate lives as long as the app.
    unsafe { f.setDelegate(Some(ProtocolObject::from_ref(delegate))) };
    f.setAccessibilityLabel(Some(&ns(a11y)));
    f
}

fn text_field(mtm: MainThreadMarker, delegate: &Delegate, a11y: &str) -> Retained<NSTextField> {
    let f = NSTextField::new(mtm);
    // SAFETY: the delegate lives as long as the app.
    unsafe { f.setDelegate(Some(ProtocolObject::from_ref(delegate))) };
    f.setAccessibilityLabel(Some(&ns(a11y)));
    f
}

/// The text: plain, with undo, find bar and spelling marks, and none of macOS's automatic changes (smart quotes,
/// dashes, text replacement, autocorrect, links, data detectors), which would change the text without a word.
fn configure_body(body: &NSTextView, delegate: &Delegate) {
    body.setFont(Some(&NSFont::systemFontOfSize(14.0)));
    body.setAccessibilityLabel(Some(&ns(tr("Text", "Text"))));
    body.setRichText(false);
    body.setImportsGraphics(false);
    body.setAllowsUndo(true);
    body.setUsesFindBar(true);
    body.setContinuousSpellCheckingEnabled(true);
    body.setAutomaticQuoteSubstitutionEnabled(false);
    body.setAutomaticDashSubstitutionEnabled(false);
    body.setAutomaticTextReplacementEnabled(false);
    body.setAutomaticSpellingCorrectionEnabled(false);
    body.setAutomaticLinkDetectionEnabled(false);
    body.setAutomaticDataDetectionEnabled(false);
    body.setTextContainerInset(NSSize::new(8.0, 8.0));
    body.setDelegate(Some(ProtocolObject::from_ref(delegate)));
    // TextKit 1 on purpose, before any text: the red marks are temporary attributes of its layout manager.
    // SAFETY: plain getter on the main thread.
    let _ = unsafe { body.layoutManager() };
}

/// Where the red marks go (UTF-16 position, length): each invisible character together with one neighbour, the next
/// character or else the previous one. Most of them have no width; a mark on them alone would not show.
pub fn mark_ranges(s: &str) -> Vec<(usize, usize)> {
    let u: Vec<u16> = s.encode_utf16().collect();
    let newline = u16::from(b'\n');
    let high = |c: u16| (0xD800..0xDC00).contains(&c);
    let low = |c: u16| (0xDC00..0xE000).contains(&c);
    forbidden_utf16(s)
        .into_iter()
        .map(|(p, l)| {
            let end = p + l;
            if end < u.len() && u[end] != newline {
                (p, l + if high(u[end]) { 2 } else { 1 })
            } else if p > 0 && u[p - 1] != newline {
                let w = if p >= 2 && low(u[p - 1]) { 2 } else { 1 };
                (p - w, l + w)
            } else {
                (p, l)
            }
        })
        .collect()
}

/// Pins `inner` to the edges of `outer`.
pub fn pin(inner: &NSView, outer: &NSView) {
    inner.setTranslatesAutoresizingMaskIntoConstraints(false);
    let c = [
        inner
            .leadingAnchor()
            .constraintEqualToAnchor(&outer.leadingAnchor()),
        inner
            .trailingAnchor()
            .constraintEqualToAnchor(&outer.trailingAnchor()),
        inner
            .topAnchor()
            .constraintEqualToAnchor(&outer.topAnchor()),
        inner
            .bottomAnchor()
            .constraintEqualToAnchor(&outer.bottomAnchor()),
    ];
    NSLayoutConstraint::activateConstraints(&NSArray::from_retained_slice(&c));
}

/// Makes `view` as wide as the stack's content.
pub fn fill_width(view: &NSView, stack: &NSStackView, inset: f64) {
    view.setTranslatesAutoresizingMaskIntoConstraints(false);
    view.widthAnchor()
        .constraintEqualToAnchor_constant(&stack.widthAnchor(), -2.0 * inset)
        .setActive(true);
}

impl Editor {
    /// Builds the editor into `content`.
    pub fn new(mtm: MainThreadMarker, delegate: &Delegate, content: &NSView) -> Editor {
        let undo = UndoBar::new(mtm, delegate);
        let banner = Banner::new(mtm, delegate);
        let from = NSPopUpButton::initWithFrame_pullsDown(
            NSPopUpButton::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(260.0, 24.0)),
            false,
        );
        let target: &AnyObject = delegate.as_ref();
        // SAFETY: the target is the app delegate, which lives as long as the app.
        unsafe {
            from.setTarget(Some(target));
            from.setAction(Some(sel!(fromChanged:)));
        }
        from.setAccessibilityLabel(Some(&ns(tr("Absender", "Sender"))));
        let from_name = text_field(mtm, delegate, tr("Absendername", "Sender name"));
        from_name.setPlaceholderString(Some(&ns(tr("Name", "Name"))));
        let to = token_field(mtm, delegate, tr("An", "To"));
        let cc = token_field(mtm, delegate, tr("Kopie", "Cc"));
        let bcc = token_field(mtm, delegate, tr("Blindkopie", "Bcc"));
        let subject = text_field(mtm, delegate, tr("Betreff", "Subject"));
        let from_row = stack(mtm, &[&from, &from_name], false, 8.0);

        let mut widest: f64 = 0.0;
        let rows: Vec<Retained<NSArray<NSView>>> = [
            (tr("Von:", "From:"), &*from_row as &NSView),
            (tr("An:", "To:"), &to),
            (tr("Kopie:", "Cc:"), &cc),
            (tr("Blindkopie:", "Bcc:"), &bcc),
            (tr("Betreff:", "Subject:"), &subject),
        ]
        .into_iter()
        .map(|(title, field)| {
            let l = label(mtm, title);
            l.setTextColor(Some(&NSColor::secondaryLabelColor()));
            widest = widest.max(l.fittingSize().width);
            NSArray::from_slice(&[&*l as &NSView, field])
        })
        .collect();
        let grid = NSGridView::gridViewWithViews(&NSArray::from_retained_slice(&rows), mtm);
        // The labels' column is as wide as its widest label, so the fields get the rest. Left to itself the grid gave
        // the extra width to the labels whenever the draft could not be edited: the fields then have a width of
        // their own and resist growing, the right-aligned labels do not.
        grid.columnAtIndex(0).setWidth(widest.ceil());
        grid.columnAtIndex(0)
            .setXPlacement(NSGridCellPlacement::Trailing);
        grid.columnAtIndex(1)
            .setXPlacement(NSGridCellPlacement::Fill);
        grid.setRowSpacing(6.0);
        grid.setColumnSpacing(8.0);

        let body_scroll = NSTextView::scrollableTextView(mtm);
        let body = body_scroll
            .documentView()
            .and_then(|v| v.downcast::<NSTextView>().ok())
            .expect("a scrollable text view holds a text view");
        configure_body(&body, delegate);

        let labels =
            NSArray::from_retained_slice(&[ns(tr("Text", "Text")), ns(tr("Vorschau", "Preview"))]);
        // SAFETY: the target is the app delegate, which lives as long as the app.
        let toggle = unsafe {
            NSSegmentedControl::segmentedControlWithLabels_trackingMode_target_action(
                &labels,
                NSSegmentSwitchTracking::SelectOne,
                Some(target),
                Some(sel!(previewToggled:)),
                mtm,
            )
        };
        toggle.setSelectedSegment(0);
        toggle.setAccessibilityLabel(Some(&ns(tr("Text oder Vorschau", "Text or preview"))));
        let preview = preview::web_view(mtm, delegate);
        preview.setHidden(true);
        let strip = Strip::new(mtm);
        let findings = Findings::new(mtm);
        let thread = Thread::new(mtm, delegate);

        let root = stack(
            mtm,
            &[
                &undo.view,
                &banner.view,
                &grid,
                &strip.view,
                &findings.view,
                &toggle,
                &body_scroll,
                &preview,
                &thread.row,
                &thread.scroll,
            ],
            true,
            10.0,
        );
        root.setEdgeInsets(insets(12.0, 16.0, 12.0, 16.0));
        content.addSubview(&root);
        pin(&root, content);
        for v in [
            &*undo.view as &NSView,
            &banner.view,
            &grid,
            &strip.view,
            &findings.view,
            &body_scroll,
            &preview,
            &thread.scroll,
        ] {
            fill_width(v, &root, 16.0);
        }
        // The text (or the preview in its place) takes the height that is left.
        for v in [&*body_scroll as &NSView, &preview] {
            v.setContentHuggingPriority_forOrientation(
                1.0,
                NSLayoutConstraintOrientation::Vertical,
            );
            v.setContentCompressionResistancePriority_forOrientation(
                1.0,
                NSLayoutConstraintOrientation::Vertical,
            );
        }

        let empty = label(mtm, tr("Keine Entwürfe", "No drafts"));
        empty.setFont(Some(&NSFont::systemFontOfSize(17.0)));
        empty.setTextColor(Some(&NSColor::tertiaryLabelColor()));
        content.addSubview(&empty);
        empty.setTranslatesAutoresizingMaskIntoConstraints(false);
        empty
            .centerXAnchor()
            .constraintEqualToAnchor(&content.centerXAnchor())
            .setActive(true);
        empty
            .centerYAnchor()
            .constraintEqualToAnchor(&content.centerYAnchor())
            .setActive(true);

        let e = Editor {
            root,
            grid,
            undo,
            banner,
            from,
            from_name,
            to,
            cc,
            bcc,
            subject,
            strip,
            findings,
            toggle,
            body_scroll,
            body,
            preview,
            thread,
            empty,
            senders: Vec::new(),
        };
        e.show_nothing();
        e
    }

    /// The conversation of the draft; hidden when the agent handed none over. Stays folded or open as the person
    /// left it.
    pub fn show_thread(&self, thread: Option<&str>) {
        self.thread.text.setString(&ns(thread.unwrap_or("")));
        self.thread.row.setHidden(thread.is_none());
        self.sync_thread();
    }

    /// Shows or folds the conversation as the disclosure button says.
    pub fn sync_thread(&self) {
        let open = self.thread.disclosure.state() == NSControlStateValueOn;
        self.thread
            .scroll
            .setHidden(!open || self.thread.row.isHidden());
    }

    pub fn toggle_thread(&self) {
        let open = self.thread.disclosure.state() == NSControlStateValueOn;
        self.thread.disclosure.setState(if open {
            NSControlStateValueOff
        } else {
            NSControlStateValueOn
        });
        self.sync_thread();
    }

    /// The text or, in its place, the preview.
    pub fn set_preview(&self, on: bool) {
        self.toggle.setSelectedSegment(if on { 1 } else { 0 });
        self.body_scroll.setHidden(on);
        self.preview.setHidden(!on);
        self.refresh_preview();
    }

    pub fn preview_on(&self) -> bool {
        self.toggle.selectedSegment() == 1
    }

    /// The preview always shows the text as it is now.
    fn refresh_preview(&self) {
        if self.preview_on() {
            preview::show(&self.preview, &self.body.string().to_string());
        }
    }

    /// No draft selected: only "No drafts".
    pub fn show_nothing(&self) {
        self.root.setHidden(true);
        self.empty.setHidden(false);
    }

    /// An invalid draft: none of its fields can be shown, only the hint (with "Move to Archive"). No field of the
    /// draft shown before may stay visible.
    pub fn show_invalid(&self) {
        self.root.setHidden(false);
        self.empty.setHidden(true);
        for v in [
            &*self.grid as &NSView,
            &self.strip.view,
            &self.findings.view,
            &self.toggle,
            &self.body_scroll,
            &self.preview,
            &self.thread.row,
            &self.thread.scroll,
        ] {
            v.setHidden(true);
        }
    }

    /// Fills every field from the item. The senders come from the account; a sender that is no longer configured
    /// stays as the only choice.
    pub fn load(&mut self, item: &Item, cfg: &Config) {
        self.senders = cfg
            .account(&item.account)
            .map(|a| {
                a.senders
                    .iter()
                    .map(|s| SenderChoice {
                        address: s.address.clone(),
                        name: s.name.clone().unwrap_or_default(),
                        fixed: s.fixed_name,
                    })
                    .collect()
            })
            .unwrap_or_default();
        if !self.senders.iter().any(|s| s.address == item.from.address) {
            self.senders.push(SenderChoice {
                address: item.from.address.clone(),
                name: item.from.name.clone(),
                fixed: false,
            });
        }
        self.from.removeAllItems();
        for s in &self.senders {
            let title = if s.name.is_empty() {
                s.address.clone()
            } else {
                format!("{} <{}>", s.name, s.address)
            };
            self.from.addItemWithTitle(&ns(&title));
        }
        let index = self
            .senders
            .iter()
            .position(|s| s.address == item.from.address)
            .unwrap_or(0);
        self.from.selectItemAtIndex(index as isize);
        self.from_name.setStringValue(&ns(&item.from.name));
        set_tokens(&self.to, &item.to);
        set_tokens(&self.cc, &item.cc);
        set_tokens(&self.bcc, &item.bcc);
        self.subject.setStringValue(&ns(&item.subject));
        self.body.setString(&ns(&item.body));
        // Another draft (or its saved state again): undo steps of the text before would act on the wrong text.
        if let Some(undo) = self.body.undoManager() {
            undo.removeAllActions();
        }
        self.root.setHidden(false);
        self.empty.setHidden(true);
        // Back from an invalid draft: the header and the text or preview (strip, findings and conversation follow
        // their own content).
        self.grid.setHidden(false);
        self.toggle.setHidden(false);
        self.set_preview(self.preview_on());
    }

    /// The fields as saved, after a save at a checkpoint: only what the save changed (a recipient split in two, a
    /// trimmed name), so the cursor and the undo steps of the text stay.
    pub fn redraw(&self, item: &Item) {
        if self.from_name.stringValue().to_string() != item.from.name {
            self.from_name.setStringValue(&ns(&item.from.name));
        }
        for (field, list) in [
            (&self.to, &item.to),
            (&self.cc, &item.cc),
            (&self.bcc, &item.bcc),
        ] {
            if tokens(field) != *list {
                set_tokens(field, list);
            }
        }
        if self.subject.stringValue().to_string() != item.subject {
            self.subject.setStringValue(&ns(&item.subject));
        }
        if self.body.string().to_string() != item.body {
            self.body.setString(&ns(&item.body));
            self.refresh_preview();
        }
    }

    /// Marks the invisible characters of the text in red; returns how many the text and the subject hold.
    pub fn highlight_invisible(&self) -> usize {
        let text = self.body.string();
        let s = text.to_string();
        // SAFETY: plain getter on the main thread.
        if let Some(lm) = unsafe { self.body.layoutManager() } {
            // SAFETY: a static AppKit constant.
            let key = unsafe { NSBackgroundColorAttributeName };
            lm.removeTemporaryAttribute_forCharacterRange(key, NSRange::new(0, text.length()));
            let red = NSColor::systemRedColor().colorWithAlphaComponent(0.35);
            for (p, l) in mark_ranges(&s) {
                // SAFETY: an NSColor is a valid value for the background colour key; the range lies in the text.
                unsafe {
                    lm.addTemporaryAttribute_value_forCharacterRange(key, &red, NSRange::new(p, l))
                };
            }
        }
        forbidden_utf16(&s).len() + forbidden_utf16(&self.subject.stringValue().to_string()).len()
    }

    /// Replaces the whole text as one undo step (Cmd+Z brings the old text back); the delegate hears of it.
    pub fn replace_body(&self, new: &str) {
        let all = NSRange::new(0, self.body.string().length());
        let keep = self.body.selectedRange().location;
        self.body.breakUndoCoalescing();
        if self
            .body
            .shouldChangeTextInRange_replacementString(all, Some(&ns(new)))
        {
            self.body.replaceCharactersInRange_withString(all, &ns(new));
            self.body.didChangeText();
            self.body.breakUndoCoalescing();
            let len = self.body.string().length();
            self.body.setSelectedRange(NSRange::new(keep.min(len), 0));
            self.refresh_preview();
        }
    }

    /// Removes the invisible characters: from the text as one undo step, from the subject directly.
    // ponytail: the subject's removal is not undoable; the characters are invisible and refused for sending anyway.
    pub fn strip_invisible(&self) {
        let text = self.body.string().to_string();
        let clean = strip_forbidden(&text);
        if clean != text {
            self.replace_body(&clean);
        }
        let subject = self.subject.stringValue().to_string();
        let clean = strip_forbidden(&subject);
        if clean != subject {
            self.subject.setStringValue(&ns(&clean));
        }
    }

    fn sender(&self) -> Option<&SenderChoice> {
        usize::try_from(self.from.indexOfSelectedItem())
            .ok()
            .and_then(|i| self.senders.get(i))
    }

    /// What the fields hold now.
    pub fn form(&self) -> Form {
        Form {
            from_address: self.sender().map(|s| s.address.clone()).unwrap_or_default(),
            from_name: self.from_name.stringValue().to_string(),
            to: tokens(&self.to),
            cc: tokens(&self.cc),
            bcc: tokens(&self.bcc),
            subject: self.subject.stringValue().to_string(),
            body: self.body.string().to_string(),
        }
    }

    /// Another sender picked: a fixed name is filled in and locked.
    pub fn from_changed(&self) {
        if let Some(s) = self.sender()
            && s.fixed
        {
            self.from_name.setStringValue(&ns(&s.name));
        }
        self.lock_name();
    }

    fn lock_name(&self) {
        let fixed = self.sender().is_some_and(|s| s.fixed);
        self.from_name
            .setEditable(!fixed && self.subject.isEditable());
        self.from_name
            .setEnabled(!fixed && self.subject.isEditable());
    }

    /// Only a `ready` draft without a countdown is editable.
    pub fn set_editable(&self, editable: bool) {
        self.from.setEnabled(editable);
        for f in [&self.to, &self.cc, &self.bcc] {
            f.setEditable(editable);
        }
        self.subject.setEditable(editable);
        self.body.setEditable(editable);
        self.lock_name();
        self.restrict_drops(editable);
    }

    /// The text takes dropped text only: a dropped file belongs to the attachments, not as its path into the text.
    /// AppKit registers the text view's own types again whenever its editability changes, so this follows every
    /// `setEditable`.
    // ponytail: re-applied by hand after setEditable; a subclass overriding `acceptableDragTypes` if AppKit ever
    // re-registers at another moment (checked on the Mac).
    fn restrict_drops(&self, editable: bool) {
        self.body.unregisterDraggedTypes();
        if editable {
            // SAFETY: a static AppKit constant.
            let text = unsafe { NSPasteboardTypeString };
            self.body
                .registerForDraggedTypes(&NSArray::from_slice(&[text]));
        }
    }
}

/// The tokens of a field, trimmed, without blanks (also text typed but not yet turned into a token).
pub fn tokens(f: &NSTokenField) -> Vec<String> {
    let Some(value) = f.objectValue() else {
        return Vec::new();
    };
    let Ok(array) = value.downcast::<NSArray>() else {
        return Vec::new();
    };
    array
        .iter()
        .filter_map(|o| o.downcast::<NSString>().ok())
        .map(|s| s.to_string().trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

pub fn set_tokens(f: &NSTokenField, list: &[String]) {
    let items: Vec<Retained<NSString>> = list.iter().map(|s| ns(s)).collect();
    let array = NSArray::from_retained_slice(&items);
    // SAFETY: an NSArray of NSStrings is the object value a token field takes.
    unsafe { f.setObjectValue(Some(&array)) };
}

#[cfg(test)]
mod tests {
    use super::mark_ranges;

    #[test]
    fn marks_take_a_visible_neighbour() {
        // Zero-width space at 1: marked with the "b" after it.
        assert_eq!(mark_ranges("a\u{200B}b"), vec![(1, 2)]);
        // At a line end: with the character before it.
        assert_eq!(mark_ranges("a\u{200B}\nb"), vec![(0, 2)]);
        // Alone: just itself.
        assert_eq!(mark_ranges("\u{202E}"), vec![(0, 1)]);
        // Neighbours outside the basic plane take two UTF-16 units.
        assert_eq!(mark_ranges("x\u{202E}\u{1F600}"), vec![(1, 3)]);
        assert_eq!(mark_ranges("\u{1F600}\u{200B}"), vec![(0, 3)]);
        assert!(mark_ranges("plain text\n").is_empty());
    }
}
