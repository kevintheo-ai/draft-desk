//! The follow-up under the text: a checkbox with the agent's label, a line of text and one pop-up menu per choice.
//! Shown only for a draft that has one; it goes into the form and is saved like the other fields.

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{MainThreadMarker, sel};
use objc2_app_kit::{
    NSAccessibility, NSButton, NSColor, NSControlStateValueOff, NSControlStateValueOn,
    NSLayoutConstraintOrientation, NSPopUpButton, NSStackView, NSTextField,
};

use draftdesk_core::follow_up::{Answer, FollowUp, MAX_CHOICES};

use crate::app::Delegate;
use crate::i18n::tr;
use crate::ui::{label, ns, stack};

pub struct FollowUpRow {
    pub view: Retained<NSStackView>,
    pub check: Retained<NSButton>,
    pub text: Retained<NSTextField>,
    choices: Retained<NSStackView>,
    /// One name and pop-up menu per possible choice, built once; the unused ones are hidden.
    names: Vec<Retained<NSTextField>>,
    pub popups: Vec<Retained<NSPopUpButton>>,
    /// How many choices the shown draft has; `None`: it has no follow-up.
    shown: Option<usize>,
}

impl FollowUpRow {
    pub fn new(mtm: MainThreadMarker, delegate: &Delegate) -> FollowUpRow {
        let target: &AnyObject = delegate.as_ref();
        // SAFETY: the target is the app delegate, which lives as long as the app.
        let check = unsafe {
            NSButton::checkboxWithTitle_target_action(
                &ns(""),
                Some(target),
                Some(sel!(followUpChanged:)),
                mtm,
            )
        };
        let text = NSTextField::new(mtm);
        // SAFETY: the delegate lives as long as the app.
        unsafe { text.setDelegate(Some(ProtocolObject::from_ref(delegate))) };
        text.setPlaceholderString(Some(&ns(tr(
            "Was danach geschehen soll",
            "What should happen then",
        ))));
        text.setContentHuggingPriority_forOrientation(
            1.0,
            NSLayoutConstraintOrientation::Horizontal,
        );
        let top = stack(mtm, &[&check, &text], false, 8.0);
        let (mut names, mut popups) = (Vec::new(), Vec::new());
        let choices = stack(mtm, &[], false, 6.0);
        for _ in 0..MAX_CHOICES {
            let name = label(mtm, "");
            name.setTextColor(Some(&NSColor::secondaryLabelColor()));
            let popup = NSPopUpButton::new(mtm);
            // SAFETY: as above.
            unsafe {
                popup.setTarget(Some(target));
                popup.setAction(Some(sel!(followUpChanged:)));
            }
            choices.addArrangedSubview(&name);
            choices.addArrangedSubview(&popup);
            names.push(name);
            popups.push(popup);
        }
        let view = stack(mtm, &[&top, &choices], true, 6.0);
        top.setTranslatesAutoresizingMaskIntoConstraints(false);
        top.widthAnchor()
            .constraintEqualToAnchor(&view.widthAnchor())
            .setActive(true);
        view.setHidden(true);
        FollowUpRow {
            view,
            check,
            text,
            choices,
            names,
            popups,
            shown: None,
        }
    }

    /// Fills the row from the draft; hidden when it has no follow-up.
    pub fn load(&mut self, f: Option<&FollowUp>) {
        self.shown = f.map(|f| f.choices.len());
        let Some(f) = f else {
            self.view.setHidden(true);
            return;
        };
        self.check.setTitle(&ns(&f.label));
        self.text.setStringValue(&ns(&f.text));
        self.text.setAccessibilityLabel(Some(&ns(&f.label)));
        for (k, (name, popup)) in self.names.iter().zip(&self.popups).enumerate() {
            let c = f.choices.get(k);
            name.setHidden(c.is_none());
            popup.setHidden(c.is_none());
            popup.removeAllItems();
            let Some(c) = c else { continue };
            name.setStringValue(&ns(&format!("{}:", c.name)));
            popup.setAccessibilityLabel(Some(&ns(&c.name)));
            for o in &c.options {
                popup.addItemWithTitle(&ns(o));
            }
        }
        self.choices.setHidden(f.choices.is_empty());
        self.show_answer(f);
        self.view.setHidden(false);
    }

    /// Checkbox, text and selections as saved, touching only what differs (the person may be typing).
    pub fn redraw(&self, f: Option<&FollowUp>) {
        if let Some(f) = f
            && self.shown.is_some()
        {
            self.show_answer(f);
        }
    }

    fn show_answer(&self, f: &FollowUp) {
        let state = if f.checked {
            NSControlStateValueOn
        } else {
            NSControlStateValueOff
        };
        if self.check.state() != state {
            self.check.setState(state);
        }
        if self.text.stringValue().to_string() != f.text {
            self.text.setStringValue(&ns(&f.text));
        }
        for (c, popup) in f.choices.iter().zip(&self.popups) {
            if popup.titleOfSelectedItem().map(|t| t.to_string()) != Some(c.selected.clone()) {
                popup.selectItemWithTitle(&ns(&c.selected));
            }
        }
    }

    /// What the row holds; `None` for a draft without a follow-up.
    pub fn answer(&self) -> Option<Answer> {
        let n = self.shown?;
        Some(Answer {
            checked: self.check.state() == NSControlStateValueOn,
            text: self.text.stringValue().to_string(),
            selected: self.popups[..n]
                .iter()
                .map(|p| {
                    p.titleOfSelectedItem()
                        .map(|t| t.to_string())
                        .unwrap_or_default()
                })
                .collect(),
        })
    }

    /// Like the other fields: only a `ready` draft without a countdown takes an answer.
    pub fn set_editable(&self, editable: bool) {
        self.check.setEnabled(editable);
        self.text.setEditable(editable);
        self.text.setEnabled(editable);
        for p in &self.popups {
            p.setEnabled(editable);
        }
    }
}
