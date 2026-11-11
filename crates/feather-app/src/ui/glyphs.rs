//! Small drawn icons for the kinds of results (Documents, Images, ...).
//!
//! Windows' own file icons are the generic white page for many types, so
//! the sidebar and Settings draw these instead: one shape and one color per
//! kind, the same at any size and in both themes.

use super::gfx::{Color, Painter, Rect};
use crate::filters::Category;

/// The color that marks a kind.
pub fn tint(category: Category, accent: Color, dark: bool) -> Color {
    let hex =
        |dark_shade: u32, light_shade: u32| Color::hex(if dark { dark_shade } else { light_shade });
    match category {
        Category::Everything => accent,
        Category::Apps => hex(0xa594ff, 0x6b52e0),
        Category::Files => hex(0xb8b0a6, 0x7a7168),
        Category::Folders => hex(0xf2bd4b, 0xc58a12),
        Category::Documents => hex(0x6fa8ff, 0x2f6fd6),
        Category::Images => hex(0x46c9b5, 0x138b78),
        Category::Video => hex(0xf27a96, 0xc93f60),
        Category::Audio => hex(0xe187e6, 0xa43fae),
        Category::Archives => hex(0xe0a46a, 0xa8642a),
        Category::Executables => hex(0x9fb2c6, 0x55687c),
        Category::Code => hex(0x79d17d, 0x2e8b3a),
        Category::Custom => hex(0xb8b0a6, 0x7a7168),
    }
}

