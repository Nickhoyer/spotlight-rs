//! Shared visual tokens: the cyan-on-navy palette and a few helpers. Kept in one
//! place so the shell and view-extensions (e.g. Jira) render consistently.

use gpui::{rgb, rgba, Rgba};

/// Bright cyan accent (search icon, caret, selection, focus).
pub const ACCENT: u32 = 0x6e_e7ff;
/// Primary text.
pub const TEXT: u32 = 0xe8_ecf4;
/// Secondary / muted text.
pub const MUTED: u32 = 0x8a_93a6;

/// Deep-navy panel background (RGBA, slightly translucent).
pub const PANEL_BG: u32 = 0x12_141c_f2;
/// Translucent cyan panel border (RGBA).
pub const BORDER: u32 = 0x6e_e7ff_40;
/// Subtle white hairline divider (RGBA).
pub const DIVIDER: u32 = 0xff_ffff_14;
/// Faint cyan fill for the selected/active row or chip (RGBA).
pub const SELECTED: u32 = 0x6e_e7ff_1f;
/// Faint cyan fill for letter tiles / icon chips (RGBA).
pub const TILE: u32 = 0x6e_e7ff_22;
/// Neutral backing for icon tiles — a hair lighter than the panel so transparent
/// icons and glyphs sit on a subtle solid tile rather than floating (RGBA).
pub const ICON_BG: u32 = 0xff_ffff_12;
/// Hover wash for clickable rows/buttons (RGBA).
pub const HOVER: u32 = 0x6e_e7ff_14;
/// Stronger hover for buttons that already have a faint cyan fill (so the
/// hover is clearly brighter than the resting state).
pub const HOVER_STRONG: u32 = 0x6e_e7ff_3a;

pub fn accent() -> Rgba {
    rgb(ACCENT)
}
pub fn text() -> Rgba {
    rgb(TEXT)
}
pub fn muted() -> Rgba {
    rgb(MUTED)
}
pub fn panel_bg() -> Rgba {
    rgba(PANEL_BG)
}
/// The panel background, fully opaque — the solid end of edge fades (so the
/// gradient actually masks clipped content rather than ghosting it).
pub fn panel_opaque() -> Rgba {
    rgba(PANEL_BG | 0x00_0000_ff)
}
/// The panel background with zero alpha — the transparent end of edge fades.
pub fn panel_transparent() -> Rgba {
    rgba(PANEL_BG & 0xff_ffff_00)
}
pub fn border() -> Rgba {
    rgba(BORDER)
}
pub fn divider() -> Rgba {
    rgba(DIVIDER)
}
pub fn selected() -> Rgba {
    rgba(SELECTED)
}
pub fn tile() -> Rgba {
    rgba(TILE)
}
pub fn icon_bg() -> Rgba {
    rgba(ICON_BG)
}
pub fn hover() -> Rgba {
    rgba(HOVER)
}
pub fn hover_strong() -> Rgba {
    rgba(HOVER_STRONG)
}

/// The `0xRRGGBB` part of an `0xRRGGBBAA` token — the color it resolves to over
/// an identical backdrop.
pub fn opaque_rgb(rgba: u32) -> u32 {
    rgba >> 8
}

/// Composite an `0xRRGGBBAA` wash over an opaque `0xRRGGBB` base, giving the
/// solid color the pair resolves to.
///
/// Anything rendered as an image (the HTML reading panes) can't participate in
/// the panel's alpha blending, so it needs the flattened color rather than the
/// translucent token.
pub fn wash(base_rgb: u32, over_rgba: u32) -> u32 {
    let alpha = (over_rgba & 0xff) as f32 / 255.0;
    let mix = |shift: u32| {
        let base = ((base_rgb >> shift) & 0xff) as f32;
        let over = ((over_rgba >> (shift + 8)) & 0xff) as f32;
        (base + (over - base) * alpha).round().clamp(0.0, 255.0) as u32
    };
    mix(16) << 16 | mix(8) << 8 | mix(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wash_lifts_the_panel_toward_the_overlay() {
        // A fully opaque wash replaces the base; a zero-alpha one is a no-op.
        assert_eq!(wash(0x12141c, 0xff_ffff_ff), 0xffffff);
        assert_eq!(wash(0x12141c, 0xff_ffff_00), 0x12141c);
        // The icon-tile wash over the panel lands a hair lighter than it.
        let lifted = wash(opaque_rgb(PANEL_BG), ICON_BG);
        assert!(lifted > opaque_rgb(PANEL_BG), "{lifted:#08x}");
        assert_eq!(lifted, 0x23252c);
    }
}
