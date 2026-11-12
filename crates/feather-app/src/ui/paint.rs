//! Draws one frame of the main window from a snapshot of the UI state.
//!
//! `Ui::build_frame` gathers everything into a [`Frame`] first, so painting
//! never needs the index or the search controller.

use windows::Win32::Graphics::DirectWrite::{IDWriteTextLayout, DWRITE_HIT_TEST_METRICS};

use super::gfx::{Color, Font, Painter, Rect};
use super::glyphs;
use super::icons::{IconCache, IconSize};
use super::layout::{Button, Layout, NavEntry, NavItem, ROW_H};
use super::theme::Palette;

/// What the pointer is over or pressing.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Hit {
    #[default]
    None,
    Nav(usize),
    Button(Button),
    Row(usize),
    Search,
    ScrollThumb,
    ScrollTrack,
}

pub struct RowView {
    /// Index of the row within the results.
    pub index: usize,
    pub top: f32,
    pub name: String,
    /// The parts of `name`, in UTF-16 units, the typed words matched.
    pub spans: Vec<(usize, usize)>,
    pub folder: String,
    pub size: String,
    pub modified: String,
    pub icon: i32,
    pub selected: bool,
}

pub struct DetailsView {
    pub name: String,
    pub kind: String,
    pub size: String,
    /// For a folder: how many items are below it ("1,204 items").
    pub items: String,
    pub modified: String,
    pub folder: String,
    pub icon: i32,
}

/// The box a result's new name is typed in (F2).
pub struct RenameView {
    pub rect: Rect,
    pub layout: Option<IDWriteTextLayout>,
    pub scroll: f32,
    pub caret_x: f32,
    pub show_caret: bool,
    pub selection: Vec<(f32, f32)>,
}

pub struct SearchView {
    pub layout: Option<IDWriteTextLayout>,
    pub empty: bool,
    pub scroll: f32,
    pub caret_x: f32,
    pub show_caret: bool,
    pub selection: Vec<(f32, f32)>,
}

/// The custom theme's picture, ready to draw.
pub struct Background {
    pub bitmap: windows::Win32::Graphics::Direct2D::ID2D1Bitmap,
    pub width: f32,
    pub height: f32,
    pub fit: crate::settings::BackgroundFit,
    /// 0 to 1: how strongly it shows.
    pub strength: f32,
}

pub struct Frame<'a> {
    pub palette: Palette,
    pub background: Option<Background>,
    pub layout: &'a Layout,
    pub nav: &'a [NavEntry],
    pub nav_rects: &'a [Rect],
    pub nav_icons: Vec<i32>,
    /// The tab being dragged, drawn raised above the others.
    pub nav_lifted: Option<usize>,
    /// 1 with the sidebar open, 0 folded to icons.
    pub sidebar_open: f32,
    /// A name shown beside a folded sidebar entry under the pointer.
    pub tooltip: Option<(Rect, String)>,
    /// Left see-through so Windows' own caption buttons show (solid
    /// background only).
    pub caption_hole: Option<Rect>,
    /// How far the details pane is out, for its toggle's icon.
    pub details_w: f32,
    /// Image index for Feather's icon in the sidebar header.
    pub logo_icon: i32,
    pub hover: Hit,
    pub pressed: Hit,
    /// How hovered each target is (0 to 1) while hover highlights fade.
    pub hovers: Vec<(Hit, f32)>,
    /// Screen top of the sliding selection highlight in the result list.
    pub row_pill: Option<f32>,
    /// Sliding highlights behind the selected sidebar entries.
    pub nav_pills: Vec<Rect>,
    /// Fades the details pane in after the selection changes.
    pub details_opacity: f32,
    pub search: SearchView,
    pub count_text: String,
    pub scope_text: String,
    pub notice: String,
    pub index_text: String,
    pub filter_count: usize,
    pub rows: Vec<RowView>,
    /// A result being renamed in place.
    pub rename: Option<RenameView>,
    pub empty_message: Option<String>,
    pub sort_column: Button,
    pub sort_ascending: bool,
    pub scrollbar: Option<(Rect, Rect)>,
    pub details: Option<DetailsView>,
    /// True for screenshots and the solid material: paint an opaque base.
    pub opaque: bool,
    /// Keep icon bitmaps for the next frame (false for one-off targets).
    pub cache_icons: bool,
}

