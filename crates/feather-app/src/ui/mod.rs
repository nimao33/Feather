mod anim;
mod canvas;
pub mod color_space;
pub(crate) mod context_menu;
mod eyedropper;
mod fileops;
pub(crate) mod form;
mod frames;
mod fullscreen;
mod gfx;
pub(crate) mod glyphs;
mod help;
mod helper;
pub(crate) mod hotkey;
mod icons;
pub(crate) mod indexer;
mod keyhook;
mod layout;
mod live;
mod memory;
mod model;
mod paint;
pub(crate) mod picker;
mod screenshot;
mod setup;
mod shortcuts;
mod sort;
mod startup;
pub(crate) mod textfield;
pub(crate) mod theme;
mod tray;
pub(crate) mod wallpaper;
mod watch;
mod win;

use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Once;
use std::time::{Duration, Instant};

use feather_core::scan::ScanOptions;
use feather_core::{EntryRef, IndexSet, MatchMode, Query};
use windows::Win32::Graphics::Direct2D::ID2D1RenderTarget;
use windows_sys::core::w;
use windows_sys::Win32::Foundation::{
    FILETIME, HWND, LPARAM, LRESULT, POINT, RECT, SYSTEMTIME, WPARAM,
};
use windows_sys::Win32::Graphics::Dwm::{
    DwmDefWindowProc, DwmGetWindowAttribute, DWMWA_CAPTION_BUTTON_BOUNDS,
};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, EndPaint, InvalidateRect, MonitorFromRect, ScreenToClient, UpdateWindow, HDC,
    MONITOR_DEFAULTTONULL, PAINTSTRUCT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Time::FileTimeToSystemTime;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, IsWindowEnabled, ReleaseCapture, SetCapture, SetFocus, TrackMouseEvent, TME_LEAVE,
    TME_NONCLIENT, TRACKMOUSEEVENT, VK_APPS, VK_BACK, VK_CONTROL, VK_DELETE, VK_DOWN, VK_END,
    VK_ESCAPE, VK_F10, VK_F2, VK_HOME, VK_LEFT, VK_NEXT, VK_PRIOR, VK_RETURN, VK_RIGHT, VK_SHIFT,
    VK_TAB, VK_UP,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateAcceleratorTableW, CreateWindowExW, DefWindowProcW, DestroyAcceleratorTable,
    DestroyWindow, DispatchMessageW, FindWindowW, GetCaretBlinkTime, GetCursorPos,
    GetForegroundWindow, GetLastActivePopup, GetSystemMetrics, GetWindowLongPtrW,
    GetWindowPlacement, GetWindowThreadProcessId, IsIconic, IsWindowVisible, IsZoomed, KillTimer,
    LoadCursorW, PeekMessageW, PostMessageW, PostQuitMessage, RegisterClassW, SendMessageW,
    SetCursor, SetForegroundWindow, SetTimer, SetWindowLongPtrW, SetWindowPos, ShowWindow,
    TranslateAcceleratorW, TranslateMessage, ACCEL, CS_DBLCLKS, CS_HREDRAW, CS_VREDRAW,
    CW_USEDEFAULT, FCONTROL, FVIRTKEY, GWLP_USERDATA, HACCEL, HTCAPTION, HTCLIENT, HTTOP,
    HTTOPLEFT, HTTOPRIGHT, HWND_NOTOPMOST, HWND_TOPMOST, ICON_BIG, ICON_SMALL, IDC_ARROW,
    IDC_IBEAM, MINMAXINFO, MSG, NCCALCSIZE_PARAMS, PM_REMOVE, SM_CXPADDEDBORDER, SM_CYFRAME,
    SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SW_HIDE, SW_MINIMIZE,
    SW_RESTORE, SW_SHOW, SW_SHOWMAXIMIZED, WINDOWPLACEMENT, WM_ACTIVATE, WM_CHAR, WM_CLOSE,
    WM_COMMAND, WM_DESTROY, WM_ENDSESSION, WM_ERASEBKGND, WM_GETMINMAXINFO, WM_HOTKEY, WM_KEYDOWN,
    WM_KILLFOCUS, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL,
    WM_NCCALCSIZE, WM_NCDESTROY, WM_NCHITTEST, WM_PAINT, WM_RBUTTONUP, WM_SETCURSOR, WM_SETFOCUS,
    WM_SETICON, WM_SETTINGCHANGE, WM_SIZE, WM_SYSKEYDOWN, WM_TIMER, WNDCLASSW, WS_OVERLAPPEDWINDOW,
};

use crate::filters::{Category, FilterSet, KindFilter};
use crate::search::{SearchController, SearchOptions};
use crate::settings::{self, DarkMode, Material, OpenMode, Settings, WindowPlacement};
use crate::text::{self, parent_path, size_in, type_label};
use anim::Motion;
use gfx::{Font, Gfx, Painter, Rect};
use icons::IconCache;
use indexer::{IndexEvent, Indexer};
use layout::{Button, Layout, NavEntry, NavItem, NAV_H, ROW_H};
use model::UiModel;
use paint::{DetailsView, Frame, Hit, RenameView, RowView, SearchView};
use textfield::TextField;
use theme::Palette;

const TIMER_RESULTS: usize = 1;
const TIMER_DEBOUNCE: usize = 2;
/// Filtering more rows than this on the window's thread would stall typing;
/// a search on the worker, split across cores, is as fast by then.
const NARROW_MAX: usize = 100_000;
const TIMER_CARET: usize = 3;
/// Fires a while after the window loses focus, to give memory back.
const TIMER_TRIM: usize = 4;
/// The least time between two live updates of the visible results.
const LIVE_EVERY: Duration = Duration::from_secs(3);
/// Checks once a second whether a game or other full-screen app is in
/// front, to set the shortcut aside while it is.
const TIMER_FULLSCREEN: usize = 9;
const ID_CLEAR_SEARCH: usize = 3001;
const ID_FOCUS_SEARCH: usize = 3002;
const DEFAULT_WIDTH: i32 = 1200;
const WM_MOUSELEAVE: u32 = 0x02a3;
const DEFAULT_HEIGHT: i32 = 760;
/// Posted once the main window is up, to show first-run setup over it.
const WM_RUN_SETUP: u32 = windows_sys::Win32::UI::WindowsAndMessaging::WM_APP + 0x41;
/// Posted by a second copy of Feather to bring this one forward.
const WM_SHOW_FEATHER: u32 = windows_sys::Win32::UI::WindowsAndMessaging::WM_APP + 0x43;
/// Quit, as from the tray menu: sent by `feather quit`.
const WM_QUIT_FEATHER: u32 = windows_sys::Win32::UI::WindowsAndMessaging::WM_APP + 0x48;
const HOTKEY_ID: i32 = 1;
/// Settings changed in the open Settings window; `lparam` owns a boxed
/// `Settings` to show right away.
const WM_PREVIEW_SETTINGS: u32 = windows_sys::Win32::UI::WindowsAndMessaging::WM_APP + 0x44;
/// "Reindex now" was pressed in the open Settings window.
const WM_REINDEX: u32 = windows_sys::Win32::UI::WindowsAndMessaging::WM_APP + 0x45;
/// Settings started sharing with these settings (a boxed `Settings`).
const WM_APPLY_SHARING: u32 = windows_sys::Win32::UI::WindowsAndMessaging::WM_APP + 0x46;
/// Settings asks for sharing's status lines (lparam points at the answer).
const WM_SHARING_STATUS: u32 = windows_sys::Win32::UI::WindowsAndMessaging::WM_APP + 0x47;
/// A newer Feather was found (`crate::update::found`).
const WM_UPDATE_FOUND: u32 = windows_sys::Win32::UI::WindowsAndMessaging::WM_APP + 0x49;

/// An in-progress mouse drag.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum Drag {
    #[default]
    None,
    /// Extending the search text selection.
    Text,
    /// Moving the scrollbar thumb from a starting pointer y and scroll offset.
    Thumb { start_y: f32, start_scroll: f64 },
    /// Holding a sidebar tab; past a few pixels it lifts and reorders.
    Nav {
        index: usize,
        grab: f32,
        start_y: f32,
        y: f32,
        moving: bool,
    },
}

pub struct Ui {
    hwnd: HWND,
    gfx: Gfx,
    canvas: canvas::Canvas,
    icons: IconCache,
    palette: Palette,
    material: Material,
    field: TextField,
    search_scroll: f32,
    caret_on: bool,
    focused: bool,
    index: Arc<IndexSet>,
    controller: SearchController,
    model: UiModel,
    filters: FilterSet,
    /// The query the visible results came from, for highlighting the terms
    /// that matched each name.
    query: Query,
    /// The options the visible results were searched with; results are
    /// only narrowed while these stay the same.
    search_key: String,
    scope: Option<String>,
    settings: Settings,
    persist: bool,
    accel: HACCEL,
    sort_column: usize,
    sort_ascending: bool,
    layout: Layout,
    nav: Vec<NavEntry>,
    nav_rects: Vec<Rect>,
    scroll: f64,
    selected: Option<usize>,
    hover: Hit,
    pressed: Hit,
    drag: Drag,
    tracking: bool,
    notice: String,
    motion: Motion,
    anim_timer: bool,
    /// Changes whenever the result rows are replaced or reordered, so the
    /// selection highlight snaps instead of sliding to an unrelated row.
    epoch: u64,
    /// The library directory the in-app indexer rebuilds; `None` in tests.
    library: Option<PathBuf>,
    indexer: Option<Indexer>,
    /// Lets indexing ask for admin rights (off in the test scenarios).
    allow_elevation: bool,
    /// The folder being scanned right now, shown in the sidebar.
    indexing_root: String,
    last_progress: Instant,
    /// Whether the global shortcut is registered to this window.
    hotkey_on: bool,
    /// The shortcut is set aside because a full-screen app is in front.
    hotkey_paused: bool,
    /// Whether the tray icon is showing.
    tray_on: bool,
    /// The tray icon was added only to announce an update.
    update_icon: bool,
    /// Set when the shortcut brought the window up; Escape then hides it.
    summoned: bool,
    /// Set by Quit, so closing really exits instead of hiding to the tray.
    quitting: bool,
    /// Window procedure calls using this `Ui` right now. More than one
    /// means a handler is running a message loop of its own (Settings, a
    /// menu, a file picker) and is still using it.
    entered: u32,
    /// Closing was asked for while a handler was still using the window; it
    /// closes when that handler returns.
    close_pending: bool,
    /// The window is gone; the outermost call still using this `Ui` frees it.
    destroyed: bool,
    /// The sidebar's width, animated between folded and open.
    sidebar_w: anim::Tween,
    /// How far the details pane is out, animated by its toggle.
    details_w: anim::Tween,
    /// The color last set behind Windows' caption buttons.
    caption_shown: Option<u32>,
    /// The custom theme's background picture.
    wallpaper: wallpaper::Wallpaper,
    /// The web and FTP servers, while sharing is on.
    sharing: Option<crate::share::Sharing>,
    /// The sharing line last shown, so changes show once.
    sharing_shown: String,
    /// How far each sidebar entry is pushed while a tab is dragged.
    nav_shift: Vec<anim::Tween>,
    /// A new index being searched in the background; it replaces the
    /// current one once its results are ready, so the list never blanks.
    swap: Option<Swap>,
    /// Notices files appearing and disappearing under the searched places.
    watcher: Option<watch::Watcher>,
    /// Changes already covered by the last build.
    seen_changes: u64,
    /// When unhandled changes were first seen.
    changes_since: Option<Instant>,
    /// When the last build finished, and how long it took.
    last_build: Option<Instant>,
    build_took: Duration,
    build_started: Instant,
    /// Keys act on the selected result (Delete, Ctrl+C, Ctrl+V) instead of
    /// the search text: set by clicking a result or moving through the list
    /// with the arrow keys, cleared by typing or clicking the search box.
    list_focus: bool,
    /// The name being typed over a result to rename it (F2).
    rename: Option<Rename>,
    /// Places Feather changed itself (rename, delete, paste), read again
    /// shortly after `refresh_at`.
    refresh_roots: Vec<String>,
    refresh_at: Option<Instant>,
    /// A path to select once the next index is in, such as a renamed item.
    reselect: Option<String>,
    /// Follows the change journals of whole NTFS drives.
    live: Option<live::Live>,
    /// The places `live` follows; the watcher leaves them alone.
    live_roots: Vec<String>,
    /// Live updates could not get admin rights this session.
    live_off: bool,
    /// The newest live snapshot, waiting for the running swap to finish.
    live_pending: Option<Arc<IndexSet>>,
    /// When the last live snapshot was shown: changes are taken in at most
    /// every few seconds, so a busy drive doesn't keep the list moving.
    live_shown: Option<Instant>,
    /// A live snapshot was taken in while hidden; search it when shown.
    live_stale: bool,
    /// The live overlay is large: fold it into a new base when idle.
    fold_wanted: bool,
    /// Places to read again as soon as nothing else is being built.
    pending_rebuild: Vec<String>,
    /// The places the watcher watches, in its order.
    watched_roots: Vec<String>,
}

/// Renaming a result in place.
struct Rename {
    row: usize,
    entry: EntryRef,
    path: String,
    field: TextField,
    scroll: f32,
}

/// A freshly built index and its first search, waiting to be shown.
struct Swap {
    index: Arc<IndexSet>,
    controller: SearchController,
    model: UiModel,
    /// Paths to select in the new results, the first one found: an item
    /// Feather just renamed, then the item that was selected.
    keep: Vec<String>,
    scroll: f64,
    /// A new library on disk (a build or a fold), as opposed to a live
    /// snapshot of the one in use.
    rebuilt: bool,
}

impl Ui {
    fn new(
        hwnd: HWND,
        index: Arc<IndexSet>,
        settings: Settings,
        persist: bool,
    ) -> Result<Self, String> {
        let gfx = Gfx::new()?;
        let controller = SearchController::new(Arc::clone(&index));
        let accelerators = [
            ACCEL {
                fVirt: FVIRTKEY | FCONTROL,
                key: b'F' as u16,
                cmd: ID_FOCUS_SEARCH as u16,
            },
            ACCEL {
                fVirt: FVIRTKEY,
                key: VK_ESCAPE,
                cmd: ID_CLEAR_SEARCH as u16,
            },
        ];
        let accel =
            unsafe { CreateAcceleratorTableW(accelerators.as_ptr(), accelerators.len() as i32) };
        if accel.is_null() {
            return Err("could not create keyboard shortcuts".into());
        }

        let filters = if settings.remember_filters {
            settings.filters.clone()
        } else {
            FilterSet::default()
        };
        let scope = if settings.remember_scope {
            settings.scope.clone()
        } else {
            None
        };
        let settings_collapsed = settings.sidebar_collapsed;
        let details_shown = settings.details_pane;
        let mut ui = Self {
            hwnd,
            gfx,
            canvas: canvas::Canvas::default(),
            icons: IconCache::default(),
            palette: Palette::new(false, settings.accent, settings.material),
            material: settings.material,
            field: TextField::default(),
            search_scroll: 0.0,
            caret_on: true,
            focused: false,
            index,
            controller,
            model: UiModel::default(),
            filters,
            query: Query::default(),
            search_key: String::new(),
            scope,
            settings,
            persist,
            accel,
            sort_column: sort::COLUMN_NAME,
            sort_ascending: true,
            layout: Layout::default(),
            nav: Vec::new(),
            nav_rects: Vec::new(),
            scroll: 0.0,
            selected: None,
            hover: Hit::None,
            pressed: Hit::None,
            drag: Drag::None,
            tracking: false,
            notice: String::new(),
            motion: Motion::new(anim::system_animations_enabled()),
            anim_timer: false,
            epoch: 0,
            library: None,
            indexer: None,
            allow_elevation: true,
            indexing_root: String::new(),
            last_progress: Instant::now(),
            hotkey_on: false,
            hotkey_paused: false,
            tray_on: false,
            update_icon: false,
            summoned: false,
            quitting: false,
            entered: 0,
            close_pending: false,
            destroyed: false,
            sidebar_w: anim::Tween::new(if settings_collapsed {
                layout::SIDEBAR_COLLAPSED_W
            } else {
                layout::SIDEBAR_W
            }),
            wallpaper: wallpaper::Wallpaper::default(),
            sharing: None,
            sharing_shown: String::new(),
            details_w: anim::Tween::new(if details_shown {
                layout::DETAILS_W
            } else {
                0.0
            }),
            caption_shown: None,
            nav_shift: Vec::new(),
            swap: None,
            watcher: None,
            seen_changes: 0,
            changes_since: None,
            last_build: None,
            build_took: Duration::from_secs(3),
            build_started: Instant::now(),
            list_focus: false,
            rename: None,
            refresh_roots: Vec::new(),
            refresh_at: None,
            reselect: None,
            live: None,
            live_roots: Vec::new(),
            live_off: false,
            live_pending: None,
            live_shown: None,
            live_stale: false,
            fold_wanted: false,
            pending_rebuild: Vec::new(),
            watched_roots: Vec::new(),
        };
        ui.refresh_theme(false);
        ui.rebuild_nav();
        ui.layout();
        unsafe {
            SetTimer(hwnd, TIMER_RESULTS, 16, None);
            SetTimer(hwnd, TIMER_FULLSCREEN, 1000, None);
            SetTimer(
                hwnd,
                TIMER_CARET,
                GetCaretBlinkTime().clamp(200, 2000),
                None,
            );
        }
        ui.apply_always_on_top();
        ui.start_search("");
        Ok(ui)
    }

    // ----- Theme and layout -------------------------------------------------

    /// Re-reads the theme settings. With `animate`, the window cross-fades
    /// from the colors currently on screen.
    fn refresh_theme(&mut self, animate: bool) {
        let now = Instant::now();
        let shown = self.motion.palette(self.palette, now);
        let dark = theme::resolve_dark(self.settings.dark_mode);
        self.material = if theme::backdrop_supported() {
            self.settings.material
        } else {
            Material::Solid
        };
        self.palette = if self.settings.custom_theme {
            Palette::custom(&self.settings.theme_colors, self.material)
        } else {
            Palette::new(dark, self.settings.accent, self.material)
        };
        theme::apply_window_effects(self.hwnd, self.palette.dark, self.material);
        theme::set_menu_dark(self.palette.dark);
        self.caption_shown = None;
        self.sync_caption();
        let picture = if self.settings.custom_theme {
            self.settings.background_image.clone()
        } else {
            String::new()
        };
        self.wallpaper.set(&picture);
        if animate {
            self.motion.change_theme(shown, now);
            self.kick();
        }
        self.invalidate();
    }

    fn toggle_theme(&mut self) {
        // From a custom theme, the button goes back to the built-in colors.
        self.settings.custom_theme = false;
        self.settings.dark_mode = if self.palette.dark {
            DarkMode::Light
        } else {
            DarkMode::Full
        };
        self.refresh_theme(true);
        self.save_settings();
    }

    /// The kinds shown in the sidebar, in the person's order.
    fn visible_tabs(&self) -> Vec<Category> {
        self.settings
            .sidebar_tabs
            .iter()
            .filter(|(_, shown)| *shown)
            .map(|(category, _)| *category)
            .collect()
    }

    fn rebuild_nav(&mut self) {
        let mut nav: Vec<NavEntry> = self
            .visible_tabs()
            .into_iter()
            .map(|category| NavEntry::Item(NavItem::Category(category)))
            .collect();
        nav.push(NavEntry::Label("Look in"));
        nav.push(NavEntry::Item(NavItem::Scope(None)));
        let prefixes: Vec<String> = self
            .index
            .infos()
            .into_iter()
            .map(|info| info.prefix)
            .collect();
        for prefix in &prefixes {
            nav.push(NavEntry::Item(NavItem::Scope(Some(prefix.clone()))));
        }
        if let Some(scope) = &self.scope {
            if !prefixes
                .iter()
                .any(|prefix| prefix.eq_ignore_ascii_case(scope))
            {
                nav.push(NavEntry::Item(NavItem::Scope(Some(scope.clone()))));
            }
        }
        nav.push(NavEntry::Item(NavItem::ChooseFolder));
        self.nav_shift = (0..nav.len()).map(|_| anim::Tween::new(0.0)).collect();
        self.nav = nav;
        self.update_nav_rects();
    }

    // ----- Sidebar ------------------------------------------------------------

    fn chrome(&self) -> layout::Chrome {
        let (caption_w, caption_h) = caption_bounds(self.hwnd);
        layout::Chrome {
            caption_w,
            caption_h,
            sidebar_w: self.sidebar_w.value(Instant::now()),
            details_w: self.details_w.value(Instant::now()),
        }
    }

    /// Keeps the strip behind Windows' caption buttons the color of what
    /// Feather paints around them: the window's background while the
    /// details pane is under them, the results column's once it is hidden.
    fn sync_caption(&mut self) {
        if self.material != Material::Solid {
            self.caption_shown = None;
            return;
        }
        let pal = &self.palette;
        let over = |under: gfx::Color, top: gfx::Color| gfx::Color {
            a: 1.0,
            ..under.mix(gfx::Color { a: 1.0, ..top }, top.a)
        };
        let mut color = if pal.base.a < 1.0 {
            pal.opaque_base()
        } else {
            pal.base
        };
        color = over(color, pal.tint);
        if self.details_w.target() < 0.5 {
            color = over(color, pal.card);
        }
        let packed = color.to_rgb();
        if self.caption_shown != Some(packed) {
            theme::set_caption_color(self.hwnd, color);
            self.caption_shown = Some(packed);
        }
    }

