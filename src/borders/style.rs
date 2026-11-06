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

/// The window facts [`CornerPreference::resolve`] decides from, read by
/// `registry::win32::corner_facts`: the chrome bits of its style and whether
/// its client area covers the whole window rect (no non-client frame).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CornerFacts {
    /// `WS_CAPTION | WS_THICKFRAME` masked out of `GWL_STYLE` (`0`: neither).
    pub chrome: u32,
    /// The client rect on screen equals the window rect within 1 px: the
    /// app answers `WM_NCCALCSIZE` with no frame (winit / Alacritty with
    /// `decorations = "none"`, which keeps both chrome bits).
    pub client_covers: bool,
}

impl CornerPreference {
    /// The corners Windows 11 actually draws for a window whose DWM
    /// preference reads `dwm` (`None`: unreadable) and whose
    /// [`CornerFacts`] are `facts` (`None`: the handle is gone). An explicit
    /// preference is kept; the default resolves from the facts: Windows
    /// rounds a top-level window only when it has a non-client frame, so a
    /// window with neither chrome bit (a borderless popup) or whose client
    /// area covers the whole window (`window.decorations = "none"`) is
    /// square. See `docs/src/dev-guide/borders.md`, *Corner preference*,
    /// and ADR-0027.
    #[must_use]
    pub const fn resolve(dwm: Option<Self>, facts: Option<CornerFacts>) -> Self {
        match (dwm, facts) {
            (Some(explicit @ (Self::Square | Self::Rounded | Self::RoundedSmall)), _) => explicit,
            (_, Some(CornerFacts { chrome: 0, .. }))
            | (
                _,
                Some(CornerFacts {
                    client_covers: true,
                    ..
                }),
            ) => Self::Square,
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
    use super::{CornerFacts, CornerPreference as C};

    const WS_CAPTION: u32 = 0x00C0_0000;
    const WS_THICKFRAME: u32 = 0x0004_0000;

    fn f(chrome: u32, client_covers: bool) -> Option<CornerFacts> {
        Some(CornerFacts {
            chrome,
            client_covers,
        })
    }

    #[test]
    fn an_explicit_dwm_preference_is_kept_whatever_the_facts() {
        for explicit in [C::Square, C::Rounded, C::RoundedSmall] {
            assert_eq!(C::resolve(Some(explicit), f(0, true)), explicit);
            assert_eq!(C::resolve(Some(explicit), f(0x00C4_0000, false)), explicit);
            assert_eq!(
                C::resolve(Some(explicit), f(WS_CAPTION | WS_THICKFRAME, true)),
                explicit
            );
            assert_eq!(C::resolve(Some(explicit), None), explicit);
        }
    }

    #[test]
    fn the_default_is_square_without_caption_or_sizing_frame() {
        // A WinForms `FormBorderStyle = None`: no chrome, and its client
        // covers the window anyway.
        assert_eq!(C::resolve(Some(C::Default), f(0, true)), C::Square);
        assert_eq!(C::resolve(None, f(0, false)), C::Square);
    }

    #[test]
    fn the_default_is_square_when_the_client_covers_the_window() {
        // Alacritty `decorations = "none"` (winit): WS_CAPTION and
        // WS_THICKFRAME kept, WM_NCCALCSIZE removes the whole frame.
        for chrome in [WS_CAPTION | WS_THICKFRAME, WS_THICKFRAME, WS_CAPTION] {
            assert_eq!(C::resolve(Some(C::Default), f(chrome, true)), C::Square);
            assert_eq!(C::resolve(None, f(chrome, true)), C::Square);
        }
    }

    #[test]
    fn the_default_stays_rounded_with_a_frame() {
        for chrome in [
            WS_CAPTION,
            WS_THICKFRAME,
            WS_CAPTION | WS_THICKFRAME,
            0x0080_0000,
        ] {
            assert_eq!(C::resolve(Some(C::Default), f(chrome, false)), C::Default);
        }
        // A handle that is gone: nothing to tell, the renderer default.
        assert_eq!(C::resolve(Some(C::Default), None), C::Default);
        assert_eq!(C::resolve(None, None), C::Default);
    }
}