/// Draws the icon for `category` centered in `rect` (about 18 px works).
pub fn draw(p: &Painter, category: Category, rect: Rect, color: Color) {
    let size = rect.width().min(rect.height());
    let s = size / 20.0;
    let (cx, cy) = (rect.l + rect.width() / 2.0, rect.t + rect.height() / 2.0);
    // Points in a 20 x 20 box centered on the rect.
    let at = |x: f32, y: f32| (cx + (x - 10.0) * s, cy + (y - 10.0) * s);
    let r = |l: f32, t: f32, rr: f32, b: f32| {
        let (l, t) = at(l, t);
        let (rr, b) = at(rr, b);
        Rect::new(l, t, rr, b)
    };
    let w = 1.5 * s.max(0.8);
    let line = |x0: f32, y0: f32, x1: f32, y1: f32| {
        let (a, b) = at(x0, y0);
        let (c, d) = at(x1, y1);
        p.line(a, b, c, d, color, w);
    };
    let wash = color.fade(0.22);
    match category {
        Category::Everything => {
            // A four-pointed star, with a small one beside it.
            let star = |x: f32, y: f32, outer: f32, inner: f32| -> Vec<(f32, f32)> {
                (0..8)
                    .map(|step| {
                        let angle =
                            step as f32 * std::f32::consts::FRAC_PI_4 - std::f32::consts::FRAC_PI_2;
                        let radius = if step % 2 == 0 { outer } else { inner };
                        at(x + angle.cos() * radius, y + angle.sin() * radius)
                    })
                    .collect()
            };
            p.polygon(&star(9.0, 11.0, 7.5, 2.2), color);
            p.polygon(&star(15.5, 4.5, 3.2, 1.0), color.fade(0.7));
        }
        Category::Apps => {
            for (x, y, filled) in [
                (3.5, 3.5, true),
                (11.0, 3.5, false),
                (3.5, 11.0, false),
                (11.0, 11.0, false),
            ] {
                let tile = r(x, y, x + 5.5, y + 5.5);
                if filled {
                    p.fill_round(tile, 1.6 * s, color);
                } else {
                    p.fill_round(tile, 1.6 * s, wash);
                    p.stroke_round(tile, 1.6 * s, color, w);
                }
            }
        }
        Category::Files | Category::Custom => {
            page(p, &at, color, wash, w);
        }
        Category::Documents => {
            page(p, &at, color, wash, w);
            line(7.0, 9.5, 13.0, 9.5);
            line(7.0, 12.5, 13.0, 12.5);
            line(7.0, 15.5, 11.0, 15.5);
        }
        Category::Folders => {
            let back = [
                at(2.5, 5.0),
                at(8.0, 5.0),
                at(10.0, 7.0),
                at(17.5, 7.0),
                at(17.5, 16.0),
                at(2.5, 16.0),
            ];
            p.polygon(&back, color.fade(0.55));
            let front = [at(2.5, 8.5), at(17.5, 8.5), at(17.5, 16.0), at(2.5, 16.0)];
            p.polygon(&front, color);
        }
        Category::Images => {
            let frame = r(3.0, 4.0, 17.0, 16.0);
            p.fill_round(frame, 2.5 * s, wash);
            p.clip(frame);
            p.polygon(
                &[
                    at(3.0, 16.5),
                    at(8.0, 10.0),
                    at(11.5, 13.5),
                    at(13.5, 11.5),
                    at(17.5, 16.5),
                ],
                color,
            );
            p.unclip();
            p.stroke_round(frame, 2.5 * s, color, w);
            let (sx, sy) = at(13.0, 7.8);
            p.fill_round(
                Rect::new(sx - 1.6 * s, sy - 1.6 * s, sx + 1.6 * s, sy + 1.6 * s),
                1.6 * s,
                color,
            );
        }
        Category::Video => {
            let frame = r(2.5, 4.5, 17.5, 15.5);
            p.fill_round(frame, 2.5 * s, wash);
            p.stroke_round(frame, 2.5 * s, color, w);
            p.polygon(&[at(8.5, 7.5), at(13.0, 10.0), at(8.5, 12.5)], color);
        }
        Category::Audio => {
            let dot = |x: f32, y: f32| {
                let (x, y) = at(x, y);
                p.fill_round(
                    Rect::new(x - 2.4 * s, y - 2.0 * s, x + 2.4 * s, y + 2.0 * s),
                    2.0 * s,
                    color,
                );
            };
            dot(6.0, 14.5);
            dot(14.0, 13.0);
            line(8.2, 14.0, 8.2, 5.0);
            line(16.2, 12.5, 16.2, 3.5);
            p.polygon(
                &[at(7.5, 4.3), at(16.9, 2.8), at(16.9, 5.6), at(7.5, 7.1)],
                color,
            );
        }
        Category::Archives => {
            let body = r(3.5, 7.5, 16.5, 16.5);
            p.fill_round(body, 2.0 * s, wash);
            p.stroke_round(body, 2.0 * s, color, w);
            p.fill_round(r(2.5, 4.0, 17.5, 8.0), 1.6 * s, color);
            line(8.5, 10.5, 11.5, 10.5);
        }
        Category::Executables => {
            let frame = r(2.5, 4.0, 17.5, 16.0);
            p.fill_round(frame, 2.5 * s, wash);
            p.stroke_round(frame, 2.5 * s, color, w);
            line(6.0, 8.0, 8.5, 10.0);
            line(8.5, 10.0, 6.0, 12.0);
            line(10.0, 12.5, 14.0, 12.5);
        }
        Category::Code => {
            line(7.0, 5.5, 3.0, 10.0);
            line(3.0, 10.0, 7.0, 14.5);
            line(13.0, 5.5, 17.0, 10.0);
            line(17.0, 10.0, 13.0, 14.5);
            line(11.3, 4.5, 8.7, 15.5);
        }
    }
}

/// A sheet of paper with a folded corner.
fn page(p: &Painter, at: &dyn Fn(f32, f32) -> (f32, f32), color: Color, wash: Color, w: f32) {
    let outline = [
        at(5.0, 2.5),
        at(12.0, 2.5),
        at(15.5, 6.0),
        at(15.5, 17.5),
        at(5.0, 17.5),
    ];
    p.polygon(&outline, wash);
    p.polyline(&outline, true, color, w);
    p.polyline(
        &[at(12.0, 2.5), at(12.0, 6.0), at(15.5, 6.0)],
        false,
        color,
        w,
    );
}