    /// Slides the details pane out or back in.
    fn toggle_details(&mut self) {
        self.settings.details_pane = !self.settings.details_pane;
        let target = if self.settings.details_pane {
            layout::DETAILS_W
        } else {
            0.0
        };
        let time = if self.motion.enabled {
            Duration::from_millis(240)
        } else {
            Duration::ZERO
        };
        self.details_w.animate_to(target, time, Instant::now());
        self.kick();
        self.layout();
        self.save_settings();
    }

    /// 1 with the sidebar open, 0 folded to icons; labels fade with it.
    fn sidebar_open(&self) -> f32 {
        let width = self.sidebar_w.value(Instant::now());
        ((width - layout::SIDEBAR_COLLAPSED_W) / (layout::SIDEBAR_W - layout::SIDEBAR_COLLAPSED_W))
            .clamp(0.0, 1.0)
    }

    fn toggle_sidebar(&mut self) {
        self.settings.sidebar_collapsed = !self.settings.sidebar_collapsed;
        let target = if self.settings.sidebar_collapsed {
            layout::SIDEBAR_COLLAPSED_W
        } else {
            layout::SIDEBAR_W
        };
        let time = if self.motion.enabled {
            Duration::from_millis(260)
        } else {
            Duration::ZERO
        };
        self.sidebar_w.animate_to(target, time, Instant::now());
        self.kick();
        self.layout();
        self.save_settings();
    }

    /// The slot among the visible tabs a dragged tab would drop into.
    fn nav_drop_slot(&self, index: usize, top: f32) -> usize {
        let tabs = self.visible_tabs().len();
        if tabs == 0 {
            return 0;
        }
        let step = NAV_H + 2.0;
        let first = self.nav_rects.first().map(|rect| rect.t).unwrap_or(0.0);
        let slot = ((top - first) / step).round();
        (slot.max(0.0) as usize)
            .min(tabs - 1)
            .min(index.max(tabs - 1))
    }

    /// Slides the other tabs out of the way of the dragged one.
    fn shift_tabs(&mut self, index: usize, slot: usize) {
        let now = Instant::now();
        let time = if self.motion.enabled {
            Duration::from_millis(150)
        } else {
            Duration::ZERO
        };
        let step = NAV_H + 2.0;
        let tabs = self.visible_tabs().len();
        for (other, shift) in self.nav_shift.iter_mut().enumerate().take(tabs) {
            let target = if other == index {
                0.0
            } else if index < other && other <= slot {
                -step
            } else if slot <= other && other < index {
                step
            } else {
                0.0
            };
            if (shift.target() - target).abs() > 0.1 {
                shift.animate_to(target, time, now);
            }
        }
        self.kick();
    }

    /// Moves the visible tab at `from` to `to`, keeping hidden tabs where
    /// they were in the full list.
    fn move_tab(&mut self, from: usize, to: usize) {
        let mut visible = self.visible_tabs();
        if from >= visible.len() || from == to {
            return;
        }
        let moved = visible.remove(from);
        visible.insert(to.min(visible.len()), moved);
        let mut order = visible.into_iter();
        for (category, shown) in &mut self.settings.sidebar_tabs {
            if *shown {
                if let Some(next) = order.next() {
                    *category = next;
                }
            }
        }
        self.rebuild_nav();
        self.save_settings();
        self.invalidate();
    }

