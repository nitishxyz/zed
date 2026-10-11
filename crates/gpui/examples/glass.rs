#![cfg_attr(target_family = "wasm", no_main)]

//! Frosted glass ("material") demo: backdrop blur + squircle corners + the
//! element scale transform used for tvOS-style focus zoom.
//!
//! Run with `cargo run -p gpui --example glass [path/to/background.jpg]`.
//! Set `GLASS_FORCE_HOVER=1` to render card 2 in its zoomed (focused) state.

use std::{path::PathBuf, time::Duration};

use gpui::{
    Animation, AnimationExt as _, App, Bounds, BoxShadow, Context, FontWeight, Hsla, ObjectFit,
    SharedString, Window, WindowBounds, WindowOptions, div, hsla, img, point, prelude::*, px, size,
};
use gpui_platform::application;

const SQUIRCLE: f32 = 5.0;

struct Glass {
    background: PathBuf,
    focused: Option<usize>,
    pinned_zoom: bool,
}

/// tvOS-style smoky glass: dark tint over a saturated blur.
fn tint() -> Hsla {
    hsla(0.0, 0.0, 0.0, 0.42)
}

fn hairline() -> Hsla {
    hsla(0.0, 0.0, 1.0, 0.11)
}

fn glass_panel() -> gpui::Div {
    div()
        .backdrop_blur(px(30.0))
        .backdrop_saturation(1.6)
        .bg(tint())
        .border_1()
        .border_color(hairline())
        .corner_smoothing(SQUIRCLE)
}

/// Light material variant for comparison.
fn light_glass_panel() -> gpui::Div {
    div()
        .backdrop_blur(px(30.0))
        .backdrop_saturation(1.8)
        .bg(hsla(0.0, 0.0, 1.0, 0.22))
        .border_1()
        .border_color(hsla(0.0, 0.0, 1.0, 0.3))
        .corner_smoothing(SQUIRCLE)
}

/// Opaque swatch for judging corner shape against the background.
fn swatch(smoothing: f32) -> gpui::Div {
    div()
        .bg(hsla(0.0, 0.0, 0.97, 1.0))
        .corner_smoothing(smoothing)
        .flex()
        .items_center()
        .justify_center()
        .text_xs()
        .text_color(hsla(0.0, 0.0, 0.1, 1.0))
}

impl Glass {
    fn sidebar(&self) -> impl IntoElement {
        let row = |label: &'static str, selected: bool| {
            div()
                .px_3()
                .py_2()
                .rounded(px(10.0))
                .corner_smoothing(SQUIRCLE)
                .when(selected, |d| d.bg(hsla(0.0, 0.0, 1.0, 0.16)))
                .text_color(hsla(0.0, 0.0, 1.0, 0.92))
                .child(label)
        };
        glass_panel()
            .absolute()
            .left(px(20.0))
            .top(px(20.0))
            .bottom(px(20.0))
            .w(px(220.0))
            .rounded(px(22.0))
            .p_3()
            .flex()
            .flex_col()
            .gap_1()
            .shadow(vec![BoxShadow {
                color: hsla(0.0, 0.0, 0.0, 0.25),
                offset: point(px(0.0), px(10.0)),
                blur_radius: px(30.0),
                spread_radius: px(0.0),
                inset: false,
            }])
            .child(
                div()
                    .px_3()
                    .pb_2()
                    .text_color(hsla(0.0, 0.0, 1.0, 0.55))
                    .text_xs()
                    .child("LIBRARY"),
            )
            .child(row("Home", true))
            .child(row("Movies", false))
            .child(row("TV Shows", false))
            .child(row("Downloads", false))
            .child(row("Settings", false))
    }

    fn card(&self, index: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let zoomed = self.focused == Some(index) || (self.pinned_zoom && index == 1);
        let hue = [0.58, 0.08, 0.83][index % 3];
        div()
            .id(("card", index))
            .w(px(200.0))
            .h(px(130.0))
            .rounded(px(18.0))
            .corner_smoothing(SQUIRCLE)
            .backdrop_blur(px(24.0))
            .backdrop_saturation(1.6)
            .bg(hsla(hue, 0.5, 0.35, 0.35))
            .border_1()
            .border_color(hairline())
            .p_4()
            .flex()
            .flex_col()
            .justify_end()
            .text_color(hsla(0.0, 0.0, 1.0, 0.95))
            .when(zoomed, |d| {
                d.shadow(vec![BoxShadow {
                    color: hsla(0.0, 0.0, 0.0, 0.4),
                    offset: point(px(0.0), px(16.0)),
                    blur_radius: px(32.0),
                    spread_radius: px(0.0),
                    inset: false,
                }])
            })
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                if *hovered {
                    this.focused = Some(index);
                } else if this.focused == Some(index) {
                    this.focused = None;
                }
                cx.notify();
            }))
            .child(
                div()
                    .text_lg()
                    .child(SharedString::from(format!("Card {}", index + 1))),
            )
            .child(div().text_xs().opacity(0.7).child("Hover to zoom"))
            .with_animation(
                ("zoom", index * 2 + zoomed as usize),
                Animation::new(Duration::from_millis(180)).with_easing(gpui::ease_out_quint()),
                move |card, t| {
                    let (from, to) = if zoomed { (1.0, 1.08) } else { (1.08, 1.0) };
                    card.scale(from + (to - from) * t)
                },
            )
            .into_any_element()
    }
}

