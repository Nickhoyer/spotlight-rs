//! Reusable focusable controls. Use these (rather than bare `div().on_mouse_down`)
//! for anything clickable that should also be keyboard-reachable, so the whole
//! app stays navigable by Tab + Enter.

use std::rc::Rc;

use gpui::prelude::*;
use gpui::{div, px, Context, FocusHandle, KeyDownEvent, MouseButton, SharedString, Window};

use crate::theme;

/// A focusable, keyboard-activatable pill button.
///
/// The caller owns `focus` (so it persists across renders and participates in
/// Tab order via `tab_index(0)`). Enter/Space while focused, or a left-click,
/// invoke `on_activate`. Renders a hover wash and an accent focus ring.
pub fn button<T: 'static>(
    focus: &FocusHandle,
    label: impl Into<SharedString>,
    cx: &mut Context<T>,
    on_activate: impl Fn(&mut T, &mut Window, &mut Context<T>) + 'static,
) -> impl IntoElement {
    let cb = Rc::new(on_activate);
    let on_key = cb.clone();
    let on_click = cb;
    div()
        .track_focus(focus)
        .tab_index(0)
        .px_4()
        .py_2()
        .rounded_lg()
        .bg(theme::tile())
        .text_color(theme::accent())
        .border_1()
        .border_color(gpui::rgba(0x00_0000_00))
        .hover(|s| s.bg(theme::hover_strong()))
        .focus(|s| s.border_color(theme::accent()))
        .child(label.into())
        .on_key_down(cx.listener(
            move |this, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<T>| {
                if ev.keystroke.key == "enter" || ev.keystroke.key == "space" {
                    on_key(this, window, cx);
                    cx.stop_propagation();
                }
            },
        ))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _, window: &mut Window, cx: &mut Context<T>| {
                on_click(this, window, cx);
            }),
        )
}

/// A small square focusable icon button (e.g. a remove "✕"). Same keyboard
/// semantics as [`button`].
pub fn icon_button<T: 'static>(
    focus: &FocusHandle,
    glyph: impl Into<SharedString>,
    cx: &mut Context<T>,
    on_activate: impl Fn(&mut T, &mut Window, &mut Context<T>) + 'static,
) -> impl IntoElement {
    let cb = Rc::new(on_activate);
    let on_key = cb.clone();
    let on_click = cb;
    div()
        .track_focus(focus)
        .tab_index(0)
        .size(px(28.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_lg()
        .text_color(theme::muted())
        .border_1()
        .border_color(gpui::rgba(0x00_0000_00))
        .hover(|s| s.bg(theme::hover()))
        .focus(|s| s.border_color(theme::accent()))
        .child(glyph.into())
        .on_key_down(cx.listener(
            move |this, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<T>| {
                if ev.keystroke.key == "enter" || ev.keystroke.key == "space" {
                    on_key(this, window, cx);
                    cx.stop_propagation();
                }
            },
        ))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _, window: &mut Window, cx: &mut Context<T>| {
                on_click(this, window, cx);
            }),
        )
}