    /// Right-clicking the sidebar: hide this tab, show or hide any tab,
    /// fold the sidebar, or reset it.
    fn show_sidebar_menu(&mut self, category: Option<Category>) {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            AppendMenuW, CreatePopupMenu, DestroyMenu, TrackPopupMenuEx, MF_CHECKED, MF_SEPARATOR,
            MF_STRING, TPM_RETURNCMD, TPM_RIGHTBUTTON,
        };
        const HIDE: usize = 1;
        const FOLD: usize = 2;
        const RESET: usize = 3;
        const TOGGLE: usize = 100;
        let text = |label: &str| win::to_wide(label);
        unsafe {
            let menu = CreatePopupMenu();
            if menu.is_null() {
                return;
            }
            if let Some(category) = category {
                AppendMenuW(
                    menu,
                    MF_STRING,
                    HIDE,
                    text(&format!("Hide {}", category.label())).as_ptr(),
                );
                AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
            }
            for (index, (tab, shown)) in self.settings.sidebar_tabs.iter().enumerate() {
                let flags = if *shown {
                    MF_STRING | MF_CHECKED
                } else {
                    MF_STRING
                };
                AppendMenuW(menu, flags, TOGGLE + index, text(tab.label()).as_ptr());
            }
            AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
            let fold = if self.settings.sidebar_collapsed {
                "Expand sidebar"
            } else {
                "Collapse sidebar"
            };
            AppendMenuW(menu, MF_STRING, FOLD, text(fold).as_ptr());
            AppendMenuW(menu, MF_STRING, RESET, text("Reset sidebar").as_ptr());
            let mut point = POINT { x: 0, y: 0 };
            GetCursorPos(&mut point);
            let command = TrackPopupMenuEx(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON,
                point.x,
                point.y,
                self.hwnd,
                std::ptr::null(),
            ) as usize;
            DestroyMenu(menu);
            match command {
                0 => return,
                HIDE => {
                    if let Some(entry) = self
                        .settings
                        .sidebar_tabs
                        .iter_mut()
                        .find(|(tab, _)| Some(*tab) == category)
                    {
                        entry.1 = false;
                    }
                }
                FOLD => {
                    self.toggle_sidebar();
                    return;
                }
                RESET => self.settings.sidebar_tabs = settings::default_sidebar_tabs(),
                toggle if toggle >= TOGGLE => {
                    if let Some(entry) = self.settings.sidebar_tabs.get_mut(toggle - TOGGLE) {
                        entry.1 = !entry.1;
                    }
                }
                _ => return,
            }
        }
        self.rebuild_nav();
        self.save_settings();
        self.invalidate();
    }

    /// Recomputes sidebar entry positions. If they moved, the highlights
    /// re-anchor without sliding.
    fn update_nav_rects(&mut self) {
        let rects = layout::nav_rects(&self.layout, &self.nav);
        if rects != self.nav_rects {
            self.nav_rects = rects;
            self.motion.reset_nav();
        }
    }

    fn layout(&mut self) {
        let (width, height) = win::client_size(self.hwnd);
        if width <= 0 || height <= 0 {
            return;
        }
        self.layout = layout::compute(
            width as f32,
            height as f32,
            self.selected.is_some(),
            self.chrome(),
        );
        self.update_nav_rects();
        self.sync_caption();
        self.tick();
        self.invalidate();
    }

    // ----- Indexing -------------------------------------------------------------

    /// Watches the searched places for new, deleted, and renamed items.
    /// Places followed through their change journal are not watched.
    fn start_watcher(&mut self) {
        self.watcher = None;
        self.watched_roots.clear();
        if !self.persist || self.settings.index_roots.is_empty() {
            return;
        }
        let Some(library) = &self.library else {
            return;
        };
        self.watched_roots = indexer::outermost_roots(&self.settings.index_roots)
            .into_iter()
            .filter(|root| !self.live_roots.contains(root))
            .collect();
        if self.watched_roots.is_empty() {
            return;
        }
        let roots: Vec<PathBuf> = self
            .watched_roots
            .iter()
            .map(|root| indexer::root_path(root))
            .collect();
        let mut ignore: Vec<String> = Vec::new();
        if let Some(parent) = library.parent() {
            ignore.push(parent.to_string_lossy().to_string());
        }
        if let Ok(temp) = std::env::var("TEMP") {
            ignore.push(temp);
        }
        if let Some(settings_file) = settings::file_path() {
            if let Some(parent) = settings_file.parent() {
                ignore.push(parent.to_string_lossy().to_string());
            }
        }
        ignore.extend(self.settings.scan_excluded_paths.iter().cloned());
        // Folders that change all the time and hold nothing to search for.
        let mut names: Vec<String> = [
            "Cache",
            "Code Cache",
            "GPUCache",
            "INetCache",
            "Temp",
            "Prefetch",
        ]
        .iter()
        .map(|name| name.to_string())
        .collect();
        names.extend(self.settings.scan_excluded_names.iter().cloned());
        let watcher = watch::Watcher::start(&roots, ignore, names);
        self.seen_changes = watcher.changes.count();
        self.changes_since = None;
        self.watcher = Some(watcher);
    }

    /// Rebuilds the index on its own once things changed and settled: after
    /// a few quiet seconds (or a minute of constant change), and never more
    /// often than about twenty times the length of the last build.
    fn maybe_refresh(&mut self) {
        if self.indexer.is_some() || self.swap.is_some() || self.library.is_none() {
            return;
        }
        // Feather changed files itself: read those places again right away
        // rather than waiting for the watcher to settle.
        if let Some(at) = self.refresh_at {
            if at.elapsed() < Duration::from_millis(800) {
                return;
            }
            self.refresh_at = None;
            let mut roots = std::mem::take(&mut self.refresh_roots);
            if let Some(watcher) = &self.watcher {
                let all = indexer::outermost_roots(&self.settings.index_roots);
                for index in watcher.changes.take_dirty() {
                    if let Some(root) = all.get(index) {
                        if !roots.contains(root) {
                            roots.push(root.clone());
                        }
                    }
                }
            }
            self.changes_since = None;
            if !roots.is_empty() {
                self.start_indexing_of(&roots);
            }
            return;
        }
        let Some(watcher) = &self.watcher else {
            return;
        };
        let count = watcher.changes.count();
        if count == self.seen_changes {
            self.changes_since = None;
            return;
        }
        // "Never" in Settings: the index only changes when asked.
        let Some(wait) = self.settings.auto_update.wait() else {
            return;
        };
        let now = Instant::now();
        let since = *self.changes_since.get_or_insert(now);
        let gap = (self.build_took * 20)
            .max(Duration::from_secs(45))
            .max(wait);
        // A timed setting also counts from when Feather started.
        let last = self
            .last_build
            .or((!wait.is_zero()).then_some(self.build_started));
        if last.is_some_and(|done| now.duration_since(done) < gap) {
            return;
        }
        let settled = watcher.changes.quiet_for() >= Duration::from_secs(4);
        if settled || now.duration_since(since) >= Duration::from_secs(60) {
            self.seen_changes = count;
            self.changes_since = None;
            // Only the drives and folders that changed are read again.
            let changed: Vec<String> = watcher
                .changes
                .take_dirty()
                .into_iter()
                .filter_map(|index| self.watched_roots.get(index).cloned())
                .collect();
            if !changed.is_empty() {
                self.start_indexing_of(&changed);
            }
        }
    }

    /// At startup: whole NTFS drives whose index recorded a journal
    /// position catch up from the journal; everything else is read again.
    fn start_live_or_index(&mut self) {
        self.restart_live(None);
        let roots = indexer::outermost_roots(&self.settings.index_roots);
        let rest: Vec<String> = roots
            .iter()
            .filter(|root| !self.live_roots.contains(root))
            .cloned()
            .collect();
        if rest.len() == roots.len() {
            self.start_indexing();
        } else if !rest.is_empty() {
            self.start_indexing_of(&rest);
        }
    }

    /// Follows the change journals of the index in use, for the searched
    /// places that are whole NTFS drives indexed with a journal position.
    /// Only with "Live" updates; then restarts the watcher for the rest.
    /// Changes `previous` (the index shown before) already holds for a
    /// drive whose base is unchanged are kept rather than read again.
    fn restart_live(&mut self, previous: Option<Arc<IndexSet>>) {
        self.live = None;
        self.live_pending = None;
        self.live_roots.clear();
        self.fold_wanted = false;
        let wanted = self.persist
            && !self.live_off
            && self.settings.auto_update == crate::settings::AutoUpdate::Live;
        if let (true, Some(library)) = (wanted, &self.library) {
            let mut tracked = Vec::new();
            for root in indexer::outermost_roots(&self.settings.index_roots) {
                let path = indexer::root_path(&root).to_string_lossy().to_string();
                let bytes = path.as_bytes();
                if bytes.len() != 3 || bytes[1] != b':' {
                    continue;
                }
                let key = feather_core::normalize_path(&path);
                let found = self
                    .index
                    .volumes()
                    .iter()
                    .enumerate()
                    .find(|(_, volume)| feather_core::normalize_path(&volume.prefix) == key);
                let Some((volume_index, volume)) = found else {
                    continue;
                };
                let Some(cursor) = volume.journal() else {
                    continue;
                };
                let carry = previous
                    .as_ref()
                    .and_then(|previous| {
                        previous
                            .volumes()
                            .iter()
                            .find(|old| old.same_base(volume))
                            .and_then(|old| old.overlay())
                    })
                    .map(|overlay| (**overlay).clone());
                tracked.push(live::Tracked {
                    volume: volume_index,
                    root: root.clone(),
                    drive: path[..2].to_string(),
                    cursor,
                    carry,
                });
            }
            if !tracked.is_empty() {
                // What the index leaves out, as the build left it out.
                let mut excluded = self.settings.scan_excluded_paths.clone();
                if let Some(parent) = library.parent() {
                    excluded.push(parent.to_string_lossy().to_string());
                }
                let rules =
                    feather_core::live::Rules::new(&self.settings.scan_excluded_names, &excluded);
                self.live_roots = tracked.iter().map(|tracked| tracked.root.clone()).collect();
                self.live = Some(live::Live::start(
                    Arc::clone(&self.index),
                    tracked,
                    rules,
                    self.allow_elevation,
                ));
            }
        }
        self.start_watcher();
    }

    /// Takes in what the journals said, shows new snapshots, and starts a
    /// rebuild or a fold when one is due and nothing else is running.
    fn poll_live(&mut self) {
        if let Some(live) = &self.live {
            let events: Vec<live::LiveEvent> = std::iter::from_fn(|| live.try_event()).collect();
            for event in events {
                match event {
                    live::LiveEvent::Snapshot(set) => self.live_pending = Some(set),
                    live::LiveEvent::Rebuild(roots) => {
                        self.live_roots.retain(|root| !roots.contains(root));
                        self.pending_rebuild.extend(roots);
                    }
                    live::LiveEvent::Unavailable(roots) => {
                        // Declined: watch and rebuild for the rest of the
                        // session, as without live updates.
                        self.live_off = true;
                        self.live_roots.retain(|root| !roots.contains(root));
                        self.pending_rebuild.extend(roots);
                        self.start_watcher();
                    }
                    live::LiveEvent::Fold => self.fold_wanted = true,
                }
            }
        }
        // Live changes wait while a rename is being typed, and come in at
        // most every few seconds while the window is shown.
        let calm = self.rename.is_none()
            && self
                .live_shown
                .is_none_or(|at| at.elapsed() >= LIVE_EVERY || !self.is_shown());
        if self.swap.is_none() && calm {
            if let Some(set) = self.live_pending.take() {
                self.live_shown = Some(Instant::now());
                self.install_snapshot(set);
            } else if self.live_stale && self.is_shown() {
                self.live_stale = false;
                self.install_snapshot(Arc::clone(&self.index));
            }
        }
        if self.indexer.is_some() || self.swap.is_some() {
            return;
        }
        if !self.pending_rebuild.is_empty() {
            let roots = std::mem::take(&mut self.pending_rebuild);
            self.start_indexing_of(&roots);
        } else if self.fold_wanted {
            self.fold_wanted = false;
            if let Some(library) = self.library.clone() {
                let folder = Indexer::fold(&library, Arc::clone(&self.index));
                self.indexing_root = folder.roots.first().cloned().unwrap_or_default();
                self.indexer = Some(folder);
                self.build_started = Instant::now();
                self.invalidate();
            }
        }
    }

    /// Searches a live snapshot of the index in use and shows it when the
    /// results are in, like a new build. While the window is hidden it is
    /// taken in without searching; the search runs when it shows again.
    fn install_snapshot(&mut self, set: Arc<IndexSet>) {
        if !self.is_shown() {
            self.index = set;
            if let Some(sharing) = &self.sharing {
                sharing.set_index(Arc::clone(&self.index));
            }
            self.live_stale = true;
            return;
        }
        let controller = SearchController::new(Arc::clone(&set));
        let mut model = UiModel::default();
        let text = self.search_text();
        self.submit_to(&controller, &mut model, &text, &set);
        self.swap = Some(Swap {
            index: set,
            controller,
            model,
            // A live snapshot may come before the change Feather made
            // itself, so the renamed item stays wanted until one has it.
            keep: self
                .reselect
                .iter()
                .cloned()
                .chain(self.selected.and_then(|row| self.path_of(row)))
                .collect(),
            scroll: self.scroll,
            rebuilt: false,
        });
    }

    /// Rebuilds the index from `index_roots` on a background thread. The
    /// current index keeps answering searches until the new one is ready.
    fn start_indexing(&mut self) {
        self.start_indexing_of(&[]);
    }

    /// Like `start_indexing`, but with places in `only` just those are read
    /// again; the rest of the index is carried over as it is.
    fn start_indexing_of(&mut self, only: &[String]) {
        let Some(library) = self.library.clone() else {
            return;
        };
        if self.settings.index_roots.is_empty() {
            self.set_notice("Choose folders to search in Settings.".into());
            return;
        }
        let options = ScanOptions {
            excluded_paths: self.settings.scan_excluded_paths.clone(),
            excluded_names: self.settings.scan_excluded_names.clone(),
            ..ScanOptions::default()
        };
        // Dropping a running indexer cancels it and waits for its thread.
        self.indexer = None;
        let indexer = Indexer::start(
            &library,
            &self.settings.index_roots,
            only,
            options,
            self.allow_elevation,
        );
        self.indexing_root = indexer.roots.first().cloned().unwrap_or_default();
        self.indexer = Some(indexer);
        self.build_started = Instant::now();
        if let Some(watcher) = &self.watcher {
            self.seen_changes = watcher.changes.count();
        }
        self.notice.clear();
        self.invalidate();
    }

    fn poll_indexer(&mut self) {
        let Some(indexer) = &self.indexer else {
            return;
        };
        let events: Vec<IndexEvent> = std::iter::from_fn(|| indexer.try_event()).collect();
        for event in events {
            match event {
                IndexEvent::Root { root, .. } => self.indexing_root = root,
                IndexEvent::Done {
                    staged, failures, ..
                } => {
                    self.indexer = None;
                    self.build_took = self.build_started.elapsed();
                    self.last_build = Some(Instant::now());
                    self.install_index(staged, failures);
                }
                IndexEvent::Failed(error) => {
                    self.indexer = None;
                    self.last_build = Some(Instant::now());
                    self.set_notice(format!("Indexing failed: {error}"));
                }
                IndexEvent::Cancelled => self.indexer = None,
            }
        }
        // The running count in the sidebar, a few times a second.
        if self.indexer.is_some() && self.last_progress.elapsed() >= Duration::from_millis(250) {
            self.last_progress = Instant::now();
            self.invalidate();
        }
    }

    /// Opens the new index beside the current one and runs the current
    /// search on it; `finish_swap` shows it when the results are in.
    fn install_index(&mut self, staged: PathBuf, failures: Vec<String>) {
        let set = match IndexSet::open(&staged) {
            Ok(set) => Arc::new(set),
            Err(error) => {
                self.set_notice(format!("Could not open the new index: {error}"));
                return;
            }
        };
        if let Some(library) = &self.library {
            if let Err(error) = indexer::activate(library, &staged) {
                eprintln!("index: {error}");
            }
        }
        let controller = SearchController::new(Arc::clone(&set));
        let mut model = UiModel::default();
        let text = self.search_text();
        self.submit_to(&controller, &mut model, &text, &set);
        self.swap = Some(Swap {
            index: set,
            controller,
            model,
            keep: self
                .reselect
                .take()
                .into_iter()
                .chain(self.selected.and_then(|row| self.path_of(row)))
                .collect(),
            scroll: self.scroll,
            rebuilt: true,
        });
        if failures.is_empty() {
            if self.notice.starts_with("Could not read") {
                self.notice.clear();
            }
        } else {
            self.set_notice(format!("Could not read {}", failures.join(", ")));
        }
        self.invalidate();
    }

    /// Feeds the waiting index's search; swaps it in once it is done.
    fn poll_swap(&mut self) {
        let Some(swap) = &mut self.swap else {
            return;
        };
        while let Ok(event) = swap.controller.try_recv() {
            swap.model.apply(event);
        }
        if swap.model.is_idle() {
            self.finish_swap();
        }
    }

    /// Starts, restarts, or stops the servers to match Settings.
    fn update_sharing(&mut self) {
        let config = crate::share::Config::from_settings(&self.settings);
        if !config.wanted() || !self.persist {
            self.sharing = None;
            self.sharing_shown.clear();
            return;
        }
        if self
            .sharing
            .as_ref()
            .is_some_and(|sharing| *sharing.config() == config)
        {
            return;
        }
        // The old servers let go of their ports before the new ones start.
        self.sharing = None;
        self.sharing = Some(crate::share::Sharing::start(
            config,
            Arc::clone(&self.index),
        ));
        self.sharing_shown.clear();
        self.poll_sharing();
    }

    /// Shows sharing's address once it is known, and any problem.
    fn poll_sharing(&mut self) {
        let Some(sharing) = &self.sharing else {
            return;
        };
        // Settings > Sharing shows the address; the window stays quiet.
        self.sharing_shown = sharing.status().summary();
    }

    /// Sharing's status as lines for Settings.
    fn sharing_lines(&self) -> (Vec<String>, Option<String>) {
        let Some(sharing) = &self.sharing else {
            return (Vec::new(), None);
        };
        let status = sharing.status();
        let mut lines: Vec<String> = status
            .addresses
            .iter()
            .map(|address| format!("Open {address}"))
            .collect();
        lines.extend(status.problems.iter().cloned());
        if !status.fingerprint.is_empty() {
            let short: String = status.fingerprint.chars().take(23).collect();
            lines.push(format!(
                "Browsers warn about Feather's own certificate; its fingerprint starts {short}"
            ));
        }
        let address = status
            .addresses
            .iter()
            .find(|address| address.starts_with("https://") && !address.contains("localhost"))
            .cloned();
        (lines, address)
    }

    fn finish_swap(&mut self) {
        let Some(swap) = self.swap.take() else {
            return;
        };
        let previous = std::mem::replace(&mut self.index, swap.index);
        let previous = swap.rebuilt.then_some(previous);
        if let Some(sharing) = &self.sharing {
            sharing.set_index(Arc::clone(&self.index));
        }
        self.controller = swap.controller;
        let quiet = !swap.rebuilt;
        let mut model = swap.model;
        if quiet {
            // Shown in place: the same search, so its timing stays.
            model.elapsed_ms = self.model.elapsed_ms;
        }
        let before = self.selected;
        self.model = model;
        if !quiet {
            self.epoch += 1;
        }
        self.rename = None;
        if self.sort_column != sort::COLUMN_NAME || !self.sort_ascending {
            self.apply_sort();
        }
        // Keep the same item selected and the list where it was.
        let keep = swap.keep.iter().find_map(|path| {
            let name = path.rsplit('\\').next().unwrap_or("").to_string();
            let row = self.model.ids.iter().position(|entry_ref| {
                let entry = self.index.entry(entry_ref);
                String::from_utf16_lossy(entry.name_display).eq_ignore_ascii_case(&name)
                    && self
                        .index
                        .resolve_path(entry_ref)
                        .is_some_and(|candidate| candidate.eq_ignore_ascii_case(path))
            })?;
            Some((row, path))
        });
        if keep.is_some_and(|(_, path)| self.reselect.as_ref() == Some(path)) {
            self.reselect = None;
        }
        let keep = keep.map(|(row, _)| row);
        self.selected = None;
        if let Some(row) = keep {
            self.selected = Some(row);
            if !(quiet && before == Some(row)) {
                self.motion
                    .select_row(Some((self.epoch, row)), Instant::now());
            }
        } else if self.model.result_count() > 0 {
            self.set_selected(Some(0));
        }
        self.motion.scroll.jump(swap.scroll);
        self.tick();
        self.rebuild_nav();
        self.layout();
        if let Some(previous) = previous {
            // Follow the journals on from the new index, keeping what was
            // followed on bases it carried over. This lets go of the old
            // index, which the follower held too.
            self.restart_live(Some(previous));
            // The old index is closed now; delete its files, unless a
            // build is writing its own next to them.
            if let Some(library) = &self.library {
                if self.indexer.is_none() {
                    indexer::cleanup(library);
                }
            }
        }
        self.invalidate();
    }

    /// First-run setup: where to search and how Feather looks.
    fn run_setup(&mut self) {
        let start = setup::SetupStart {
            hotkey: self.settings.hotkey.clone(),
            // A fresh install starts at sign-in unless turned off here.
            start_at_login: true,
            close_to_tray: self.settings.close_to_tray,
        };
        match setup::show(self.hwnd, self.look(), &start) {
            Ok(Some(choice)) => {
                self.settings.setup_done = true;
                self.settings.dark_mode = choice.dark_mode;
                self.settings.accent = choice.accent;
                if !choice.skipped {
                    if !choice.roots.is_empty() {
                        self.settings.index_roots = choice.roots;
                    }
                    self.settings.hotkey = choice.hotkey;
                    self.settings.close_to_tray = choice.close_to_tray;
                    self.settings.start_at_login = choice.start_at_login;
                    self.settings.normalize();
                    if self.persist && startup::enabled() != choice.start_at_login {
                        if let Err(error) = startup::set(choice.start_at_login) {
                            self.set_notice(format!(
                                "Could not change starting at sign-in: {error}"
                            ));
                        }
                    }
                    self.register_hotkey();
                    self.update_tray();
                }
                self.refresh_theme(true);
                self.save_settings();
                if !self.settings.index_roots.is_empty() {
                    self.restart_live(None);
                    self.start_indexing();
                }
            }
            // Closed without choosing: ask again next time.
            Ok(None) => {}
            Err(error) => self.set_notice(format!("Could not open setup: {error}")),
        }
        self.focus_search();
    }

    // ----- Animation ----------------------------------------------------------

    /// Asks for a frame on every display refresh until animations settle.
    fn kick(&mut self) {
        if !self.anim_timer {
            frames::request(self.hwnd);
            self.anim_timer = true;
        }
    }

    /// Advances animated state to the current time. Returns true while
    /// anything is still moving.
    fn tick(&mut self) -> bool {
        let now = Instant::now();
        let max = self.max_scroll();
        if self.motion.scroll.target() > max {
            self.motion.scroll.jump(max);
        }
        self.scroll = self.motion.scroll.value(now).clamp(0.0, max);
        self.motion.prune(now);
        self.motion.active(now)
            || self.sidebar_w.running(now)
            || self.details_w.running(now)
            || self.nav_shift.iter().any(|shift| shift.running(now))
            || matches!(self.drag, Drag::Nav { moving: true, .. })
    }

    fn on_anim_frame(&mut self) {
        let scrolling = self.motion.scroll.running(Instant::now());
        // The sidebar's width moves everything beside it.
        if self.sidebar_w.running(Instant::now())
            || (self.layout.sidebar.r - self.sidebar_w.target()).abs() > 0.5
            || self.details_w.running(Instant::now())
        {
            self.layout();
        }
        let active = self.tick();
        // Rows slide under a resting pointer while the list scrolls.
        if scrolling && self.tracking && self.drag == Drag::None {
            let (x, y) = cursor_client(self.hwnd);
            let hit = self.hit_at(x, y);
            self.set_hover(hit);
        }
        self.invalidate();
        if !active {
            frames::stop(self.hwnd);
            self.anim_timer = false;
        }
    }

    /// Points the selection highlights at the current selection; they slide
    /// there if they were already showing something nearby.
    fn sync_motion(&mut self, now: Instant) {
        self.motion
            .select_row(self.selected.map(|row| (self.epoch, row)), now);
        for (group, (index, top)) in self.nav_selection().into_iter().enumerate() {
            self.motion.select_nav(group, index, top, now);
        }
        if self.motion.active(now) {
            self.kick();
        }
    }

    /// The selected entry and its top edge for each sidebar group: kinds
    /// above the first label, places below it.
    fn nav_selection(&self) -> [(Option<usize>, f32); 2] {
        let mut result = [(None, 0.0); 2];
        let mut group = 0;
        for (index, entry) in self.nav.iter().enumerate() {
            match entry {
                NavEntry::Label(_) => group = 1,
                NavEntry::Item(item) => {
                    let rect = self.nav_rects[index];
                    if result[group].0.is_none() && !rect.is_empty() && self.nav_selected(item) {
                        result[group] = (Some(index), rect.t);
                    }
                }
            }
        }
        result
    }

    fn content_height(&self) -> f64 {
        self.model.result_count() as f64 * f64::from(ROW_H)
    }

    fn max_scroll(&self) -> f64 {
        (self.content_height() - f64::from(self.layout.list.height())).max(0.0)
    }

    /// Scrolls the result list, gliding there when `smooth` is set.
    fn set_scroll(&mut self, target: f64, smooth: bool) {
        let target = target.clamp(0.0, self.max_scroll());
        self.motion.scroll_to(target, smooth, Instant::now());
        if smooth {
            self.kick();
        } else {
            self.scroll = target;
        }
        self.invalidate();
    }

    fn invalidate(&self) {
        unsafe {
            InvalidateRect(self.hwnd, std::ptr::null(), 0);
        }
    }

    // ----- Searching ----------------------------------------------------------

    fn search_text(&self) -> String {
        self.field.text()
    }

    /// The query for `text` against `index`, or `None` when the empty
    /// search box should show the empty state.
    fn search_request(
        &self,
        text: &str,
        index: &IndexSet,
    ) -> Option<(Query, Vec<String>, SearchOptions)> {
        if text.trim().is_empty()
            && self.filters.is_empty()
            && self.scope.is_none()
            && !self.settings.show_all_when_empty
        {
            return None;
        }
        let mut composed = self.filters.compose(text);
        if let Some(scope) = &self.scope {
            if !composed.is_empty() {
                composed.push(' ');
            }
            composed.push_str(&format!("in:\"{scope}\""));
        }
        let query = Query::parse(&composed);
        let warnings = query.warnings().to_vec();
        let options = SearchOptions {
            show_hidden: self.settings.show_hidden,
            show_system: self.settings.show_system,
            excluded_paths: self.settings.excluded_folders.clone(),
            volume_mask: index.volume_mask(&self.settings.excluded_volumes),
        }
        .for_index(index);
        Some((query, warnings, options))
    }

    fn submit_to(
        &self,
        controller: &SearchController,
        model: &mut UiModel,
        text: &str,
        index: &IndexSet,
    ) -> Query {
        match self.search_request(text, index) {
            Some((query, warnings, options)) => {
                let cap = self.settings.result_cap(index.item_count());
                let highlight = query.clone();
                let generation = controller.submit(query, cap, options);
                model.begin(generation, warnings);
                highlight
            }
            None => {
                let generation = controller.invalidate();
                model.clear(generation);
                Query::default()
            }
        }
    }

    fn search_key(&self) -> String {
        format!(
            "{}|{}|{:?}|{:?}|{}",
            self.settings.show_hidden,
            self.settings.show_system,
            self.settings.excluded_folders,
            self.settings.excluded_volumes,
            self.settings.result_cap(self.index.item_count())
        )
    }

    /// Typing further filters the rows already found instead of searching
    /// the whole index again. Only when they are complete: not capped, not
    /// still arriving, and from the same options.
    fn narrow_search(&mut self, text: &str) -> bool {
        if self.swap.is_some()
            || !self.model.started
            || !self.model.search_done
            || self.model.capped
            || self.model.stale
            || self.model.result_count() > NARROW_MAX
        {
            return false;
        }
        let key = self.search_key();
        if key != self.search_key {
            return false;
        }
        let Some((query, warnings, _)) = self.search_request(text, &self.index) else {
            return false;
        };
        if !query.narrows(&self.query) {
            return false;
        }
        let started = Instant::now();
        let cap = self.settings.result_cap(self.index.item_count());
        let Some(rows) = crate::search::narrow(&self.index, &self.model.ids, &query, cap) else {
            return false;
        };
        let generation = self.controller.invalidate();
        self.model
            .narrowed(generation, warnings, rows, started.elapsed());
        self.query = query;
        self.set_scroll(0.0, false);
        self.set_selected((self.model.result_count() > 0).then_some(0));
        true
    }

    fn start_search(&mut self, text: &str) {
        unsafe {
            KillTimer(self.hwnd, TIMER_DEBOUNCE);
        }
        self.epoch += 1;
        self.rename = None;
        if self.narrow_search(text) {
            self.invalidate();
            return;
        }
        self.search_key = self.search_key();
        self.set_scroll(0.0, false);
        self.set_selected(None);
        let mut model = std::mem::take(&mut self.model);
        self.query = self.submit_to(&self.controller, &mut model, text, &self.index);
        self.model = model;
        // A waiting new index searches for the same thing.
        if let Some(mut swap) = self.swap.take() {
            self.submit_to(&swap.controller, &mut swap.model, text, &swap.index);
            swap.keep.clear();
            swap.scroll = 0.0;
            self.swap = Some(swap);
        }
        self.invalidate();
    }

    fn set_query(&mut self, text: &str) {
        self.field.set_text(text);
        self.search_scroll = 0.0;
        self.start_search(text);
    }

    fn set_filters(&mut self, filters: FilterSet) {
        self.filters = filters;
        let text = self.search_text();
        self.start_search(&text);
    }

    fn apply_category(&mut self, category: Category) {
        let mut filters = self.filters.clone();
        category.apply(&mut filters);
        self.set_filters(filters);
        if self.settings.remember_filters {
            self.save_settings();
        }
    }

    fn set_scope(&mut self, scope: Option<String>) {
        self.scope = scope;
        self.rebuild_nav();
        let text = self.search_text();
        self.start_search(&text);
    }

    fn apply_settings(&mut self, mut settings: Settings) {
        settings.normalize();
        settings.filters = if settings.remember_filters {
            self.filters.clone()
        } else {
            FilterSet::default()
        };
        let cap_changed = settings.results_percent != self.settings.results_percent;
        let empty_state_changed = settings.show_all_when_empty != self.settings.show_all_when_empty;
        let matching_changed = settings.show_hidden != self.settings.show_hidden
            || settings.show_system != self.settings.show_system
            || settings.excluded_folders != self.settings.excluded_folders
            || settings.excluded_volumes != self.settings.excluded_volumes;
        let remember_scope_changed = settings.remember_scope != self.settings.remember_scope;
        let tabs_changed = settings.sidebar_tabs != self.settings.sidebar_tabs;
        self.settings = settings;
        if tabs_changed {
            self.rebuild_nav();
        }
        if remember_scope_changed && !self.settings.remember_scope {
            self.settings.scope = None;
        }
        self.apply_always_on_top();
        self.refresh_theme(true);
        if cap_changed || empty_state_changed || matching_changed {
            let text = self.search_text();
            self.start_search(&text);
        }
    }

    fn apply_always_on_top(&self) {
        let insert_after = if self.settings.always_on_top {
            HWND_TOPMOST
        } else {
            HWND_NOTOPMOST
        };
        unsafe {
            SetWindowPos(
                self.hwnd,
                insert_after,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    }

    fn save_settings(&mut self) {
        if !self.persist {
            return;
        }
        self.settings.normalize();
        self.settings.filters = if self.settings.remember_filters {
            self.filters.clone()
        } else {
            FilterSet::default()
        };
        self.settings.window = if self.settings.remember_window {
            capture_placement(self.hwnd)
        } else {
            None
        };
        self.settings.scope = if self.settings.remember_scope {
            self.scope.clone()
        } else {
            None
        };
        if let Err(error) = self.settings.save() {
            eprintln!("settings: {error}");
            self.set_notice(format!("Could not save settings: {error}"));
        }
    }

    fn set_notice(&mut self, notice: String) {
        self.notice = notice;
        self.invalidate();
    }

    /// Reorders the currently loaded rows. The comparator reads index records
    /// directly; only the path column needs a transient resolved key.
    fn apply_sort(&mut self) {
        self.epoch += 1;
        let column = self.sort_column;
        let ascending = self.sort_ascending;
        if column == sort::COLUMN_PATH {
            let index = Arc::clone(&self.index);
            let mut keyed: Vec<(String, EntryRef)> = self
                .model
                .ids
                .iter()
                .map(|entry_ref| {
                    let path = index.resolve_path(entry_ref).unwrap_or_default();
                    (parent_path(&path), entry_ref)
                })
                .collect();
            keyed.sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
            if !ascending {
                keyed.reverse();
            }
            self.model.ids =
                crate::rows::Rows::from_refs(keyed.into_iter().map(|(_, id)| id).collect());
        } else {
            let index = Arc::clone(&self.index);
            self.model.ids.sort_by(|a, b| {
                let entry_a = index.entry(a);
                let entry_b = index.entry(b);
                let order = sort::compare_entries(&entry_a, &entry_b, column);
                let order = if ascending { order } else { order.reverse() };
                order.then_with(|| a.cmp(&b))
            });
        }
    }

    fn sort_by(&mut self, column: usize, ascending: bool) {
        self.sort_column = column;
        self.sort_ascending = ascending;
        self.apply_sort();
        self.set_scroll(0.0, false);
        if self.model.result_count() > 0 {
            self.set_selected(Some(0));
        }
        self.invalidate();
    }

    fn toggle_sort(&mut self, column: usize) {
        let ascending = if column == self.sort_column {
            !self.sort_ascending
        } else {
            column == sort::COLUMN_NAME || column == sort::COLUMN_PATH
        };
        self.sort_by(column, ascending);
    }

    fn on_search_changed(&mut self) {
        let delay = self.settings.search_delay_ms as u32;
        if delay == 0 {
            let text = self.search_text();
            self.start_search(&text);
        } else {
            unsafe {
                SetTimer(self.hwnd, TIMER_DEBOUNCE, delay, None);
            }
        }
        self.invalidate();
    }

    fn on_timer(&mut self, timer: usize) {
        match timer {
            TIMER_DEBOUNCE => {
                unsafe {
                    KillTimer(self.hwnd, TIMER_DEBOUNCE);
                }
                let text = self.search_text();
                self.start_search(&text);
            }
            TIMER_FULLSCREEN => self.follow_fullscreen(),
            TIMER_TRIM => {
                unsafe {
                    KillTimer(self.hwnd, TIMER_TRIM);
                }
                if !self.anim_timer && self.indexer.is_none() {
                    memory::trim();
                }
            }
            TIMER_CARET => {
                if self.focused {
                    self.caret_on = !self.caret_on;
                    if self.rename.is_some() {
                        // The caret blinks in the rename box over the list.
                        self.invalidate();
                    } else {
                        self.invalidate_search();
                    }
                }
            }
            TIMER_RESULTS => {
                self.poll_sharing();
                self.poll_indexer();
                self.poll_swap();
                self.poll_live();
                self.maybe_refresh();
                let mut changed = false;
                while let Ok(event) = self.controller.try_recv() {
                    if self.model.apply(event) {
                        changed = true;
                        if self.model.is_idle()
                            && (self.sort_column != sort::COLUMN_NAME || !self.sort_ascending)
                        {
                            self.apply_sort();
                        }
                    }
                }
                if changed {
                    if self.selected.is_none() && self.model.result_count() > 0 {
                        self.set_selected(Some(0));
                    }
                    self.invalidate();
                }
            }
            _ => {}
        }
    }

    fn invalidate_search(&self) {
        let area = self.layout.search_text;
        let rect = RECT {
            left: area.l as i32 - 4,
            top: area.t as i32,
            right: area.r as i32 + 4,
            bottom: area.b as i32,
        };
        unsafe {
            InvalidateRect(self.hwnd, &rect, 0);
        }
    }

    // ----- Status text --------------------------------------------------------

    fn count_text(&self) -> String {
        if self.model.searching && self.model.stale {
            "Searching\u{2026}".to_string()
        } else if self.model.searching {
            format!("{} so far", group_digits(self.model.result_count() as u64))
        } else if !self.model.started {
            String::new()
        } else if self.model.total == 0 {
            "No results".to_string()
        } else {
            let suffix = if self.model.capped { "+" } else { "" };
            format!(
                "{}{} found in {} ms",
                group_digits(self.model.total),
                suffix,
                self.model.elapsed_ms
            )
        }
    }

    fn scope_text(&self) -> String {
        let place = match &self.scope {
            Some(scope) => format!("Looking in {scope}"),
            None => "Looking in all locations".to_string(),
        };
        if self.filters.is_empty() {
            place
        } else {
            format!("{place}, filtered to {}", self.filters.summary())
        }
    }

    fn empty_message(&self) -> Option<String> {
        if self.model.result_count() > 0 || self.model.searching {
            return None;
        }
        if self.index.volume_count() == 0 {
            return Some(if self.indexer.is_some() {
                "Feather is reading your files. Results appear here when it finishes.".into()
            } else {
                "Nothing is indexed yet. Open Settings to choose folders to search.".into()
            });
        }
        if !self.model.started {
            return Some("Type to search every file and folder.".to_string());
        }
        let text = self.search_text();
        if !text.trim().is_empty() {
            Some(format!(
                "Nothing matches \u{201c}{}\u{201d}. Check the spelling or clear a filter.",
                text.trim()
            ))
        } else {
            Some("Nothing matches these filters.".to_string())
        }
    }

    // ----- Rows and selection -------------------------------------------------

    fn cell_text(&self, row: usize, column: usize) -> Option<String> {
        let entry_ref = self.model.ids.get(row)?;
        let entry = self.index.entry(entry_ref);
        match column {
            0 => Some(String::from_utf16_lossy(entry.name_display)),
            1 => Some(
                self.index
                    .resolve_path(entry_ref)
                    .map(|path| parent_path(&path))
                    .unwrap_or_default(),
            ),
            2 => {
                // Folders show their total once the index has one.
                if entry.is_dir() && !self.index.has_folder_totals(entry_ref) {
                    Some(String::new())
                } else {
                    Some(size_in(entry.size, self.settings.size_format))
                }
            }
            3 => Some(format_filetime(entry.mtime)),
            4 => Some(type_label(entry.name_search, entry.is_dir())),
            _ => Some(String::new()),
        }
    }

    fn path_of(&self, row: usize) -> Option<String> {
        let entry_ref = self.model.ids.get(row)?;
        self.index.resolve_path(entry_ref)
    }

    fn set_selected(&mut self, row: Option<usize>) {
        let had_selection = self.selected.is_some();
        self.selected = row.filter(|&row| row < self.model.result_count());
        if had_selection != self.selected.is_some() {
            self.layout();
        }
    }

    fn select_row(&mut self, row: usize) {
        let count = self.model.result_count();
        if count == 0 {
            return;
        }
        let row = row.min(count - 1);
        self.set_selected(Some(row));
        let target = layout::scroll_to_row(
            self.motion.scroll.target(),
            row,
            f64::from(self.layout.list.height()),
        );
        self.set_scroll(target, true);
    }

    fn move_selection(&mut self, delta: isize) {
        let count = self.model.result_count();
        if count == 0 {
            return;
        }
        let current = self.selected.map(|row| row as isize).unwrap_or(-1);
        let next = (current + delta).clamp(0, count as isize - 1);
        self.select_row(next as usize);
    }

    fn page_rows(&self) -> isize {
        ((self.layout.list.height() / ROW_H).floor() as isize - 1).max(1)
    }

    fn open_row(&mut self, row: usize, reveal: bool) {
        let Some(entry_ref) = self.model.ids.get(row) else {
            return;
        };
        let Some(path) = self.index.resolve_path(entry_ref) else {
            return;
        };
        let result = if reveal {
            context_menu::reveal_item(self.hwnd, &path)
        } else {
            context_menu::open_item(self.hwnd, &path)
        };
        match result {
            Ok(()) => {}
            Err(error) => self.set_notice(format!("Could not open {path}: {error}")),
        }
    }

    /// Double-click and Enter follow the open-mode setting.
    fn activate_row(&mut self, row: usize) {
        let Some(entry_ref) = self.model.ids.get(row) else {
            return;
        };
        let reveal = self.settings.open_mode == OpenMode::ContainingFolder
            && !self.index.entry(entry_ref).is_dir();
        self.open_row(row, reveal);
    }

    fn copy_row_path(&mut self, row: usize) {
        if let Some(path) = self.path_of(row) {
            match context_menu::copy_text(self.hwnd, &path) {
                Ok(()) => self.set_notice("Copied the full path".to_string()),
                Err(error) => self.set_notice(format!("Could not copy: {error}")),
            }
        }
    }

    fn show_context_menu(&mut self, row: usize) {
        self.select_row(row);
        unsafe {
            UpdateWindow(self.hwnd);
        }
        if let Some(path) = self.path_of(row) {
            if let Err(error) =
                context_menu::show(self.hwnd, &path, self.settings.use_shell_context_menu)
            {
                self.set_notice(format!("Context menu failed: {error}"));
            }
        }
    }

    // ----- File management ----------------------------------------------------

    /// The selected result's path and whether it is a folder.
    fn selected_item(&self) -> Option<(String, bool)> {
        let row = self.selected?;
        let entry_ref = self.model.ids.get(row)?;
        let path = self.index.resolve_path(entry_ref)?;
        Some((path, self.index.entry(entry_ref).is_dir()))
    }

    /// Reads the places holding `paths` again soon, so the results show
    /// what Feather just changed. Places followed live are left alone: the
    /// change journal reports the change within about a second, without a
    /// rebuild.
    fn refresh_soon(&mut self, paths: &[String]) {
        let roots = indexer::outermost_roots(&self.settings.index_roots);
        let live = self.live.is_some();
        for path in paths {
            for root in &roots {
                let followed = live && self.live_roots.contains(root);
                if feather_core::path_within(path, root)
                    && !followed
                    && !self.refresh_roots.contains(root)
                {
                    self.refresh_roots.push(root.clone());
                }
            }
        }
        if !self.refresh_roots.is_empty() {
            self.refresh_at = Some(Instant::now());
        }
    }

    /// Copy (or cut) the selected result to the clipboard as a file.
    fn copy_selected(&mut self, cut: bool) {
        let Some((path, _)) = self.selected_item() else {
            return;
        };
        let name = path.rsplit('\\').next().unwrap_or(&path).to_string();
        match fileops::set_clipboard_files(self.hwnd, std::slice::from_ref(&path), cut) {
            Ok(()) if cut => self.set_notice(format!(
                "Cut \u{201c}{name}\u{201d}. Paste it into a folder here or in Explorer."
            )),
            Ok(()) => self.set_notice(format!("Copied \u{201c}{name}\u{201d}")),
            Err(error) => self.set_notice(format!("Could not copy: {error}")),
        }
    }

    /// Pastes files from the clipboard into the selected folder, or into
    /// the folder of the selected file. Windows shows the progress and asks
    /// about conflicts.
    fn paste_files(&mut self, files: fileops::ClipboardFiles) {
        let Some((path, is_dir)) = self.selected_item() else {
            self.set_notice("Select a folder to paste into.".into());
            return;
        };
        let folder = fileops::paste_folder(&path, is_dir);
        let result = fileops::Shell::new(self.hwnd).paste(&files.paths, &folder, files.cut);
        match result {
            Ok(fileops::Outcome::Done) => {
                if files.cut {
                    fileops::clear_clipboard(self.hwnd);
                }
                let count = files.paths.len();
                let what = if count == 1 {
                    "1 item".to_string()
                } else {
                    format!("{count} items")
                };
                let verb = if files.cut { "Moved" } else { "Pasted" };
                self.set_notice(format!("{verb} {what} into {folder}"));
                let mut changed = files.paths.clone();
                changed.push(folder);
                self.refresh_soon(&changed);
            }
            Ok(fileops::Outcome::Aborted) => {
                let mut changed = files.paths.clone();
                changed.push(folder);
                self.refresh_soon(&changed);
            }
            Err(error) => self.set_notice(format!("Could not paste: {error}")),
        }
    }

    /// Delete moves the selected result to the Recycle Bin; Shift+Delete
    /// deletes it for good after asking.
    fn delete_selected(&mut self, permanently: bool) {
        let Some((path, _)) = self.selected_item() else {
            return;
        };
        let paths = vec![path.clone()];
        let shell = fileops::Shell::new(self.hwnd);
        let result = if permanently {
            if !fileops::confirm_permanent_delete(self.hwnd, &paths) {
                return;
            }
            shell.delete_permanently(&paths)
        } else {
            shell.recycle(&paths)
        };
        let name = path.rsplit('\\').next().unwrap_or(&path).to_string();
        match result {
            Ok(fileops::Outcome::Done) => {
                self.set_notice(if permanently {
                    format!("Deleted \u{201c}{name}\u{201d}")
                } else {
                    format!("Moved \u{201c}{name}\u{201d} to the Recycle Bin")
                });
                self.refresh_soon(&paths);
            }
            Ok(fileops::Outcome::Aborted) => {}
            Err(error) => self.set_notice(format!("Could not delete {name}: {error}")),
        }
    }

    /// F2: type a new name over the result's name, with the part before
    /// the extension selected.
    fn begin_rename(&mut self, row: usize) {
        let Some(entry_ref) = self.model.ids.get(row) else {
            return;
        };
        let Some(path) = self.index.resolve_path(entry_ref) else {
            return;
        };
        let entry = self.index.entry(entry_ref);
        let mut field = TextField::default();
        field.set_text(&String::from_utf16_lossy(entry.name_display));
        field.move_to(0, false);
        field.move_to(
            fileops::rename_selection(entry.name_display, entry.is_dir()),
            true,
        );
        self.select_row(row);
        self.list_focus = true;
        self.rename = Some(Rename {
            row,
            entry: entry_ref,
            path,
            field,
            scroll: 0.0,
        });
        self.caret_on = true;
        self.invalidate();
    }

    /// Renames the item to what was typed. A bad name keeps the box open
    /// and says why; Windows asks if the name is taken.
    fn commit_rename(&mut self) {
        let Some(rename) = self.rename.take() else {
            return;
        };
        self.invalidate();
        let old_name = rename
            .path
            .rsplit('\\')
            .next()
            .unwrap_or(&rename.path)
            .to_string();
        let typed = rename.field.text();
        if typed == old_name {
            return;
        }
        let name = match fileops::validate_new_name(&typed) {
            Ok(name) => name,
            Err(message) => {
                self.set_notice(message);
                self.rename = Some(rename);
                return;
            }
        };
        if name == old_name {
            return;
        }
        let result = fileops::Shell::new(self.hwnd).rename(&rename.path, &name);
        match result {
            Ok(fileops::Outcome::Done) => {
                let new_path = format!("{}\\{name}", parent_path(&rename.path));
                self.set_notice(format!("Renamed to \u{201c}{name}\u{201d}"));
                self.reselect = Some(new_path.clone());
                self.refresh_soon(&[rename.path, new_path]);
            }
            Ok(fileops::Outcome::Aborted) => {}
            Err(error) => self.set_notice(format!("Could not rename {old_name}: {error}")),
        }
    }

    fn on_rename_key(&mut self, key: u16, shift: bool, control: bool) -> bool {
        match key {
            VK_RETURN | VK_TAB => {
                self.commit_rename();
                return true;
            }
            VK_ESCAPE => {
                self.rename = None;
                self.invalidate();
                return true;
            }
            // Moving to another result keeps the new name, then moves.
            VK_UP | VK_DOWN | VK_PRIOR | VK_NEXT => {
                self.commit_rename();
                if self.rename.is_none() {
                    return self.on_key(key);
                }
                return true;
            }
            _ => {}
        }
        let clipboard_text = if control && key == b'V' as u16 {
            context_menu::paste_text(self.hwnd)
        } else {
            None
        };
        let hwnd = self.hwnd;
        let Some(rename) = &mut self.rename else {
            return false;
        };
        let field = &mut rename.field;
        match key {
            VK_LEFT => field.move_left(shift, control),
            VK_RIGHT => field.move_right(shift, control),
            VK_HOME => field.home(shift),
            VK_END => field.end(shift),
            VK_BACK => {
                field.backspace(control);
            }
            VK_DELETE => {
                field.delete(control);
            }
            _ if control && key == b'A' as u16 => field.select_all(),
            _ if control && key == b'C' as u16 => {
                if field.has_selection() {
                    let _ = context_menu::copy_text(hwnd, &field.selected_text());
                }
            }
            _ if control && key == b'X' as u16 => {
                if field.has_selection() {
                    let _ = context_menu::copy_text(hwnd, &field.selected_text());
                    field.delete_selection();
                }
            }
            _ if control && key == b'V' as u16 => {
                if let Some(text) = clipboard_text {
                    let clean: String = text
                        .chars()
                        .filter(|&c| !fileops::is_forbidden_char(c))
                        .collect();
                    field.insert(&clean);
                }
            }
            // Other keys do nothing while a name is being typed.
            _ => {}
        }
        self.caret_on = true;
        self.invalidate();
        true
    }

    /// The box the new name is typed in, on screen.
    fn rename_rect(&self) -> Option<Rect> {
        let rename = self.rename.as_ref()?;
        let list = self.layout.list;
        let top = list.t + (rename.row as f64 * f64::from(ROW_H) - self.scroll) as f32;
        Some(Rect::new(
            list.l + 52.0,
            top + 3.0,
            self.layout.name_col.r,
            top + 29.0,
        ))
    }

    fn rename_layout(&self) -> Option<windows::Win32::Graphics::DirectWrite::IDWriteTextLayout> {
        let rename = self.rename.as_ref()?;
        self.gfx
            .layout(rename.field.wide(), Font::BodyBold, 100_000.0, 20.0)
    }

    fn rename_hit(&self, x: f32, y: f32) -> bool {
        self.rename_rect().is_some_and(|rect| rect.contains(x, y))
    }

    fn on_rename_click(&mut self, x: f32, y: f32, shift: bool) {
        let (Some(rect), Some(layout)) = (self.rename_rect(), self.rename_layout()) else {
            return;
        };
        let Some(rename) = &mut self.rename else {
            return;
        };
        let position =
            paint::position_at(&layout, x - rect.l - 6.0 + rename.scroll, y - rect.t - 3.0);
        rename.field.move_to(position, shift);
        self.caret_on = true;
        self.invalidate();
    }

    /// The rename box for this frame, scrolled to keep the caret in view.
    /// Gone if the results changed under it.
    fn rename_view(&mut self) -> Option<RenameView> {
        let rename = self.rename.as_ref()?;
        if self.model.ids.get(rename.row) != Some(rename.entry) {
            self.rename = None;
            return None;
        }
        let rect = self.rename_rect()?;
        let layout = self.rename_layout();
        let rename = self.rename.as_mut()?;
        let (caret_x, end_x, selection) = match &layout {
            Some(text) => {
                let (start, end) = rename.field.selection();
                (
                    paint::caret_offset(text, rename.field.caret()),
                    paint::caret_offset(text, rename.field.wide().len()),
                    paint::selection_spans(text, start, end),
                )
            }
            None => (0.0, 0.0, Vec::new()),
        };
        let width = rect.width() - 14.0;
        if caret_x - rename.scroll > width {
            rename.scroll = caret_x - width;
        }
        if caret_x < rename.scroll {
            rename.scroll = caret_x;
        }
        rename.scroll = rename.scroll.min((end_x - width).max(0.0)).max(0.0);
        Some(RenameView {
            rect,
            layout,
            scroll: rename.scroll,
            caret_x,
            show_caret: self.caret_on,
            selection,
        })
    }

    // ----- Commands -----------------------------------------------------------

    fn on_command(&mut self, wparam: WPARAM) -> bool {
        match win::loword(wparam) as usize {
            ID_FOCUS_SEARCH => {
                // Ctrl+F gives the keys back to the search text; a name
                // being typed is dropped, as Escape would.
                self.rename = None;
                self.list_focus = false;
                self.field.select_all();
                self.focus_search();
                true
            }
            ID_CLEAR_SEARCH if self.rename.is_some() => {
                // Escape while renaming keeps the old name.
                self.rename = None;
                self.invalidate();
                true
            }
            ID_CLEAR_SEARCH => {
                self.list_focus = false;
                if self.field.is_empty() && self.summoned {
                    self.dismiss();
                } else {
                    self.set_query("");
                    self.focus_search();
                }
                true
            }
            _ => false,
        }
    }

    fn press(&mut self, button: Button) {
        match button {
            Button::Filters => self.open_filters(),
            Button::Settings => self.open_settings(),
            Button::Help => {
                if let Err(error) = help::show(self.hwnd, self.look()) {
                    self.set_notice(format!("Could not open Help: {error}"));
                }
            }
            Button::Theme => self.toggle_theme(),
            Button::Details => self.toggle_details(),
            Button::Sidebar => self.toggle_sidebar(),
            Button::Open => {
                if let Some(row) = self.selected {
                    self.open_row(row, false);
                }
            }
            Button::Reveal => {
                if let Some(row) = self.selected {
                    self.open_row(row, true);
                }
            }
            Button::CopyPath => {
                if let Some(row) = self.selected {
                    self.copy_row_path(row);
                }
            }
            Button::SortName => self.toggle_sort(sort::COLUMN_NAME),
            Button::SortSize => self.toggle_sort(sort::COLUMN_SIZE),
            Button::SortModified => self.toggle_sort(sort::COLUMN_MODIFIED),
        }
        self.focus_search();
    }

    fn activate_nav(&mut self, index: usize) {
        let Some(NavEntry::Item(item)) = self.nav.get(index).cloned() else {
            return;
        };
        if self.nav_selected(&item) {
            return;
        }
        match item {
            NavItem::Category(category) => self.apply_category(category),
            NavItem::Scope(scope) => self.set_scope(scope),
            NavItem::ChooseFolder => match picker::pick_folder(self.hwnd) {
                Ok(Some(path)) => self.set_scope(Some(path)),
                Ok(None) => {}
                Err(error) => self.set_notice(format!("Could not choose a folder: {error}")),
            },
        }
        self.focus_search();
    }

    fn nav_selected(&self, item: &NavItem) -> bool {
        match item {
            NavItem::Category(category) => self.filters.category() == *category,
            NavItem::Scope(scope) => match (scope, &self.scope) {
                (None, None) => true,
                (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
                _ => false,
            },
            NavItem::ChooseFolder => false,
        }
    }

    /// The colors and backdrop the dialogs use, matching this window.
    fn look(&self) -> form::Look {
        form::Look {
            dark: self.palette.dark,
            accent: self.settings.accent,
            material: self.material,
        }
    }

    fn open_filters(&mut self) {
        match crate::filters::show(self.hwnd, self.filters.clone(), self.look()) {
            Ok(Some(filters)) => {
                self.set_filters(filters);
                if self.settings.remember_filters {
                    self.save_settings();
                }
            }
            Ok(None) => {}
            Err(error) => self.set_notice(format!("Could not open Filters: {error}")),
        }
    }

    fn open_settings(&mut self) {
        let mut initial = self.settings.clone();
        initial.filters = self.filters.clone();
        // The switch shows what Windows will really do at sign-in.
        let starts = startup::enabled();
        initial.start_at_login = starts;
        // The shortcut switches show which shortcuts really exist.
        let links = [shortcuts::Place::StartMenu, shortcuts::Place::Desktop]
            .map(|place| (place, shortcuts::exists(place)));
        initial.start_menu_shortcut = links[0].1;
        initial.desktop_shortcut = links[1].1;
        let before = self.settings.clone();
        let old_roots = self.settings.index_roots.clone();
        let old_live = (
            self.settings.auto_update,
            self.settings.scan_excluded_names.clone(),
            self.settings.scan_excluded_paths.clone(),
        );
        let (sharing_lines, sharing_address) = self.sharing_lines();
        // The shortcut box must be able to receive the current shortcut.
        let had_hotkey = self.hotkey_on;
        self.unregister_hotkey();
        // Every change shows in this window at once, through a posted
        // message so the dialog never reaches into this window directly.
        let hwnd = self.hwnd;
        let preview: settings::Preview = Box::new(move |changed: &Settings| {
            let boxed = Box::into_raw(Box::new(changed.clone()));
            unsafe {
                if PostMessageW(hwnd, WM_PREVIEW_SETTINGS, 0, boxed as isize) == 0 {
                    drop(Box::from_raw(boxed));
                }
            }
        });
        let live = settings::Live {
            preview: Some(preview),
            reindex: Some(Box::new(move |place: Option<&str>| unsafe {
                // One place travels as a boxed string; zero means everything.
                let boxed = place.map_or(std::ptr::null_mut(), |place| {
                    Box::into_raw(Box::new(place.to_string()))
                });
                if PostMessageW(hwnd, WM_REINDEX, 0, boxed as isize) == 0 && !boxed.is_null() {
                    drop(Box::from_raw(boxed));
                }
            })),
            items: self.index.item_count(),
            sharing: sharing_lines,
            address: sharing_address,
            apply_sharing: Some(Box::new(move |changed: &Settings| {
                let boxed = Box::into_raw(Box::new(changed.clone()));
                unsafe {
                    if PostMessageW(hwnd, WM_APPLY_SHARING, 0, boxed as isize) == 0 {
                        drop(Box::from_raw(boxed));
                    }
                }
            })),
            sharing_status: Some(Box::new(move || {
                let mut answer: (Vec<String>, Option<String>) = (Vec::new(), None);
                unsafe {
                    SendMessageW(
                        hwnd,
                        WM_SHARING_STATUS,
                        0,
                        &mut answer as *mut (Vec<String>, Option<String>) as isize,
                    );
                }
                answer
            })),
        };
        match settings::show(self.hwnd, initial, self.look(), live) {
            Ok(Some(settings)) => {
                let wants_start = settings.start_at_login;
                let wants_links = [settings.start_menu_shortcut, settings.desktop_shortcut];
                self.apply_settings(settings);
                self.save_settings();
                if wants_start != starts && self.persist {
                    if let Err(error) = startup::set(wants_start) {
                        self.set_notice(format!("Could not change starting at sign-in: {error}"));
                    }
                }
                for ((place, had), wants) in links.into_iter().zip(wants_links) {
                    if wants != had && self.persist {
                        if let Err(error) = shortcuts::set(place, wants) {
                            self.set_notice(format!("Could not change the shortcut: {error}"));
                        }
                    }
                }
                self.register_hotkey();
                self.update_tray();
                // Test and screenshot runs never touch the person's pictures.
                if self.persist {
                    wallpaper::cleanup(&self.settings.background_image);
                }
                self.update_sharing();
                let new_live = (
                    self.settings.auto_update,
                    self.settings.scan_excluded_names.clone(),
                    self.settings.scan_excluded_paths.clone(),
                );
                if self.settings.index_roots != old_roots || new_live != old_live {
                    // Live updates start or stop with the "Live" setting.
                    self.restart_live(Some(Arc::clone(&self.index)));
                }
                if self.settings.index_roots != old_roots {
                    self.start_indexing();
                }
            }
            Ok(None) => {
                // Cancelled: put back what the preview changed. Sharing set
                // up from Settings already runs and stays.
                let mut before = before;
                copy_sharing(&self.settings, &mut before);
                self.apply_settings(before);
                // Test and screenshot runs never touch the person's pictures.
                if self.persist {
                    wallpaper::cleanup(&self.settings.background_image);
                }
                if had_hotkey {
                    self.register_hotkey();
                }
            }
            Err(error) => {
                self.apply_settings(before);
                if had_hotkey {
                    self.register_hotkey();
                }
                self.set_notice(format!("Could not open Settings: {error}"));
            }
        }
    }

    /// Shows settings from the open Settings window without saving them.
    /// What to search, the shortcut, and starting at sign-in wait for Save.
    fn preview_settings(&mut self, mut changed: Settings) {
        changed.index_roots = self.settings.index_roots.clone();
        changed.hotkey = self.settings.hotkey.clone();
        changed.start_at_login = self.settings.start_at_login;
        changed.start_menu_shortcut = self.settings.start_menu_shortcut;
        changed.desktop_shortcut = self.settings.desktop_shortcut;
        changed.sidebar_collapsed = self.settings.sidebar_collapsed;
        changed.window = self.settings.window;
        // Sharing changes wait for Save, so servers don't restart mid-edit.
        let now = &self.settings;
        changed.http_mode = now.http_mode;
        changed.ftp_mode = now.ftp_mode;
        changed.share_access = now.share_access;
        changed.tunnel = now.tunnel;
        changed.http_port = now.http_port;
        changed.ftp_port = now.ftp_port;
        changed.share_user = now.share_user.clone();
        changed.share_password = now.share_password.clone();
        changed.share_folders = now.share_folders.clone();
        changed.ftp_plain = now.ftp_plain;
        changed.tunnel_token = now.tunnel_token.clone();
        changed.tunnel_domain = now.tunnel_domain.clone();
        self.apply_settings(changed);
    }

    // ----- Shortcut and tray ------------------------------------------------

    /// Registers the shortcut from settings, replacing any earlier one.
    fn register_hotkey(&mut self) {
        self.unregister_hotkey();
        if !self.persist {
            return;
        }
        let Ok(Some(shortcut)) = hotkey::Shortcut::parse(&self.settings.hotkey) else {
            return;
        };
        // Windows watches plain combinations itself; anything else (one
        // side of a modifier, modifiers alone) goes through the hook.
        self.hotkey_on = match shortcut.registrable() {
            Some((modifiers, key)) => hotkey::register(self.hwnd, HOTKEY_ID, modifiers, key),
            None => {
                keyhook::watch(Some((shortcut.clone(), self.hwnd)));
                true
            }
        };
        if !self.hotkey_on {
            self.set_notice(format!(
                "{shortcut} is taken by another app. Choose a different shortcut in Settings."
            ));
        }
    }

    fn unregister_hotkey(&mut self) {
        if self.hotkey_on {
            hotkey::unregister(self.hwnd, HOTKEY_ID);
            keyhook::watch(None);
            self.hotkey_on = false;
        }
    }

    /// Shows the tray icon while closing hides Feather there.
    fn update_tray(&mut self) {
        let want = self.persist && self.settings.close_to_tray;
        if want && !self.tray_on {
            self.tray_on = tray::add(self.hwnd);
        } else if !want && self.tray_on {
            tray::remove(self.hwnd);
            self.tray_on = false;
        }
    }

    fn is_shown(&self) -> bool {
        unsafe { IsWindowVisible(self.hwnd) != 0 && IsIconic(self.hwnd) == 0 }
    }

    /// Brings the window forward with the search box ready for typing.
    fn summon(&mut self) {
        unsafe {
            // A dialog is open: bring that forward instead.
            if IsWindowEnabled(self.hwnd) == 0 {
                SetForegroundWindow(GetLastActivePopup(self.hwnd));
                return;
            }
            if IsIconic(self.hwnd) != 0 {
                ShowWindow(self.hwnd, SW_RESTORE);
            } else if IsWindowVisible(self.hwnd) == 0 {
                ShowWindow(self.hwnd, SW_SHOW);
            }
            if SetForegroundWindow(self.hwnd) == 0 {
                // A shortcut seen by the keyboard hook does not give this
                // process the right to come forward; borrow the input queue
                // of the window in front for a moment.
                use windows_sys::Win32::System::Threading::AttachThreadInput;
                use windows_sys::Win32::System::Threading::GetCurrentThreadId;
                let front = GetWindowThreadProcessId(GetForegroundWindow(), std::ptr::null_mut());
                let ours = GetCurrentThreadId();
                AttachThreadInput(front, ours, 1);
                SetForegroundWindow(self.hwnd);
                windows_sys::Win32::UI::WindowsAndMessaging::BringWindowToTop(self.hwnd);
                AttachThreadInput(front, ours, 0);
            }
        }
        self.field.select_all();
        self.focus_search();
    }

    /// Lets go of everything drawing needs (the render target, icon and
    /// picture bitmaps) and hands unused memory back to Windows. They are
    /// made again on the next paint.
    fn release_memory(&mut self) {
        self.canvas = canvas::Canvas::default();
        self.icons.reset_device();
        self.wallpaper.reset_device();
        memory::trim();
    }

    /// Hides to the tray, or minimizes when there is no tray icon.
    fn dismiss(&mut self) {
        self.summoned = false;
        if self.tray_on {
            self.save_settings();
            unsafe {
                ShowWindow(self.hwnd, SW_HIDE);
            }
            self.release_memory();
        } else {
            unsafe {
                ShowWindow(self.hwnd, SW_MINIMIZE);
            }
        }
    }

    /// The shortcut toggles: it hides Feather when Feather is in front.
    /// Releases the shortcut while a game or other full-screen app is in
    /// front, so the keys reach it and Feather doesn't pop up over it, and
    /// takes it back afterwards.
    fn follow_fullscreen(&mut self) {
        let pause = self.persist
            && self.settings.pause_in_fullscreen
            && fullscreen::app_in_front(self.hwnd);
        if pause && self.hotkey_on {
            self.unregister_hotkey();
            self.hotkey_paused = true;
        } else if !pause && self.hotkey_paused {
            self.hotkey_paused = false;
            self.register_hotkey();
        }
    }

    fn on_hotkey(&mut self) {
        // A press that arrives just as a game takes the screen.
        if self.settings.pause_in_fullscreen && fullscreen::app_in_front(self.hwnd) {
            return;
        }
        let in_front = unsafe { GetForegroundWindow() == self.hwnd };
        if in_front && self.is_shown() {
            self.dismiss();
        } else {
            self.summon();
            self.summoned = true;
        }
    }

    fn on_tray(&mut self, lparam: LPARAM) {
        match tray::event(lparam) {
            tray::TrayEvent::Open => self.summon(),
            tray::TrayEvent::Menu => match tray::show_menu(self.hwnd) {
                Some(tray::MENU_OPEN) => self.summon(),
                Some(tray::MENU_SETTINGS) => {
                    self.summon();
                    if unsafe { IsWindowEnabled(self.hwnd) } != 0 {
                        self.open_settings();
                    }
                }
                Some(tray::MENU_QUIT) => self.quit(),
                _ => {}
            },
            tray::TrayEvent::NoteClicked => {
                if let Some(release) = crate::update::found() {
                    let _ = context_menu::open_url(&release.download);
                }
                self.drop_update_icon();
            }
            tray::TrayEvent::NoteGone => self.drop_update_icon(),
            tray::TrayEvent::Other => {}
        }
    }

    /// Asks GitHub for a newer Feather in the background, at most once a
    /// day, when the person wants that.
    fn check_for_update(&self) {
        if !self.persist || !self.settings.check_for_updates || !crate::update::due() {
            return;
        }
        let hwnd = self.hwnd as isize;
        let _ = std::thread::Builder::new()
            .name("feather-update-check".into())
            .spawn(move || {
                // Not while Windows is still starting everything else.
                std::thread::sleep(Duration::from_secs(20));
                if crate::update::check().is_ok() && crate::update::found().is_some() {
                    unsafe {
                        windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW(
                            hwnd as HWND,
                            WM_UPDATE_FOUND,
                            0,
                            0,
                        );
                    }
                }
            });
    }

    /// Shows a notification about a newer Feather; clicking it opens the
    /// download.
    fn announce_update(&mut self) {
        let Some(release) = crate::update::found() else {
            return;
        };
        if !self.tray_on && !self.update_icon {
            self.update_icon = tray::add(self.hwnd);
        }
        tray::notify(
            self.hwnd,
            &format!("Feather {} is out", release.version),
            "Click to download it. Install it over this one; your settings stay.",
        );
    }

    fn drop_update_icon(&mut self) {
        if self.update_icon {
            self.update_icon = false;
            if !self.tray_on {
                tray::remove(self.hwnd);
            }
        }
    }

    fn quit(&mut self) {
        self.quitting = true;
        unsafe {
            PostMessageW(self.hwnd, WM_CLOSE, 0, 0);
        }
    }

    fn focus_search(&mut self) {
        unsafe {
            SetFocus(self.hwnd);
        }
        self.caret_on = true;
        self.invalidate();
    }

    // ----- Hit testing and mouse ----------------------------------------------

    fn scrollbar(&self) -> Option<(Rect, Rect)> {
        let list = self.layout.list;
        let content = self.content_height();
        let viewport = f64::from(list.height());
        if content <= viewport || viewport <= 0.0 {
            return None;
        }
        let track = Rect::new(list.r - 10.0, list.t + 4.0, list.r - 2.0, list.b - 4.0);
        let thumb_h = ((f64::from(track.height()) * viewport / content) as f32)
            .max(28.0)
            .min(track.height());
        let travel = track.height() - thumb_h;
        let top = track.t + travel * (self.scroll / (content - viewport)).clamp(0.0, 1.0) as f32;
        Some((track, Rect::new(track.l, top, track.r, top + thumb_h)))
    }

    fn row_at(&self, x: f32, y: f32) -> Option<usize> {
        let list = self.layout.list;
        if !list.contains(x, y) || x >= list.r - 10.0 {
            return None;
        }
        let row = ((f64::from(y - list.t) + self.scroll) / f64::from(ROW_H)).floor() as usize;
        (row < self.model.result_count()).then_some(row)
    }

    fn hit_at(&self, x: f32, y: f32) -> Hit {
        if let Some(button) = self.layout.button_at(x, y) {
            return Hit::Button(button);
        }
        if let Some((track, thumb)) = self.scrollbar() {
            if thumb.inset(-3.0, 0.0).contains(x, y) {
                return Hit::ScrollThumb;
            }
            if track.inset(-3.0, 0.0).contains(x, y) {
                return Hit::ScrollTrack;
            }
        }
        if let Some(row) = self.row_at(x, y) {
            return Hit::Row(row);
        }
        let search = self.layout.search_text;
        if Rect::new(self.layout.search_icon.l, search.t, search.r, search.b).contains(x, y) {
            return Hit::Search;
        }
        for (index, rect) in self.nav_rects.iter().enumerate() {
            if rect.contains(x, y) && matches!(self.nav[index], NavEntry::Item(_)) {
                return Hit::Nav(index);
            }
        }
        Hit::None
    }

    fn set_hover(&mut self, hit: Hit) {
        if self.hover != hit {
            self.motion.hover(self.hover, hit, Instant::now());
            self.hover = hit;
            self.kick();
            self.invalidate();
        }
    }

    fn track_leave(&mut self, nonclient: bool) {
        let mut event = TRACKMOUSEEVENT {
            cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_LEAVE | if nonclient { TME_NONCLIENT } else { 0 },
            hwndTrack: self.hwnd,
            dwHoverTime: 0,
        };
        unsafe {
            TrackMouseEvent(&mut event);
        }
        self.tracking = true;
    }

    fn search_layout(&self) -> Option<windows::Win32::Graphics::DirectWrite::IDWriteTextLayout> {
        self.gfx.layout(
            self.field.wide(),
            Font::Search,
            100_000.0,
            self.layout.search_text.height(),
        )
    }

    fn search_position_at(&self, x: f32, y: f32) -> usize {
        let Some(layout) = self.search_layout() else {
            return 0;
        };
        let area = self.layout.search_text;
        paint::position_at(&layout, x - area.l + self.search_scroll, y - area.t)
    }

    fn on_mouse_move(&mut self, x: f32, y: f32) {
        if !self.tracking {
            self.track_leave(false);
        }
        match self.drag {
            Drag::Text => {
                let position = self.search_position_at(x, y);
                self.field.move_to(position, true);
                self.caret_on = true;
                self.invalidate();
            }
            Drag::Thumb {
                start_y,
                start_scroll,
            } => {
                if let Some((track, thumb)) = self.scrollbar() {
                    let range = self.max_scroll();
                    let travel = f64::from((track.height() - thumb.height()).max(1.0));
                    let scroll = start_scroll + f64::from(y - start_y) * range / travel;
                    self.set_scroll(scroll, false);
                }
            }
            Drag::Nav {
                index,
                grab,
                start_y,
                moving,
                ..
            } => {
                let moving = moving || (y - start_y).abs() > 4.0;
                self.drag = Drag::Nav {
                    index,
                    grab,
                    start_y,
                    y,
                    moving,
                };
                if moving {
                    self.set_hover(Hit::None);
                    let slot = self.nav_drop_slot(index, y - grab);
                    self.shift_tabs(index, slot);
                    self.invalidate();
                }
            }
            Drag::None => {
                let hit = self.hit_at(x, y);
                self.set_hover(hit);
            }
        }
    }

    fn on_mouse_down(&mut self, x: f32, y: f32) {
        let hit = self.hit_at(x, y);
        self.pressed = hit;
        let shift = key_down(VK_SHIFT);
        // Clicking anywhere but the name being typed keeps the new name,
        // as in Explorer.
        if self.rename.is_some() {
            if self.rename_hit(x, y) {
                self.on_rename_click(x, y, shift);
                return;
            }
            self.commit_rename();
        }
        match hit {
            Hit::Search => {
                self.list_focus = false;
                let position = self.search_position_at(x, y);
                self.field.move_to(position, shift);
                self.drag = Drag::Text;
                self.caret_on = true;
                unsafe {
                    SetCapture(self.hwnd);
                }
            }
            Hit::Row(row) => {
                self.list_focus = true;
                self.select_row(row);
            }
            Hit::ScrollThumb => {
                self.drag = Drag::Thumb {
                    start_y: y,
                    start_scroll: self.scroll,
                };
                unsafe {
                    SetCapture(self.hwnd);
                }
            }
            Hit::ScrollTrack => {
                if let Some((_, thumb)) = self.scrollbar() {
                    let page = self.layout.list.height() - ROW_H;
                    let delta = if y < thumb.t { -page } else { page };
                    self.set_scroll(self.motion.scroll.target() + f64::from(delta), true);
                }
            }
            Hit::Nav(index) => {
                if matches!(
                    self.nav.get(index),
                    Some(NavEntry::Item(NavItem::Category(_)))
                ) {
                    self.drag = Drag::Nav {
                        index,
                        grab: y - self.nav_rects[index].t,
                        start_y: y,
                        y,
                        moving: false,
                    };
                }
                unsafe {
                    SetCapture(self.hwnd);
                }
            }
            Hit::Button(_) => unsafe {
                SetCapture(self.hwnd);
            },
            Hit::None => {}
        }
        self.invalidate();
    }

    fn on_mouse_up(&mut self, x: f32, y: f32) {
        let pressed = std::mem::take(&mut self.pressed);
        let drag = std::mem::take(&mut self.drag);
        unsafe {
            ReleaseCapture();
        }
        if let Drag::Nav {
            index,
            grab,
            moving: true,
            ..
        } = drag
        {
            let slot = self.nav_drop_slot(index, y - grab);
            for shift in &mut self.nav_shift {
                shift.jump(0.0);
            }
            self.move_tab(index, slot);
            let hit = self.hit_at(x, y);
            self.set_hover(hit);
            return;
        }
        let was_dragging = !matches!(drag, Drag::None | Drag::Nav { .. });
        if was_dragging {
            self.invalidate();
            return;
        }
        let hit = self.hit_at(x, y);
        if hit != pressed {
            self.invalidate();
            return;
        }
        match hit {
            Hit::Button(button) => self.press(button),
            Hit::Nav(index) => self.activate_nav(index),
            _ => {}
        }
        self.invalidate();
    }

    fn on_double_click(&mut self, x: f32, y: f32) {
        match self.hit_at(x, y) {
            Hit::Row(row) => self.activate_row(row),
            Hit::Search => {
                let position = self.search_position_at(x, y);
                self.field.select_word_at(position);
                self.invalidate();
            }
            _ => self.on_mouse_down(x, y),
        }
    }

    fn on_wheel(&mut self, delta: i16) {
        let lines = 3.0;
        let amount = -(delta as f32 / 120.0) * lines * ROW_H;
        self.set_scroll(self.motion.scroll.target() + f64::from(amount), true);
    }

    // ----- Keyboard -----------------------------------------------------------

    fn on_key(&mut self, key: u16) -> bool {
        let shift = key_down(VK_SHIFT);
        let control = key_down(VK_CONTROL);
        if self.rename.is_some() {
            return self.on_rename_key(key, shift, control);
        }
        // Keys that move the caret or edit text belong to the search box,
        // and keys that move through the results give the list the focus.
        if matches!(key, VK_LEFT | VK_RIGHT | VK_BACK)
            || (matches!(key, VK_HOME | VK_END) && !control)
        {
            self.list_focus = false;
        }
        if matches!(key, VK_UP | VK_DOWN | VK_PRIOR | VK_NEXT)
            || (matches!(key, VK_HOME | VK_END) && control)
        {
            self.list_focus = true;
        }
        let mut edited = false;
        match key {
            VK_LEFT => self.field.move_left(shift, control),
            VK_RIGHT => self.field.move_right(shift, control),
            VK_HOME if control => self.select_row(0),
            VK_END if control => self.select_row(usize::MAX),
            VK_HOME => self.field.home(shift),
            VK_END => self.field.end(shift),
            VK_BACK => edited = self.field.backspace(control),
            VK_DELETE if self.list_focus && self.selected.is_some() => {
                self.delete_selected(shift);
            }
            VK_DELETE => edited = self.field.delete(control),
            VK_F2 => {
                if let Some(row) = self.selected {
                    self.begin_rename(row);
                }
            }
            VK_UP => self.move_selection(-1),
            VK_DOWN => self.move_selection(1),
            VK_PRIOR => self.move_selection(-self.page_rows()),
            VK_NEXT => self.move_selection(self.page_rows()),
            VK_RETURN => {
                if let Some(row) = self.selected {
                    if control {
                        self.open_row(row, true);
                    } else {
                        self.activate_row(row);
                    }
                }
            }
            VK_APPS => {
                if let Some(row) = self.selected {
                    self.show_context_menu(row);
                }
            }
            VK_F10 if shift => {
                if let Some(row) = self.selected {
                    self.show_context_menu(row);
                }
            }
            _ if control && key == b'A' as u16 => self.field.select_all(),
            _ if control && key == b'C' as u16 => {
                if !self.list_focus && self.field.has_selection() {
                    let _ = context_menu::copy_text(self.hwnd, &self.field.selected_text());
                } else if self.selected.is_some() {
                    // As files for Explorer, and as the path for text boxes.
                    self.copy_selected(false);
                }
            }
            _ if control && key == b'X' as u16 => {
                if !self.list_focus && self.field.has_selection() {
                    let _ = context_menu::copy_text(self.hwnd, &self.field.selected_text());
                    edited = self.field.delete_selection();
                } else if self.list_focus && self.selected.is_some() {
                    self.copy_selected(true);
                }
            }
            _ if control && key == b'V' as u16 => {
                let files = if self.list_focus {
                    fileops::clipboard_files(self.hwnd)
                } else {
                    None
                };
                if let Some(files) = files {
                    self.paste_files(files);
                } else if let Some(text) = context_menu::paste_text(self.hwnd) {
                    self.list_focus = false;
                    edited = self.field.insert(&text);
                }
            }
            _ => return false,
        }
        self.caret_on = true;
        if edited {
            self.on_search_changed();
        }
        self.invalidate();
        true
    }

    fn on_char(&mut self, unit: u16) {
        // Control characters arrive for keys handled in `on_key`.
        if unit < 0x20 || unit == 0x7f {
            return;
        }
        if let Some(rename) = &mut self.rename {
            // Characters no name can hold are refused as they are typed.
            let bad = char::from_u32(u32::from(unit)).is_some_and(fileops::is_forbidden_char);
            if bad {
                self.set_notice(fileops::FORBIDDEN_MESSAGE.into());
            } else {
                rename.field.insert_units(&[unit]);
                self.caret_on = true;
            }
            self.invalidate();
            return;
        }
        self.list_focus = false;
        self.field.insert_units(&[unit]);
        self.caret_on = true;
        self.on_search_changed();
    }

    // ----- Rendering ----------------------------------------------------------

    fn build_frame(&mut self, opaque: bool, cache_icons: bool) -> FrameData {
        let now = Instant::now();
        self.tick();
        self.sync_motion(now);
        let list = self.layout.list;
        let count = self.model.result_count();
        let first = (self.scroll / f64::from(ROW_H)).floor() as usize;
        let visible = self.layout.visible_rows();
        let mut rows = Vec::with_capacity(visible);
        for row in first..(first + visible).min(count) {
            let Some(entry_ref) = self.model.ids.get(row) else {
                continue;
            };
            let entry = self.index.entry(entry_ref);
            let name = String::from_utf16_lossy(entry.name_display);
            let is_dir = entry.is_dir();
            let icon = self.icons.index_for(&name, is_dir);
            let spans = self.query.match_spans(&name);
            rows.push(RowView {
                index: row,
                top: list.t + (row as f64 * f64::from(ROW_H) - self.scroll) as f32,
                name,
                spans,
                folder: self.cell_text(row, 1).unwrap_or_default(),
                size: self.cell_text(row, 2).unwrap_or_default(),
                modified: self.cell_text(row, 3).unwrap_or_default(),
                icon,
                selected: self.selected == Some(row),
            });
        }

        let rename = self.rename_view();
        let details = self.selected.and_then(|row| {
            let entry_ref = self.model.ids.get(row)?;
            let entry = self.index.entry(entry_ref);
            let name = String::from_utf16_lossy(entry.name_display);
            let is_dir = entry.is_dir();
            let totals = self.index.folder_totals(entry_ref);
            Some(DetailsView {
                icon: self.icons.index_for(&name, is_dir),
                kind: self.cell_text(row, 4).unwrap_or_default(),
                size: if is_dir && totals.is_none() {
                    String::new()
                } else {
                    size_details(entry.size, self.settings.size_format)
                },
                items: totals
                    .map(|(_, items)| item_count_text(items))
                    .unwrap_or_default(),
                modified: self.cell_text(row, 3).unwrap_or_default(),
                folder: self.cell_text(row, 1).unwrap_or_default(),
                name,
            })
        });

        let mut nav_icons = Vec::with_capacity(self.nav.len());
        for entry in &self.nav {
            match entry {
                NavEntry::Item(item) => {
                    nav_icons.push(match item {
                        NavItem::Category(category) => {
                            let (probe, is_dir) = category_icon(*category);
                            self.icons.index_for(probe, is_dir)
                        }
                        NavItem::Scope(Some(scope)) => self.icons.index_for_scope(scope),
                        NavItem::Scope(None) => self.icons.index_for("", true),
                        NavItem::ChooseFolder => -1,
                    });
                }
                NavEntry::Label(_) => {
                    nav_icons.push(-1);
                }
            }
        }

        // Keep the caret inside the visible part of the search text.
        let text_layout = self.search_layout();
        let (caret_x, end_x, selection) = match &text_layout {
            Some(text) => {
                let (start, end) = self.field.selection();
                (
                    paint::caret_offset(text, self.field.caret()),
                    paint::caret_offset(text, self.field.wide().len()),
                    paint::selection_spans(text, start, end),
                )
            }
            None => (0.0, 0.0, Vec::new()),
        };
        let width = self.layout.search_text.width() - 4.0;
        if caret_x - self.search_scroll > width {
            self.search_scroll = caret_x - width;
        }
        if caret_x < self.search_scroll {
            self.search_scroll = caret_x;
        }
        self.search_scroll = self.search_scroll.min((end_x - width).max(0.0)).max(0.0);

        let sort_column = match self.sort_column {
            sort::COLUMN_SIZE => Button::SortSize,
            sort::COLUMN_MODIFIED => Button::SortModified,
            _ => Button::SortName,
        };
        let warning = self
            .model
            .warnings
            .first()
            .map(|warning| format!("Warning: {warning}"));
        let items = self.index.item_count();
        let (notice, index_text) = match &self.indexer {
            // The first index: say what is being read.
            Some(indexer) if items == 0 => (
                format!("Reading {}", self.indexing_root),
                format!("{} found so far", group_digits(indexer.found())),
            ),
            // Keeping it fresh happens quietly.
            Some(_) => (
                warning.unwrap_or_else(|| self.notice.clone()),
                format!("{} items, updating", group_digits(items)),
            ),
            None => (
                warning.unwrap_or_else(|| self.notice.clone()),
                format!("{} items", group_digits(items)),
            ),
        };
        // Entries pushed aside by a dragged tab, and the tab itself under
        // the pointer.
        let mut nav_rects = self.nav_rects.clone();
        for (rect, shift) in nav_rects.iter_mut().zip(&self.nav_shift) {
            if !rect.is_empty() {
                let offset = shift.value(now);
                rect.t += offset;
                rect.b += offset;
            }
        }
        let mut nav_lifted = None;
        if let Drag::Nav {
            index,
            grab,
            y,
            moving: true,
            ..
        } = self.drag
        {
            if let Some(rect) = nav_rects.get_mut(index) {
                let top = y - grab;
                *rect = Rect::new(rect.l, top, rect.r, top + NAV_H);
                nav_lifted = Some(index);
            }
        }
        let nav_pills = (0..2)
            .zip(self.nav_selection())
            .filter_map(|(group, (index, _))| {
                let index = index?;
                let rect = nav_rects[index];
                // A selected tab being dragged carries its highlight along.
                let top =
                    if group == 0 && (nav_lifted.is_some() || self.nav_shift[index].running(now)) {
                        rect.t
                    } else {
                        self.motion.nav_top(group, now)
                    };
                Some(Rect::new(rect.l, top, rect.r, top + NAV_H))
            })
            .collect();
        let sidebar_open = self.sidebar_open();
        // Names of folded tabs appear beside the pointer.
        let tooltip = match self.hover {
            _ if sidebar_open > 0.05 || self.drag != Drag::None => None,
            Hit::Nav(index) => match self.nav.get(index) {
                Some(NavEntry::Item(item)) => Some((nav_rects[index], paint::nav_label(item))),
                _ => None,
            },
            Hit::Button(button @ (Button::Settings | Button::Help)) => self
                .layout
                .button(button)
                .map(|rect| (rect, format!("{button:?}"))),
            Hit::Button(Button::Sidebar) => self
                .layout
                .button(Button::Sidebar)
                .map(|rect| (rect, "Expand sidebar".to_string())),
            _ => None,
        };
        let solid_live = !opaque && self.material == Material::Solid;
        FrameData {
            palette: self.motion.palette(self.palette, now),
            hovers: if matches!(self.drag, Drag::Nav { moving: true, .. }) {
                Vec::new()
            } else {
                self.motion.hover_levels(now)
            },
            row_pill: self
                .selected
                .map(|_| list.t + (self.motion.row_top(now) - self.scroll) as f32),
            nav_pills,
            details_opacity: self.motion.details_opacity(now),
            rows,
            rename,
            details,
            nav_icons,
            search: SearchView {
                layout: text_layout,
                empty: self.field.is_empty(),
                scroll: self.search_scroll,
                caret_x,
                // The caret leaves the search box while keys act on the
                // results.
                show_caret: self.focused
                    && self.caret_on
                    && !opaque
                    && !self.list_focus
                    && self.rename.is_none(),
                selection,
            },
            count_text: self.count_text(),
            scope_text: self.scope_text(),
            notice,
            index_text,
            nav_rects,
            nav_lifted,
            sidebar_open,
            tooltip,
            caption_hole: solid_live.then_some(self.layout.caption),
            details_w: self.details_w.value(now),
            logo_icon: icons::APP_ICON,
            filter_count: self.filters.active_categories(),
            empty_message: self.empty_message(),
            sort_column,
            sort_ascending: self.sort_ascending,
            scrollbar: self.scrollbar(),
            opaque: opaque || self.material == Material::Solid,
            cache_icons,
        }
    }

    fn draw(
        &mut self,
        rt: &ID2D1RenderTarget,
        opaque: bool,
        cache_icons: bool,
    ) -> windows::core::Result<()> {
        let data = self.build_frame(opaque, cache_icons);
        let strength = self.settings.background_strength as f32 / 100.0;
        let background = if strength > 0.0 {
            self.wallpaper
                .bitmap(&self.gfx.wic, rt, cache_icons)
                .map(|(bitmap, width, height)| paint::Background {
                    bitmap,
                    width,
                    height,
                    fit: self.settings.background_fit,
                    strength,
                })
        } else {
            None
        };
        let frame = Frame {
            palette: data.palette,
            background,
            layout: &self.layout,
            nav: &self.nav,
            nav_rects: &data.nav_rects,
            nav_icons: data.nav_icons,
            nav_lifted: data.nav_lifted,
            sidebar_open: data.sidebar_open,
            tooltip: data.tooltip,
            caption_hole: data.caption_hole,
            details_w: data.details_w,
            logo_icon: data.logo_icon,
            hover: self.hover,
            pressed: self.pressed,
            hovers: data.hovers,
            row_pill: data.row_pill,
            nav_pills: data.nav_pills,
            details_opacity: data.details_opacity,
            search: data.search,
            count_text: data.count_text,
            scope_text: data.scope_text,
            notice: data.notice,
            index_text: data.index_text,
            filter_count: data.filter_count,
            rows: data.rows,
            rename: data.rename,
            empty_message: data.empty_message,
            sort_column: data.sort_column,
            sort_ascending: data.sort_ascending,
            scrollbar: data.scrollbar,
            details: data.details,
            opaque: data.opaque,
            cache_icons: data.cache_icons,
        };
        let Some(painter) = Painter::new(rt, &self.gfx) else {
            return Ok(());
        };
        unsafe {
            rt.BeginDraw();
            rt.SetTextAntialiasMode(
                windows::Win32::Graphics::Direct2D::D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE,
            );
            paint::paint(&painter, &mut self.icons, &frame);
            rt.EndDraw(None, None)
        }
    }

    /// Draws the window with Direct2D. See `canvas` for how translucent
    /// pixels reach the Mica or Acrylic backdrop.
    fn render(&mut self, hdc: HDC) {
        let (width, height) = win::client_size(self.hwnd);
        let mut canvas = std::mem::take(&mut self.canvas);
        let d2d = self.gfx.d2d.clone();
        // The frame lives on in the window's redirection surface, so the
        // bitmap is only kept while an animation needs it every frame.
        let keep = self.anim_timer;
        let presented = canvas.present(&d2d, hdc, width, height, keep, |rt| {
            self.draw(rt, false, true)
        });
        self.canvas = canvas;
        if let canvas::Presented::Lost = presented {
            self.icons.reset_device();
            self.wallpaper.reset_device();
            self.invalidate();
        }
    }

    /// Renders the window into a PNG with an opaque background.
    fn capture(&mut self, path: &Path) -> Result<(), String> {
        let (width, height) = win::client_size(self.hwnd);
        if width <= 0 || height <= 0 {
            return Err("window has no client area".into());
        }
        // Screenshots show where every animation ends up.
        self.sync_motion(Instant::now());
        self.motion.settle();
        self.tick();
        let d2d = self.gfx.d2d.clone();
        let wic = self.gfx.wic.clone();
        canvas::capture_png(&d2d, &wic, width as u32, height as u32, path, |rt| {
            self.draw(rt, true, false)
        })
    }

    // ----- Scenario helpers ---------------------------------------------------

    /// Average time to draw and present one full frame to the window, the
    /// work done on every animation frame.
    fn time_frames(&mut self, count: u32) -> f64 {
        let keep = self.anim_timer;
        self.anim_timer = true;
        let hdc = unsafe { windows_sys::Win32::Graphics::Gdi::GetDC(self.hwnd) };
        self.render(hdc);
        let started = Instant::now();
        for _ in 0..count {
            self.render(hdc);
        }
        let elapsed = started.elapsed().as_secs_f64() * 1000.0 / count.max(1) as f64;
        unsafe {
            windows_sys::Win32::Graphics::Gdi::ReleaseDC(self.hwnd, hdc);
        }
        self.anim_timer = keep;
        elapsed
    }

    fn is_idle(&self) -> bool {
        self.model.is_idle()
    }

    fn result_count(&self) -> usize {
        self.model.result_count()
    }

    fn total(&self) -> u64 {
        self.model.total()
    }

    fn leading_sizes(&self, count: usize) -> Vec<u64> {
        self.model
            .ids
            .iter()
            .take(count)
            .map(|entry_ref| self.index.entry(entry_ref).size)
            .collect()
    }

    fn leading_folders(&self, count: usize) -> Vec<String> {
        self.model
            .ids
            .iter()
            .take(count)
            .filter_map(|entry_ref| self.index.resolve_path(entry_ref))
            .map(|path| parent_path(&path))
            .collect()
    }

    fn leading_names(&self, count: usize) -> Vec<String> {
        self.model
            .ids
            .iter()
            .take(count)
            .map(|entry_ref| String::from_utf16_lossy(self.index.entry(entry_ref).name_display))
            .collect()
    }

    fn leading_paths(&self, count: usize) -> Vec<String> {
        self.model
            .ids
            .iter()
            .take(count)
            .filter_map(|entry_ref| self.index.resolve_path(entry_ref))
            .collect()
    }

    // ----- Window frame -------------------------------------------------------

    fn caption_hit(&self, x: f32, y: f32) -> Option<u32> {
        let maximized = unsafe { IsZoomed(self.hwnd) != 0 };
        if !maximized && y < layout::RESIZE_EDGE {
            return Some(if x < layout::RESIZE_EDGE * 2.0 {
                HTTOPLEFT
            } else if x > self.layout.client.r - layout::RESIZE_EDGE * 2.0 {
                HTTOPRIGHT
            } else {
                HTTOP
            });
        }
        if self.layout.button_at(x, y).is_some() {
            return Some(HTCLIENT);
        }
        let in_topbar = self.layout.topbar.contains(x, y);
        let in_wordmark = x < self.layout.sidebar.r && y < self.layout.nav.t;
        if in_topbar || in_wordmark {
            return Some(HTCAPTION);
        }
        None
    }
}

/// Owned per-frame data; `Frame` borrows the layout and nav from `Ui`.
struct FrameData {
    palette: Palette,
    hovers: Vec<(Hit, f32)>,
    row_pill: Option<f32>,
    nav_pills: Vec<Rect>,
    details_opacity: f32,
    rows: Vec<RowView>,
    rename: Option<RenameView>,
    details: Option<DetailsView>,
    nav_icons: Vec<i32>,
    search: SearchView,
    count_text: String,
    scope_text: String,
    notice: String,
    index_text: String,
    nav_rects: Vec<Rect>,
    nav_lifted: Option<usize>,
    sidebar_open: f32,
    tooltip: Option<(Rect, String)>,
    caption_hole: Option<Rect>,
    details_w: f32,
    logo_icon: i32,
    filter_count: usize,
    empty_message: Option<String>,
    sort_column: Button,
    sort_ascending: bool,
    scrollbar: Option<(Rect, Rect)>,
    opaque: bool,
    cache_icons: bool,
}

impl Drop for Ui {
    fn drop(&mut self) {
        unsafe {
            if !self.accel.is_null() {
                DestroyAcceleratorTable(self.accel);
            }
        }
    }
}

/// Width of the system minimize, maximize, and close buttons that DWM draws
/// over the backdrop, so the toolbar can stay clear of them.
/// Width and height of the minimize, maximize, and close buttons.
fn caption_bounds(hwnd: HWND) -> (f32, f32) {
    let mut bounds = RECT::default();
    let ok = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CAPTION_BUTTON_BOUNDS as u32,
            &mut bounds as *mut RECT as *mut _,
            size_of::<RECT>() as u32,
        )
    } == 0;
    let width = (bounds.right - bounds.left) as f32;
    let height = (bounds.bottom - bounds.top) as f32;
    if ok && width > 0.0 && height > 0.0 {
        (width, height)
    } else {
        (138.0, 32.0)
    }
}