impl Render for Glass {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let cards: Vec<_> = (0..3).map(|i| self.card(i, cx)).collect();
        // Sharp, high-contrast content under the glass makes the blur obvious.
        let stripes = div()
            .absolute()
            .left(px(260.0))
            .bottom(px(20.0))
            .flex()
            .gap(px(14.0))
            .children((0..14).map(|i| {
                div().w(px(8.0)).h(px(150.0)).bg(if i % 2 == 0 {
                    hsla(0.0, 0.0, 1.0, 1.0)
                } else {
                    hsla(0.0, 0.0, 0.0, 1.0)
                })
            }));
        let corners = div()
            .absolute()
            .right(px(28.0))
            .bottom(px(28.0))
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(
                        swatch(0.0)
                            .size(px(110.0))
                            .rounded(px(28.0))
                            .child("circular"),
                    )
                    .child(
                        swatch(SQUIRCLE)
                            .size(px(110.0))
                            .rounded(px(28.0))
                            .child("squircle"),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(
                        swatch(0.0)
                            .w(px(110.0))
                            .h(px(40.0))
                            .rounded_full()
                            .child("pill"),
                    )
                    .child(
                        swatch(SQUIRCLE)
                            .w(px(110.0))
                            .h(px(40.0))
                            .rounded_full()
                            .child("smooth pill"),
                    ),
            );
        div()
            .size_full()
            .relative()
            .bg(hsla(0.6, 0.3, 0.1, 1.0))
            .child(
                img(self.background.clone())
                    .absolute()
                    .size_full()
                    .object_fit(ObjectFit::Cover),
            )
            .child(
                div()
                    .absolute()
                    .left(px(300.0))
                    .top(px(95.0))
                    .text_size(px(110.0))
                    .font_weight(FontWeight::BLACK)
                    .text_color(hsla(0.0, 0.0, 1.0, 0.95))
                    .child("TORRZ GLASS"),
            )
            .child(stripes)
            .child(self.sidebar())
            // Capsule pill.
            .child(
                glass_panel()
                    .absolute()
                    .top(px(28.0))
                    .left(px(280.0))
                    .h(px(44.0))
                    .px_5()
                    .rounded_full()
                    .flex()
                    .items_center()
                    .gap_4()
                    .text_color(hsla(0.0, 0.0, 1.0, 0.95))
                    .child("Featured")
                    .child(div().opacity(0.6).child("New"))
                    .child(div().opacity(0.6).child("Popular")),
            )
            // Circle button.
            .child(
                glass_panel()
                    .absolute()
                    .top(px(28.0))
                    .right(px(28.0))
                    .size(px(44.0))
                    .rounded_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(hsla(0.0, 0.0, 1.0, 0.95))
                    .child("+"),
            )
            // Overlapping siblings: the light chip must blur the dark panel
            // (and the stripes) painted before it.
            .child(
                glass_panel()
                    .absolute()
                    .left(px(280.0))
                    .bottom(px(28.0))
                    .w(px(320.0))
                    .h(px(110.0))
                    .rounded(px(26.0))
                    .p_4()
                    .text_color(hsla(0.0, 0.0, 1.0, 0.95))
                    .child("Dark material"),
            )
            .child(
                light_glass_panel()
                    .absolute()
                    .left(px(500.0))
                    .bottom(px(100.0))
                    .w(px(170.0))
                    .h(px(70.0))
                    .rounded(px(20.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(hsla(0.0, 0.0, 1.0, 0.95))
                    .child("Light material"),
            )
            .child(corners)
            .child(
                div()
                    .absolute()
                    .left(px(300.0))
                    .top(px(175.0))
                    .flex()
                    .gap_6()
                    .children(cards),
            )
    }
}

fn run_example() {
    let background = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp/bg.jpg"));
    let pinned_zoom = std::env::var("GLASS_FORCE_HOVER").is_ok_and(|v| v == "1");
    application().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1100.0), px(700.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| {
                cx.new(|_| Glass {
                    background,
                    focused: None,
                    pinned_zoom,
                })
            },
        )
        .unwrap();
        cx.activate(true);
    });
}

#[cfg(not(target_family = "wasm"))]
fn main() {
    run_example();
}

#[cfg(target_family = "wasm")]
#[wasm_bindgen::prelude::wasm_bindgen(start)]
pub fn start() {
    gpui_platform::web_init();
    run_example();
}
