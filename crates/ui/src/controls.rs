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

/// A focusable on/off switch. Renders a pill track with a knob that slides to the
/// right when `on`. Same keyboard/click activation and accent focus ring as
/// [`button`] — Enter/Space or a left-click invoke `on_toggle`, which should flip
/// the caller's boolean. Use this (not an "On"/"Off" text button) for settings.
pub fn toggle<T: 'static>(
    focus: &FocusHandle,
    on: bool,
    cx: &mut Context<T>,
    on_toggle: impl Fn(&mut T, &mut Window, &mut Context<T>) + 'static,
) -> impl IntoElement {
    let cb = Rc::new(on_toggle);
    let on_key = cb.clone();
    let on_click = cb;
    let knob = div()
        .size(px(16.))
        .rounded_full()
        .bg(if on { theme::accent() } else { theme::muted() });
    div()
        .track_focus(focus)
        .tab_index(0)
        .w(px(40.))
        .h(px(22.))
        .flex()
        .items_center()
        .px(px(3.))
        .rounded_full()
        .bg(if on { theme::selected() } else { theme::tile() })
        .border_1()
        .border_color(gpui::rgba(0x00_0000_00))
        .when(on, |s| s.justify_end())
        .focus(|s| s.border_color(theme::accent()))
        .child(knob)
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

/// A settings row: a `label` (with an optional muted `hint` beneath) on the left
/// and a `control` (toggle, button, text field, …) on the right. The control owns
/// its own focus; this wrapper is purely layout, so it stays keyboard-navigable.
pub fn settings_row(
    label: impl Into<SharedString>,
    hint: impl Into<SharedString>,
    control: impl IntoElement,
) -> impl IntoElement {
    let hint = hint.into();
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_3()
        .py_2()
        .child(
            div()
                .flex()
                .flex_col()
                .child(div().text_color(theme::text()).child(label.into()))
                .when(!hint.is_empty(), |this| {
                    this.child(div().text_xs().text_color(theme::muted()).child(hint))
                }),
        )
        .child(control)
}

/// A titled settings group: a small muted label above a bordered card holding
/// `body`. The settings-panel counterpart of the Home `section_label`.
pub fn section(label: impl Into<SharedString>, body: impl IntoElement) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_2()
        .pb_3()
        .child(div().text_xs().text_color(theme::muted()).child(label.into()))
        .child(
            div()
                .flex()
                .flex_col()
                .p_3()
                .rounded_lg()
                .bg(theme::hover())
                .border_1()
                .border_color(theme::divider())
                .child(body),
        )
}

/// A focusable disclosure ("Advanced ▸"): a header row with a rotating chevron
/// that toggles `expanded` (owned by the caller); `body` renders beneath when
/// open. Same keyboard/click semantics as [`button`].
pub fn disclosure<T: 'static>(
    focus: &FocusHandle,
    label: impl Into<SharedString>,
    expanded: bool,
    cx: &mut Context<T>,
    on_toggle: impl Fn(&mut T, &mut Window, &mut Context<T>) + 'static,
    body: impl IntoElement,
) -> impl IntoElement {
    let cb = Rc::new(on_toggle);
    let on_key = cb.clone();
    let on_click = cb;
    let header = div()
        .track_focus(focus)
        .tab_index(0)
        .flex()
        .items_center()
        .gap_2()
        .py_1()
        .rounded_lg()
        .text_color(theme::muted())
        .border_1()
        .border_color(gpui::rgba(0x00_0000_00))
        .focus(|s| s.border_color(theme::accent()))
        .child(div().child(if expanded { "\u{2304}" } else { "\u{203a}" }))
        .child(div().text_xs().child(label.into()))
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
        );
    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(header)
        .when(expanded, |this| this.child(body))
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