impl Frame<'_> {
    /// How strongly to draw the hover highlight for `hit`, from 0 to 1.
    pub fn hover_level(&self, hit: Hit) -> f32 {
        self.hovers
            .iter()
            .find(|(candidate, _)| *candidate == hit)
            .map(|(_, level)| *level)
            .unwrap_or(0.0)
    }
}

pub fn paint(p: &Painter, icons: &mut IconCache, frame: &Frame) {
    let pal = &frame.palette;
    let layout = frame.layout;
    if frame.opaque && pal.base.a < 1.0 {
        p.clear(pal.opaque_base());
    } else {
        p.clear(pal.base);
    }
    p.fill(layout.client, pal.tint);
    let mut card = pal.card;
    if let Some(background) = &frame.background {
        for (dest, source) in super::wallpaper::placements(
            background.fit,
            background.width,
            background.height,
            layout.client,
        ) {
            p.bitmap(&background.bitmap, dest, source, background.strength);
        }
        // A light wash keeps text readable over busy pictures, and the
        // results column lets the picture through.
        p.fill(layout.client, pal.tint.alpha(0.18));
        card = card.alpha(card.a.min(0.7));
    }
    p.fill(layout.main, card);
    p.fill(
        Rect::new(layout.main.l, 0.0, layout.main.l + 1.0, layout.client.b),
        pal.line,
    );
    if layout.details.is_some() {
        p.fill(
            Rect::new(layout.main.r - 1.0, 0.0, layout.main.r, layout.client.b),
            pal.line,
        );
    }

    p.clip(layout.sidebar);
    paint_sidebar(p, icons, frame);
    p.unclip();
    paint_topbar(p, frame);
    paint_search(p, frame);
    paint_list(p, icons, frame);
    paint_details(p, icons, frame);
    paint_tooltip(p, frame);
    if let Some(hole) = frame.caption_hole {
        p.clip(hole);
        p.clear(Color::hex(0).alpha(0.0));
        p.unclip();
    }
}

/// The name of a folded sidebar entry, in a small card beside it.
fn paint_tooltip(p: &Painter, frame: &Frame) {
    let Some((anchor, text)) = &frame.tooltip else {
        return;
    };
    let pal = &frame.palette;
    let width = p.gfx.measure_width(text, Font::Small).ceil() + 24.0;
    let mid = anchor.t + anchor.height() / 2.0;
    let rect = Rect::new(
        anchor.r + 10.0,
        mid - 15.0,
        anchor.r + 10.0 + width,
        mid + 15.0,
    );
    p.fill_round(rect.inset(-1.0, -1.0), 8.0, pal.line);
    p.fill_round(rect, 7.0, pal.opaque_base().mix(pal.text, 0.04));
    p.text(
        text,
        Font::Small,
        Rect::new(rect.l + 12.0, rect.t, rect.r - 8.0, rect.b),
        pal.text,
    );
}

/// The fold button: a window whose side pane is wide while the sidebar is
/// open and a sliver once it is folded.
fn paint_sidebar_toggle(p: &Painter, frame: &Frame, rect: Rect) {
    let pal = &frame.palette;
    let open = frame.sidebar_open;
    hover_fill(p, frame, Button::Sidebar, rect);
    let (cx, cy) = (rect.l + 20.0, rect.t + rect.height() / 2.0);
    let (cx, cy) = (cx.round(), cy.round());
    let outline = Rect::new(cx - 8.5, cy - 6.5, cx + 8.5, cy + 6.5);
    p.stroke_round(outline, 3.5, pal.text2, 1.3);
    let pane = Rect::new(cx - 6.0, cy - 4.0, cx - 3.5 + 3.5 * open, cy + 4.0);
    p.fill_round(pane, 1.4, pal.text2);
    p.text(
        "Collapse",
        Font::Body,
        Rect::new(rect.l + 38.0, rect.t, rect.l + 192.0, rect.b),
        pal.text.fade(label_fade(open)),
    );
}

