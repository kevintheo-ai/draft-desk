//! Attachments: the strip under the subject (a button per file for Quick Look, a remove button, "Add Attachment…"),
//! files dropped onto the editor (`DropView`) and the list Quick Look shows. A file is never handed to another
//! program: a `.command` file would run.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAccessibility, NSButton, NSDragOperation, NSDraggingDestination, NSDraggingInfo,
    NSLayoutConstraint, NSPasteboardTypeFileURL, NSScrollView, NSStackView, NSView, NSWorkspace,
};
use objc2_foundation::{
    NSArray, NSByteCountFormatter, NSByteCountFormatterCountStyle, NSObjectProtocol, NSSize, NSURL,
};

use draftdesk_core::item::Item;

use crate::app::{Delegate, with_app, with_app_now};
use crate::i18n::tr;
use crate::ui::{ns, stack, symbol};

/// What Quick Look may show (as in the Python version); anything else gets a hint instead.
pub const QUICKLOOK: &[&str] = &[
    "pdf", "png", "jpg", "jpeg", "gif", "txt", "csv", "docx", "xlsx", "pptx",
];

pub fn quicklook_ok(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| QUICKLOOK.contains(&e.to_ascii_lowercase().as_str()))
}

thread_local! {
    /// The files Quick Look shows. Its own slot: the panel asks while the app may be busy.
    static PREVIEW: RefCell<Vec<Retained<NSURL>>> = const { RefCell::new(Vec::new()) };
}

pub fn set_preview_items(urls: Vec<Retained<NSURL>>) {
    PREVIEW.with(|p| *p.borrow_mut() = urls);
}

pub fn preview_count() -> usize {
    PREVIEW.with(|p| p.borrow().len())
}

pub fn preview_item(index: usize) -> Option<Retained<NSURL>> {
    PREVIEW.with(|p| p.borrow().get(index).cloned())
}

/// The file paths a drag carries; file URLs only.
fn dragged_paths(info: &ProtocolObject<dyn NSDraggingInfo>) -> Vec<PathBuf> {
    // SAFETY: a static AppKit constant.
    let file_url = unsafe { NSPasteboardTypeFileURL };
    let Some(items) = info.draggingPasteboard().pasteboardItems() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| -> Option<PathBuf> {
            let text = item.stringForType(file_url)?;
            let url = NSURL::URLWithString(&text)?;
            // Finder hands over file reference URLs (file:///.file/id=...); the path URL resolves them.
            let path = url.filePathURL()?.path()?;
            Some(PathBuf::from(path.to_string()))
        })
        .collect()
}

/// A drag over the editor (text or preview): copy, when the shown draft is editable and the drag holds files.
pub fn drag_operation(info: &ProtocolObject<dyn NSDraggingInfo>) -> NSDragOperation {
    let open = with_app_now(|a| a.accepts_drop()).unwrap_or(false);
    if open && !dragged_paths(info).is_empty() {
        NSDragOperation::Copy
    } else {
        NSDragOperation::None
    }
}

/// The drop: the files become attachments of the shown draft (`add_files` checks again).
pub fn drop_files(info: &ProtocolObject<dyn NSDraggingInfo>) -> bool {
    let paths = dragged_paths(info);
    let any = !paths.is_empty();
    if any {
        with_app(move |a| a.add_files(paths));
    }
    any
}

define_class!(
    /// The view the editor sits in: files dropped anywhere on it become attachments of an editable draft.
    // SAFETY: NSView has no subclassing requirements, and `DropView` implements no `Drop`.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "DraftDeskDropView"]
    pub struct DropView;

    unsafe impl NSObjectProtocol for DropView {}

    unsafe impl NSDraggingDestination for DropView {
        #[unsafe(method(draggingEntered:))]
        fn dragging_entered(&self, info: &ProtocolObject<dyn NSDraggingInfo>) -> NSDragOperation {
            drag_operation(info)
        }

        #[unsafe(method(performDragOperation:))]
        fn perform_drag_operation(&self, info: &ProtocolObject<dyn NSDraggingInfo>) -> bool {
            drop_files(info)
        }
    }
);

impl DropView {
    pub fn new(mtm: MainThreadMarker) -> Retained<DropView> {
        let this = Self::alloc(mtm).set_ivars(());
        // SAFETY: NSView's initializer.
        let view: Retained<DropView> = unsafe { msg_send![super(this), init] };
        // SAFETY: a static AppKit constant.
        let file_url = unsafe { NSPasteboardTypeFileURL };
        view.registerForDraggedTypes(&NSArray::from_slice(&[file_url]));
        view
    }
}

/// "12 KB", "1.2 MB", as Finder writes sizes.
fn size_text(bytes: u64) -> String {
    let n = i64::try_from(bytes).unwrap_or(i64::MAX);
    NSByteCountFormatter::stringFromByteCount_countStyle(n, NSByteCountFormatterCountStyle::File)
        .to_string()
}

/// The strip of attachments, scrolling sideways when there are more than fit.
pub struct Strip {
    pub view: Retained<NSScrollView>,
    row: Retained<NSStackView>,
}