/// Icons for the pages of a dialog's side rail.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageIcon {
    Appearance,
    Places,
    Shortcut,
    Sidebar,
    Window,
    /// A brush, for the custom theme.
    Theme,
    /// A globe, for sharing over the network.
    Sharing,
}

/// Draws a page icon as outlines in `color`, centered in `rect`.
pub fn draw_page(p: &Painter, icon: PageIcon, rect: Rect, color: Color) {
    let s = rect.width().min(rect.height()) / 20.0;
    let (cx, cy) = (rect.l + rect.width() / 2.0, rect.t + rect.height() / 2.0);
    let at = |x: f32, y: f32| (cx + (x - 10.0) * s, cy + (y - 10.0) * s);
    let r = |l: f32, t: f32, rr: f32, b: f32| {
        let (l, t) = at(l, t);
        let (rr, b) = at(rr, b);
        Rect::new(l, t, rr, b)
    };
    let w = 1.5 * s.max(0.8);
    let line = |x0: f32, y0: f32, x1: f32, y1: f32| {
        let (a, b) = at(x0, y0);
        let (c, d) = at(x1, y1);
        p.line(a, b, c, d, color, w);
    };
    let dot = |x: f32, y: f32, radius: f32| {
        let (x, y) = at(x, y);
        let radius = radius * s;
        p.fill_round(
            Rect::new(x - radius, y - radius, x + radius, y + radius),
            radius,
            color,
        );
    };
    match icon {
        PageIcon::Appearance => {
            // A palette: a round board with paint dots.
            let (x, y) = at(10.0, 10.0);
            p.circle(x, y, 7.5 * s, color, w);
            dot(7.0, 7.5, 1.3);
            dot(11.5, 6.0, 1.3);
            dot(14.0, 9.5, 1.3);
            dot(8.0, 12.5, 1.6);
        }
        PageIcon::Places => {
            // A drive: a flat box with a light.
            p.stroke_round(r(2.5, 6.0, 17.5, 14.5), 2.5 * s, color, w);
            line(5.0, 11.5, 11.0, 11.5);
            dot(14.5, 11.0, 1.1);
        }
        PageIcon::Shortcut => {
            // A keyboard.
            p.stroke_round(r(2.0, 5.0, 18.0, 15.0), 2.5 * s, color, w);
            for x in [5.5, 8.5, 11.5, 14.5] {
                dot(x, 8.3, 0.9);
            }
            line(6.5, 11.8, 13.5, 11.8);
        }
        PageIcon::Sidebar => {
            p.stroke_round(r(2.5, 4.0, 17.5, 16.0), 2.5 * s, color, w);
            line(7.5, 4.5, 7.5, 15.5);
            line(4.5, 7.5, 5.5, 7.5);
            line(4.5, 10.0, 5.5, 10.0);
        }
        PageIcon::Window => {
            p.stroke_round(r(2.5, 4.0, 17.5, 16.0), 2.5 * s, color, w);
            line(2.5, 7.5, 17.5, 7.5);
            dot(5.0, 5.8, 0.7);
            dot(7.2, 5.8, 0.7);
        }
        PageIcon::Theme => {
            // A brush: a handle and a rounded tip.
            line(15.5, 3.5, 9.5, 9.5);
            p.stroke_round(r(4.0, 9.5, 10.5, 16.5), 3.0 * s, color, w);
        }
        PageIcon::Sharing => {
            // A globe: a circle, its equator and one meridian.
            let (x, y) = at(10.0, 10.0);
            p.circle(x, y, 7.0 * s, color, w);
            line(3.0, 10.0, 17.0, 10.0);
            p.stroke_round(r(7.0, 3.0, 13.0, 17.0), 3.0 * s, color, w);
        }
    }
}
