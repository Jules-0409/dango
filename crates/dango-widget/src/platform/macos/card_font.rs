//! Font resolution for card text (Optima, Avenir Next, SF Mono / PingFang SC).

use objc2::rc::Retained;
use objc2_app_kit::NSFont;
use objc2_foundation::NSString;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FontKind {
    Display,
    Ui,
    Mono,
}

pub fn font_by_kind(kind: FontKind, size: f64, weight: u32) -> Retained<NSFont> {
    match kind {
        FontKind::Display => display_font_for(size, weight),
        FontKind::Ui => font_for(size, weight),
        FontKind::Mono => mono_font_for_weight(size, weight),
    }
}

pub fn display_font_for(size: f64, weight: u32) -> Retained<NSFont> {
    let name = if weight >= 600 {
        "Optima-Bold"
    } else if weight >= 500 {
        "Optima-DemiBold"
    } else {
        "Optima-Regular"
    };
    let ns_name = NSString::from_str(name);
    unsafe {
        NSFont::fontWithName_size(&ns_name, size)
            .or_else(|| NSFont::fontWithName_size(&NSString::from_str("Optima"), size))
            .unwrap_or_else(|| font_for(size, weight))
    }
}

/// A font for the given point size and CSS-ish weight using Avenir Next with
/// fallback to system font (PingFang SC).
pub fn font_for(size: f64, weight: u32) -> Retained<NSFont> {
    let name = if weight >= 700 {
        "AvenirNext-Bold"
    } else if weight >= 600 {
        "AvenirNext-DemiBold"
    } else if weight >= 500 {
        "AvenirNext-Medium"
    } else {
        "AvenirNext-Regular"
    };
    let ns_name = NSString::from_str(name);
    if let Some(font) = unsafe { NSFont::fontWithName_size(&ns_name, size) } {
        return font;
    }
    // Fallback to system font (SF Pro / PingFang SC)
    let ns_weight = match weight {
        0..=400 => 0.0,
        500 => 0.23,
        600 => 0.30,
        _ => 0.40,
    };
    unsafe { NSFont::systemFontOfSize_weight(size, ns_weight) }
}

/// Monospaced font for numbers and countdowns (SF Mono / Menlo).
pub fn mono_font_for(size: f64) -> Retained<NSFont> {
    mono_font_for_weight(size, 400)
}

pub fn mono_font_for_weight(size: f64, weight: u32) -> Retained<NSFont> {
    let name = if weight >= 600 {
        "SFMono-Bold"
    } else if weight >= 500 {
        "SFMono-Medium"
    } else {
        "SFMono-Regular"
    };
    let ns_name = NSString::from_str(name);
    if let Some(font) = unsafe { NSFont::fontWithName_size(&ns_name, size) } {
        return font;
    }
    let menlo_name = if weight >= 600 { "Menlo-Bold" } else { "Menlo" };
    if let Some(font) = unsafe { NSFont::fontWithName_size(&NSString::from_str(menlo_name), size) }
    {
        return font;
    }
    let ns_weight = if weight >= 600 { 0.30 } else { 0.0 };
    unsafe { NSFont::monospacedDigitSystemFontOfSize_weight(size, ns_weight) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_font_loading() {
        let display = display_font_for(14.0, 600);
        let name = unsafe { display.fontName() };
        assert!(!name.to_string().is_empty());

        let ui = font_for(11.0, 500);
        let ui_name = unsafe { ui.fontName() };
        assert!(!ui_name.to_string().is_empty());

        let mono = mono_font_for(10.0);
        let mono_name = unsafe { mono.fontName() };
        assert!(!mono_name.to_string().is_empty());
    }
}