/// A representative file name whose icon stands for a category.
fn category_icon(category: Category) -> (&'static str, bool) {
    match category {
        Category::Everything | Category::Folders => ("", true),
        Category::Apps => ("a.exe", false),
        Category::Files => ("file", false),
        Category::Documents => ("a.docx", false),
        Category::Images => ("a.png", false),
        Category::Video => ("a.mp4", false),
        Category::Audio => ("a.mp3", false),
        Category::Archives => ("a.zip", false),
        Category::Executables => ("a.bat", false),
        Category::Code => ("a.json", false),
        Category::Custom => ("file", false),
    }
}

/// "1.2 MB (1,234,567 bytes)" for the details pane.
fn size_details(bytes: u64, format: settings::SizeFormat) -> String {
    format!(
        "{} ({} {})",
        size_in(bytes, format),
        group_digits(bytes),
        if bytes == 1 { "byte" } else { "bytes" }
    )
}

/// "1,204 items" below a folder.
fn item_count_text(items: u32) -> String {
    format!(
        "{} {}",
        group_digits(u64::from(items)),
        if items == 1 { "item" } else { "items" }
    )
}

fn group_digits(value: u64) -> String {
    text::group(value)
}

fn key_down(key: u16) -> bool {
    unsafe { GetKeyState(key as i32) < 0 }
}

