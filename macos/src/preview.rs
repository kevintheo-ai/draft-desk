//! The preview (a switch "Text | Preview" above the text): the HTML part of the mail exactly as the core builds it
//! for sending. A WKWebView without JavaScript, without storage and without navigation; the core's HTML links to
//! nothing, the CSP refuses every load on top of that.

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSAccessibility, NSDragOperation, NSDraggingDestination, NSDraggingInfo,
    NSPasteboardTypeFileURL, NSResponder, NSView,
};
use objc2_foundation::{NSArray, NSObject, NSObjectProtocol, NSRect, NSURL};
use objc2_web_kit::{WKWebView, WKWebViewConfiguration, WKWebsiteDataStore};

use draftdesk_core::html::{body_to_html, unwrap_body};

use crate::app::Delegate;
use crate::attachments;
use crate::i18n::tr;
use crate::ui::ns;

/// Refuses every load: no images, scripts, fonts or frames; inline styles only.
pub const CSP: &str = "<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; style-src 'unsafe-inline'\">";

/// The HTML part as it is sent (built from the rewrapped text, like `mime::build`), with [`CSP`] first in the head.
pub fn preview_html(body: &str) -> String {
    body_to_html(&unwrap_body(body)).replacen("<head>", &format!("<head>{CSP}"), 1)
}

/// The page itself is the only place the preview may go: `about:blank`, its base URL.
pub fn allow_navigation(url: &str) -> bool {
    url == "about:blank" || url.starts_with("about:blank#")
}

define_class!(
    /// The web view of the preview. A file dropped on it becomes an attachment, as on the text: WebKit's own drop
    /// handling swallowed it without a word, also with no drop types registered (found on the Mac 2026-11-07), so
    /// every step of a drop is answered here.
    // SAFETY: WKWebView may be subclassed; `Preview` adds no state and implements no `Drop`.
    #[unsafe(super(WKWebView, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "DraftDeskPreview"]
    pub struct Preview;

    unsafe impl NSObjectProtocol for Preview {}

    unsafe impl NSDraggingDestination for Preview {
        #[unsafe(method(draggingEntered:))]
        fn dragging_entered(&self, info: &ProtocolObject<dyn NSDraggingInfo>) -> NSDragOperation {
            attachments::drag_operation(info)
        }

        #[unsafe(method(draggingUpdated:))]
        fn dragging_updated(&self, info: &ProtocolObject<dyn NSDraggingInfo>) -> NSDragOperation {
            attachments::drag_operation(info)
        }

        #[unsafe(method(draggingExited:))]
        fn dragging_exited(&self, _info: Option<&ProtocolObject<dyn NSDraggingInfo>>) {}

        #[unsafe(method(prepareForDragOperation:))]
        fn prepare_for_drag_operation(&self, _info: &ProtocolObject<dyn NSDraggingInfo>) -> bool {
            true
        }

        #[unsafe(method(performDragOperation:))]
        fn perform_drag_operation(&self, info: &ProtocolObject<dyn NSDraggingInfo>) -> bool {
            attachments::drop_files(info)
        }

        #[unsafe(method(concludeDragOperation:))]
        fn conclude_drag_operation(&self, _info: Option<&ProtocolObject<dyn NSDraggingInfo>>) {}
    }
);

/// The web view: no JavaScript, no windows, nothing stored; a dropped file becomes an attachment, never a page. The
/// delegate decides every navigation.
pub fn web_view(mtm: MainThreadMarker, delegate: &Delegate) -> Retained<WKWebView> {
    // SAFETY: a fresh configuration and plain setters on it, main thread.
    let config = unsafe { WKWebViewConfiguration::new(mtm) };
    // SAFETY: as above.
    unsafe {
        config.setWebsiteDataStore(&WKWebsiteDataStore::nonPersistentDataStore(mtm));
        config
            .defaultWebpagePreferences()
            .setAllowsContentJavaScript(false);
        config
            .preferences()
            .setJavaScriptCanOpenWindowsAutomatically(false);
    }
    let this = Preview::alloc(mtm).set_ivars(());
    // SAFETY: WKWebView's designated initializer on a fresh allocation with a valid configuration.
    let view: Retained<Preview> =
        unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO, configuration: &*config] };
    let view: Retained<WKWebView> = Retained::into_super(view);
    // SAFETY: the delegate lives as long as the app.
    unsafe { view.setNavigationDelegate(Some(ProtocolObject::from_ref(delegate))) };
    // Files only (WebKit registered text, links and images too).
    view.unregisterDraggedTypes();
    // SAFETY: a static AppKit constant.
    let file_url = unsafe { NSPasteboardTypeFileURL };
    view.registerForDraggedTypes(&NSArray::from_slice(&[file_url]));
    view.setAccessibilityLabel(Some(&ns(tr("Vorschau", "Preview"))));
    view
}

/// Shows `body` as it will be sent.
pub fn show(view: &WKWebView, body: &str) {
    // The base URL is set, not nil: the page is then `about:blank` for sure.
    let base = NSURL::URLWithString(&ns("about:blank"));
    // SAFETY: a string and an optional URL, main thread; the returned navigation is not needed.
    let _ = unsafe { view.loadHTMLString_baseURL(&ns(&preview_html(body)), base.as_deref()) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csp_inserted_once_rest_identical() {
        let body = "Hello <head> & you,\n\nthanks.\n";
        let sent = body_to_html(&unwrap_body(body));
        let shown = preview_html(body);
        assert_eq!(shown.matches("Content-Security-Policy").count(), 1);
        assert_eq!(shown.replacen(CSP, "", 1), sent);
        assert!(shown.starts_with(&format!("<!DOCTYPE html><html><head>{CSP}")));
    }

    #[test]
    fn preview_is_the_html_part_that_is_sent() {
        // A hard-wrapped paragraph: the mail joins its lines before it builds the HTML part (`mime::build`).
        let body =
            "Hello Ann,\n\nthe offer for Friday is\nattached as promised.\n\nKind regards\nMe\n";
        assert_ne!(unwrap_body(body), body, "an example the core rewraps");
        let shown = preview_html(body).replacen(CSP, "", 1);
        assert_eq!(shown, body_to_html(&unwrap_body(body)));
    }

    #[test]
    fn only_about_blank_navigates() {
        assert!(allow_navigation("about:blank"));
        assert!(allow_navigation("about:blank#top"));
        for url in [
            "",
            "about:blankx",
            "https://example.org/",
            "file:///etc/passwd",
            "data:text/html,x",
            "javascript:alert(1)",
            "mailto:a@example.org",
        ] {
            assert!(!allow_navigation(url), "{url}");
        }
    }
}
