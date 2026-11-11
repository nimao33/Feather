//! Where everything in the main window goes. Pure geometry, so it can be
//! unit-tested without a window.
//!
//! ```text
//! +----------+--------------------------------------------+-------------+
//! | feather  | Looking in ...                     Filters | ☀  (caption)|
//! |          +--------------------------------------------+             |
//! | kinds    |  (search)  invoice                96 found | details     |
//! |          |--------------------------------------------| (icon,      |
//! | look in  |  Name and folder           Size  Modified  |  name,      |
//! |          |  [icon] row                                |  metadata,  |
//! | Settings |  [icon] row                                |  actions)   |
//! | Help     |                                            |             |
//! | notice   |                                            |             |
//! +----------+--------------------------------------------+-------------+
//!
//! Every top-bar button stays inside its own column, so a hover highlight
//! never crosses the line between the results and the details pane.
//! ```

use super::gfx::Rect;
use crate::filters::Category;

pub const SIDEBAR_W: f32 = 216.0;
/// The sidebar folded down to its icons.
pub const SIDEBAR_COLLAPSED_W: f32 = 64.0;
/// Width of the theme button, the same as a Windows caption button.
pub const CAPTION_BUTTON_W: f32 = 46.0;
pub const DETAILS_W: f32 = 320.0;
pub const DETAILS_MIN_WINDOW: f32 = 1000.0;
pub const TOPBAR_H: f32 = 44.0;
pub const SEARCH_H: f32 = 72.0;
pub const HEADER_H: f32 = 34.0;
pub const ROW_H: f32 = 50.0;
pub const NAV_H: f32 = 32.0;
pub const NAV_LABEL_H: f32 = 30.0;
pub const SIZE_COL_W: f32 = 84.0;
pub const DATE_COL_W: f32 = 132.0;
pub const SCROLL_GUTTER: f32 = 14.0;
/// Height of the strip along the top edge that resizes the window.
pub const RESIZE_EDGE: f32 = 6.0;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum Button {
    Filters,
    Settings,
    Help,
    Theme,
    /// Shows and hides the details pane.
    Details,
    /// Folds the sidebar down to its icons and back.
    Sidebar,
    Open,
    Reveal,
    CopyPath,
    SortName,
    SortSize,
    SortModified,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NavItem {
    Category(Category),
    /// `None` is "All locations".
    Scope(Option<String>),
    ChooseFolder,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NavEntry {
    Label(&'static str),
    Item(NavItem),
}

#[derive(Clone, Debug, Default)]
pub struct Layout {
    pub client: Rect,
    pub sidebar: Rect,
    /// The results column, including its part of the top bar.
    pub main: Rect,
    pub details: Option<Rect>,
    /// The draggable strip across the main column and details pane.
    pub topbar: Rect,
    pub wordmark: Rect,
    /// The app icon left of the wordmark.
    pub logo: Rect,
    /// Where Windows draws minimize, maximize, and close.
    pub caption: Rect,
    /// The vertical band available for sidebar entries.
    pub nav: Rect,
    pub notice: Rect,
    pub scope_label: Rect,
    pub buttons: Vec<(Button, Rect)>,
    pub search_icon: Rect,
    pub search_text: Rect,
    pub count_label: Rect,
    pub header: Rect,
    pub name_col: Rect,
    pub size_col: Rect,
    pub date_col: Rect,
    pub list: Rect,
}

impl Layout {
    pub fn button(&self, button: Button) -> Option<Rect> {
        self.buttons
            .iter()
            .find(|(candidate, _)| *candidate == button)
            .map(|(_, rect)| *rect)
    }

    pub fn button_at(&self, x: f32, y: f32) -> Option<Button> {
        self.buttons
            .iter()
            .find(|(_, rect)| rect.contains(x, y))
            .map(|(button, _)| *button)
    }

    /// Rows that fit in the list viewport, rounded up to include a partial row.
    pub fn visible_rows(&self) -> usize {
        (self.list.height() / ROW_H).ceil().max(0.0) as usize + 1
    }
}

/// What the window frame and the sidebar's fold state contribute.
#[derive(Clone, Copy, Debug)]
pub struct Chrome {
    /// Width and height of the Windows caption buttons.
    pub caption_w: f32,
    pub caption_h: f32,
    /// The sidebar's current width, between collapsed and full.
    pub sidebar_w: f32,
    /// How far the details pane is out: 0 hidden, `DETAILS_W` shown.
    pub details_w: f32,
}

pub fn compute(width: f32, height: f32, has_selection: bool, chrome: Chrome) -> Layout {
    let client = Rect::new(0.0, 0.0, width, height);
    let sidebar = Rect::new(0.0, 0.0, chrome.sidebar_w.min(width), height);
    // The pane fits only in a wide window; the toggle slides it in and out.
    let fits = width >= DETAILS_MIN_WINDOW;
    let details_w = if fits {
        chrome.details_w.clamp(0.0, DETAILS_W)
    } else {
        0.0
    };
    let show_details = details_w > 0.5;
    let main_right = width - details_w;
    let main = Rect::new(sidebar.r, 0.0, main_right.max(sidebar.r), height);
    let details = show_details.then(|| Rect::new(main.r, TOPBAR_H, main.r + DETAILS_W, height));
    let topbar = Rect::new(sidebar.r, 0.0, width, TOPBAR_H);

    let mut buttons = Vec::new();
    // Windows draws its own minimize, maximize, and close buttons in the
    // top-right corner of the backdrop; keep that strip clear.
    let caption_left = width - chrome.caption_w.max(0.0);
    let caption = Rect::new(caption_left, 0.0, width, chrome.caption_h);
    // The theme button sits beside them as a fourth caption button.
    let theme = Rect::new(
        caption_left - CAPTION_BUTTON_W,
        0.0,
        caption_left,
        chrome.caption_h,
    );
    buttons.push((Button::Theme, theme));
    // The details toggle sits beside it, while the window is wide enough.
    let toggle =
        fits.then(|| Rect::new(theme.l - CAPTION_BUTTON_W, 0.0, theme.l, chrome.caption_h));
    if let Some(rect) = toggle {
        buttons.push((Button::Details, rect));
    }
    let toolbar_r = toggle.map_or(theme.l, |rect| rect.l) - 6.0;
    // Filters belongs to the results column: flush with its right edge when
    // the details pane is open, otherwise just left of the caption buttons.
    let filters_r = if show_details {
        (main.r - 12.0).min(toolbar_r)
    } else {
        toolbar_r
    };
    let filters = Rect::new(filters_r - 82.0, 6.0, filters_r, 38.0);
    buttons.push((Button::Filters, filters));
    let scope_label = Rect::new(
        main.l + 28.0,
        0.0,
        (filters.l - 12.0).max(main.l + 28.0),
        TOPBAR_H,
    );

    let search_row = Rect::new(main.l, TOPBAR_H, main.r, TOPBAR_H + SEARCH_H);
    let search_icon = Rect::new(
        main.l + 26.0,
        search_row.t + 22.0,
        main.l + 50.0,
        search_row.t + 46.0,
    );
    let count_w = 150.0_f32.min((main.width() - 120.0).max(0.0));
    let count_label = Rect::new(
        main.r - 24.0 - count_w,
        search_row.t,
        main.r - 24.0,
        search_row.b,
    );
    let search_text = Rect::new(
        search_icon.r + 14.0,
        search_row.t + 8.0,
        count_label.l - 12.0,
        search_row.b - 8.0,
    );

    let header = Rect::new(main.l, search_row.b, main.r, search_row.b + HEADER_H);
    let content_r = main.r - SCROLL_GUTTER;
    let date_col = Rect::new(content_r - DATE_COL_W, header.t, content_r, header.b);
    let size_col = Rect::new(
        date_col.l - 16.0 - SIZE_COL_W,
        header.t,
        date_col.l - 16.0,
        header.b,
    );
    let name_col = Rect::new(main.l + 28.0, header.t, size_col.l - 16.0, header.b);
    buttons.push((
        Button::SortName,
        Rect::new(main.l + 8.0, header.t, size_col.l - 8.0, header.b),
    ));
    buttons.push((
        Button::SortSize,
        Rect::new(size_col.l - 8.0, header.t, size_col.r + 8.0, header.b),
    ));
    buttons.push((
        Button::SortModified,
        Rect::new(date_col.l - 8.0, header.t, date_col.r, header.b),
    ));
    let list = Rect::new(main.l + 8.0, header.b + 2.0, main.r - 4.0, height - 8.0);

    if let (Some(details), true) = (details, has_selection) {
        let inner = details.inset(28.0, 0.0);
        let reveal_row = Rect::new(inner.l, details.b - 24.0 - 40.0, inner.r, details.b - 24.0);
        let half = (inner.width() - 8.0) / 2.0;
        buttons.push((
            Button::Reveal,
            Rect::new(inner.l, reveal_row.t, inner.l + half, reveal_row.b),
        ));
        buttons.push((
            Button::CopyPath,
            Rect::new(inner.r - half, reveal_row.t, inner.r, reveal_row.b),
        ));
        buttons.push((
            Button::Open,
            Rect::new(
                inner.l,
                reveal_row.t - 8.0 - 44.0,
                inner.r,
                reveal_row.t - 8.0,
            ),
        ));
    }

    // The app icon sits where folded tabs' icons do, so it stays put.
    let logo = Rect::new(21.0, 19.0, 43.0, 41.0);
    let wordmark = Rect::new(52.0, 12.0, SIDEBAR_W - 12.0, 48.0);
    let notice = Rect::new(24.0, height - 58.0, SIDEBAR_W - 16.0, height - 12.0);
    // Settings, Help, and the fold button at the bottom, above the status.
    // Folded, the status text is gone, so they slide down into its place.
    let open = ((sidebar.width() - SIDEBAR_COLLAPSED_W) / (SIDEBAR_W - SIDEBAR_COLLAPSED_W))
        .clamp(0.0, 1.0);
    let footer_b = height - 12.0 - (notice.height() + 6.0) * open;
    let toggle = Rect::new(12.0, footer_b - NAV_H, sidebar.r - 12.0, footer_b);
    buttons.push((Button::Sidebar, toggle));
    let help = Rect::new(
        12.0,
        toggle.t - 2.0 - NAV_H,
        sidebar.r - 12.0,
        toggle.t - 2.0,
    );
    let settings = Rect::new(help.l, help.t - 2.0 - NAV_H, help.r, help.t - 2.0);
    buttons.push((Button::Settings, settings));
    buttons.push((Button::Help, help));
    let nav = Rect::new(12.0, 62.0, sidebar.r - 12.0, settings.t - 12.0);

    Layout {
        client,
        sidebar,
        main,
        details,
        topbar,
        wordmark,
        logo,
        caption,
        nav,
        notice,
        scope_label,
        buttons,
        search_icon,
        search_text,
        count_label,
        header,
        name_col,
        size_col,
        date_col,
        list,
    }
}

/// Stacks sidebar entries top to bottom. Entries that do not fit get an
/// empty rectangle and are skipped by painting and hit testing.
pub fn nav_rects(layout: &Layout, entries: &[NavEntry]) -> Vec<Rect> {
    let mut y = layout.nav.t;
    entries
        .iter()
        .map(|entry| {
            let height = match entry {
                NavEntry::Label(_) => NAV_LABEL_H,
                NavEntry::Item(_) => NAV_H,
            };
            let rect = Rect::new(layout.nav.l, y, layout.nav.r, y + height);
            y += height
                + if matches!(entry, NavEntry::Item(_)) {
                    2.0
                } else {
                    0.0
                };
            if rect.b > layout.nav.b {
                Rect::default()
            } else {
                rect
            }
        })
        .collect()
}

/// Scroll offset that keeps `row` fully visible.
///
/// Offsets are `f64`: a few hundred thousand rows already pass what `f32`
/// holds to the pixel.
pub fn scroll_to_row(scroll: f64, row: usize, viewport: f64) -> f64 {
    let top = row as f64 * f64::from(ROW_H);
    let bottom = top + f64::from(ROW_H);
    if top < scroll {
        top
    } else if bottom > scroll + viewport {
        (bottom - viewport).max(0.0)
    } else {
        scroll
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAPTION: Chrome = Chrome {
        caption_w: 138.0,
        caption_h: 32.0,
        sidebar_w: SIDEBAR_W,
        details_w: DETAILS_W,
    };

    #[test]
    fn the_details_pane_can_be_hidden() {
        let hidden = Chrome {
            details_w: 0.0,
            ..CAPTION
        };
        let layout = compute(1200.0, 760.0, true, hidden);
        assert!(layout.details.is_none());
        assert_eq!(layout.main.r, 1200.0);
        let toggle = layout.button(Button::Details).unwrap();
        let filters = layout.button(Button::Filters).unwrap();
        assert!(filters.r <= toggle.l);
        // Half way out, it slides in from the right edge.
        let half = Chrome {
            details_w: DETAILS_W / 2.0,
            ..CAPTION
        };
        let layout = compute(1200.0, 760.0, true, half);
        assert_eq!(layout.details.unwrap().l, 1200.0 - DETAILS_W / 2.0);
        assert!(compute(800.0, 600.0, true, CAPTION)
            .button(Button::Details)
            .is_none());
    }

    #[test]
    fn details_pane_hides_on_narrow_windows() {
        assert!(compute(1200.0, 760.0, true, CAPTION).details.is_some());
        let narrow = compute(800.0, 600.0, true, CAPTION);
        assert!(narrow.details.is_none());
        assert!(narrow.button(Button::Open).is_none());
        assert_eq!(narrow.main.r, 800.0);
    }

    #[test]
    fn toolbar_stays_clear_of_the_system_caption_buttons() {
        let layout = compute(1200.0, 760.0, false, CAPTION);
        let theme = layout.button(Button::Theme).unwrap();
        assert!(theme.r <= 1200.0 - CAPTION.caption_w);
        assert_eq!(theme.height(), CAPTION.caption_h);
        assert_eq!(layout.button_at(1199.0, 10.0), None);
        assert!(layout.button(Button::Open).is_none());
    }

    #[test]
    fn top_bar_buttons_stay_inside_their_column() {
        for width in [1000.0, 1200.0, 1600.0, 800.0] {
            let layout = compute(width, 760.0, true, CAPTION);
            let filters = layout.button(Button::Filters).unwrap();
            let theme = layout.button(Button::Theme).unwrap();
            assert!(filters.l >= layout.main.l && filters.r <= layout.main.r);
            assert!(filters.r <= theme.l);
            if let Some(details) = layout.details {
                assert!(theme.l >= details.l, "theme button crosses into results");
            }
            assert!(layout.scope_label.r <= filters.l);
        }
    }

    #[test]
    fn settings_and_help_sit_below_the_sidebar_entries() {
        let layout = compute(1200.0, 760.0, false, CAPTION);
        let settings = layout.button(Button::Settings).unwrap();
        let help = layout.button(Button::Help).unwrap();
        assert!(settings.r <= layout.sidebar.r && help.r <= layout.sidebar.r);
        assert!(layout.nav.b <= settings.t);
        assert!(settings.b <= help.t);
        assert!(help.b <= layout.notice.t);
        let toggle = layout.button(Button::Sidebar).unwrap();
        assert!(help.b <= toggle.t && toggle.b <= layout.notice.t);
    }

    #[test]
    fn columns_do_not_overlap() {
        let layout = compute(1200.0, 760.0, true, CAPTION);
        assert!(layout.name_col.r <= layout.size_col.l);
        assert!(layout.size_col.r <= layout.date_col.l);
        assert!(layout.date_col.r <= layout.main.r);
        assert!(layout.search_text.r <= layout.count_label.l);
    }

    #[test]
    fn nav_entries_that_do_not_fit_are_dropped() {
        let layout = compute(1200.0, 300.0, false, CAPTION);
        let entries: Vec<NavEntry> = (0..20)
            .map(|_| NavEntry::Item(NavItem::ChooseFolder))
            .collect();
        let rects = nav_rects(&layout, &entries);
        assert!(!rects[0].is_empty());
        assert!(rects[19].is_empty());
    }

    #[test]
    fn a_collapsed_sidebar_keeps_its_buttons_inside() {
        let chrome = Chrome {
            sidebar_w: SIDEBAR_COLLAPSED_W,
            ..CAPTION
        };
        let layout = compute(1200.0, 760.0, false, chrome);
        assert_eq!(layout.sidebar.r, SIDEBAR_COLLAPSED_W);
        assert_eq!(layout.main.l, SIDEBAR_COLLAPSED_W);
        let toggle = layout.button(Button::Sidebar).unwrap();
        assert!(toggle.r <= SIDEBAR_COLLAPSED_W);
        // With no status text under them, the buttons sit at the bottom.
        assert_eq!(toggle.b, 760.0 - 12.0);
        assert!(layout.nav.r <= SIDEBAR_COLLAPSED_W);
    }

    #[test]
    fn scrolling_keeps_rows_in_view() {
        assert_eq!(scroll_to_row(0.0, 0, 500.0), 0.0);
        assert_eq!(
            scroll_to_row(0.0, 10, 500.0),
            11.0 * f64::from(ROW_H) - 500.0
        );
        assert_eq!(scroll_to_row(400.0, 2, 500.0), 2.0 * f64::from(ROW_H));
        // Far down a long list the offset still lands exactly on a row.
        let row = 3_000_001;
        assert_eq!(
            scroll_to_row(0.0, row, 500.0),
            (row + 1) as f64 * 50.0 - 500.0
        );
    }
}