fn lparam_point(lparam: LPARAM) -> (f32, f32) {
    let x = (lparam & 0xffff) as u16 as i16 as f32;
    let y = ((lparam >> 16) & 0xffff) as u16 as i16 as f32;
    (x, y)
}

fn cursor_client(hwnd: HWND) -> (f32, f32) {
    let mut point = POINT::default();
    unsafe {
        GetCursorPos(&mut point);
        ScreenToClient(hwnd, &mut point);
    }
    (point.x as f32, point.y as f32)
}

fn format_filetime(value: u64) -> String {
    if value == 0 {
        return String::new();
    }
    let filetime = FILETIME {
        dwLowDateTime: value as u32,
        dwHighDateTime: (value >> 32) as u32,
    };
    let mut system = SYSTEMTIME::default();
    if unsafe { FileTimeToSystemTime(&filetime, &mut system) } == 0 {
        return String::new();
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        system.wYear, system.wMonth, system.wDay, system.wHour, system.wMinute
    )
}
fn capture_placement(hwnd: HWND) -> Option<WindowPlacement> {
    let mut placement = WINDOWPLACEMENT {
        length: size_of::<WINDOWPLACEMENT>() as u32,
        ..Default::default()
    };
    if unsafe { GetWindowPlacement(hwnd, &mut placement) } == 0 {
        return None;
    }
    let rect = placement.rcNormalPosition;
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    if width <= 0 || height <= 0 {
        return None;
    }
    Some(WindowPlacement {
        x: rect.left,
        y: rect.top,
        width,
        height,
        maximized: placement.showCmd == SW_SHOWMAXIMIZED as u32,
    })
}