fn paint_sidebar(p: &Painter, icons: &mut IconCache, frame: &Frame) {
    let pal = &frame.palette;
    let layout = frame.layout;
    let open = frame.sidebar_open;
    draw_icon(
        p,
        icons,
        frame,
        frame.logo_icon,
        IconSize::Large,
        layout.logo,
        1.0,
    );
    p.text(
        "feather",
        Font::Wordmark,
        layout.wordmark,
        pal.text.fade(label_fade(open)),
    );
    for &pill in &frame.nav_pills {
        p.fill_round(pill, 8.0, pal.nav_selected);
    }
    let mut lifted = None;
    for (index, entry) in frame.nav.iter().enumerate() {
        let rect = frame.nav_rects[index];
        if rect.is_empty() {
            continue;
        }
        if frame.nav_lifted == Some(index) {
            lifted = Some((index, entry, rect));
            continue;
        }
        paint_nav_entry(p, icons, frame, index, entry, rect);
    }
    // The dragged tab floats above the rest on a raised card.
    if let Some((index, entry, rect)) = lifted {
        p.fill_round(rect.inset(-1.0, -1.0), 9.0, Color::hex(0).alpha(0.18));
        p.fill_round(rect, 8.0, pal.opaque_base().mix(pal.text, 0.07));
        p.stroke_round(rect, 8.0, pal.accent.fade(0.55), 1.2);
        paint_nav_entry(p, icons, frame, index, entry, rect);
    }
    paint_sidebar_footer(p, frame);
}

