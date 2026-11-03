//! Style types passed from the daemon to the border crate.

use crate::config::Color;

/// Per-window border styling.
///
/// Constructed by the daemon (typically via [`crate::style_for_state`]) and
/// passed to [`crate::Border::create`] / [`crate::Border::set_style`]. The
/// border reads these values each time it redraws the overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BorderStyle {
    /// Border ring color.
    pub color: Color,
    /// Border ring thickness in pixels, applied uniformly on all four sides.
    pub width_px: u32,
    /// The target window's corners, which the ring follows
    /// ([`CornerPreference::resolve`]).
    pub corner_preference: CornerPreference,
}

impl BorderStyle {
    /// Construct a new style from the three independent values.
    #[must_use]
    pub const fn new(color: Color, width_px: u32, corner_preference: CornerPreference) -> Self {
        Self {
            color,
            width_px,
            corner_preference,
        }
    }
}

/// Corner shape of the target window, which the overlay ring follows
/// (`DWMWA_WINDOW_CORNER_PREFERENCE`, resolved by [`CornerPreference::resolve`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CornerPreference {
    /// Let Windows decide (typically rounded on Win11).
    #[default]
    Default,
    /// Square corners (don't round).
    Square,
    /// Rounded corners.
    Rounded,
    /// Small rounded corners.
    RoundedSmall,
}

impl CornerPreference {
    /// The corners Windows 11 actually draws for a window whose DWM
    /// preference reads `dwm` (`None`: unreadable) and whose chrome bits
    /// (`WS_CAPTION | WS_THICKFRAME` of its style) are `chrome` (`None`:
    /// the handle is gone). An explicit preference is kept; the default
    /// resolves from the styles: Windows rounds a top-level window only
    /// when it has a caption or a sizing frame, so a window with neither
    /// (`window.decorations = "none"`, a borderless popup) is square. See
    /// `docs/src/dev-guide/borders.md`, *Corner preference*.
    #[must_use]
    pub const fn resolve(dwm: Option<Self>, chrome: Option<u32>) -> Self {
        match (dwm, chrome) {
            (Some(explicit @ (Self::Square | Self::Rounded | Self::RoundedSmall)), _) => explicit,
            (_, Some(0)) => Self::Square,
            _ => Self::Default,
        }
    }

    /// The name `flow query all` reports for this preference
    /// (`border_corner`).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Square => "square",
            Self::Rounded => "rounded",
            Self::RoundedSmall => "rounded-small",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::CornerPreference as C;

    #[test]
    fn an_explicit_dwm_preference_is_kept_whatever_the_styles() {
        for explicit in [C::Square, C::Rounded, C::RoundedSmall] {
            assert_eq!(C::resolve(Some(explicit), Some(0)), explicit);
            assert_eq!(C::resolve(Some(explicit), Some(0x00C4_0000)), explicit);
            assert_eq!(C::resolve(Some(explicit), None), explicit);
        }
    }

    #[test]
    fn the_default_is_square_without_caption_or_sizing_frame() {
        // Alacritty `decorations = "none"`, a WinForms `FormBorderStyle = None`.
        assert_eq!(C::resolve(Some(C::Default), Some(0)), C::Square);
        assert_eq!(C::resolve(None, Some(0)), C::Square);
    }

    #[test]
    fn the_default_stays_rounded_with_any_chrome_bit() {
        const WS_CAPTION: u32 = 0x00C0_0000;
        const WS_THICKFRAME: u32 = 0x0004_0000;
        for chrome in [
            WS_CAPTION,
            WS_THICKFRAME,
            WS_CAPTION | WS_THICKFRAME,
            0x0080_0000,
        ] {
            assert_eq!(C::resolve(Some(C::Default), Some(chrome)), C::Default);
        }
        // A handle that is gone: nothing to tell, the renderer default.
        assert_eq!(C::resolve(Some(C::Default), None), C::Default);
        assert_eq!(C::resolve(None, None), C::Default);
    }
}