fn restore_placement(hwnd: HWND, placement: &WindowPlacement) -> bool {
    let rect = RECT {
        left: placement.x,
        top: placement.y,
        right: placement.x.saturating_add(placement.width),
        bottom: placement.y.saturating_add(placement.height),
    };
    let monitor = unsafe { MonitorFromRect(&rect, MONITOR_DEFAULTTONULL) };
    if monitor.is_null() {
        return false;
    }
    unsafe {
        SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            placement.x,
            placement.y,
            placement.width,
            placement.height,
            SWP_NOZORDER | SWP_NOACTIVATE,
        ) != 0
    }
}

static REGISTERED: Once = Once::new();

fn register_class(hinstance: windows_sys::Win32::Foundation::HINSTANCE) -> Result<(), String> {
    let mut result = Ok(());
    REGISTERED.call_once(|| {
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW | CS_DBLCLKS,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: tray::app_icon(0),
            hCursor: unsafe { LoadCursorW(std::ptr::null_mut(), IDC_ARROW) },
            // No background brush: the renderer paints every pixel, and
            // transparent pixels must stay transparent for the backdrop.
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: w!("FeatherMainWindow"),
        };
        let atom = unsafe { RegisterClassW(&class) };
        if atom == 0 {
            result = Err("RegisterClassW failed".into());
        }
    });
    result
}

fn create_window(
    index: Arc<IndexSet>,
    show: bool,
    settings: Settings,
    persist: bool,
) -> Result<HWND, String> {
    let hinstance = unsafe { GetModuleHandleW(std::ptr::null()) };
    register_class(hinstance)?;
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            w!("FeatherMainWindow"),
            w!("Feather"),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            DEFAULT_WIDTH,
            DEFAULT_HEIGHT,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinstance,
            std::ptr::null(),
        )
    };
    if hwnd.is_null() {
        return Err("CreateWindowExW failed".into());
    }

    let mut maximized = false;
    if settings.remember_window {
        if let Some(placement) = &settings.window {
            if restore_placement(hwnd, placement) {
                maximized = placement.maximized;
            }
        }
    }

    let ui = match Ui::new(hwnd, index, settings, persist) {
        Ok(ui) => Box::new(ui),
        Err(error) => {
            unsafe { DestroyWindow(hwnd) };
            return Err(error);
        }
    };
    let pointer = Box::into_raw(ui);
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, pointer as isize);
        // Recalculate the frame now that WM_NCCALCSIZE removes the caption.
        SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
    let ui = unsafe { &mut *pointer };
    unsafe {
        let small = tray::app_icon(tray::small_icon_size());
        SendMessageW(hwnd, WM_SETICON, ICON_SMALL as usize, small as isize);
        SendMessageW(
            hwnd,
            WM_SETICON,
            ICON_BIG as usize,
            tray::app_icon(0) as isize,
        );
    }
    ui.layout();
    if show {
        let command = if maximized { SW_SHOWMAXIMIZED } else { SW_SHOW };
        unsafe {
            ShowWindow(hwnd, command);
            UpdateWindow(hwnd);
            SetForegroundWindow(hwnd);
        }
        ui.focus_search();
    }
    Ok(hwnd)
}

unsafe fn ui_ref<'a>(hwnd: HWND) -> Option<&'a mut Ui> {
    let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Ui;
    if pointer.is_null() {
        None
    } else {
        Some(&mut *pointer)
    }
}

fn translate_accelerator(hwnd: HWND, message: &MSG) -> bool {
    if hwnd.is_null() {
        return false;
    }
    unsafe {
        match ui_ref(hwnd) {
            Some(ui) => TranslateAcceleratorW(hwnd, ui.accel, message) != 0,
            None => false,
        }
    }
}