impl Strip {
    pub fn new(mtm: MainThreadMarker) -> Strip {
        let row = stack(mtm, &[], false, 6.0);
        let view = NSScrollView::new(mtm);
        view.setHasHorizontalScroller(true);
        view.setHasVerticalScroller(false);
        view.setAutohidesScrollers(true);
        view.setDrawsBackground(false);
        view.setDocumentView(Some(&row));
        row.setTranslatesAutoresizingMaskIntoConstraints(false);
        let clip = view.contentView();
        NSLayoutConstraint::activateConstraints(&NSArray::from_retained_slice(&[
            row.leadingAnchor()
                .constraintEqualToAnchor(&clip.leadingAnchor()),
            row.topAnchor().constraintEqualToAnchor(&clip.topAnchor()),
            row.bottomAnchor()
                .constraintEqualToAnchor(&clip.bottomAnchor()),
        ]));
        view.heightAnchor()
            .constraintEqualToConstant(30.0)
            .setActive(true);
        view.setAccessibilityLabel(Some(&ns(tr("Anhänge", "Attachments"))));
        view.setHidden(true);
        Strip { view, row }
    }

    /// One button per attachment of `item` (Quick Look), each with a remove button when `editable`, then
    /// "Add Attachment…". `dir` is the item's folder (for the file icons). Hidden when there is nothing to show.
    pub fn show(
        &self,
        mtm: MainThreadMarker,
        delegate: &Delegate,
        item: Option<&Item>,
        dir: Option<&Path>,
        editable: bool,
    ) {
        for v in self.row.views().iter() {
            self.row.removeView(&v);
        }
        let target: &AnyObject = delegate.as_ref();
        let list = item.map(|i| i.attachments.as_slice()).unwrap_or_default();
        for (i, a) in list.iter().enumerate() {
            let tag = i as isize;
            let title = format!("{} ({})", a.name, size_text(a.size));
            let path = dir.map(|d| d.join("att").join(&a.name));
            let icon = match &path {
                Some(p) => NSWorkspace::sharedWorkspace().iconForFile(&ns(&p.to_string_lossy())),
                None => continue,
            };
            icon.setSize(NSSize::new(16.0, 16.0));
            // SAFETY: the target is the app delegate, which lives as long as the app.
            let open = unsafe {
                NSButton::buttonWithTitle_image_target_action(
                    &ns(&title),
                    &icon,
                    Some(target),
                    Some(sel!(openAttachment:)),
                    mtm,
                )
            };
            open.setTag(tag);
            open.setAccessibilityLabel(Some(&ns(&format!(
                "{} {title}",
                tr("Anhang ansehen:", "View attachment:")
            ))));
            self.row.addArrangedSubview(&open);
            if editable
                && let Some(img) = symbol(
                    "xmark.circle.fill",
                    &format!("{} {}", tr("Entfernen:", "Remove:"), a.name),
                )
            {
                // SAFETY: as above.
                let remove = unsafe {
                    NSButton::buttonWithImage_target_action(
                        &img,
                        Some(target),
                        Some(sel!(removeAttachment:)),
                        mtm,
                    )
                };
                remove.setBordered(false);
                remove.setTag(tag);
                remove.setAccessibilityLabel(Some(&ns(&format!(
                    "{} {}",
                    tr("Anhang entfernen:", "Remove attachment:"),
                    a.name
                ))));
                self.row.addArrangedSubview(&remove);
            }
        }
        if editable {
            // SAFETY: as above.
            let add = unsafe {
                NSButton::buttonWithTitle_target_action(
                    &ns(tr("Anhang hinzufügen …", "Add Attachment…")),
                    Some(target),
                    Some(sel!(addAttachment:)),
                    mtm,
                )
            };
            self.row.addArrangedSubview(&add);
        }
        self.view.setHidden(list.is_empty() && !editable);
    }

    fn buttons(&self, action: objc2::runtime::Sel) -> Vec<Retained<NSButton>> {
        self.row
            .views()
            .iter()
            .filter_map(|v| v.downcast::<NSButton>().ok())
            .filter(|b| b.action() == Some(action))
            .collect()
    }

    /// The titles of the attachment buttons.
    pub fn titles(&self) -> Vec<String> {
        self.buttons(sel!(openAttachment:))
            .iter()
            .map(|b| b.title().to_string())
            .collect()
    }

    /// The remove button of attachment `index`.
    pub fn remove_button(&self, index: usize) -> Option<Retained<NSButton>> {
        self.buttons(sel!(removeAttachment:))
            .into_iter()
            .find(|b| b.tag() == index as isize)
    }
}

#[cfg(test)]
mod tests {
    use super::quicklook_ok;

    #[test]
    fn quicklook_allowlist() {
        for ok in ["a.pdf", "Scan.PDF", "x.jpeg", "Table.xlsx", "notes.txt"] {
            assert!(quicklook_ok(ok), "{ok}");
        }
        for no in [
            "run.command",
            "a.app",
            "b.sh",
            "noext",
            ".pdf",
            "archive.tar.gz",
            "page.html",
        ] {
            assert!(!quicklook_ok(no), "{no}");
        }
    }
}
