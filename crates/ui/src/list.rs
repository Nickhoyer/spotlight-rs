//! Shared keyboard-list navigation. Every scrollable, arrow-navigable list in
//! the app should hold a [`ListNav`] and call `.track_scroll(&nav.scroll)` on its
//! scroll container, so the selection always scrolls into view.
//!
//! See the keyboard-navigation note in memory for the app-wide expectation.

use gpui::prelude::*;
use gpui::{div, linear_color_stop, linear_gradient, px, AnyElement, ScrollHandle};

use crate::theme;

/// A selected index paired with a scroll handle that keeps it visible.
#[derive(Clone)]
pub struct ListNav {
    pub selected: usize,
    pub scroll: ScrollHandle,
}

impl ListNav {
    pub fn new() -> Self {
        Self {
            selected: 0,
            scroll: ScrollHandle::new(),
        }
    }

    /// Move down one (clamped to `len - 1`) and reveal with a one-row peek.
    pub fn next(&mut self, len: usize) {
        if len > 0 {
            self.selected = (self.selected + 1).min(len - 1);
            peek(&self.scroll, self.selected, len, true);
        }
    }

    /// Move up one (clamped to 0) and reveal with a one-row peek.
    pub fn prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
        peek(&self.scroll, self.selected, 0, false);
    }

    /// Jump to a specific index and reveal.
    pub fn set(&mut self, index: usize) {
        self.selected = index;
        self.reveal();
    }

    /// Keep the selection in range after the backing data changes.
    pub fn clamp(&mut self, len: usize) {
        self.selected = if len == 0 { 0 } else { self.selected.min(len - 1) };
    }

    /// Scroll the selected item into view. The list container must be built with
    /// `.track_scroll(&nav.scroll)` for this to have an effect.
    pub fn reveal(&self) {
        self.scroll.scroll_to_item(self.selected);
    }
}

/// Scroll so the row just past `selected` (in the travel direction) is also
/// visible — the peeking neighbor signals "more items". On the last item we
/// scroll fully to the bottom instead, so the list's trailing padding shows
/// (rather than parking the final row flush against the edge).
pub fn peek(scroll: &ScrollHandle, selected: usize, len: usize, forward: bool) {
    if forward {
        if selected + 1 >= len {
            scroll.scroll_to_bottom();
        } else {
            scroll.scroll_to_item(selected + 1);
        }
    } else {
        scroll.scroll_to_item(selected.saturating_sub(1));
    }
}

/// Wrap a vertical scroll list with soft top/bottom edge fades, each shown only
/// when content is clipped past that edge (a passive "more items" cue for mouse
/// scrolling). `fill` = the inner list uses `flex_1` (vs. a max-height that
/// sizes to content). The fade values are read from the prior frame's layout, so
/// they settle after the first paint/interaction.
pub fn faded_scroll(scroll: &ScrollHandle, fill: bool, inner: AnyElement) -> AnyElement {
    let count = scroll.children_count();
    let show_top = scroll.top_item() > 0;
    let show_bottom = count > 0 && scroll.bottom_item() + 1 < count;

    let mut wrapper = div().relative();
    if fill {
        wrapper = wrapper.flex().flex_col().flex_1();
    }
    wrapper
        .child(inner)
        .when(show_top, |w| w.child(edge_fade(true)))
        .when(show_bottom, |w| w.child(edge_fade(false)))
        .into_any_element()
}

/// A 28px gradient strip pinned to the top or bottom edge, fading content into
/// the panel color.
fn edge_fade(top: bool) -> impl IntoElement {
    let angle = if top { 0. } else { 180. };
    let base = div()
        .absolute()
        .left(px(0.))
        .right(px(0.))
        .h(px(28.))
        .bg(linear_gradient(
            angle,
            linear_color_stop(theme::panel_transparent(), 0.),
            linear_color_stop(theme::panel_opaque(), 1.),
        ));
    if top {
        base.top(px(0.))
    } else {
        base.bottom(px(0.))
    }
}

impl Default for ListNav {
    fn default() -> Self {
        Self::new()
    }
}