/// Handles queued messages for up to 50 ms. Timers and paints keep arriving
/// while the window is alive, so the queue may never be empty.
fn pump_messages(hwnd: HWND) {
    let mut message = MSG::default();
    let started = Instant::now();
    while started.elapsed() < Duration::from_millis(50)
        && unsafe { PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) } > 0
    {
        if message.message == windows_sys::Win32::UI::WindowsAndMessaging::WM_QUIT {
            break;
        }
        if translate_accelerator(hwnd, &message) {
            continue;
        }
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

fn pump_until_idle(hwnd: HWND, timeout: Duration) {
    let started = Instant::now();
    loop {
        pump_messages(hwnd);
        let idle = unsafe { ui_ref(hwnd).map(|ui| ui.is_idle()).unwrap_or(true) };
        if idle || started.elapsed() >= timeout {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    pump_messages(hwnd);
}

/// Removes the standard caption while keeping the resize borders. When
/// maximized, Windows positions the window so its frame hangs off-screen;
/// the client area is inset by that much so content stays visible.
unsafe fn on_nccalcsize(hwnd: HWND, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if wparam == 0 {
        return DefWindowProcW(hwnd, WM_NCCALCSIZE, wparam, lparam);
    }
    let params = &mut *(lparam as *mut NCCALCSIZE_PARAMS);
    let original_top = params.rgrc[0].top;
    let result = DefWindowProcW(hwnd, WM_NCCALCSIZE, wparam, lparam);
    if result != 0 {
        return result;
    }
    params.rgrc[0].top = original_top;
    if IsZoomed(hwnd) != 0 {
        params.rgrc[0].top += GetSystemMetrics(SM_CYFRAME) + GetSystemMetrics(SM_CXPADDEDBORDER);
    }
    0
}

unsafe fn on_nchittest(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    let default = DefWindowProcW(hwnd, WM_NCHITTEST, 0, lparam);
    if default != HTCLIENT as LRESULT {
        return default;
    }
    let (x, y) = lparam_point(lparam);
    let mut point = POINT {
        x: x as i32,
        y: y as i32,
    };
    ScreenToClient(hwnd, &mut point);
    match ui_ref(hwnd).and_then(|ui| ui.caption_hit(point.x as f32, point.y as f32)) {
        Some(code) => code as LRESULT,
        None => HTCLIENT as LRESULT,
    }
}

unsafe extern "system" fn wndproc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // DWM draws and hit-tests the system caption buttons over the backdrop.
    let mut dwm_result: LRESULT = 0;
    if DwmDefWindowProc(hwnd, message, wparam, lparam, &mut dwm_result) != 0 {
        return dwm_result;
    }
    match message {
        WM_GETMINMAXINFO => {
            let info = &mut *(lparam as *mut MINMAXINFO);
            info.ptMinTrackSize = POINT { x: 680, y: 420 };
            return 0;
        }
        WM_NCCALCSIZE => return on_nccalcsize(hwnd, wparam, lparam),
        WM_NCHITTEST => return on_nchittest(hwnd, lparam),
        WM_ERASEBKGND => return 1,
        _ => {}
    }
    if let Some(result) = context_menu::handle_menu_message(message, wparam, lparam) {
        return result;
    }

    let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Ui;
    if !pointer.is_null() {
        // Keeps the `Ui` alive until this call is done with it, even if a
        // nested message loop below destroys the window.
        let _entered = Entered::new(pointer);
        let ui = &mut *pointer;
        match message {
            WM_PAINT => {
                let mut paint = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut paint);
                ui.render(hdc);
                EndPaint(hwnd, &paint);
                return 0;
            }
            WM_SIZE => {
                // Minimized, nothing is drawn until it comes back.
                if wparam == 1 {
                    ui.release_memory();
                    return 0;
                }
                ui.layout();
                return 0;
            }
            WM_COMMAND => {
                if ui.on_command(wparam) {
                    return 0;
                }
            }
            WM_TIMER => {
                ui.on_timer(wparam);
                return 0;
            }
            frames::WM_ANIM_FRAME => {
                ui.on_anim_frame();
                return 0;
            }
            WM_RUN_SETUP => {
                ui.run_setup();
                return 0;
            }
            WM_HOTKEY => {
                if wparam as i32 == HOTKEY_ID {
                    ui.on_hotkey();
                }
                return 0;
            }
            keyhook::WM_SHORTCUT => {
                ui.on_hotkey();
                return 0;
            }
            WM_REINDEX => {
                let only: Vec<String> = (lparam != 0)
                    .then(|| *Box::from_raw(lparam as *mut String))
                    .into_iter()
                    .collect();
                ui.start_indexing_of(only.as_slice());
                return 0;
            }
            WM_APPLY_SHARING => {
                let changed = Box::from_raw(lparam as *mut Settings);
                copy_sharing(&changed, &mut ui.settings);
                ui.save_settings();
                ui.update_sharing();
                return 0;
            }
            WM_UPDATE_FOUND => {
                ui.announce_update();
                return 0;
            }
            WM_SHARING_STATUS => {
                if lparam != 0 {
                    let answer = &mut *(lparam as *mut (Vec<String>, Option<String>));
                    *answer = ui.sharing_lines();
                }
                return 0;
            }
            WM_PREVIEW_SETTINGS => {
                let changed = Box::from_raw(lparam as *mut Settings);
                ui.preview_settings(*changed);
                return 0;
            }
            WM_SHOW_FEATHER => {
                ui.summon();
                return 0;
            }
            WM_QUIT_FEATHER => {
                ui.quit();
                return 0;
            }
            tray::WM_TRAY => {
                ui.on_tray(lparam);
                return 0;
            }
            WM_ENDSESSION => {
                if wparam != 0 {
                    ui.save_settings();
                }
                return 0;
            }
            _ if message == taskbar_created() => {
                // Explorer restarted and lost the icon.
                if ui.tray_on {
                    ui.tray_on = tray::add(hwnd);
                }
                return 0;
            }
            WM_SETFOCUS => {
                ui.focused = true;
                ui.caret_on = true;
                ui.invalidate();
                return 0;
            }
            WM_KILLFOCUS => {
                ui.focused = false;
                ui.invalidate();
                return 0;
            }
            WM_ACTIVATE => {
                ui.invalidate();
                // In the background for a while, Feather gives memory back.
                if wparam & 0xffff == 0 {
                    SetTimer(hwnd, TIMER_TRIM, 20_000, None);
                } else {
                    KillTimer(hwnd, TIMER_TRIM);
                }
            }
            WM_SETTINGCHANGE => {
                ui.motion.enabled = anim::system_animations_enabled();
                if ui.settings.dark_mode == DarkMode::FollowSystem {
                    ui.refresh_theme(true);
                }
            }
            WM_SETCURSOR => {
                if (lparam & 0xffff) as u32 == HTCLIENT {
                    let (x, y) = cursor_client(hwnd);
                    let cursor = if ui.drag == Drag::Text || ui.hit_at(x, y) == Hit::Search {
                        IDC_IBEAM
                    } else {
                        IDC_ARROW
                    };
                    SetCursor(LoadCursorW(std::ptr::null_mut(), cursor));
                    return 1;
                }
            }
            WM_MOUSEMOVE => {
                let (x, y) = lparam_point(lparam);
                ui.on_mouse_move(x, y);
                return 0;
            }
            WM_MOUSELEAVE => {
                ui.tracking = false;
                if ui.drag == Drag::None {
                    ui.set_hover(Hit::None);
                }
                return 0;
            }
            WM_LBUTTONDOWN => {
                let (x, y) = lparam_point(lparam);
                ui.on_mouse_down(x, y);
                return 0;
            }
            WM_LBUTTONUP => {
                let (x, y) = lparam_point(lparam);
                ui.on_mouse_up(x, y);
                return 0;
            }
            WM_LBUTTONDBLCLK => {
                let (x, y) = lparam_point(lparam);
                ui.on_double_click(x, y);
                return 0;
            }
            WM_RBUTTONUP => {
                let (x, y) = lparam_point(lparam);
                match ui.hit_at(x, y) {
                    Hit::Row(row) => ui.show_context_menu(row),
                    Hit::Nav(index) => {
                        let category = match ui.nav.get(index) {
                            Some(NavEntry::Item(NavItem::Category(category))) => Some(*category),
                            _ => None,
                        };
                        ui.show_sidebar_menu(category);
                    }
                    _ if x < ui.layout.sidebar.r => ui.show_sidebar_menu(None),
                    _ => {}
                }
                return 0;
            }
            WM_MOUSEWHEEL => {
                ui.on_wheel(((wparam >> 16) & 0xffff) as u16 as i16);
                return 0;
            }
            WM_KEYDOWN | WM_SYSKEYDOWN => {
                if message == WM_KEYDOWN && ui.on_key(wparam as u16) {
                    return 0;
                }
                if message == WM_SYSKEYDOWN && wparam as u16 == VK_F10 && key_down(VK_SHIFT) {
                    ui.on_key(VK_F10);
                    return 0;
                }
            }
            WM_CHAR => {
                ui.on_char(wparam as u16);
                return 0;
            }
            WM_CLOSE => {
                if ui.tray_on && !ui.quitting {
                    ui.dismiss();
                    return 0;
                }
                if ui.entered > 1 {
                    // Quit arrived while Settings, a menu or a picker runs
                    // its own message loop for a handler further up, which
                    // still uses this window. Ask those to end; the window
                    // closes when that handler returns.
                    ui.close_pending = true;
                    form::close_owned_by(hwnd);
                    windows_sys::Win32::UI::WindowsAndMessaging::EndMenu();
                    return 0;
                }
                ui.save_settings();
                DestroyWindow(hwnd);
                return 0;
            }
            WM_DESTROY => {
                ui.unregister_hotkey();
                if ui.tray_on || ui.update_icon {
                    tray::remove(hwnd);
                    ui.tray_on = false;
                    ui.update_icon = false;
                }
                KillTimer(hwnd, TIMER_RESULTS);
                KillTimer(hwnd, TIMER_DEBOUNCE);
                KillTimer(hwnd, TIMER_CARET);
                frames::stop(hwnd);
                PostQuitMessage(0);
                return 0;
            }
            WM_NCDESTROY => {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                // Freed by `Entered` once no call is using it any more.
                ui.destroyed = true;
                return DefWindowProcW(hwnd, message, wparam, lparam);
            }
            _ => {}
        }
    }
    DefWindowProcW(hwnd, message, wparam, lparam)
}

/// One window procedure call using the `Ui` behind the main window.
///
/// A handler that runs a message loop of its own (Settings, a menu, a file
/// picker) still holds the `Ui` while that loop handles more messages for
/// the window. Counting the calls lets closing wait for the outermost one,
/// and the `Ui` is freed only once nothing is using it; freeing it under
/// Settings is what made quitting while Settings was open a double free.
struct Entered(*mut Ui);

impl Entered {
    /// `pointer` must be the live `Ui` stored in the window.
    unsafe fn new(pointer: *mut Ui) -> Self {
        (*pointer).entered += 1;
        Self(pointer)
    }
}

impl Drop for Entered {
    fn drop(&mut self) {
        // SAFETY: the `Ui` is freed only here, by the last call to leave,
        // so it is still alive.
        unsafe {
            let pointer = self.0;
            (*pointer).entered -= 1;
            if (*pointer).entered > 0 {
                return;
            }
            if (*pointer).destroyed {
                drop(Box::from_raw(pointer));
            } else if (*pointer).close_pending {
                (*pointer).close_pending = false;
                PostMessageW((*pointer).hwnd, WM_CLOSE, 0, 0);
            }
        }
    }
}

/// The `TaskbarCreated` message id, looked up once.
fn taskbar_created() -> u32 {
    static ID: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *ID.get_or_init(tray::taskbar_created)
}

/// Brings an already running Feather forward. Returns false when none is
/// running, so this copy should start normally.
/// Removes what this copy of Feather set up for the user outside its own
/// folder: starting at sign-in and its shortcuts. Run by the uninstaller.
/// Entries that start another copy (a second install, a build) are left.
pub fn forget_installation() {
    if startup::enabled() {
        let _ = startup::set(false);
    }
    for place in [shortcuts::Place::StartMenu, shortcuts::Place::Desktop] {
        if shortcuts::points_here(place) {
            let _ = shortcuts::set(place, false);
        }
    }
}

/// Asks every running copy of this same Feather executable to quit, and
/// waits for them, so an uninstaller or upgrade can replace the file. Other
/// copies (a build elsewhere) keep running. True when one was running.
pub fn quit_running_copies() -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, LPARAM, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcessId, OpenProcess, QueryFullProcessImageNameW, TerminateProcess,
        WaitForSingleObject, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{EnumWindows, GetClassNameW};

    unsafe extern "system" fn collect(hwnd: HWND, list: LPARAM) -> i32 {
        let mut class = [0u16; 64];
        let len = unsafe { GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32) };
        if String::from_utf16_lossy(&class[..len.max(0) as usize]) == "FeatherMainWindow" {
            unsafe { (*(list as *mut Vec<HWND>)).push(hwnd) };
        }
        1
    }

    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let exe = exe.to_string_lossy().to_string();
    let mut windows: Vec<HWND> = Vec::new();
    unsafe {
        EnumWindows(Some(collect), &mut windows as *mut Vec<HWND> as LPARAM);
    }
    let mut found = false;
    for hwnd in windows {
        let mut process = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, &mut process) };
        if process == 0 || process == unsafe { GetCurrentProcessId() } {
            continue;
        }
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_TERMINATE,
                0,
                process,
            )
        };
        if handle.is_null() {
            continue;
        }
        let mut name = [0u16; 1024];
        let mut size = name.len() as u32;
        let same = unsafe {
            QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, name.as_mut_ptr(), &mut size)
        } != 0
            && String::from_utf16_lossy(&name[..size as usize]).eq_ignore_ascii_case(&exe);
        if same {
            found = true;
            unsafe {
                PostMessageW(hwnd, WM_QUIT_FEATHER, 0, 0);
                // Quitting takes a moment; a copy stuck in a dialog is ended.
                if WaitForSingleObject(handle, 5000) != WAIT_OBJECT_0 {
                    TerminateProcess(handle, 0);
                    WaitForSingleObject(handle, 2000);
                }
            }
        }
        unsafe { CloseHandle(handle) };
    }
    found
}

pub fn activate_running() -> bool {
    unsafe {
        let hwnd = FindWindowW(w!("FeatherMainWindow"), std::ptr::null());
        if hwnd.is_null() {
            return false;
        }
        let mut process = 0u32;
        GetWindowThreadProcessId(hwnd, &mut process);
        // This copy was just started by the person, so it may hand them
        // the right to take the foreground.
        windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(process);
        PostMessageW(hwnd, WM_SHOW_FEATHER, 0, 0) != 0
    }
}

/// Runs `feather index scan` for `roots` with admin rights (one UAC prompt)
/// and waits for it.
pub fn scan_elevated(
    library: &Path,
    roots: &[String],
    options: &ScanOptions,
) -> std::io::Result<()> {
    indexer::run_elevated(library, roots, options)
}

/// Moves an index finished by the previous run into place. Call before
/// opening the library.
pub fn apply_pending_index(library: &Path) {
    indexer::apply_pending(library);
}

/// The library folder the app should open for the base path `library`.
pub fn active_library(library: &Path) -> PathBuf {
    indexer::active_library(library)
}

/// `feather index serve`: the elevated helper's side of the pipe.
pub fn serve_helper(pipe: &str) -> std::io::Result<()> {
    helper::serve(pipe)
}

