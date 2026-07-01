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