/// How visible sidebar labels are: they fade out early while folding and
/// in late while unfolding, so text never slides under the edge.
fn label_fade(open: f32) -> f32 {
    let t = ((open - 0.45) / 0.55).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn paint_nav_entry(
    p: &Painter,
    icons: &mut IconCache,
    frame: &Frame,
    index: usize,
    entry: &NavEntry,
    rect: Rect,
) {
    let pal = &frame.palette;
    let open = label_fade(frame.sidebar_open);
    {
        match entry {
            NavEntry::Label(label) => {
                p.text(
                    label,
                    Font::Small,
                    Rect::new(
                        rect.l + 12.0,
                        rect.t + 8.0,
                        rect.r.max(rect.l + 200.0),
                        rect.b,
                    ),
                    pal.text2.fade(open),
                );
                // Folded, a short rule stands in for the group name.
                if open < 1.0 {
                    let mid = rect.t + rect.height() / 2.0 + 2.0;
                    p.fill(
                        Rect::new(rect.l + 8.0, mid, rect.r - 8.0, mid + 1.0),
                        pal.line.fade(1.0 - open),
                    );
                }
            }
            NavEntry::Item(item) => {
                let level = frame.hover_level(Hit::Nav(index));
                if level > 0.0 {
                    p.fill_round(rect, 8.0, pal.hover.fade(level));
                }
                let icon_rect =
                    Rect::new(rect.l + 12.0, rect.t + 8.0, rect.l + 28.0, rect.t + 24.0);
                if let NavItem::Category(category) = item {
                    let glyph = Rect::new(
                        icon_rect.l - 2.0,
                        icon_rect.t - 2.0,
                        icon_rect.r + 2.0,
                        icon_rect.b + 2.0,
                    );
                    glyphs::draw(
                        p,
                        *category,
                        glyph,
                        glyphs::tint(*category, pal.accent, pal.dark),
                    );
                } else if *item == NavItem::ChooseFolder {
                    let (cx, cy) = (icon_rect.l + 8.0, icon_rect.t + 8.0);
                    p.line(cx - 5.0, cy, cx + 5.0, cy, pal.text2, 1.6);
                    p.line(cx, cy - 5.0, cx, cy + 5.0, pal.text2, 1.6);
                } else {
                    draw_icon(
                        p,
                        icons,
                        frame,
                        frame.nav_icons[index],
                        IconSize::Small,
                        icon_rect,
                        1.0,
                    );
                }
                let label = nav_label(item);
                let color = if *item == NavItem::ChooseFolder {
                    pal.text2
                } else {
                    pal.text
                };
                // Labels keep their full width while the sidebar folds, so
                // they slide under its edge instead of wrapping.
                p.text(
                    &label,
                    Font::Body,
                    Rect::new(rect.l + 38.0, rect.t, rect.l + 192.0, rect.b),
                    color.fade(open),
                );
            }
        }
    }
}

fn paint_sidebar_footer(p: &Painter, frame: &Frame) {
    let pal = &frame.palette;
    let layout = frame.layout;
    let open = label_fade(frame.sidebar_open);
    if let Some(toggle) = layout.button(Button::Sidebar) {
        paint_sidebar_toggle(p, frame, toggle);
    }
    // Folded, a short rule sets these three apart from the tabs above.
    if open < 1.0 {
        if let Some(rect) = layout.button(Button::Settings) {
            let y = rect.t - 7.0;
            p.fill(
                Rect::new(rect.l + 8.0, y, rect.r - 8.0, y + 1.0),
                pal.line.fade(1.0 - open),
            );
        }
    }
    for (button, label) in [(Button::Settings, "Settings"), (Button::Help, "Help")] {
        let Some(rect) = layout.button(button) else {
            continue;
        };
        hover_fill(p, frame, button, rect);
        let (cx, cy) = (rect.l + 20.0, rect.t + rect.height() / 2.0);
        if button == Button::Settings {
            // Two sliders.
            p.line(cx - 6.5, cy - 3.0, cx + 6.5, cy - 3.0, pal.text2, 1.4);
            p.line(cx - 6.5, cy + 3.5, cx + 6.5, cy + 3.5, pal.text2, 1.4);
            p.fill_round(
                Rect::new(cx + 0.5, cy - 5.5, cx + 4.5, cy - 0.5),
                2.0,
                pal.text2,
            );
            p.fill_round(
                Rect::new(cx - 4.5, cy + 1.0, cx - 0.5, cy + 6.0),
                2.0,
                pal.text2,
            );
        } else {
            p.circle(cx, cy, 6.5, pal.text2, 1.4);
            p.text(
                "?",
                Font::Small,
                Rect::new(cx - 2.6, cy - 8.0, cx + 6.0, cy + 7.5),
                pal.text2,
            );
        }
        p.text(
            label,
            Font::Body,
            Rect::new(rect.l + 38.0, rect.t, rect.l + 192.0, rect.b),
            pal.text.fade(open),
        );
    }

    let notice = layout.notice;
    let half = notice.height() / 2.0;
    p.text(
        &frame.notice,
        Font::Small,
        Rect::new(notice.l, notice.t, notice.r, notice.t + half),
        pal.text2.fade(open),
    );
    p.text(
        &frame.index_text,
        Font::Small,
        Rect::new(notice.l, notice.t + half, notice.r, notice.b),
        pal.text2.fade(open),
    );
}

pub fn nav_label(item: &NavItem) -> String {
    match item {
        NavItem::Category(category) => category.label().to_string(),
        NavItem::Scope(None) => "All locations".to_string(),
        NavItem::Scope(Some(scope)) => scope.clone(),
        NavItem::ChooseFolder => "Choose a folder...".to_string(),
    }
}

fn paint_topbar(p: &Painter, frame: &Frame) {
    let pal = &frame.palette;
    let layout = frame.layout;
    p.text(
        &frame.scope_text,
        Font::Small,
        layout.scope_label,
        pal.text2,
    );
    for &(button, rect) in &layout.buttons {
        match button {
            Button::Theme => {
                // Square like the caption buttons it sits beside.
                let hit = Hit::Button(button);
                let pressed = frame.hover == hit && frame.pressed == hit;
                let level = if pressed { 2.0 } else { frame.hover_level(hit) };
                if level > 0.0 {
                    p.fill(rect, pal.hover.alpha((pal.hover.a * 1.6 * level).min(1.0)));
                }
                let (cx, cy) = (rect.l + rect.width() / 2.0, rect.t + rect.height() / 2.0);
                if pal.dark {
                    // A sun: switch to light.
                    p.circle(cx, cy, 3.1, pal.text, 1.4);
                    for step in 0..8 {
                        let angle = step as f32 * std::f32::consts::FRAC_PI_4;
                        let (s, c) = angle.sin_cos();
                        p.line(
                            cx + c * 5.1,
                            cy + s * 5.1,
                            cx + c * 6.6,
                            cy + s * 6.6,
                            pal.text,
                            1.4,
                        );
                    }
                } else {
                    // A crescent: switch to dark.
                    p.moon(cx, cy, 6.0, pal.text, 1.4);
                }
            }
            Button::Details => {
                let hit = Hit::Button(button);
                let pressed = frame.hover == hit && frame.pressed == hit;
                let level = if pressed { 2.0 } else { frame.hover_level(hit) };
                if level > 0.0 {
                    p.fill(rect, pal.hover.alpha((pal.hover.a * 1.6 * level).min(1.0)));
                }
                // The sidebar's fold button, mirrored: a window whose
                // right pane is wide while details show and a sliver once
                // they are hidden.
                let (cx, cy) = (rect.l + rect.width() / 2.0, rect.t + rect.height() / 2.0);
                let (cx, cy) = (cx.round(), cy.round());
                let outline = Rect::new(cx - 8.5, cy - 6.5, cx + 8.5, cy + 6.5);
                p.stroke_round(outline, 3.5, pal.text, 1.3);
                let open = (frame.details_w / super::layout::DETAILS_W).clamp(0.0, 1.0);
                let pane = Rect::new(cx + 3.5 - 3.5 * open, cy - 4.0, cx + 6.0, cy + 4.0);
                p.fill_round(pane, 1.4, pal.text);
            }
            Button::Filters => {
                hover_fill(p, frame, button, rect);
                let label = if frame.filter_count > 0 {
                    format!("Filters ({})", frame.filter_count)
                } else {
                    "Filters".to_string()
                };
                p.text(&label, Font::ButtonLight, rect, pal.text);
            }
            _ => {}
        }
    }
}

/// The rounded hover and press highlight behind a borderless button.
fn hover_fill(p: &Painter, frame: &Frame, button: Button, rect: Rect) {
    let pal = &frame.palette;
    let hit = Hit::Button(button);
    let pressed = frame.hover == hit && frame.pressed == hit;
    let level = if pressed { 2.0 } else { frame.hover_level(hit) };
    if level > 0.0 {
        p.fill_round(rect, 8.0, pal.hover.alpha(pal.hover.a * level));
    }
}

fn paint_search(p: &Painter, frame: &Frame) {
    let pal = &frame.palette;
    let layout = frame.layout;
    let icon = layout.search_icon;
    let (cx, cy) = (icon.l + 10.0, icon.t + 10.0);
    p.circle(cx, cy, 7.0, pal.text2, 2.0);
    p.line(cx + 5.2, cy + 5.2, cx + 10.0, cy + 10.0, pal.text2, 2.0);

    let area = layout.search_text;
    p.clip(area);
    let search = &frame.search;
    let origin = area.l - search.scroll;
    for &(left, right) in &search.selection {
        p.fill(
            Rect::new(origin + left, area.t + 10.0, origin + right, area.b - 10.0),
            pal.accent.alpha(0.28),
        );
    }
    if search.empty {
        p.text(
            "Search files and folders",
            Font::Search,
            area,
            pal.text2.alpha(0.55),
        );
    } else if let Some(text) = &search.layout {
        p.text_layout(text, origin, area.t, pal.text);
    }
    if search.show_caret {
        let x = (origin + search.caret_x).round() + 0.5;
        p.line(x, area.t + 12.0, x, area.b - 12.0, pal.accent, 2.0);
    }
    p.unclip();
    p.text(
        &frame.count_text,
        Font::BodyRight,
        layout.count_label,
        pal.text2,
    );
    p.fill(
        Rect::new(
            layout.main.l,
            layout.header.t - 1.0,
            layout.main.r,
            layout.header.t,
        ),
        pal.line,
    );
}

fn paint_list(p: &Painter, icons: &mut IconCache, frame: &Frame) {
    let pal = &frame.palette;
    let layout = frame.layout;
    for (button, rect, label, font) in [
        (
            Button::SortName,
            layout.name_col,
            "Name and folder",
            Font::Small,
        ),
        (Button::SortSize, layout.size_col, "Size", Font::SmallRight),
        (
            Button::SortModified,
            layout.date_col,
            "Modified",
            Font::Small,
        ),
    ] {
        let color = pal
            .text2
            .mix(pal.text, frame.hover_level(Hit::Button(button)));
        p.text(label, font, rect, color);
        if frame.sort_column == button {
            let width = p.gfx.measure_width(label, font);
            let x = if button == Button::SortSize {
                rect.r - width - 9.0
            } else {
                rect.l + width + 9.0
            };
            let y = rect.t + rect.height() / 2.0;
            let dir = if frame.sort_ascending { -1.0 } else { 1.0 };
            p.line(x - 3.5, y - dir * 1.8, x, y + dir * 1.8, color, 1.3);
            p.line(x, y + dir * 1.8, x + 3.5, y - dir * 1.8, color, 1.3);
        }
    }

    let list = layout.list;
    p.clip(list);
    let row_right = list.r - 10.0;
    for row in &frame.rows {
        let level = frame.hover_level(Hit::Row(row.index));
        if level > 0.0 && !row.selected {
            let rect = Rect::new(list.l, row.top, row_right, row.top + ROW_H);
            p.fill_round(rect, 10.0, pal.hover.fade(level));
        }
    }
    if let Some(top) = frame.row_pill {
        let pill = Rect::new(list.l, top, row_right, top + ROW_H);
        p.fill_round(pill, 10.0, pal.row_selected());
        p.stroke_round(pill, 10.0, pal.accent.fade(0.35), 1.0);
    }
    for row in &frame.rows {
        let rect = Rect::new(list.l, row.top, row_right, row.top + ROW_H);
        let icon_rect = Rect::new(rect.l + 14.0, rect.t + 9.0, rect.l + 46.0, rect.t + 41.0);
        draw_icon(p, icons, frame, row.icon, IconSize::Large, icon_rect, 1.0);
        let text_l = rect.l + 58.0;
        let name_r = layout.name_col.r;
        let name_rect = Rect::new(text_l, rect.t + 6.0, name_r, rect.t + 26.0);
        if row.spans.is_empty() {
            p.text(&row.name, Font::BodyBold, name_rect, pal.text);
        } else {
            p.text_marked(
                &row.name,
                Font::BodyBold,
                name_rect,
                pal.text,
                pal.text2,
                &row.spans,
            );
        }
        p.text(
            &row.folder,
            Font::Small,
            Rect::new(text_l, rect.t + 26.0, name_r, rect.t + 44.0),
            pal.text2,
        );
        p.text(
            &row.size,
            Font::BodyRight,
            Rect::new(layout.size_col.l, rect.t, layout.size_col.r, rect.b),
            pal.text2,
        );
        p.text(
            &row.modified,
            Font::Body,
            Rect::new(layout.date_col.l, rect.t, layout.date_col.r, rect.b),
            pal.text2,
        );
    }
    if let Some(rename) = &frame.rename {
        paint_rename(p, frame, rename);
    }
    if let Some(message) = &frame.empty_message {
        p.text(
            message,
            Font::BodyCenter,
            Rect::new(list.l + 24.0, list.t, list.r - 24.0, list.t + 160.0),
            pal.text2,
        );
    }
    p.unclip();

    if let Some((track, thumb)) = frame.scrollbar {
        let level = if frame.pressed == Hit::ScrollThumb {
            1.0
        } else {
            frame
                .hover_level(Hit::ScrollThumb)
                .max(frame.hover_level(Hit::ScrollTrack))
        };
        let thumb = Rect::new(thumb.l + 3.0 * (1.0 - level), thumb.t, thumb.r, thumb.b);
        if level > 0.0 {
            p.fill_round(track, 4.0, pal.hover.fade(level));
        }
        p.fill_round(thumb, 3.0, pal.scrollbar);
    }
}

/// An edit box over the name, like Explorer's: the name, its selection, and
/// the caret, on a solid field with an accent outline.
fn paint_rename(p: &Painter, frame: &Frame, rename: &RenameView) {
    let pal = &frame.palette;
    let rect = rename.rect;
    p.fill_round(rect, 6.0, pal.opaque_base());
    p.stroke_round(rect, 6.0, pal.accent, 1.5);
    let area = Rect::new(rect.l + 6.0, rect.t + 3.0, rect.r - 6.0, rect.b - 3.0);
    p.clip(area);
    let origin = area.l - rename.scroll;
    for &(left, right) in &rename.selection {
        p.fill(
            Rect::new(origin + left, area.t, origin + right, area.b),
            pal.accent.alpha(0.28),
        );
    }
    if let Some(text) = &rename.layout {
        p.text_layout(text, origin, area.t, pal.text);
    }
    if rename.show_caret {
        let x = (origin + rename.caret_x).round() + 0.5;
        p.line(x, area.t + 1.0, x, area.b - 1.0, pal.accent, 1.5);
    }
    p.unclip();
}

fn paint_details(p: &Painter, icons: &mut IconCache, frame: &Frame) {
    let Some(area) = frame.layout.details else {
        return;
    };
    let pal = &frame.palette;
    let Some(details) = &frame.details else {
        p.text(
            "Select a result to see its details.",
            Font::BodyCenter,
            Rect::new(area.l + 28.0, area.t, area.r - 28.0, area.t + 200.0),
            pal.text2,
        );
        return;
    };
    let fade = frame.details_opacity;
    let rise = (1.0 - fade) * 10.0;
    let inner = area.inset(28.0, 0.0);
    let icon_box = Rect::new(
        inner.l,
        area.t + 20.0 + rise,
        inner.l + 96.0,
        area.t + 116.0 + rise,
    );
    draw_icon(
        p,
        icons,
        frame,
        details.icon,
        IconSize::Jumbo,
        icon_box,
        fade,
    );

    let title_top = icon_box.b + 18.0;
    let title_h = p
        .gfx
        .measure_height(&details.name, Font::Title, inner.width())
        .min(100.0);
    p.clip(Rect::new(
        inner.l,
        title_top,
        inner.r,
        title_top + title_h + 2.0,
    ));
    p.text(
        &details.name,
        Font::Title,
        Rect::new(inner.l, title_top, inner.r, title_top + 200.0),
        pal.text.fade(fade),
    );
    p.unclip();

    let mut y = title_top + title_h + 18.0;
    let value_l = inner.l + 84.0;
    let limit = frame
        .layout
        .button(Button::Open)
        .map(|rect| rect.t - 16.0)
        .unwrap_or(area.b - 16.0);
    for (label, value) in [
        ("Type", details.kind.as_str()),
        ("Size", details.size.as_str()),
        ("Contains", details.items.as_str()),
        ("Modified", details.modified.as_str()),
        ("Folder", details.folder.as_str()),
    ] {
        if value.is_empty() {
            continue;
        }
        let height = p
            .gfx
            .measure_height(value, Font::BodyWrap, inner.r - value_l)
            .max(18.0);
        if y + height > limit {
            break;
        }
        p.text(
            label,
            Font::Body,
            Rect::new(inner.l, y, value_l - 8.0, y + 18.0),
            pal.text2.fade(fade),
        );
        p.text(
            value,
            Font::BodyWrap,
            Rect::new(value_l, y + 1.0, inner.r, y + height + 2.0),
            pal.text.fade(fade),
        );
        y += height + 10.0;
    }

    for (button, label, primary) in [
        (Button::Open, "Open", true),
        (Button::Reveal, "Show in folder", false),
        (Button::CopyPath, "Copy path", false),
    ] {
        let Some(rect) = frame.layout.button(button) else {
            continue;
        };
        let hit = Hit::Button(button);
        let level = frame.hover_level(hit);
        let pressed = frame.hover == hit && frame.pressed == hit;
        if primary {
            // Hovering lifts the fill toward the text color; pressing darkens.
            let fill = if pressed {
                pal.accent.mix(Color::hex(0x000000), 0.12)
            } else {
                pal.accent.mix(pal.text, 0.10 * level)
            };
            p.fill_round(rect, 12.0, fill);
            p.text(label, Font::Button, rect, pal.on_accent);
        } else {
            let level = if pressed { 2.0 } else { level };
            if level > 0.0 {
                p.fill_round(rect, 12.0, pal.hover.alpha(pal.hover.a * level));
            }
            p.stroke_round(rect, 12.0, pal.line, 1.0);
            p.text(label, Font::ButtonLight, rect, pal.text);
        }
    }
}

fn draw_icon(
    p: &Painter,
    icons: &mut IconCache,
    frame: &Frame,
    index: i32,
    size: IconSize,
    dest: Rect,
    opacity: f32,
) {
    let Some(icon) = icons.bitmap(&p.gfx.wic, p.rt, index, size, frame.cache_icons) else {
        return;
    };
    if size != IconSize::Jumbo {
        p.bitmap(&icon.bitmap, dest, None, opacity);
        return;
    }
    // Jumbo frames are 256 px but some types only fill a 48 px corner. Draw
    // the visible part, scaled down to fit, never up.
    let content = icon.content;
    if content.is_empty() {
        return;
    }
    let scale = (dest.width() / content.width())
        .min(dest.height() / content.height())
        .min(1.0);
    let (w, h) = (content.width() * scale, content.height() * scale);
    let left = dest.l + (dest.width() - w) / 2.0;
    let top = dest.b - h;
    p.bitmap(
        &icon.bitmap,
        Rect::new(left, top, left + w, top + h),
        Some(content),
        opacity,
    );
}

/// Pixel spans of the selected text, relative to the layout origin.
pub fn selection_spans(layout: &IDWriteTextLayout, start: usize, end: usize) -> Vec<(f32, f32)> {
    if start >= end {
        return Vec::new();
    }
    let mut metrics = [DWRITE_HIT_TEST_METRICS::default(); 4];
    let mut count = 0u32;
    let ok = unsafe {
        layout.HitTestTextRange(
            start as u32,
            (end - start) as u32,
            0.0,
            0.0,
            Some(&mut metrics),
            &mut count,
        )
    };
    if ok.is_err() {
        return Vec::new();
    }
    metrics[..(count as usize).min(metrics.len())]
        .iter()
        .map(|metric| (metric.left, metric.left + metric.width))
        .collect()
}

/// The x offset of a caret position within a text layout.
pub fn caret_offset(layout: &IDWriteTextLayout, position: usize) -> f32 {
    let mut x = 0.0f32;
    let mut y = 0.0f32;
    let mut metrics = DWRITE_HIT_TEST_METRICS::default();
    unsafe {
        let _ = layout.HitTestTextPosition(position as u32, false, &mut x, &mut y, &mut metrics);
    }
    x
}

/// The text position closest to an x offset within a text layout.
pub fn position_at(layout: &IDWriteTextLayout, x: f32, y: f32) -> usize {
    let mut trailing = windows::core::BOOL(0);
    let mut inside = windows::core::BOOL(0);
    let mut metrics = DWRITE_HIT_TEST_METRICS::default();
    unsafe {
        if layout
            .HitTestPoint(x, y, &mut trailing, &mut inside, &mut metrics)
            .is_err()
        {
            return 0;
        }
    }
    metrics.textPosition as usize
        + if trailing.as_bool() {
            metrics.length as usize
        } else {
            0
        }
}