/// Runs the app. `library` is where the in-app indexer writes; first-run
/// setup appears until it has been completed once.
/// Runs the app. With `background`, it starts hidden in the tray (as it
/// does at sign-in).
pub fn run_ui(index: Arc<IndexSet>, library: PathBuf, background: bool) -> i32 {
    let loaded = settings::load();
    for warning in &loaded.warnings {
        eprintln!("settings: {warning}");
    }
    let hwnd = match create_window(index, !background, loaded.settings, true) {
        Ok(hwnd) => hwnd,
        Err(error) => {
            eprintln!("{error}");
            return 1;
        }
    };
    unsafe {
        if let Some(ui) = ui_ref(hwnd) {
            ui.register_hotkey();
            ui.update_tray();
            ui.check_for_update();
            ui.library = Some(library);
            ui.update_sharing();
            if !ui.settings.setup_done {
                windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW(hwnd, WM_RUN_SETUP, 0, 0);
            } else if !ui.settings.index_roots.is_empty() {
                // Catch up on whatever changed while Feather was closed, and
                // keep up from now on: from the change journal where the
                // index recorded its place in it, by reading again elsewhere.
                ui.start_live_or_index();
            }
        }
    }
    let mut message = MSG::default();
    while frames::next_message(&mut message) {
        if translate_accelerator(hwnd, &message) {
            continue;
        }
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    0
}

pub fn run_screenshot(index: Arc<IndexSet>, path: &Path, query: &str) -> Result<(), String> {
    let hwnd = create_window(index, true, Settings::default(), false)?;
    unsafe {
        if let Some(ui) = ui_ref(hwnd) {
            ui.set_query(query);
        }
    }
    pump_until_idle(hwnd, Duration::from_secs(5));
    unsafe {
        if let Some(ui) = ui_ref(hwnd) {
            ui.capture(path)?;
        }
    }
    pump_messages(hwnd);
    unsafe {
        DestroyWindow(hwnd);
    }
    pump_messages(std::ptr::null_mut());
    Ok(())
}

/// Copies every sharing setting from one settings value to another.
fn copy_sharing(from: &Settings, to: &mut Settings) {
    to.http_mode = from.http_mode;
    to.ftp_mode = from.ftp_mode;
    to.share_access = from.share_access;
    to.tunnel = from.tunnel;
    to.http_port = from.http_port;
    to.ftp_port = from.ftp_port;
    to.share_user = from.share_user.clone();
    to.share_password = from.share_password.clone();
    to.share_password_saved = from.share_password_saved.clone();
    to.share_folders = from.share_folders.clone();
    to.ftp_plain = from.ftp_plain;
    to.tunnel_token = from.tunnel_token.clone();
    to.share_e2e = from.share_e2e;
    to.tunnel_domain = from.tunnel_domain.clone();
}

/// Prints the memory the window uses after each step, for `--memory`.
pub fn run_memory_report(index: Arc<IndexSet>) -> Result<(), String> {
    let line = |label: &str| {
        let usage = memory::usage();
        println!(
            "{label:<34} memory {:>7.1} MB   private {:>7.1} MB   working set {:>7.1} MB",
            memory::megabytes(usage.private_working_set),
            memory::megabytes(usage.private_bytes),
            memory::megabytes(usage.working_set),
        );
    };
    println!("{} items in the index", index.item_count());
    line("index open, no window");
    let hwnd = create_window(index, true, Settings::default(), false)?;
    pump_until_idle(hwnd, Duration::from_secs(5));
    line("window open");
    let steps: [(&str, &str, Option<usize>); 6] = [
        ("everything (empty search)", "", None),
        ("typed \"a\"", "a", None),
        ("typed \"report\"", "report", None),
        ("sorted by size", "report", Some(sort::COLUMN_SIZE)),
        ("nothing found", "zzzzzz-no-such-file", None),
        ("everything again", "", None),
    ];
    for (label, query, sort_column) in steps {
        unsafe {
            if let Some(ui) = ui_ref(hwnd) {
                ui.set_query(query);
            }
        }
        pump_until_idle(hwnd, Duration::from_secs(120));
        unsafe {
            if let Some(ui) = ui_ref(hwnd) {
                if let Some(column) = sort_column {
                    ui.sort_by(column, false);
                }
                ui.invalidate();
            }
        }
        pump_messages(hwnd);
        let rows = unsafe { ui_ref(hwnd).map(|ui| ui.result_count()).unwrap_or(0) };
        line(&format!("{label} ({rows} rows)"));
    }
    memory::trim();
    line("after giving memory back");
    unsafe {
        DestroyWindow(hwnd);
    }
    Ok(())
}

pub fn run_scenarios(index: Arc<IndexSet>, screenshots: &Path) -> Result<(), String> {
    std::fs::create_dir_all(screenshots).map_err(|error| error.to_string())?;
    let hwnd = create_window(index, true, Settings::default(), false)?;
    let scenarios = [
        ("01-all", "", true),
        ("02-match", "file0000005", true),
        ("03-no-results", "zzzzzz-no-such-file", false),
    ];
    let mut report = String::new();

    for (name, query, expect_results) in scenarios {
        unsafe {
            if let Some(ui) = ui_ref(hwnd) {
                ui.set_query(query);
            }
        }
        pump_until_idle(hwnd, Duration::from_secs(5));
        let (count, total) = unsafe {
            ui_ref(hwnd)
                .map(|ui| (ui.result_count(), ui.total()))
                .unwrap_or((0, 0))
        };
        if expect_results && count == 0 {
            return Err(format!("scenario {name}: expected results, got none"));
        }
        if !expect_results && count != 0 {
            return Err(format!("scenario {name}: expected no results, got {count}"));
        }
        let path = screenshots.join(format!("{name}.png"));
        unsafe {
            if let Some(ui) = ui_ref(hwnd) {
                ui.capture(&path)?;
            }
        }
        report.push_str(&format!(
            "{name}: rows={count} total={total} screenshot={}\n",
            path.display()
        ));
    }

    unsafe {
        if let Some(ui) = ui_ref(hwnd) {
            ui.set_query("ext:pdf");
            ui.sort_by(sort::COLUMN_SIZE, false);
        }
    }
    pump_until_idle(hwnd, Duration::from_secs(5));
    let (count, total) = unsafe {
        ui_ref(hwnd)
            .map(|ui| (ui.result_count(), ui.total()))
            .unwrap_or((0, 0))
    };
    if count == 0 {
        return Err("scenario 04-sorted: expected results, got none".into());
    }
    let sizes = unsafe {
        ui_ref(hwnd)
            .map(|ui| ui.leading_sizes(10))
            .unwrap_or_default()
    };
    if sizes.windows(2).any(|pair| pair[0] < pair[1]) {
        return Err("scenario 04-sorted: size order is not descending".into());
    }
    let path = screenshots.join("04-sorted.png");
    unsafe {
        if let Some(ui) = ui_ref(hwnd) {
            ui.capture(&path)?;
        }
    }
    report.push_str(&format!(
        "04-sorted: rows={count} total={total} screenshot={}\n",
        path.display()
    ));

    unsafe {
        if let Some(ui) = ui_ref(hwnd) {
            ui.set_query("type:folder");
        }
    }
    pump_until_idle(hwnd, Duration::from_secs(5));
    unsafe {
        if let Some(ui) = ui_ref(hwnd) {
            ui.sort_by(sort::COLUMN_PATH, true);
        }
    }
    let (count, total) = unsafe {
        ui_ref(hwnd)
            .map(|ui| (ui.result_count(), ui.total()))
            .unwrap_or((0, 0))
    };
    if count == 0 {
        return Err("scenario 05-path: expected results, got none".into());
    }
    let folders = unsafe {
        ui_ref(hwnd)
            .map(|ui| ui.leading_folders(10))
            .unwrap_or_default()
    };
    if folders.windows(2).any(|pair| pair[0] > pair[1]) {
        return Err("scenario 05-path: path order is not ascending".into());
    }
    let path = screenshots.join("05-path.png");
    unsafe {
        if let Some(ui) = ui_ref(hwnd) {
            ui.capture(&path)?;
        }
    }
    report.push_str(&format!(
        "05-path: rows={count} total={total} screenshot={}\n",
        path.display()
    ));

    // Folders sorted by their totals, the biggest selected so the details
    // pane shows its size and item count.
    unsafe {
        if let Some(ui) = ui_ref(hwnd) {
            ui.sort_by(sort::COLUMN_SIZE, false);
            ui.select_row(0);
        }
    }
    pump_until_idle(hwnd, Duration::from_secs(5));
    let (count, sizes, size_cell, items) = unsafe {
        ui_ref(hwnd)
            .map(|ui| {
                let items = ui
                    .model
                    .ids
                    .get(0)
                    .and_then(|entry| ui.index.folder_totals(entry))
                    .map(|(_, items)| item_count_text(items));
                (
                    ui.result_count(),
                    ui.leading_sizes(10),
                    ui.cell_text(0, 2).unwrap_or_default(),
                    items,
                )
            })
            .unwrap_or_default()
    };
    if count == 0 {
        return Err("scenario 38-folder-sizes: expected folders, got none".into());
    }
    if sizes.windows(2).any(|pair| pair[0] < pair[1]) || sizes.first() == Some(&0) {
        return Err(format!(
            "scenario 38-folder-sizes: folder totals missing or out of order: {sizes:?}"
        ));
    }
    let Some(items) = items else {
        return Err("scenario 38-folder-sizes: no item count for the folder".into());
    };
    if size_cell.is_empty() {
        return Err("scenario 38-folder-sizes: the Size column is empty".into());
    }
    let path = screenshots.join("38-folder-sizes.png");
    unsafe {
        if let Some(ui) = ui_ref(hwnd) {
            ui.capture(&path)?;
        }
    }
    report.push_str(&format!(
        "38-folder-sizes: rows={count} size={size_cell} items={items} screenshot={}\n",
        path.display()
    ));

    let sample_filters = FilterSet {
        kind: KindFilter::Files,
        extensions: vec!["pdf".into(), "docx".into()],
        min_size: Some(1024 * 1024),
        max_size: None,
        match_mode: MatchMode::StartsWith,
        modified: crate::filters::Modified::Range {
            from: "2026-01-01".into(),
            to: "2026-03-31".into(),
        },
    };
    let path = screenshots.join("06-filters.png");
    let look = unsafe { ui_ref(hwnd).map(|ui| ui.look()) }.ok_or("window closed")?;
    crate::filters::capture(hwnd, sample_filters.clone(), look, &path)?;
    report.push_str(&format!("06-filters: screenshot={}\n", path.display()));
    // The date section, at the bottom, with custom dates showing.
    let path = screenshots.join("06b-filters-dates.png");
    crate::filters::capture_at(hwnd, sample_filters, look, &path, 10_000.0)?;
    report.push_str(&format!(
        "06b-filters-dates: screenshot={}\n",
        path.display()
    ));

    unsafe {
        if let Some(ui) = ui_ref(hwnd) {
            ui.set_filters(FilterSet {
                kind: KindFilter::Files,
                extensions: vec!["pdf".into()],
                min_size: None,
                max_size: None,
                match_mode: MatchMode::Anywhere,
                modified: crate::filters::Modified::Any,
            });
            ui.set_query("file");
        }
    }
    pump_until_idle(hwnd, Duration::from_secs(5));
    let (count, total, names) = unsafe {
        ui_ref(hwnd)
            .map(|ui| (ui.result_count(), ui.total(), ui.leading_names(20)))
            .unwrap_or((0, 0, Vec::new()))
    };
    if count == 0 {
        return Err("scenario 07-filtered: expected results, got none".into());
    }
    if names
        .iter()
        .any(|name| !name.to_ascii_lowercase().ends_with(".pdf"))
    {
        return Err("scenario 07-filtered: a row does not match the PDF filter".into());
    }
    let path = screenshots.join("07-filtered.png");
    unsafe {
        if let Some(ui) = ui_ref(hwnd) {
            ui.capture(&path)?;
        }
    }
    report.push_str(&format!(
        "07-filtered: rows={count} total={total} screenshot={}\n",
        path.display()
    ));

    // A date filter keeps some files and drops others: the synthetic index
    // dates its entries a whole number of days back from when it was built.
    let totals = |filters: FilterSet| {
        unsafe {
            if let Some(ui) = ui_ref(hwnd) {
                ui.set_filters(filters);
                ui.set_query("file");
            }
        }
        pump_until_idle(hwnd, Duration::from_secs(5));
        unsafe { ui_ref(hwnd).map(|ui| (ui.result_count(), ui.total())) }.unwrap_or((0, 0))
    };
    let (_, all_files) = totals(FilterSet::default());
    let (count, total) = totals(FilterSet {
        modified: crate::filters::Modified::Today,
        ..FilterSet::default()
    });
    if count == 0 || total >= all_files {
        return Err(format!(
            "scenario 07b-modified: expected some of {all_files} files, got {total}"
        ));
    }
    let path = screenshots.join("07b-modified.png");
    unsafe {
        if let Some(ui) = ui_ref(hwnd) {
            ui.capture(&path)?;
        }
    }
    report.push_str(&format!(
        "07b-modified: rows={count} total={total} of {all_files} screenshot={}\n",
        path.display()
    ));
    // Back to the PDF filter of 07 for the scenarios that follow.
    totals(FilterSet {
        kind: KindFilter::Files,
        extensions: vec!["pdf".into()],
        ..FilterSet::default()
    });

    let path = screenshots.join("08-settings.png");
    settings::capture(hwnd, Settings::default(), look, &path)?;
    report.push_str(&format!("08-settings: screenshot={}\n", path.display()));

    let path = screenshots.join("09-help.png");
    help::capture(hwnd, look, &path)?;
    report.push_str(&format!("09-help: screenshot={}\n", path.display()));

    // The dialogs in the other theme.
    let other = form::Look {
        dark: !look.dark,
        ..look
    };
    let suffix = if other.dark { "dark" } else { "light" };
    let path = screenshots.join(format!("16-settings-{suffix}.png"));
    settings::capture(hwnd, Settings::default(), other, &path)?;
    report.push_str(&format!(
        "16-settings-{suffix}: screenshot={}\n",
        path.display()
    ));
    let path = screenshots.join(format!("17-filters-{suffix}.png"));
    crate::filters::capture(hwnd, FilterSet::default(), other, &path)?;
    report.push_str(&format!(
        "17-filters-{suffix}: screenshot={}\n",
        path.display()
    ));
    let path = screenshots.join(format!("18-help-{suffix}.png"));
    help::capture(hwnd, other, &path)?;
    report.push_str(&format!(
        "18-help-{suffix}: screenshot={}\n",
        path.display()
    ));
    let start = setup::SetupStart {
        hotkey: hotkey::DEFAULT.to_string(),
        start_at_login: true,
        close_to_tray: true,
    };
    for (step, name) in ["welcome", "places", "look", "shortcut", "ready"]
        .iter()
        .enumerate()
    {
        let path = screenshots.join(format!("19-setup-{}-{name}.png", step + 1));
        setup::capture(hwnd, look, &start, step, &path)?;
        report.push_str(&format!(
            "19-setup-{}-{name}: screenshot={}\n",
            step + 1,
            path.display()
        ));
    }
    let path = screenshots.join("21-settings-places.png");
    let places = Settings {
        index_roots: vec![r"C:\".into(), r"D:\Projects".into()],
        ..Settings::default()
    };
    settings::capture_page(hwnd, places, look, &path, settings::PAGE_INDEXES)?;
    report.push_str(&format!(
        "21-settings-places: screenshot={}\n",
        path.display()
    ));
    let path = screenshots.join("25-shortcut-recording.png");
    // Left Ctrl, left Shift, and right Shift held together.
    settings::capture_recording(hwnd, Settings::default(), look, &path, &[0xa2, 0xa0, 0xa1])?;
    report.push_str(&format!(
        "25-shortcut-recording: screenshot={}\n",
        path.display()
    ));
    let path = screenshots.join("29-settings-tab-drag.png");
    let order = settings::capture_tab_drag(hwnd, Settings::default(), look, &path)?;
    report.push_str(&format!(
        "29-settings-tab-drag: order={} screenshot={}\n",
        order.join(","),
        path.display()
    ));
    // Clicks land where aimed, even with the scroll left past the end.
    for scroll in [900.0, 5000.0] {
        let probe = settings::probe_password_click(hwnd, Settings::default(), look, scroll)?;
        if !probe.contains("switches changed: []") || !probe.contains("Field(") {
            return Err(format!("scenario click-probe: {probe}"));
        }
        report.push_str(&format!(
            "click-probe: {probe}
"
        ));
    }
    for (name, qr, scroll) in [
        ("36-settings-sharing-tip", false, 0.0),
        ("37-settings-sharing-qr", true, 0.0),
    ] {
        let path = screenshots.join(format!("{name}.png"));
        let network = Settings {
            share_access: crate::settings::ShareAccess::Network,
            ..Settings::default()
        };
        settings::capture_sharing_open(hwnd, network, look, &path, qr, scroll)?;
        report.push_str(&format!("{name}: screenshot={}\n", path.display()));
    }
    let path = screenshots.join("34-settings-color-picker.png");
    settings::capture_color_picker(hwnd, Settings::default(), look, &path)?;
    report.push_str(&format!(
        "34-settings-color-picker: screenshot={}\n",
        path.display()
    ));
    for (name, page, scroll) in [
        ("27-settings-sidebar", settings::PAGE_SIDEBAR, 0.0),
        ("28-settings-search", settings::PAGE_INDEXES, 520.0),
        ("30-settings-theme", settings::PAGE_THEME, 0.0),
        ("31-settings-appearance", settings::PAGE_APPEARANCE, 0.0),
        ("32-settings-sharing", settings::PAGE_SHARING, 0.0),
        ("33-settings-sharing-more", settings::PAGE_SHARING, 640.0),
        ("35-settings-sharing-end", settings::PAGE_SHARING, 5000.0),
    ] {
        let path = screenshots.join(format!("{name}.png"));
        let mut sample = Settings {
            results_percent: 35,
            custom_theme: true,
            share_access: crate::settings::ShareAccess::Network,
            ..Settings::default()
        };
        sample.sidebar_tabs[1].1 = false;
        settings::capture_page_at(hwnd, sample, look, &path, page, scroll)?;
        report.push_str(&format!("{name}: screenshot={}\n", path.display()));
    }

    unsafe {
        if let Some(ui) = ui_ref(hwnd) {
            ui.set_scope(None);
            ui.apply_category(Category::Images);
        }
    }
    pump_until_idle(hwnd, Duration::from_secs(5));
    let (count, total, names) = unsafe {
        ui_ref(hwnd)
            .map(|ui| (ui.result_count(), ui.total(), ui.leading_names(20)))
            .unwrap_or((0, 0, Vec::new()))
    };
    if count == 0 {
        return Err("scenario 10-quick-filter: expected results, got none".into());
    }
    let image_extensions = Category::Images.extensions();
    if names.iter().any(|name| {
        let lower = name.to_ascii_lowercase();
        !image_extensions
            .iter()
            .any(|extension| lower.ends_with(&format!(".{extension}")))
    }) {
        return Err("scenario 10-quick-filter: a row is not an image".into());
    }
    let path = screenshots.join("10-quick-filter.png");
    unsafe {
        if let Some(ui) = ui_ref(hwnd) {
            ui.capture(&path)?;
        }
    }
    report.push_str(&format!(
        "10-quick-filter: rows={count} total={total} screenshot={}\n",
        path.display()
    ));

    let scope = r"C:\FeatherBench\dir0001";
    unsafe {
        if let Some(ui) = ui_ref(hwnd) {
            ui.apply_category(Category::Everything);
            ui.set_scope(Some(scope.to_string()));
            ui.set_query("");
        }
    }
    pump_until_idle(hwnd, Duration::from_secs(5));
    let (count, total, paths) = unsafe {
        ui_ref(hwnd)
            .map(|ui| (ui.result_count(), ui.total(), ui.leading_paths(20)))
            .unwrap_or((0, 0, Vec::new()))
    };
    if count == 0 {
        return Err("scenario 11-scope: expected results, got none".into());
    }
    if paths
        .iter()
        .any(|path| !feather_core::path_within(path, scope))
    {
        return Err("scenario 11-scope: a row is outside the chosen folder".into());
    }
    let path = screenshots.join("11-scope.png");
    unsafe {
        if let Some(ui) = ui_ref(hwnd) {
            ui.capture(&path)?;
        }
    }
    report.push_str(&format!(
        "11-scope: rows={count} total={total} screenshot={}\n",
        path.display()
    ));

    // Both themes, whatever the Windows app theme is on this machine.
    for (name, mode) in [("12-light", DarkMode::Light), ("13-dark", DarkMode::Full)] {
        let path = screenshots.join(format!("{name}.png"));
        unsafe {
            if let Some(ui) = ui_ref(hwnd) {
                ui.set_query("file0003");
                ui.settings.dark_mode = mode;
                ui.refresh_theme(true);
            }
        }
        pump_until_idle(hwnd, Duration::from_secs(5));
        unsafe {
            if let Some(ui) = ui_ref(hwnd) {
                if ui.palette.dark != (mode == DarkMode::Full) {
                    return Err(format!("scenario {name}: theme did not switch"));
                }
                ui.capture(&path)?;
            }
        }
        report.push_str(&format!("{name}: screenshot={}\n", path.display()));
    }

    // Hover highlights must stay inside their own column.
    for (name, button) in [
        ("14-hover-filters", Button::Filters),
        ("15-hover-settings", Button::Settings),
    ] {
        let path = screenshots.join(format!("{name}.png"));
        unsafe {
            if let Some(ui) = ui_ref(hwnd) {
                ui.set_hover(Hit::Button(button));
                ui.capture(&path)?;
                ui.set_hover(Hit::None);
            }
        }
        report.push_str(&format!("{name}: screenshot={}\n", path.display()));
    }

    // Typing a folder's path lists what is inside it.
    let browse = r"C:\FeatherBench\dir0001\";
    unsafe {
        if let Some(ui) = ui_ref(hwnd) {
            ui.set_query(browse);
        }
    }
    pump_until_idle(hwnd, Duration::from_secs(5));
    let (count, paths) = unsafe {
        ui_ref(hwnd)
            .map(|ui| (ui.result_count(), ui.leading_paths(50)))
            .unwrap_or((0, Vec::new()))
    };
    if count == 0
        || paths
            .iter()
            .any(|path| !parent_path(path).eq_ignore_ascii_case(browse.trim_end_matches('\\')))
    {
        return Err(format!(
            "scenario 22-browse: expected the folder's contents, got {count} rows"
        ));
    }
    let path = screenshots.join("22-browse.png");
    unsafe {
        if let Some(ui) = ui_ref(hwnd) {
            ui.capture(&path)?;
        }
    }
    report.push_str(&format!(
        "22-browse: rows={count} screenshot={}\n",
        path.display()
    ));

    // The folded sidebar, with a tab's name beside the pointer, then a tab
    // lifted mid-drag in the open sidebar.
    unsafe {
        if let Some(ui) = ui_ref(hwnd) {
            ui.set_query("file0003");
            ui.toggle_sidebar();
            ui.sidebar_w.settle();
            ui.layout();
            ui.set_hover(Hit::Nav(1));
            ui.motion.settle();
            let path = screenshots.join("23-sidebar-folded.png");
            ui.capture(&path)?;
            if ui.layout.sidebar.r != layout::SIDEBAR_COLLAPSED_W {
                return Err("scenario 23-sidebar-folded: sidebar did not fold".into());
            }
            report.push_str(&format!(
                "23-sidebar-folded: screenshot={}\n",
                path.display()
            ));
            ui.set_hover(Hit::None);
            // Halfway through unfolding.
            ui.toggle_sidebar();
            let middle = (layout::SIDEBAR_W + layout::SIDEBAR_COLLAPSED_W) / 2.0;
            ui.sidebar_w.jump(middle);
            ui.layout();
            let path = screenshots.join("26-sidebar-folding.png");
            ui.capture(&path)?;
            report.push_str(&format!(
                "26-sidebar-folding: screenshot={}\n",
                path.display()
            ));
            ui.sidebar_w.jump(layout::SIDEBAR_W);
            ui.layout();
            let rect = ui.nav_rects[1];
            let grab = 12.0;
            let y = rect.t + grab + 3.0 * (NAV_H + 2.0) + 10.0;
            ui.drag = Drag::Nav {
                index: 1,
                grab,
                start_y: rect.t + grab,
                y,
                moving: true,
            };
            let slot = ui.nav_drop_slot(1, y - grab);
            ui.shift_tabs(1, slot);
            for shift in &mut ui.nav_shift {
                shift.settle();
            }
            let path = screenshots.join("24-sidebar-drag.png");
            ui.capture(&path)?;
            ui.drag = Drag::None;
            for shift in &mut ui.nav_shift {
                shift.jump(0.0);
            }
            if slot != 4 {
                return Err(format!(
                    "scenario 24-sidebar-drag: dropped at {slot}, expected 4"
                ));
            }
            report.push_str(&format!(
                "24-sidebar-drag: slot={slot} screenshot={}\n",
                path.display()
            ));
        }
    }

    // A full list of rows is the most expensive frame.
    unsafe {
        if let Some(ui) = ui_ref(hwnd) {
            ui.set_query("");
        }
    }
    pump_until_idle(hwnd, Duration::from_secs(5));
    let frame_ms = unsafe { ui_ref(hwnd).map(|ui| ui.time_frames(60)) }.unwrap_or(0.0);
    report.push_str(&format!(
        "frame: {frame_ms:.2} ms to draw and present one frame\n"
    ));

    // Index a small real folder from inside the app, swap the new index in
    // while the old one is open, and find a file in it.
    let tree = std::env::current_dir()
        .map_err(|error| error.to_string())?
        .join(screenshots)
        .join("index-tree");
    let library = tree.with_file_name("index-library");
    let _ = std::fs::remove_dir_all(&tree);
    let _ = std::fs::remove_dir_all(&library);
    std::fs::create_dir_all(tree.join("Projects")).map_err(|error| error.to_string())?;
    std::fs::write(tree.join("Projects").join("feather-setup-check.txt"), b"x")
        .map_err(|error| error.to_string())?;
    unsafe {
        if let Some(ui) = ui_ref(hwnd) {
            ui.library = Some(library.clone());
            ui.allow_elevation = false;
            ui.settings.index_roots = vec![tree.to_string_lossy().to_string()];
            ui.set_scope(None);
            ui.set_filters(FilterSet::default());
            ui.start_indexing();
        }
    }
    let started = Instant::now();
    loop {
        pump_messages(hwnd);
        let busy = unsafe {
            ui_ref(hwnd)
                .map(|ui| ui.indexer.is_some() || ui.swap.is_some())
                .unwrap_or(false)
        };
        if !busy {
            break;
        }
        if started.elapsed() > Duration::from_secs(30) {
            return Err("scenario 20-indexed: indexing did not finish".into());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    unsafe {
        if let Some(ui) = ui_ref(hwnd) {
            ui.set_query("feather-setup-check");
        }
    }
    pump_until_idle(hwnd, Duration::from_secs(5));
    let (count, items) = unsafe {
        ui_ref(hwnd)
            .map(|ui| (ui.result_count(), ui.index.item_count()))
            .unwrap_or((0, 0))
    };
    if count != 1 || items != 2 {
        return Err(format!(
            "scenario 20-indexed: expected 1 match in 2 items, got {count} in {items}"
        ));
    }
    if !indexer::active_library(&library)
        .join(feather_core::scan::library_dir_name(&tree))
        .is_dir()
    {
        return Err("scenario 20-indexed: the new index was not made active".into());
    }
    let path = screenshots.join("20-indexed.png");
    unsafe {
        if let Some(ui) = ui_ref(hwnd) {
            ui.capture(&path)?;
        }
    }
    report.push_str(&format!(
        "20-indexed: rows={count} items={items} screenshot={}\n",
        path.display()
    ));

    std::fs::write(screenshots.join("report.txt"), &report).map_err(|error| error.to_string())?;
    println!("{report}");
    unsafe {
        DestroyWindow(hwnd);
    }
    pump_messages(std::ptr::null_mut());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::{IsWindow, WM_QUIT};

    /// Quitting while Settings is open (`feather quit`, or Quit in the tray
    /// menu) used to destroy the window, and free its state, under the
    /// Settings call that was still using it: a double free once Settings
    /// returned. Now the window closes after Settings does.
    #[test]
    #[ignore = "opens a Settings window on the desktop; run with --ignored"]
    fn quitting_while_settings_is_open_waits_for_settings() {
        let base = std::env::temp_dir().join(format!("feather-quit-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        // Nothing here may touch the person's own settings or pictures.
        std::env::set_var("APPDATA", &base);
        std::env::set_var("LOCALAPPDATA", &base);

        let index = Arc::new(IndexSet::empty());
        let hwnd = create_window(index, false, Settings::default(), false).unwrap();
        let rect = unsafe { ui_ref(hwnd) }
            .and_then(|ui| ui.layout.button(Button::Settings))
            .expect("the Settings button");
        let (x, y) = ((rect.l + rect.r) / 2.0, (rect.t + rect.b) / 2.0);
        let hit = unsafe { ui_ref(hwnd) }.map(|ui| ui.hit_at(x, y));
        assert!(hit == Some(Hit::Button(Button::Settings)));
        let point = (x as isize & 0xffff) | ((y as isize & 0xffff) << 16);
        unsafe {
            PostMessageW(hwnd, WM_LBUTTONDOWN, 0, point);
            PostMessageW(hwnd, WM_LBUTTONUP, 0, point);
            // Handled by the loop Settings runs while it is open.
            PostMessageW(hwnd, WM_QUIT_FEATHER, 0, 0);
        }
        let started = Instant::now();
        let mut message = MSG::default();
        while unsafe { IsWindow(hwnd) } != 0 {
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "the window never closed"
            );
            if unsafe { PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) } > 0 {
                if message.message != WM_QUIT {
                    unsafe {
                        TranslateMessage(&message);
                        DispatchMessageW(&message);
                    }
                }
            } else {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        let _ = std::fs::remove_dir_all(&base);
    }
}
