//! Small view helpers shared by the window's parts.

use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_app_kit::{
    NSImage, NSLayoutAttribute, NSLayoutConstraintOrientation, NSLineBreakMode, NSStackView,
    NSTextField, NSUserInterfaceLayoutOrientation, NSView,
};
use objc2_foundation::{NSArray, NSEdgeInsets, NSString};

pub fn ns(s: &str) -> Retained<NSString> {
    NSString::from_str(s)
}

/// A one-line label that truncates at the end instead of widening its container.
pub fn label(mtm: MainThreadMarker, text: &str) -> Retained<NSTextField> {
    let l = NSTextField::labelWithString(&ns(text), mtm);
    l.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    l.setContentCompressionResistancePriority_forOrientation(
        250.0,
        NSLayoutConstraintOrientation::Horizontal,
    );
    l
}

/// An SF Symbol image with its VoiceOver description; `None` on a macOS without it.
pub fn symbol(name: &str, description: &str) -> Option<Retained<NSImage>> {
    NSImage::imageWithSystemSymbolName_accessibilityDescription(&ns(name), Some(&ns(description)))
}

/// A stack of views, leading-aligned when vertical.
pub fn stack(
    mtm: MainThreadMarker,
    views: &[&NSView],
    vertical: bool,
    spacing: f64,
) -> Retained<NSStackView> {
    let s = NSStackView::stackViewWithViews(&NSArray::from_slice(views), mtm);
    if vertical {
        s.setOrientation(NSUserInterfaceLayoutOrientation::Vertical);
        s.setAlignment(NSLayoutAttribute::Leading);
    } else {
        s.setOrientation(NSUserInterfaceLayoutOrientation::Horizontal);
    }
    s.setSpacing(spacing);
    s
}

pub fn insets(top: f64, left: f64, bottom: f64, right: f64) -> NSEdgeInsets {
    NSEdgeInsets {
        top,
        left,
        bottom,
        right,
    }
}
