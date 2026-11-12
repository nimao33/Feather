//! Custom-drawn dialog windows: Filters, Settings, and Help.
//!
//! A dialog describes its content as a [`Form`]: section headings, rows with
//! a switch, a segmented choice, or a text field, check lists, folder lists,
//! and footer buttons. This module lays the form out, paints it with the same
//! Direct2D renderer, palette, and backdrop as the main window, and handles
//! the mouse and keyboard. The dialog itself only reads and writes values by
//! id and decides what the buttons do.
//!
//! ```text
//! +--------------------------------------------------+
//! | Heading                                          |
//! | +----------------------------------------------+ |
//! | | Label                              [switch]  | |
//! | | Detail text                                  | |
//! | |----------------------------------------------| |
//! | | Label                  [ One | Two | Three ] | |
//! | +----------------------------------------------+ |
//! |--------------------------------------------------|
//! | error text                                       |
//! | [Reset]                      [Cancel] [Primary]  |
//! +--------------------------------------------------+
//! ```

use std::any::Any;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Once;
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct2D::ID2D1RenderTarget;
use windows_sys::core::w;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, EndPaint, InvalidateRect, UpdateWindow, PAINTSTRUCT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, GetKeyState, ReleaseCapture, SetCapture, SetFocus, TrackMouseEvent, TME_LEAVE,
    TRACKMOUSEEVENT, VK_BACK, VK_CONTROL, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_HOME, VK_LEFT,
    VK_NEXT, VK_PRIOR, VK_RETURN, VK_RIGHT, VK_SHIFT, VK_SPACE, VK_TAB, VK_UP,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetCaretBlinkTime, GetSystemMetrics, GetWindowLongPtrW, GetWindowRect, IsWindow, KillTimer,
    LoadCursorW, PeekMessageW, PostMessageW, RegisterClassW, SetCursor, SetForegroundWindow,
    SetTimer, SetWindowLongPtrW, SetWindowPos, ShowWindow, TranslateMessage, CS_DBLCLKS,
    GWLP_USERDATA, HTCAPTION, HTCLIENT, IDC_ARROW, IDC_IBEAM, MSG, NCCALCSIZE_PARAMS, PM_REMOVE,
    SM_CXSCREEN, SM_CYSCREEN, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SWP_NOZORDER, SW_HIDE, SW_SHOW, WM_APP, WM_CHAR, WM_CLOSE, WM_ERASEBKGND, WM_KEYDOWN,
    WM_KILLFOCUS, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL,
    WM_NCCALCSIZE, WM_NCDESTROY, WM_NCHITTEST, WM_PAINT, WM_SETCURSOR, WM_SETFOCUS, WM_SIZE,
    WM_TIMER, WNDCLASSW, WS_CAPTION, WS_EX_DLGMODALFRAME, WS_POPUP, WS_SYSMENU,
};

use super::anim::{self, Fades, Tween};
use super::canvas::{self, Canvas};
use super::color_space::{self, Model};
use super::context_menu;
use super::frames;
use super::gfx::{Color, Font, Gfx, Painter, Rect};
use super::glyphs::{self, PageIcon};
use super::hotkey;
use super::icons::{self, IconCache, IconSize};
use super::keyhook;
use super::paint;
use super::picker;
use super::textfield::TextField;
use super::theme::{self, Palette};
use super::win::{client_size, to_wide};
use crate::filters::Category;
use crate::settings::{Accent, Material};

const PAD: f32 = 24.0;
const ROW_X: f32 = 16.0;
const ROW_Y: f32 = 12.0;
const FOOTER_H: f32 = 66.0;
const ERROR_H: f32 = 26.0;
const FIELD_H: f32 = 34.0;
const SEG_H: f32 = 32.0;
const ITEM_H: f32 = 36.0;
const SWITCH_W: f32 = 40.0;
const SWITCH_H: f32 = 22.0;
const INLINE_FIELD_W: f32 = 200.0;
const SLIDER_W: f32 = 260.0;
const SLIDER_H: f32 = 42.0;
const BUTTON_H: f32 = 34.0;
/// The strip along the top that drags the window; Windows draws the close
/// button at its right end.
const TITLE_H: f32 = 44.0;
/// The page list on the left of a dialog with pages.
const RAIL_W: f32 = 208.0;
const RAIL_ITEM_H: f32 = 34.0;

const TIMER_CARET: usize = 1;
/// The dialog's once-a-second tick.
const TIMER_TICK: usize = 2;
/// Fires once when a resting pointer's tip is due.
const TIMER_TIP: usize = 3;
const WM_MOUSELEAVE: u32 = 0x02a3;
const WM_FORM_DONE: u32 = WM_APP + 2;

/// A widget's name, used by the dialog to read and write its value.
pub type Id = &'static str;

/// The colors and backdrop a dialog uses. Settings updates it live while
/// the Appearance choices change.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Look {
    pub dark: bool,
    pub accent: Accent,
    pub material: Material,
}

impl Look {
    fn palette(&self) -> Palette {
        Palette::new(self.dark, self.accent, self.material)
    }
}

/// One option of a segmented choice. A swatch shows a color dot.
pub struct Choice {
    pub label: String,
    pub swatch: Option<Color>,
}

pub fn option(label: &str) -> Choice {
    Choice {
        label: label.to_string(),
        swatch: None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ButtonStyle {
    /// Filled with the accent color, at the far right. Enter presses it.
    Primary,
    /// Outlined, to the left of the primary button.
    Secondary,
    /// Text only, at the left edge (Reset, Restore defaults).
    Quiet,
}

pub enum Action {
    Stay,
    Accept,
    Cancel,
}

pub trait FormDialog {
    /// A footer button was pressed.
    fn on_button(&mut self, form: &mut Form, id: Id) -> Action;
    /// A value changed.
    fn on_change(&mut self, _form: &Form, _id: Id) {}
    /// Another page was chosen in the page list.
    fn on_page(&mut self, _form: &mut Form, _page: usize) {}
    /// A button inside the page (not the footer) was pressed.
    fn on_action(&mut self, _form: &mut Form, _id: Id) {}
    /// The round-arrow button on one item of a list was pressed.
    fn on_item_action(&mut self, _form: &mut Form, _id: Id, _item: usize) {}
    /// Called every second while the window is open; true repaints.
    fn on_tick(&mut self, _form: &mut Form) -> bool {
        false
    }
    /// The look the dialog should have for its current values. Settings uses
    /// this to preview the Appearance choices live.
    fn look(&self, _form: &Form) -> Option<Look> {
        None
    }
    fn take_outcome(&mut self) -> Option<Box<dyn Any>> {
        None
    }
}

enum Kind {
    Heading,
    Paragraph,
    Switch {
        on: bool,
        knob: Tween,
    },
    Segments {
        options: Vec<Choice>,
        selected: usize,
        pill: Tween,
    },
    Field {
        field: TextField,
        cue: String,
        scroll: f32,
        wide: bool,
        /// Records a key combination instead of taking typed text.
        shortcut: bool,
        /// Digits only, grouped by thousands as they are typed.
        numeric: bool,
    },
    Checks {
        items: Vec<(String, bool)>,
    },
    Paths {
        items: Vec<String>,
        empty: String,
        /// Rows get a round-arrow button.
        refresh: bool,
    },
    Syntax {
        rows: Vec<(String, String)>,
    },
    /// A list that can be reordered by dragging, each entry with an icon
    /// and a check box.
    Order {
        items: Vec<(String, Category, bool)>,
    },
    /// Drives as cards with their Explorer icon and free space.
    Drives {
        items: Vec<DriveCard>,
    },
    /// A value from 1 to 100, dragged along a track. `caption` words the
    /// value shown above the track.
    Slider {
        value: u32,
        caption: Box<dyn Fn(u32) -> String>,
    },
    /// A button in the page, beside its label.
    Action {
        caption: String,
    },
    /// A button that opens a popup with a QR code of `text`.
    Qr {
        text: String,
        size: usize,
        modules: Vec<bool>,
        caption: String,
        open: bool,
        /// An address row: this icon before the label, the address in a
        /// box, and icon buttons (info, copy, QR) instead of a caption.
        icon: Option<char>,
        /// Shown in the box and the QR popup while there is no address.
        waiting: String,
        /// The copy button shows a check for a moment.
        copied: bool,
    },
    /// A one-line state with a colored dot before it.
    Status {
        dot: u32,
    },
    /// A row that shows or hides the `count` rows after it.
    Expander {
        open: bool,
        count: usize,
    },
    /// A color, as `0xRRGGBB`, with Feather's own picker under the row
    /// while `open`.
    Color {
        rgb: u32,
        open: bool,
        /// How the picker shows it, and its three values in that model,
        /// kept while picking so dragging through grays keeps the hue.
        model: Model,
        values: [f32; 3],
        /// The color when the picker opened; a new one joins the recents.
        start: u32,
    },
}

/// One drive in a [`Form::drives`] list.
#[derive(Clone, Debug)]
pub struct DriveCard {
    /// `C:\`, for the Explorer icon.
    pub root: String,
    pub label: String,
    /// Free space, such as `120 GB free of 476 GB`.
    pub space: String,
    /// How full it is, 0 to 1.
    pub used: f32,
    pub on: bool,
}

struct Widget {
    /// The page it is on, for dialogs with pages.
    page: usize,
    id: Id,
    label: String,
    detail: Option<String>,
    kind: Kind,
    /// Shown only while this segmented control has this option chosen.
    when: Option<(Id, usize)>,
    /// Help shown when the pointer rests on the label.
    tip: Option<String>,
    /// Left out, from code: an address row with nothing to show.
    hidden: bool,
}

struct FooterButton {
    id: Id,
    label: String,
    style: ButtonStyle,
    /// Left out of the footer, as Back on the first step of a guide.
    hidden: bool,
}

/// The content of a dialog.
#[derive(Default)]
pub struct Form {
    widgets: Vec<Widget>,
    buttons: Vec<FooterButton>,
    error: Option<String>,
    focus_request: Option<Id>,
    /// Durations collapse to zero when Windows animations are off.
    animate: bool,
    /// Page names and icons; empty for a single-page dialog.
    pages: Vec<(String, PageIcon)>,
    /// Pages listed indented under the page before them.
    sub_pages: Vec<usize>,
    /// The page shown.
    page: usize,
    /// The highlight in the page list, sliding between pages.
    page_pill: Tween,
}

impl Form {
    pub fn new() -> Self {
        Self {
            animate: anim::system_animations_enabled(),
            ..Self::default()
        }
    }

    fn push(&mut self, id: Id, label: &str, detail: Option<&str>, kind: Kind) {
        self.widgets.push(Widget {
            page: self.pages.len().saturating_sub(1),
            id,
            label: label.to_string(),
            detail: detail.map(str::to_string),
            kind,
            when: None,
            tip: None,
            hidden: false,
        });
    }

    /// Leaves row `id` out, or brings it back.
    pub fn set_hidden(&mut self, id: Id, hidden: bool) {
        if let Some(widget) = self.widget_mut(id) {
            widget.hidden = hidden;
        }
    }

    /// An address people can open: icon, label, the address, and info,
    /// copy and QR buttons. Copy calls [`FormDialog::on_action`]; the QR
    /// button opens a popup with the code. `tip` shows on the info button.
    pub fn address(&mut self, id: Id, label: &str, icon: char, tip: &str) {
        self.push(
            id,
            label,
            None,
            Kind::Qr {
                text: String::new(),
                size: 0,
                modules: Vec::new(),
                caption: String::new(),
                open: false,
                icon: Some(icon),
                waiting: String::new(),
                copied: false,
            },
        );
        self.tip(id, tip);
    }

    /// The address shown in row `id`, or `waiting` while there is none.
    pub fn set_address(&mut self, id: Id, url: &str, waiting: &str) {
        if let Some(Kind::Qr { waiting: shown, .. }) =
            self.widget_mut(id).map(|widget| &mut widget.kind)
        {
            *shown = waiting.to_string();
        }
        self.set_qr(id, url);
    }

    /// Shows a check on row `id`'s copy button, or the copy icon again.
    pub fn set_copied(&mut self, id: Id, copied: bool) {
        if let Some(Kind::Qr { copied: shown, .. }) =
            self.widget_mut(id).map(|widget| &mut widget.kind)
        {
            *shown = copied;
        }
    }

    /// A state line with a colored dot, the label in bold and the detail
    /// under it.
    pub fn status(&mut self, id: Id, label: &str, dot: u32) {
        self.push(id, label, None, Kind::Status { dot });
    }

    pub fn set_status(&mut self, id: Id, label: &str, detail: &str, dot: u32) {
        if let Some(widget) = self.widget_mut(id) {
            widget.label = label.to_string();
            widget.detail = (!detail.is_empty()).then(|| detail.to_string());
            widget.kind = Kind::Status { dot };
        }
    }

    /// Shows `id` only while the segmented control `control` has `option`
    /// chosen.
    pub fn only_when(&mut self, id: Id, control: Id, option: usize) {
        if let Some(widget) = self.widget_mut(id) {
            widget.when = Some((control, option));
        }
    }

    /// Help shown when the pointer rests on `id`'s label.
    pub fn tip(&mut self, id: Id, text: &str) {
        if let Some(widget) = self.widget_mut(id) {
            widget.tip = Some(text.to_string());
        }
    }

    /// A row that folds away the `count` rows added after it.
    pub fn expander(&mut self, id: Id, label: &str, count: usize) {
        self.push(id, label, None, Kind::Expander { open: false, count });
    }

    /// Starts a new page; widgets added after it appear on it. The page list
    /// on the left switches between pages.
    pub fn page(&mut self, title: &str, icon: PageIcon) {
        self.pages.push((title.to_string(), icon));
    }

    /// A page listed indented under the one before it, like a sub-tab.
    pub fn subpage(&mut self, title: &str, icon: PageIcon) {
        self.sub_pages.push(self.pages.len());
        self.pages.push((title.to_string(), icon));
    }

    /// Makes the text box `id` a password box: dots, and no copying.
    pub fn secret(&mut self, id: Id) {
        if let Some(Kind::Field { field, .. }) = self.widget_mut(id).map(|widget| &mut widget.kind)
        {
            field.set_secret(true);
        }
    }

    pub fn set_qr(&mut self, id: Id, value: &str) {
        if let Some(Kind::Qr {
            text,
            size,
            modules,
            ..
        }) = self.widget_mut(id).map(|widget| &mut widget.kind)
        {
            if text == value {
                return;
            }
            *text = value.to_string();
            match qrcodegen::QrCode::encode_text(value, qrcodegen::QrCodeEcc::Medium) {
                Ok(code) if !value.is_empty() => {
                    let n = code.size();
                    *size = n as usize;
                    *modules = (0..n)
                        .flat_map(|y| (0..n).map(move |x| (x, y)))
                        .map(|(x, y)| code.get_module(x, y))
                        .collect();
                }
                _ => {
                    *size = 0;
                    modules.clear();
                }
            }
        }
    }

    /// A row with a color swatch; clicking it opens Feather's color picker.
    pub fn color(&mut self, id: Id, label: &str, detail: Option<&str>, rgb: u32) {
        self.push(
            id,
            label,
            detail,
            Kind::Color {
                rgb,
                open: false,
                model: color_space::last_model(),
                values: color_space::last_model().from_rgb(rgb, [0.0; 3]),
                start: rgb,
            },
        );
    }

    /// The color of a color row, as `0xRRGGBB`.
    pub fn color_value(&self, id: Id) -> u32 {
        match self.widget(id).map(|widget| &widget.kind) {
            Some(Kind::Color { rgb, .. }) => *rgb,
            _ => 0,
        }
    }

    pub fn set_color(&mut self, id: Id, value: u32) {
        if let Some(Kind::Color {
            rgb, model, values, ..
        }) = self.widget_mut(id).map(|widget| &mut widget.kind)
        {
            *rgb = value & 0xff_ffff;
            *values = model.from_rgb(*rgb, *values);
        }
    }

    pub fn current_page(&self) -> usize {
        self.page
    }

    pub fn set_page(&mut self, page: usize) {
        if page < self.pages.len() && page != self.page {
            self.page = page;
            let time = self.time(anim::SELECT);
            self.page_pill.animate_to(page as f32, time, Instant::now());
        }
    }

    pub fn heading(&mut self, text: &str) {
        self.push("", text, None, Kind::Heading);
    }

    pub fn paragraph(&mut self, text: &str) {
        self.push("", text, None, Kind::Paragraph);
    }

    pub fn switch(&mut self, id: Id, label: &str, detail: Option<&str>, on: bool) {
        let knob = Tween::new(if on { 1.0 } else { 0.0 });
        self.push(id, label, detail, Kind::Switch { on, knob });
    }

    pub fn segments(
        &mut self,
        id: Id,
        label: &str,
        detail: Option<&str>,
        options: Vec<Choice>,
        selected: usize,
    ) {
        let selected = selected.min(options.len().saturating_sub(1));
        let pill = Tween::new(selected as f32);
        self.push(
            id,
            label,
            detail,
            Kind::Segments {
                options,
                selected,
                pill,
            },
        );
    }

    /// A one-line text input. `wide` puts it under the label at full width.
    pub fn field(
        &mut self,
        id: Id,
        label: &str,
        detail: Option<&str>,
        value: &str,
        cue: &str,
        wide: bool,
    ) {
        let mut field = TextField::default();
        field.set_text(value);
        self.push(
            id,
            label,
            detail,
            Kind::Field {
                field,
                cue: cue.to_string(),
                scroll: 0.0,
                wide,
                shortcut: false,
                numeric: false,
            },
        );
    }

    /// A whole-number box. Digits are grouped by thousands the way this
    /// computer writes numbers (`1.000.000` or `1,000,000`) as they are typed.
    pub fn number(&mut self, id: Id, label: &str, detail: Option<&str>, cue: &str) {
        self.field(id, label, detail, "", cue, false);
        if let Some(Kind::Field { numeric, .. }) =
            self.widget_mut(id).map(|widget| &mut widget.kind)
        {
            *numeric = true;
        }
    }

    /// A box that records the key combination pressed while it has focus.
    /// Backspace or Delete empties it.
    pub fn shortcut(&mut self, id: Id, label: &str, detail: Option<&str>, value: &str, cue: &str) {
        self.field(id, label, detail, value, cue, true);
        if let Some(Kind::Field { shortcut, .. }) =
            self.widget_mut(id).map(|widget| &mut widget.kind)
        {
            *shortcut = true;
        }
    }

    pub fn checks(&mut self, id: Id, label: &str, items: Vec<(String, bool)>) {
        self.push(id, label, None, Kind::Checks { items });
    }

    pub fn paths(&mut self, id: Id, label: &str, detail: Option<&str>, items: Vec<String>) {
        self.push(
            id,
            label,
            detail,
            Kind::Paths {
                items,
                empty: "No folders yet.".to_string(),
                refresh: false,
            },
        );
    }

    /// Gives each row of a folder list a round-arrow button, reported
    /// through `FormDialog::on_item_action`.
    pub fn refreshable(&mut self, id: Id) {
        if let Some(Kind::Paths { refresh, .. }) =
            self.widget_mut(id).map(|widget| &mut widget.kind)
        {
            *refresh = true;
        }
    }

    pub fn syntax(&mut self, rows: &[(&str, &str)]) {
        let rows = rows
            .iter()
            .map(|(code, text)| (code.to_string(), text.to_string()))
            .collect();
        self.push("", "", None, Kind::Syntax { rows });
    }

    pub fn slider(
        &mut self,
        id: Id,
        label: &str,
        detail: Option<&str>,
        value: u32,
        caption: Box<dyn Fn(u32) -> String>,
    ) {
        let value = value.clamp(1, 100);
        self.push(id, label, detail, Kind::Slider { value, caption });
    }

    pub fn value(&self, id: Id) -> u32 {
        match self.widget(id).map(|widget| &widget.kind) {
            Some(Kind::Slider { value, .. }) => *value,
            _ => 0,
        }
    }

    pub fn set_value(&mut self, id: Id, to: u32) {
        if let Some(Kind::Slider { value, .. }) = self.widget_mut(id).map(|widget| &mut widget.kind)
        {
            *value = to.clamp(1, 100);
        }
    }

    /// A row with a button at its right edge; pressing it calls
    /// [`FormDialog::on_action`].
    pub fn action(&mut self, id: Id, label: &str, detail: Option<&str>, caption: &str) {
        let caption = caption.to_string();
        self.push(id, label, detail, Kind::Action { caption });
    }

    pub fn set_detail(&mut self, id: Id, detail: &str) {
        if let Some(widget) = self.widget_mut(id) {
            widget.detail = Some(detail.to_string());
        }
    }

    pub fn button(&mut self, id: Id, label: &str, style: ButtonStyle) {
        self.buttons.push(FooterButton {
            id,
            label: label.to_string(),
            style,
            hidden: false,
        });
    }

    /// Renames footer button `id`, as Next becoming Finish on the last step.
    pub fn set_button_label(&mut self, id: Id, label: &str) {
        if let Some(button) = self.buttons.iter_mut().find(|button| button.id == id) {
            button.label = label.to_string();
        }
    }

    pub fn set_button_hidden(&mut self, id: Id, hidden: bool) {
        if let Some(button) = self.buttons.iter_mut().find(|button| button.id == id) {
            button.hidden = hidden;
        }
    }

    fn widget(&self, id: Id) -> Option<&Widget> {
        self.widgets.iter().find(|widget| widget.id == id)
    }

    fn widget_mut(&mut self, id: Id) -> Option<&mut Widget> {
        self.widgets.iter_mut().find(|widget| widget.id == id)
    }

    pub fn is_on(&self, id: Id) -> bool {
        matches!(
            self.widget(id).map(|widget| &widget.kind),
            Some(Kind::Switch { on: true, .. })
        )
    }

    pub fn set_on(&mut self, id: Id, value: bool) {
        let now = Instant::now();
        let time = self.time(anim::SELECT);
        if let Some(Kind::Switch { on, knob }) = self.widget_mut(id).map(|widget| &mut widget.kind)
        {
            *on = value;
            knob.animate_to(if value { 1.0 } else { 0.0 }, time, now);
        }
    }

    pub fn selected(&self, id: Id) -> usize {
        match self.widget(id).map(|widget| &widget.kind) {
            Some(Kind::Segments { selected, .. }) => *selected,
            _ => 0,
        }
    }

    pub fn set_selected(&mut self, id: Id, index: usize) {
        let now = Instant::now();
        let time = self.time(anim::SELECT);
        if let Some(Kind::Segments {
            options,
            selected,
            pill,
        }) = self.widget_mut(id).map(|widget| &mut widget.kind)
        {
            *selected = index.min(options.len().saturating_sub(1));
            pill.animate_to(*selected as f32, time, now);
        }
    }

    pub fn text(&self, id: Id) -> String {
        match self.widget(id).map(|widget| &widget.kind) {
            Some(Kind::Field { field, .. }) => field.text(),
            _ => String::new(),
        }
    }

    /// Writes a whole number into a number box, grouped.
    pub fn set_number(&mut self, id: Id, value: u64) {
        self.set_text(id, &crate::text::group(value));
    }

    pub fn set_text(&mut self, id: Id, value: &str) {
        if let Some(Kind::Field { field, scroll, .. }) =
            self.widget_mut(id).map(|widget| &mut widget.kind)
        {
            field.set_text(value);
            *scroll = 0.0;
        }
    }

    pub fn checked(&self, id: Id) -> Vec<bool> {
        match self.widget(id).map(|widget| &widget.kind) {
            Some(Kind::Checks { items }) => items.iter().map(|(_, on)| *on).collect(),
            Some(Kind::Drives { items }) => items.iter().map(|item| item.on).collect(),
            Some(Kind::Order { items }) => items.iter().map(|(_, _, on)| *on).collect(),
            _ => Vec::new(),
        }
    }

    pub fn set_checked(&mut self, id: Id, values: &[bool]) {
        match self.widget_mut(id).map(|widget| &mut widget.kind) {
            Some(Kind::Checks { items }) => {
                for ((_, on), value) in items.iter_mut().zip(values) {
                    *on = *value;
                }
            }
            Some(Kind::Drives { items }) => {
                for (item, value) in items.iter_mut().zip(values) {
                    item.on = *value;
                }
            }
            Some(Kind::Order { items }) => {
                for ((_, _, on), value) in items.iter_mut().zip(values) {
                    *on = *value;
                }
            }
            _ => {}
        }
    }

    /// A list of kinds to reorder by dragging and to show or hide.
    pub fn order(&mut self, id: Id, label: &str, items: Vec<(String, Category, bool)>) {
        self.push(id, label, None, Kind::Order { items });
    }

    /// The kinds in their current order, each shown or not.
    pub fn order_items(&self, id: Id) -> Vec<(Category, bool)> {
        match self.widget(id).map(|widget| &widget.kind) {
            Some(Kind::Order { items }) => items
                .iter()
                .map(|(_, category, on)| (*category, *on))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Drives as cards to switch on and off.
    pub fn drives(&mut self, id: Id, label: &str, detail: Option<&str>, items: Vec<DriveCard>) {
        self.push(id, label, detail, Kind::Drives { items });
    }

    pub fn path_items(&self, id: Id) -> Vec<String> {
        match self.widget(id).map(|widget| &widget.kind) {
            Some(Kind::Paths { items, .. }) => items.clone(),
            _ => Vec::new(),
        }
    }

    pub fn set_paths(&mut self, id: Id, values: Vec<String>) {
        if let Some(Kind::Paths { items, .. }) = self.widget_mut(id).map(|widget| &mut widget.kind)
        {
            *items = values;
        }
    }

    pub fn set_error(&mut self, error: Option<String>) {
        self.error = error;
    }

    /// Moves keyboard focus to a field, for example after a validation error.
    pub fn focus(&mut self, id: Id) {
        self.focus_request = Some(id);
    }

    fn time(&self, duration: Duration) -> Duration {
        if self.animate {
            duration
        } else {
            Duration::ZERO
        }
    }

    fn running(&self, now: Instant) -> bool {
        self.widgets.iter().any(|widget| match &widget.kind {
            Kind::Switch { knob, .. } => knob.running(now),
            Kind::Segments { pill, .. } => pill.running(now),
            _ => false,
        }) || self.page_pill.running(now)
    }

    pub fn settle(&mut self) {
        for widget in &mut self.widgets {
            match &mut widget.kind {
                Kind::Switch { knob, .. } => knob.settle(),
                Kind::Segments { pill, .. } => pill.settle(),
                _ => {}
            }
        }
        self.page_pill.settle();
    }
}

/// Something the pointer or keyboard can act on.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Part {
    Switch(usize),
    Segment(usize, usize),
    Field(usize),
    Check(usize, usize),
    Remove(usize, usize),
    /// The "do this one again" button on a drive card or a folder row.
    Refresh(usize, usize),
    Add(usize),
    Button(usize),
    Slider(usize),
    Action(usize),
    /// A button that opens and closes a popup: a color row's swatch, or a
    /// QR code row's button.
    Color(usize),
    /// A row that folds the rows under it.
    Expander(usize),
    /// One of an open picker's three sliders.
    ColorSlider(usize, usize),
    /// The picker's model button: OKLCH, RGB, HSL.
    ColorModel(usize),
    /// Picks a color from anywhere on screen.
    ColorPick(usize),
    /// Copies the color's code.
    ColorCopy(usize),
    /// Takes a color code from the clipboard.
    ColorPaste(usize),
    /// A recent or suggested color in an open picker.
    ColorPreset(usize, usize),
    /// The picker popup's background: clicks there stay in the popup.
    Popup,
    /// The eye button inside a password box.
    Reveal(usize),
    /// An entry in the page list.
    Page(usize),
    Thumb,
    Track,
}

impl Part {
    /// The keyboard stop a part belongs to: a segmented control is one stop.
    fn stop(self) -> Part {
        match self {
            Part::Segment(widget, _) => Part::Segment(widget, 0),
            Part::Page(_) => Part::Page(0),
            other => other,
        }
    }
}

/// Where a folder row's round-arrow button goes: just left of its remove
/// button.
fn refresh_beside(remove: Rect) -> Rect {
    Rect::new(remove.l - 30.0, remove.t, remove.l - 2.0, remove.b)
}

#[derive(Default)]
struct WidgetBox {
    label: Rect,
    detail: Option<Rect>,
    control: Rect,
    items: Vec<Rect>,
    extra: Vec<Rect>,
}

/// Where everything goes for one window size. Content rectangles are in
/// content coordinates (before scrolling); the footer is in client
/// coordinates.
struct Arranged {
    width: f32,
    height: f32,
    view_h: f32,
    content_h: f32,
    cards: Vec<Rect>,
    dividers: Vec<Rect>,
    boxes: Vec<WidgetBox>,
    parts: Vec<(Rect, Part)>,
    buttons: Vec<Rect>,
    error: Rect,
    scrollbar: Option<(Rect, Rect)>,
    /// Page list entries, in client coordinates (they do not scroll).
    rail: Vec<Rect>,
    /// The page's title above its content (content coordinates).
    page_title: Rect,
    /// Left edge of the content column.
    left: f32,
    /// The open popup's panel (content coordinates).
    popup: Option<Rect>,
    /// Whether each widget is laid out on the page now.
    shown: Vec<bool>,
    /// Labels with help, and their widget.
    tips: Vec<(Rect, usize)>,
}

fn label_block(
    gfx: &Gfx,
    widget: &Widget,
    x: f32,
    y: f32,
    width: f32,
) -> (Rect, Option<Rect>, f32) {
    if widget.label.is_empty() && widget.detail.is_none() {
        return (Rect::default(), None, 0.0);
    }
    if widget.label.is_empty() {
        // Only a description: it takes the label's place.
        let detail = widget.detail.as_deref().unwrap_or("");
        let height = gfx.measure_height(detail, Font::SmallWrap, width).ceil();
        return (
            Rect::default(),
            Some(Rect::new(x, y, x + width, y + height)),
            height,
        );
    }
    let label = Rect::new(x, y, x + width, y + 20.0);
    match &widget.detail {
        Some(detail) => {
            let height = gfx.measure_height(detail, Font::SmallWrap, width).ceil();
            let rect = Rect::new(x, y + 22.0, x + width, y + 22.0 + height);
            (label, Some(rect), 22.0 + height)
        }
        None => (label, None, 20.0),
    }
}

fn shift(rect: Rect, dy: f32) -> Rect {
    Rect::new(rect.l, rect.t + dy, rect.r, rect.b + dy)
}

/// How big a QR code is drawn in its popup, quiet margin included.
const QR_SIDE: f32 = 216.0;
/// The QR popup: the code, and a line under it.
const QR_POPUP_W: f32 = QR_SIDE + 32.0;
const QR_POPUP_H: f32 = QR_SIDE + 62.0;

/// The color picker popup's width, and its inner margin.
const POPUP_W: f32 = 280.0;
const PICKER_PAD: f32 = 12.0;
/// The swatch at its top.
const PICKER_SWATCH_H: f32 = 56.0;
/// One slider row.
const PICKER_ROW: f32 = 26.0;
/// Swatches in the recent colors grid.
const PICKER_COLUMNS: usize = 8;
const PICKER_CELLS: usize = 16;
const PICKER_GAP: f32 = 6.0;
/// The Pick, Copy, and Paste buttons.
const PICKER_BUTTON_H: f32 = 28.0;

/// The picker's height.
fn picker_height() -> f32 {
    PICKER_SWATCH_H
        + 10.0
        + 26.0
        + 8.0
        + PICKER_ROW * 3.0
        + 4.0
        + PICKER_BUTTON_H
        + 10.0
        + 18.0
        + 6.0
        + picker_cell() * 2.0
        + PICKER_GAP
        + PICKER_PAD
}

fn picker_cell() -> f32 {
    (POPUP_W - PICKER_PAD * 2.0 - PICKER_GAP * (PICKER_COLUMNS as f32 - 1.0))
        / PICKER_COLUMNS as f32
}

/// Ready-made colors in the picker: Feather's accents, then neutrals.
const PRESETS: [u32; 18] = [
    0xf2a33a, 0xe8664a, 0xe2557f, 0xa77bf3, 0x5b8def, 0x3cb4c8, 0x4caf7d, 0x9bc53d, 0xf2d03b,
    0xffffff, 0xe9e5df, 0xc8c2b8, 0xa39a8f, 0x6e675e, 0x3a3530, 0x272320, 0x181614, 0x000000,
];

/// The picker's grid: colors chosen lately, then suggestions.
fn picker_swatches() -> Vec<u32> {
    let mut list = color_space::recent();
    for preset in PRESETS {
        if list.len() >= PICKER_CELLS {
            break;
        }
        if !list.contains(&preset) {
            list.push(preset);
        }
    }
    list.truncate(PICKER_CELLS);
    list
}

/// Where things are in an open picker, as indexes into its box's items.
mod picker_at {
    pub const SWATCH: usize = 0;
    pub const HEX: usize = 1;
    pub const MODEL: usize = 2;
    pub const COPY: usize = 3;
    pub const PASTE: usize = 4;
    pub const TRACKS: usize = 5;
    pub const VALUES: usize = 8;
    pub const PICK: usize = 11;
    pub const LABEL: usize = 12;
    pub const CELLS: usize = 13;
}

fn button_width(gfx: &Gfx, label: &str) -> f32 {
    (gfx.measure_width(label, Font::Button) + 36.0)
        .max(96.0)
        .ceil()
}

fn arrange(form: &Form, gfx: &Gfx, width: f32, height: f32, scroll: f32) -> Arranged {
    let paged = !form.pages.is_empty();
    let left = if paged { RAIL_W } else { 0.0 };
    let card_l = left + PAD;
    let card_r = width - PAD;
    let x0 = card_l + ROW_X;
    let x1 = card_r - ROW_X;
    let inner = (x1 - x0).max(120.0);
    let mut y = TITLE_H;
    let mut page_title = Rect::default();
    if paged {
        page_title = Rect::new(card_l + 2.0, y, card_r, y + 34.0);
        y += 46.0;
    }
    let mut cards = Vec::new();
    let mut dividers = Vec::new();
    let mut boxes = Vec::with_capacity(form.widgets.len());
    let mut parts = Vec::new();
    // An open color picker floats over the page under its row.
    let mut popup_anchor: Option<(usize, Rect)> = None;
    let mut card_top: Option<f32> = None;
    let mut shown = vec![false; form.widgets.len()];
    let mut tips = Vec::new();
    // Rows folded away under a closed expander end before this index.
    let mut folded_until = 0;

    for (index, widget) in form.widgets.iter().enumerate() {
        let mut out = WidgetBox::default();
        let hidden = (paged && widget.page != form.page)
            || widget.hidden
            || index < folded_until
            || widget
                .when
                .is_some_and(|(control, option)| form.selected(control) != option);
        if hidden {
            boxes.push(out);
            continue;
        }
        shown[index] = true;
        if let Kind::Expander { open: false, count } = widget.kind {
            folded_until = index + 1 + count;
        }
        if let Kind::Heading = widget.kind {
            if let Some(top) = card_top.take() {
                cards.push(Rect::new(card_l, top, card_r, y));
                y += 22.0;
            }
            out.label = Rect::new(card_l + 2.0, y, card_r, y + 24.0);
            y += 32.0;
            boxes.push(out);
            continue;
        }
        match card_top {
            None => card_top = Some(y),
            Some(_) => dividers.push(Rect::new(x0, y, x1, y + 1.0)),
        }
        let row_top = y;
        let height;
        match &widget.kind {
            Kind::Heading => unreachable!(),
            Kind::Paragraph => {
                let text_h = gfx
                    .measure_height(&widget.label, Font::BodyWrap, inner)
                    .ceil();
                out.label = Rect::new(x0, y + ROW_Y, x1, y + ROW_Y + text_h + 2.0);
                height = text_h + ROW_Y * 2.0;
            }
            Kind::Switch { .. } => {
                let (label, detail, block) =
                    label_block(gfx, widget, x0, 0.0, inner - SWITCH_W - 24.0);
                height = block.max(SWITCH_H) + ROW_Y * 2.0;
                let top = y + (height - block) / 2.0;
                out.label = shift(label, top);
                out.detail = detail.map(|rect| shift(rect, top));
                let mid = y + height / 2.0;
                out.control = Rect::new(
                    x1 - SWITCH_W,
                    mid - SWITCH_H / 2.0,
                    x1,
                    mid + SWITCH_H / 2.0,
                );
                parts.push((
                    Rect::new(card_l, y, card_r, y + height),
                    Part::Switch(index),
                ));
            }
            Kind::Segments { options, .. } => {
                let widths: Vec<f32> = options
                    .iter()
                    .map(|choice| {
                        gfx.measure_width(&choice.label, Font::ButtonLight).ceil()
                            + 28.0
                            + if choice.swatch.is_some() { 18.0 } else { 0.0 }
                    })
                    .collect();
                let total: f32 = widths.iter().sum::<f32>() + 4.0;
                let inline = inner - total - 24.0 >= 120.0;
                let control_l;
                let control_t;
                if inline {
                    let (label, detail, block) =
                        label_block(gfx, widget, x0, 0.0, inner - total - 24.0);
                    height = block.max(SEG_H) + ROW_Y * 2.0;
                    let top = y + (height - block) / 2.0;
                    out.label = shift(label, top);
                    out.detail = detail.map(|rect| shift(rect, top));
                    control_l = x1 - total;
                    control_t = y + (height - SEG_H) / 2.0;
                } else {
                    let (label, detail, block) = label_block(gfx, widget, x0, 0.0, inner);
                    let top = y + ROW_Y;
                    out.label = shift(label, top);
                    out.detail = detail.map(|rect| shift(rect, top));
                    let gap = if block > 0.0 { 10.0 } else { 0.0 };
                    control_l = x0;
                    control_t = top + block + gap;
                    height = ROW_Y + block + gap + SEG_H + ROW_Y;
                }
                out.control = Rect::new(control_l, control_t, control_l + total, control_t + SEG_H);
                let mut x = control_l + 2.0;
                for (option, option_w) in widths.iter().enumerate() {
                    let rect = Rect::new(x, control_t + 2.0, x + option_w, control_t + SEG_H - 2.0);
                    out.items.push(rect);
                    parts.push((rect, Part::Segment(index, option)));
                    x += option_w;
                }
            }
            Kind::Field { wide, field, .. } => {
                if *wide {
                    let (label, detail, block) = label_block(gfx, widget, x0, 0.0, inner);
                    let top = y + ROW_Y;
                    out.label = shift(label, top);
                    out.detail = detail.map(|rect| shift(rect, top));
                    let field_t = top + block + 8.0;
                    out.control = Rect::new(x0, field_t, x1, field_t + FIELD_H);
                    height = ROW_Y + block + 8.0 + FIELD_H + ROW_Y;
                } else {
                    let field_w = INLINE_FIELD_W.min(inner * 0.45);
                    let (label, detail, block) =
                        label_block(gfx, widget, x0, 0.0, inner - field_w - 24.0);
                    height = block.max(FIELD_H) + ROW_Y * 2.0;
                    let top = y + (height - block) / 2.0;
                    out.label = shift(label, top);
                    out.detail = detail.map(|rect| shift(rect, top));
                    let field_t = y + (height - FIELD_H) / 2.0;
                    out.control = Rect::new(x1 - field_w, field_t, x1, field_t + FIELD_H);
                }
                // A password box has an eye button at its right end.
                if field.is_secret() {
                    let control = out.control;
                    let eye = Rect::new(
                        control.r - 36.0,
                        control.t + 3.0,
                        control.r - 3.0,
                        control.b - 3.0,
                    );
                    out.extra.push(eye);
                    parts.push((eye, Part::Reveal(index)));
                }
                parts.push((out.control, Part::Field(index)));
            }
            Kind::Slider { .. } => {
                let control_w = SLIDER_W.min(inner * 0.5);
                let (label, detail, block) =
                    label_block(gfx, widget, x0, 0.0, inner - control_w - 24.0);
                height = block.max(SLIDER_H) + ROW_Y * 2.0;
                let top = y + (height - block) / 2.0;
                out.label = shift(label, top);
                out.detail = detail.map(|rect| shift(rect, top));
                let control_t = y + (height - SLIDER_H) / 2.0;
                out.control = Rect::new(x1 - control_w, control_t, x1, control_t + SLIDER_H);
                // The caption, then the track the thumb's centre travels.
                out.items
                    .push(Rect::new(x1 - control_w, control_t, x1, control_t + 18.0));
                out.items.push(Rect::new(
                    x1 - control_w + 9.0,
                    control_t + 22.0,
                    x1 - 9.0,
                    control_t + SLIDER_H,
                ));
                parts.push((
                    Rect::new(x1 - control_w, control_t + 18.0, x1, control_t + SLIDER_H),
                    Part::Slider(index),
                ));
            }
            Kind::Status { .. } => {
                let (label, detail, block) = label_block(gfx, widget, x0 + 18.0, 0.0, inner - 18.0);
                height = block + ROW_Y * 2.0;
                let top = y + ROW_Y;
                out.label = shift(label, top);
                out.detail = detail.map(|rect| shift(rect, top));
            }
            Kind::Qr {
                open,
                icon: Some(_),
                ..
            } => {
                // icon · label · [address box] · info · copy · QR
                let size = BUTTON_H;
                let gap = 6.0;
                let qr = Rect::new(x1 - size, 0.0, x1, size);
                let copy = Rect::new(qr.l - gap - size, 0.0, qr.l - gap, size);
                let info = Rect::new(copy.l - gap - size, 0.0, copy.l - gap, size);
                height = size + ROW_Y * 2.0;
                let top = y + ROW_Y;
                let at = |rect: Rect| shift(rect, top);
                let label_w = 112.0;
                out.label = Rect::new(x0 + 26.0, top, x0 + 26.0 + label_w, top + size);
                out.items = vec![
                    Rect::new(x0, top, x0 + 20.0, top + size),
                    Rect::new(
                        x0 + 26.0 + label_w + 8.0,
                        top + 2.0,
                        info.l - 10.0,
                        top + size - 2.0,
                    ),
                    at(info),
                    at(copy),
                ];
                out.control = at(qr);
                parts.push((at(copy), Part::Action(index)));
                parts.push((at(qr), Part::Color(index)));
                if *open {
                    popup_anchor = Some((index, out.control));
                }
            }
            Kind::Qr { caption, open, .. } => {
                let button_w = button_width(gfx, caption);
                let (label, detail, block) =
                    label_block(gfx, widget, x0, 0.0, inner - button_w - 24.0);
                height = block.max(BUTTON_H) + ROW_Y * 2.0;
                let top = y + (height - block) / 2.0;
                out.label = shift(label, top);
                out.detail = detail.map(|rect| shift(rect, top));
                let button_t = y + (height - BUTTON_H) / 2.0;
                out.control = Rect::new(x1 - button_w, button_t, x1, button_t + BUTTON_H);
                parts.push((out.control, Part::Color(index)));
                if *open {
                    popup_anchor = Some((index, out.control));
                }
            }
            Kind::Expander { .. } => {
                let (label, _, block) = label_block(gfx, widget, x0, 0.0, inner - 40.0);
                height = block.max(BUTTON_H) + ROW_Y * 2.0;
                out.label = shift(label, y + (height - block) / 2.0);
                let mid = y + height / 2.0;
                out.control = Rect::new(x1 - 16.0, mid - 8.0, x1, mid + 8.0);
                parts.push((
                    Rect::new(card_l, y, card_r, y + height),
                    Part::Expander(index),
                ));
            }
            Kind::Color { open, .. } => {
                let control_w = 124.0;
                let (label, detail, block) =
                    label_block(gfx, widget, x0, 0.0, inner - control_w - 24.0);
                let row_h = block.max(BUTTON_H) + ROW_Y * 2.0;
                let top = y + (row_h - block) / 2.0;
                out.label = shift(label, top);
                out.detail = detail.map(|rect| shift(rect, top));
                let button_t = y + (row_h - BUTTON_H) / 2.0;
                out.control = Rect::new(x1 - control_w, button_t, x1, button_t + BUTTON_H);
                parts.push((out.control, Part::Color(index)));
                if *open {
                    popup_anchor = Some((index, out.control));
                }
                height = row_h;
            }
            Kind::Action { caption } => {
                let button_w = button_width(gfx, caption);
                let (label, detail, block) =
                    label_block(gfx, widget, x0, 0.0, inner - button_w - 24.0);
                height = block.max(BUTTON_H) + ROW_Y * 2.0;
                let top = y + (height - block) / 2.0;
                out.label = shift(label, top);
                out.detail = detail.map(|rect| shift(rect, top));
                let button_t = y + (height - BUTTON_H) / 2.0;
                out.control = Rect::new(x1 - button_w, button_t, x1, button_t + BUTTON_H);
                parts.push((out.control, Part::Action(index)));
            }
            Kind::Checks { items } => {
                let (label, detail, block) = label_block(gfx, widget, x0, 0.0, inner);
                let top = y + ROW_Y;
                out.label = shift(label, top);
                out.detail = detail.map(|rect| shift(rect, top));
                let mut item_y = top + block + if block > 0.0 { 6.0 } else { 0.0 };
                for item in 0..items.len() {
                    let rect = Rect::new(x0 - 8.0, item_y, x1 + 8.0, item_y + ITEM_H);
                    out.items.push(rect);
                    parts.push((rect, Part::Check(index, item)));
                    item_y += ITEM_H;
                }
                height = item_y - y + ROW_Y - 4.0;
            }
            Kind::Order { items } => {
                let (label, detail, block) = label_block(gfx, widget, x0, 0.0, inner);
                let top = y + ROW_Y;
                out.label = shift(label, top);
                out.detail = detail.map(|rect| shift(rect, top));
                let mut item_y = top + block + if block > 0.0 { 6.0 } else { 0.0 };
                for item in 0..items.len() {
                    let rect = Rect::new(x0 - 8.0, item_y, x1 + 8.0, item_y + ITEM_H);
                    out.items.push(rect);
                    parts.push((rect, Part::Check(index, item)));
                    item_y += ITEM_H + 2.0;
                }
                height = item_y - y + ROW_Y - 4.0;
            }
            Kind::Drives { items } => {
                let (label, detail, block) = label_block(gfx, widget, x0, 0.0, inner);
                let top = y + ROW_Y;
                out.label = shift(label, top);
                out.detail = detail.map(|rect| shift(rect, top));
                let grid_t = top + block + if block > 0.0 { 10.0 } else { 0.0 };
                let gap = 10.0;
                let columns = if inner >= 380.0 { 2 } else { 1 };
                let card_w = (inner + 16.0 - gap * (columns as f32 - 1.0)) / columns as f32;
                let card_h = 72.0;
                for item in 0..items.len() {
                    let (column, row) = (item % columns, item / columns);
                    let l = x0 - 8.0 + column as f32 * (card_w + gap);
                    let t = grid_t + row as f32 * (card_h + gap);
                    let rect = Rect::new(l, t, l + card_w, t + card_h);
                    out.items.push(rect);
                    parts.push((rect, Part::Check(index, item)));
                    let again = Rect::new(rect.r - 35.0, rect.b - 34.0, rect.r - 9.0, rect.b - 8.0);
                    out.extra.push(again);
                    parts.push((again, Part::Refresh(index, item)));
                }
                let rows = items.len().div_ceil(columns);
                height = grid_t - y + rows as f32 * (card_h + gap) - gap + ROW_Y + 4.0;
            }
            Kind::Paths { items, refresh, .. } => {
                let (label, detail, block) = label_block(gfx, widget, x0, 0.0, inner);
                let top = y + ROW_Y;
                out.label = shift(label, top);
                out.detail = detail.map(|rect| shift(rect, top));
                let mut item_y = top + block + 6.0;
                for item in 0..items.len() {
                    let row = Rect::new(x0 - 8.0, item_y, x1 + 8.0, item_y + ITEM_H);
                    let remove = Rect::new(x1 - 26.0, item_y + 5.0, x1 + 2.0, item_y + 31.0);
                    out.items.push(row);
                    out.extra.push(remove);
                    if *refresh {
                        parts.push((refresh_beside(remove), Part::Refresh(index, item)));
                    }
                    parts.push((remove, Part::Remove(index, item)));
                    item_y += ITEM_H;
                }
                if items.is_empty() {
                    out.control = Rect::new(x0, item_y, x1, item_y + ITEM_H);
                    item_y += ITEM_H;
                }
                let add_w = gfx.measure_width("Add a folder", Font::ButtonLight).ceil() + 44.0;
                let add = Rect::new(
                    x0 - 8.0,
                    item_y + 2.0,
                    x0 - 8.0 + add_w,
                    item_y + ITEM_H - 2.0,
                );
                out.extra.push(add);
                parts.push((add, Part::Add(index)));
                item_y += ITEM_H;
                height = item_y - y + ROW_Y - 4.0;
            }
            Kind::Syntax { rows } => {
                let code_w = 190.0_f32.min(inner * 0.4);
                let text_w = inner - code_w;
                let mut row_y = y + ROW_Y;
                for (_, text) in rows {
                    let text_h = gfx
                        .measure_height(text, Font::BodyWrap, text_w)
                        .ceil()
                        .max(22.0);
                    out.items
                        .push(Rect::new(x0, row_y, x0 + code_w, row_y + 24.0));
                    out.extra.push(Rect::new(
                        x0 + code_w,
                        row_y + 3.0,
                        x1,
                        row_y + 3.0 + text_h,
                    ));
                    row_y += text_h + 10.0;
                }
                height = row_y - y + ROW_Y - 10.0;
            }
        }
        y = row_top + height;
        if let (Some(_), Kind::Qr { icon: Some(_), .. }) = (&widget.tip, &widget.kind) {
            if let Some(&info) = out.items.get(2) {
                tips.push((info, index));
            }
        } else if widget.tip.is_some() && !out.label.is_empty() {
            let label = out.label;
            let text_w = gfx.measure_width(&widget.label, Font::Body).ceil();
            tips.push((
                Rect::new(
                    label.l - 4.0,
                    label.t - 6.0,
                    label.l + text_w + 6.0,
                    label.b + 6.0,
                ),
                index,
            ));
        }
        boxes.push(out);
    }
    if let Some(top) = card_top {
        cards.push(Rect::new(card_l, top, card_r, y));
    }
    let content_h = y + 20.0;

    let footer_h = FOOTER_H + if form.error.is_some() { ERROR_H } else { 0.0 };
    let view_h = (height - footer_h).max(0.0);

    // The picker popup: under its color button, or above it when there is
    // no room below. Its parts come first, so they win hit-testing.
    let popup = popup_anchor.map(|(index, control)| {
        let qr = matches!(form.widgets[index].kind, Kind::Qr { .. });
        let (popup_w, popup_h) = if qr {
            (QR_POPUP_W, QR_POPUP_H)
        } else {
            (POPUP_W, picker_height())
        };
        // Under its button when it fits on screen, else above it, else
        // as high as the view allows: never under the footer.
        let below = control.b + 6.0;
        let above = control.t - 6.0 - popup_h;
        let (view_top, view_bottom) = (scroll + TITLE_H + 4.0, scroll + view_h - 4.0);
        let top = if below + popup_h <= view_bottom {
            below
        } else if above >= view_top {
            above
        } else {
            (view_bottom - popup_h).max(view_top)
        };
        let panel = Rect::new(
            control.r + 8.0 - popup_w,
            top,
            control.r + 8.0,
            top + popup_h,
        );
        if qr {
            let code = Rect::new(
                panel.l + 16.0,
                panel.t + 16.0,
                panel.l + 16.0 + QR_SIDE,
                panel.t + 16.0 + QR_SIDE,
            );
            let caption = Rect::new(
                panel.l + 16.0,
                code.b + 10.0,
                panel.r - 16.0,
                panel.b - 12.0,
            );
            if let Some(widget_box) = boxes.get_mut(index) {
                // After an address row's own items: the popup's are the
                // last two.
                if matches!(form.widgets[index].kind, Kind::Qr { icon: Some(_), .. }) {
                    widget_box.items.extend([code, caption]);
                } else {
                    widget_box.items = vec![code, caption];
                }
            }
            parts.insert(0, (panel, Part::Popup));
            return panel;
        }
        let (l, r) = (panel.l + PICKER_PAD, panel.r - PICKER_PAD);
        let swatch = Rect::new(panel.l, panel.t, panel.r, panel.t + PICKER_SWATCH_H);
        let row_t = swatch.b + 10.0;
        let model = Rect::new(r - 84.0, row_t, r, row_t + 26.0);
        let hex = Rect::new(l, row_t, model.l - 8.0, row_t + 26.0);
        // Pick from screen, Copy, Paste: one row of labeled buttons.
        let tracks_t = row_t + 26.0 + 8.0;
        let buttons_t = tracks_t + PICKER_ROW * 3.0 + 4.0;
        let button_w = (r - l - 12.0) / 3.0;
        let button = |at: f32| {
            let bl = l + at * (button_w + 6.0);
            Rect::new(bl, buttons_t, bl + button_w, buttons_t + PICKER_BUTTON_H)
        };
        let (pick, copy, paste) = (button(0.0), button(1.0), button(2.0));
        let mut items = vec![swatch, hex, model, copy, paste];
        let mut popup_parts = vec![
            (model, Part::ColorModel(index)),
            (pick, Part::ColorPick(index)),
            (copy, Part::ColorCopy(index)),
            (paste, Part::ColorPaste(index)),
        ];
        let mut values = Vec::new();
        for channel in 0..3 {
            let t = tracks_t + channel as f32 * PICKER_ROW;
            let track = Rect::new(l, t + 7.0, r - 40.0, t + 19.0);
            items.push(track);
            values.push(Rect::new(track.r + 6.0, t + 3.0, r, t + 23.0));
            // A taller strip than the track, so it is easy to grab.
            popup_parts.push((
                Rect::new(track.l - 8.0, t, track.r + 8.0, t + PICKER_ROW),
                Part::ColorSlider(index, channel),
            ));
        }
        items.extend(values);
        items.push(pick);
        let label_t = buttons_t + PICKER_BUTTON_H + 10.0;
        items.push(Rect::new(l, label_t, r, label_t + 18.0));
        let cell = picker_cell();
        let grid_t = label_t + 18.0 + 6.0;
        for at in 0..picker_swatches().len() {
            let (column, row) = ((at % PICKER_COLUMNS) as f32, (at / PICKER_COLUMNS) as f32);
            let cl = l + column * (cell + PICKER_GAP);
            let ct = grid_t + row * (cell + PICKER_GAP);
            let rect = Rect::new(cl, ct, cl + cell, ct + cell);
            items.push(rect);
            popup_parts.push((rect, Part::ColorPreset(index, at)));
        }
        if let Some(widget_box) = boxes.get_mut(index) {
            widget_box.items = items;
        }
        popup_parts.push((panel, Part::Popup));
        parts.splice(0..0, popup_parts);
        panel
    });
    let error = Rect::new(card_l, view_h + 12.0, card_r, view_h + 12.0 + 20.0);
    let button_t = height - 16.0 - BUTTON_H;
    let mut buttons = vec![Rect::default(); form.buttons.len()];
    let mut left = card_l;
    let mut right = card_r;
    // The primary button sits at the far right; the others line up to its
    // left in the order they were added. Quiet buttons go on the left.
    let mut order: Vec<usize> = (0..form.buttons.len()).collect();
    order.sort_by_key(|&index| form.buttons[index].style == ButtonStyle::Primary);
    for &index in order.iter().rev() {
        let button = &form.buttons[index];
        let w = button_width(gfx, &button.label);
        if button.style == ButtonStyle::Quiet || button.hidden {
            continue;
        }
        buttons[index] = Rect::new(right - w, button_t, right, button_t + BUTTON_H);
        right -= w + 8.0;
    }
    for (index, button) in form.buttons.iter().enumerate() {
        if button.style == ButtonStyle::Quiet && !button.hidden {
            let w = button_width(gfx, &button.label);
            buttons[index] = Rect::new(left - 12.0, button_t, left - 12.0 + w, button_t + BUTTON_H);
            left += w + 8.0;
        }
    }

    let scrollbar = (content_h > view_h && view_h > 40.0).then(|| {
        let track = Rect::new(width - 12.0, TITLE_H + 4.0, width - 4.0, view_h - 8.0);
        let thumb_h = (track.height() * view_h / content_h)
            .max(28.0)
            .min(track.height());
        let travel = track.height() - thumb_h;
        let top = track.t + travel * (scroll / (content_h - view_h)).clamp(0.0, 1.0);
        (track, Rect::new(track.l, top, track.r, top + thumb_h))
    });

    let rail = (0..form.pages.len())
        .map(|page| {
            let top = 62.0 + page as f32 * (RAIL_ITEM_H + 2.0);
            Rect::new(12.0, top, RAIL_W - 12.0, top + RAIL_ITEM_H)
        })
        .collect();

    Arranged {
        popup,
        rail,
        page_title,
        left: if paged { RAIL_W } else { 0.0 },
        width,
        height,
        view_h,
        content_h,
        cards,
        dividers,
        boxes,
        parts,
        buttons,
        error,
        scrollbar,
        shown,
        tips,
    }
}

// ----- Window host -------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
enum Drag {
    None,
    Text(usize),
    /// Dragging one of an open color picker's sliders.
    Color {
        widget: usize,
        channel: usize,
    },
    Thumb {
        start_y: f32,
        start_scroll: f32,
    },
    Slider(usize),
    /// Holding an entry of a reorderable list; past a few pixels it lifts.
    Row {
        widget: usize,
        item: usize,
        grab: f32,
        start_y: f32,
        y: f32,
        moving: bool,
    },
}

/// Where a dragged row would land among `count` rows starting at `first`.
fn drop_slot(first: f32, top: f32, count: usize) -> usize {
    let step = ITEM_H + 2.0;
    (((top - first) / step).round().max(0.0) as usize).min(count.saturating_sub(1))
}

struct Host {
    hwnd: HWND,
    gfx: Gfx,
    canvas: Canvas,
    look: Look,
    palette: Palette,
    theme: Tween,
    theme_from: Option<Palette>,
    form: Form,
    dialog: Box<dyn FormDialog>,
    hover: Option<Part>,
    pressed: Option<Part>,
    fades: Fades<Part>,
    focus: Option<Part>,
    /// Shows the focus ring; set by the keyboard, cleared by the mouse.
    focus_visible: bool,
    scroll: Tween,
    drag: Drag,
    caret_on: bool,
    active: bool,
    tracking: bool,
    anim_timer: bool,
    done: bool,
    outcome: Option<Box<dyn Any>>,
    /// A shortcut box listening for keys.
    recording: Option<Recording>,
    /// The shortcut box that just took a new shortcut, and when.
    committed: Option<(usize, Instant)>,
    /// Shown in the title strip, or above the page list.
    title: String,
    /// Drive, folder, and app icons.
    icons: std::cell::RefCell<IconCache>,
    /// The label the pointer rests on, and since when; its tip shows after
    /// a moment.
    tip: Option<(usize, Instant)>,
    /// The open popup's arrival, which slides it in.
    popup_since: Option<Instant>,
    /// The picker's thumbs, from 0 to 1 along their tracks, gliding to
    /// where a click or a new color puts them.
    thumbs: [Tween; 3],
    /// The swatch fades from this color to the new one.
    swatch_from: u32,
    swatch_fade: Tween,
    /// When the code was copied, for the check mark.
    copied: Option<Instant>,
    /// A color picked while dragging, not yet shown in the main window;
    /// it follows at most every few frames so dragging stays smooth.
    preview_due: Option<usize>,
    preview_at: Option<Instant>,
}

/// How often a dragged color reaches the main window.
const PREVIEW_EVERY: Duration = Duration::from_millis(60);
/// How long the popup takes to slide in.
const POPUP_IN: Duration = Duration::from_millis(170);
/// How long the copy button shows its check mark.
const COPIED_FOR: Duration = Duration::from_millis(1400);

/// How long the pointer rests on a label before its tip shows.
const TIP_DELAY: Duration = Duration::from_millis(450);

/// Keys pressed into a shortcut box. The combination is taken when every
/// key is let go: the most keys that were down together.
struct Recording {
    widget: usize,
    held: Vec<u32>,
    /// When each held key went down, for the key cap pop.
    since: Vec<Instant>,
    peak: Vec<u32>,
    started: Instant,
}

const GLOW: Duration = Duration::from_millis(700);

impl Host {
    fn now_palette(&self, now: Instant) -> Palette {
        match self.theme_from {
            Some(from) if self.theme.running(now) => {
                Palette::mix(&from, &self.palette, self.theme.value(now))
            }
            _ => self.palette,
        }
    }

    fn invalidate(&self) {
        unsafe {
            InvalidateRect(self.hwnd, std::ptr::null(), 0);
        }
    }

    /// Asks for a frame on every display refresh until animations settle.
    fn kick(&mut self) {
        if !self.anim_timer {
            frames::request(self.hwnd);
            self.anim_timer = true;
        }
        self.invalidate();
    }

    fn running(&self, now: Instant) -> bool {
        self.scroll.running(now)
            || self.theme.running(now)
            || self.fades.running(now)
            || self.form.running(now)
            || self.recording.is_some()
            || self
                .committed
                .is_some_and(|(_, at)| now.duration_since(at) < GLOW)
            || self.thumbs.iter().any(|thumb| thumb.running(now))
            || self.swatch_fade.running(now)
            || self.preview_due.is_some()
            || self
                .popup_since
                .is_some_and(|at| now.duration_since(at) < POPUP_IN)
            || self
                .copied
                .is_some_and(|at| now.duration_since(at) < COPIED_FOR)
    }

    fn start_recording(&mut self, widget: usize) {
        if self.recording.as_ref().is_some_and(|r| r.widget == widget) {
            return;
        }
        keyhook::record(self.hwnd);
        self.recording = Some(Recording {
            widget,
            held: Vec::new(),
            since: Vec::new(),
            peak: Vec::new(),
            started: Instant::now(),
        });
        self.kick();
    }

    fn stop_recording(&mut self) {
        if self.recording.take().is_some() {
            keyhook::stop_recording();
            self.invalidate();
        }
    }

    /// A key from the keyboard hook while a shortcut box is listening.
    fn on_record_key(&mut self, vk: u32, down: bool) {
        let Some(recording) = &mut self.recording else {
            return;
        };
        let now = Instant::now();
        if down {
            if !recording.held.contains(&vk) {
                recording.held.push(vk);
                recording.since.push(now);
            }
            if recording.held.len() >= recording.peak.len() {
                recording.peak = recording.held.clone();
            }
            self.kick();
            return;
        }
        if let Some(at) = recording.held.iter().position(|&held| held == vk) {
            recording.held.remove(at);
            recording.since.remove(at);
        }
        if !recording.held.is_empty() {
            self.kick();
            return;
        }
        let widget = recording.widget;
        let peak = std::mem::take(&mut recording.peak);
        const BACK: u32 = 0x08;
        const TAB: u32 = 0x09;
        const ENTER: u32 = 0x0d;
        const ESC: u32 = 0x1b;
        const DELETE: u32 = 0x2e;
        let shift = |vk: &u32| *vk == 0xa0 || *vk == 0xa1;
        match peak.as_slice() {
            [ESC] | [ENTER] => {
                self.stop_recording();
            }
            [TAB] => {
                self.stop_recording();
                self.move_focus(false);
            }
            [a, TAB] | [TAB, a] if shift(a) => {
                self.stop_recording();
                self.move_focus(true);
            }
            [BACK] | [DELETE] => self.commit_shortcut(widget, String::new()),
            _ => match hotkey::Shortcut::from_held(&peak)
                .map(|shortcut| shortcut.check().map(|()| shortcut.to_string()))
            {
                Some(Ok(text)) => self.commit_shortcut(widget, text),
                Some(Err(error)) => {
                    self.form.set_error(Some(error));
                    self.kick();
                }
                None => {}
            },
        }
    }

    fn commit_shortcut(&mut self, widget: usize, text: String) {
        self.stop_recording();
        if let Some(field) = self.field_mut(widget) {
            field.set_text(&text);
        }
        self.form.set_error(None);
        self.committed = Some((widget, Instant::now()));
        self.changed(widget);
    }

    fn on_anim_frame(&mut self) {
        let now = Instant::now();
        if let Some(widget) = self.preview_due {
            if self
                .preview_at
                .is_none_or(|at| now.duration_since(at) >= PREVIEW_EVERY)
            {
                self.flush_preview(widget);
            }
        }
        self.fades.prune(now);
        self.invalidate();
        if !self.running(now) {
            frames::stop(self.hwnd);
            self.anim_timer = false;
            self.theme_from = None;
        }
    }

    fn arranged(&self) -> Arranged {
        let (width, height) = client_size(self.hwnd);
        arrange(
            &self.form,
            &self.gfx,
            width as f32,
            height as f32,
            self.scroll.value(Instant::now()),
        )
    }

    /// Where the page is scrolled to, never past its end: exactly what is
    /// drawn, so clicks land where they are aimed even after the page got
    /// shorter.
    fn scroll_now(&self, arranged: &Arranged) -> f32 {
        self.scroll
            .value(Instant::now())
            .clamp(0.0, self.max_scroll(arranged))
    }

    fn max_scroll(&self, arranged: &Arranged) -> f32 {
        (arranged.content_h - arranged.view_h).max(0.0)
    }

    fn scroll_to(&mut self, target: f32, smooth: bool) {
        let arranged = self.arranged();
        let target = target.clamp(0.0, self.max_scroll(&arranged));
        if smooth {
            let time = self.form.time(anim::SCROLL);
            self.scroll.animate_to(target, time, Instant::now());
            self.kick();
        } else {
            self.scroll.jump(target);
            self.invalidate();
        }
    }

    fn set_look(&mut self, look: Look) {
        if look == self.look {
            return;
        }
        let now = Instant::now();
        let shown = self.now_palette(now);
        self.look = look;
        self.palette = look.palette();
        theme::apply_window_effects(self.hwnd, look.dark, effective_material(look.material));
        if self.form.animate {
            self.theme_from = Some(shown);
            self.theme.jump(0.0);
            self.theme.animate_to(1.0, anim::THEME, now);
        }
        self.kick();
    }

    fn set_hover(&mut self, part: Option<Part>) {
        if self.hover != part {
            let time = self.form.time(anim::HOVER);
            self.fades.hover(self.hover, part, time, Instant::now());
            self.hover = part;
            self.kick();
        }
    }

    fn hit(&self, arranged: &Arranged, x: f32, y: f32) -> Option<Part> {
        for (page, rect) in arranged.rail.iter().enumerate() {
            if rect.contains(x, y) {
                return Some(Part::Page(page));
            }
        }
        for (index, rect) in arranged.buttons.iter().enumerate() {
            if rect.contains(x, y) {
                return Some(Part::Button(index));
            }
        }
        if let Some((track, thumb)) = arranged.scrollbar {
            if thumb.inset(-3.0, 0.0).contains(x, y) {
                return Some(Part::Thumb);
            }
            if track.inset(-3.0, 0.0).contains(x, y) {
                return Some(Part::Track);
            }
        }
        if y >= arranged.view_h || y < TITLE_H || x < arranged.left {
            return None;
        }
        let content_y = y + self.scroll_now(arranged);
        if let Some(panel) = arranged.popup {
            if panel.contains(x, content_y) {
                return arranged
                    .parts
                    .iter()
                    .find(|(rect, _)| rect.contains(x, content_y))
                    .map(|(_, part)| *part);
            }
        }
        // A round-arrow button sits on top of its drive card.
        let over = |(rect, _): &&(Rect, Part)| rect.contains(x, content_y);
        let part = arranged
            .parts
            .iter()
            .filter(|(_, part)| matches!(part, Part::Refresh(..)))
            .find(over)
            .or_else(|| arranged.parts.iter().find(over))
            .map(|(_, part)| *part);
        match part {
            Some(Part::Refresh(widget, item)) if !self.can_refresh(widget, item) => {
                Some(Part::Check(widget, item))
            }
            other => other,
        }
    }

    /// A drive card only has its round-arrow button while it is searched.
    fn can_refresh(&self, widget: usize, item: usize) -> bool {
        match self.form.widgets.get(widget).map(|widget| &widget.kind) {
            Some(Kind::Drives { items }) => items.get(item).is_some_and(|card| card.on),
            Some(Kind::Paths { refresh, .. }) => *refresh,
            _ => false,
        }
    }

    /// Keyboard stops in tab order.
    fn stops(&self, arranged: &Arranged) -> Vec<Part> {
        let mut stops: Vec<Part> = Vec::new();
        if !arranged.rail.is_empty() {
            stops.push(Part::Page(0));
        }
        for (_, part) in &arranged.parts {
            let stop = part.stop();
            if matches!(stop, Part::Popup) {
                continue;
            }
            if let Part::Refresh(widget, item) = stop {
                if !self.can_refresh(widget, item) {
                    continue;
                }
            }
            if !stops.contains(&stop) {
                stops.push(stop);
            }
        }
        stops.extend(
            (0..self.form.buttons.len())
                .filter(|&index| !self.form.buttons[index].hidden)
                .map(Part::Button),
        );
        stops
    }

    fn part_rect(&self, arranged: &Arranged, part: Part) -> Option<Rect> {
        match part {
            Part::Button(index) => arranged.buttons.get(index).copied(),
            Part::Segment(widget, _) => arranged.boxes.get(widget).map(|b| b.control),
            Part::Switch(widget) => arranged.boxes.get(widget).map(|b| b.control),
            Part::Page(_) => None,
            _ => arranged
                .parts
                .iter()
                .find(|(_, candidate)| *candidate == part)
                .map(|(rect, _)| *rect),
        }
    }

    fn set_focus(&mut self, part: Option<Part>, visible: bool) {
        self.focus = part.map(Part::stop);
        self.focus_visible = visible;
        self.caret_on = true;
        // A shortcut box listens while it has focus.
        match self.focus {
            Some(Part::Field(widget)) if self.is_shortcut(widget) => self.start_recording(widget),
            _ => self.stop_recording(),
        }
        if let Some(part) = self.focus {
            let arranged = self.arranged();
            if !matches!(part, Part::Button(_)) {
                if let Some(rect) = self.part_rect(&arranged, part) {
                    let scroll = self.scroll.target();
                    let target = if rect.t - 12.0 < scroll {
                        rect.t - 12.0
                    } else if rect.b + 12.0 > scroll + arranged.view_h {
                        rect.b + 12.0 - arranged.view_h
                    } else {
                        scroll
                    };
                    if target != scroll {
                        self.scroll_to(target, true);
                    }
                }
            }
        }
        self.invalidate();
    }

    fn move_focus(&mut self, backward: bool) {
        let arranged = self.arranged();
        let stops = self.stops(&arranged);
        if stops.is_empty() {
            return;
        }
        let current = self
            .focus
            .and_then(|focus| stops.iter().position(|stop| *stop == focus));
        let next = match (current, backward) {
            (None, false) => 0,
            (None, true) => stops.len() - 1,
            (Some(index), false) => (index + 1) % stops.len(),
            (Some(index), true) => (index + stops.len() - 1) % stops.len(),
        };
        self.set_focus(Some(stops[next]), true);
    }

    fn apply_focus_request(&mut self) {
        let Some(id) = self.form.focus_request.take() else {
            return;
        };
        if let Some(index) = self.form.widgets.iter().position(|widget| widget.id == id) {
            if let Kind::Field { field, .. } = &mut self.form.widgets[index].kind {
                field.select_all();
            }
            self.set_focus(Some(Part::Field(index)), true);
        }
    }

    /// Shows another page, from the top.
    fn show_page(&mut self, page: usize) {
        if page == self.form.current_page() {
            return;
        }
        self.stop_recording();
        self.form.set_page(page);
        self.dialog.on_page(&mut self.form, page);
        self.scroll.jump(0.0);
        self.focus = if self.focus_visible {
            Some(Part::Page(0))
        } else {
            None
        };
        self.set_hover(None);
        self.kick();
    }

    /// Regroups the digits of a number box after an edit, keeping the caret
    /// after the same digit.
    fn regroup(&mut self, widget: usize) {
        let Some(Kind::Field {
            field,
            numeric: true,
            ..
        }) = self
            .form
            .widgets
            .get_mut(widget)
            .map(|widget| &mut widget.kind)
        else {
            return;
        };
        let text = field.text();
        let units: Vec<u16> = text.encode_utf16().collect();
        let before = String::from_utf16_lossy(&units[..field.caret().min(units.len())]);
        let digits_before = crate::text::digits_only(&before).len();
        let grouped = crate::text::group_with(&text, crate::text::thousands_separator());
        if grouped == text {
            return;
        }
        field.set_text(&grouped);
        let mut seen = 0;
        let mut caret = 0;
        for (at, unit) in grouped.encode_utf16().enumerate() {
            if seen == digits_before {
                caret = at;
                break;
            }
            if (b'0' as u16..=b'9' as u16).contains(&unit) {
                seen += 1;
            }
            caret = at + 1;
        }
        field.move_to(caret, false);
    }

    /// Sets one of the picker's values from where the pointer is on its
    /// track. The picker follows at once; the main window a moment later.
    fn slide_color(
        &mut self,
        arranged: &Arranged,
        widget: usize,
        channel: usize,
        x: f32,
        glide: bool,
    ) {
        let Some(track) = arranged
            .boxes
            .get(widget)
            .and_then(|b| b.items.get(picker_at::TRACKS + channel))
            .copied()
        else {
            return;
        };
        let t = ((x - track.l) / track.width().max(1.0)).clamp(0.0, 1.0);
        let Kind::Color {
            rgb, model, values, ..
        } = &mut self.form.widgets[widget].kind
        else {
            return;
        };
        values[channel] = t * model.ranges()[channel];
        if *model == Model::Rgb {
            values[channel] = values[channel].round();
        }
        let picked = model.to_rgb(*values);
        let changed = picked != *rgb;
        *rgb = picked;
        self.sync_thumbs(widget, glide);
        if changed {
            self.color_changed(widget);
        }
        self.kick();
    }

    /// Nudges a picker value from the keyboard.
    fn nudge_color(&mut self, widget: usize, channel: usize, by: f32) {
        let Kind::Color {
            rgb, model, values, ..
        } = &mut self.form.widgets[widget].kind
        else {
            return;
        };
        let range = model.ranges()[channel];
        values[channel] = (values[channel].round() + by).clamp(0.0, range);
        *rgb = model.to_rgb(*values);
        self.sync_thumbs(widget, true);
        self.changed(widget);
    }

    /// Puts a whole new color in the picker: the thumbs glide and the
    /// swatch fades to it.
    fn set_picked(&mut self, widget: usize, color: u32) {
        let time = self.form.time(anim::SELECT);
        let Kind::Color {
            rgb, model, values, ..
        } = &mut self.form.widgets[widget].kind
        else {
            return;
        };
        if *rgb != color {
            self.swatch_from = *rgb;
            self.swatch_fade.jump(0.0);
            self.swatch_fade.animate_to(1.0, time, Instant::now());
        }
        *rgb = color;
        *values = model.from_rgb(color, *values);
        self.sync_thumbs(widget, true);
        self.changed(widget);
    }

    /// Moves the picker's thumbs to its values.
    fn sync_thumbs(&mut self, widget: usize, glide: bool) {
        let Some(Kind::Color { model, values, .. }) =
            self.form.widgets.get(widget).map(|widget| &widget.kind)
        else {
            return;
        };
        let ranges = model.ranges();
        let time = self.form.time(anim::SELECT);
        let now = Instant::now();
        for channel in 0..3 {
            let to = (values[channel] / ranges[channel]).clamp(0.0, 1.0);
            if glide {
                self.thumbs[channel].animate_to(to, time, now);
            } else {
                self.thumbs[channel].jump(to);
            }
        }
    }

    /// A dragged color: shown here now, in the main window every
    /// `PREVIEW_EVERY` at most.
    fn color_changed(&mut self, widget: usize) {
        let due = self
            .preview_at
            .is_none_or(|at| at.elapsed() >= PREVIEW_EVERY);
        if due {
            self.flush_preview(widget);
        } else {
            self.preview_due = Some(widget);
        }
    }

    fn flush_preview(&mut self, widget: usize) {
        self.preview_due = None;
        self.preview_at = Some(Instant::now());
        self.changed(widget);
    }

    /// Opens or closes the popup of `widget`, closing any other.
    fn toggle_popup(&mut self, widget: usize) {
        let mut opened = false;
        for at in 0..self.form.widgets.len() {
            let kind = &mut self.form.widgets[at].kind;
            match kind {
                Kind::Color {
                    open,
                    rgb,
                    start,
                    model,
                    values,
                } => {
                    let open_now = at == widget && !*open;
                    if *open && !open_now && *rgb != *start {
                        color_space::remember_color(*rgb);
                    }
                    if open_now {
                        *start = *rgb;
                        *model = color_space::last_model();
                        *values = model.from_rgb(*rgb, *values);
                        opened = true;
                    }
                    *open = open_now;
                }
                Kind::Qr { open, .. } => {
                    let open_now = at == widget && !*open;
                    opened |= open_now;
                    *open = open_now;
                }
                _ => {}
            }
        }
        if opened {
            self.popup_since = Some(Instant::now());
            self.swatch_fade.jump(1.0);
            self.copied = None;
            self.sync_thumbs(widget, false);
        }
    }

    fn slide(&mut self, arranged: &Arranged, widget: usize, x: f32) {
        let Some(track) = arranged.boxes[widget].items.get(1).copied() else {
            return;
        };
        let t = ((x - track.l) / track.width().max(1.0)).clamp(0.0, 1.0);
        let to = 1 + (t * 99.0).round() as u32;
        self.set_slider(widget, to);
    }

    fn set_slider(&mut self, widget: usize, to: u32) {
        let id = self.form.widgets[widget].id;
        let to = to.clamp(1, 100);
        if self.form.value(id) != to {
            self.form.set_value(id, to);
            self.changed(widget);
        }
        self.invalidate();
    }

    fn changed(&mut self, widget: usize) {
        self.regroup(widget);
        let id = self.form.widgets[widget].id;
        self.dialog.on_change(&self.form, id);
        self.refresh_look();
        self.kick();
    }

    fn refresh_look(&mut self) {
        if let Some(look) = self.dialog.look(&self.form) {
            self.set_look(look);
        }
    }

    fn press_button(&mut self, index: usize) {
        let Some(id) = self
            .form
            .buttons
            .get(index)
            .filter(|button| !button.hidden)
            .map(|button| button.id)
        else {
            return;
        };
        let page = self.form.current_page();
        match self.dialog.on_button(&mut self.form, id) {
            Action::Stay => {
                // A button that moved to another page, like Next.
                if self.form.current_page() != page {
                    self.stop_recording();
                    self.scroll.jump(0.0);
                    self.set_hover(None);
                }
                self.refresh_look();
                self.apply_focus_request();
                self.kick();
            }
            Action::Accept => {
                self.outcome = self.dialog.take_outcome();
                self.finish();
            }
            Action::Cancel => self.finish(),
        }
    }

    fn press_primary(&mut self) {
        if let Some(index) = self
            .form
            .buttons
            .iter()
            .position(|button| button.style == ButtonStyle::Primary)
        {
            self.press_button(index);
        }
    }

    fn finish(&mut self) {
        self.done = true;
        unsafe {
            ShowWindow(self.hwnd, SW_HIDE);
            PostMessageW(self.hwnd, WM_FORM_DONE, 0, 0);
        }
    }

    /// Clicking or pressing Space on a part.
    fn activate(&mut self, part: Part) {
        match part {
            Part::Switch(widget) => {
                let id = self.form.widgets[widget].id;
                let on = self.form.is_on(id);
                self.form.set_on(id, !on);
                self.changed(widget);
            }
            Part::Segment(widget, option) => {
                let id = self.form.widgets[widget].id;
                if self.form.selected(id) != option {
                    self.form.set_selected(id, option);
                    self.changed(widget);
                }
            }
            Part::Check(widget, item) => {
                match &mut self.form.widgets[widget].kind {
                    Kind::Checks { items } => {
                        if let Some((_, on)) = items.get_mut(item) {
                            *on = !*on;
                        }
                    }
                    Kind::Order { items } => {
                        if let Some((_, _, on)) = items.get_mut(item) {
                            *on = !*on;
                        }
                    }
                    Kind::Drives { items } => {
                        if let Some(card) = items.get_mut(item) {
                            card.on = !card.on;
                        }
                    }
                    _ => {}
                }
                self.changed(widget);
            }
            Part::Remove(widget, item) => {
                if let Kind::Paths { items, .. } = &mut self.form.widgets[widget].kind {
                    if item < items.len() {
                        items.remove(item);
                    }
                    let remaining = items.len();
                    // Keep keyboard focus on the list.
                    if self.focus == Some(Part::Remove(widget, item)) {
                        self.focus = Some(if remaining == 0 {
                            Part::Add(widget)
                        } else {
                            Part::Remove(widget, item.min(remaining - 1))
                        });
                    }
                }
                self.set_hover(None);
                self.changed(widget);
            }
            Part::Add(widget) => {
                if let Ok(Some(path)) = picker::pick_folder(self.hwnd) {
                    if let Kind::Paths { items, .. } = &mut self.form.widgets[widget].kind {
                        if !items.iter().any(|item| item.eq_ignore_ascii_case(&path)) {
                            items.push(path);
                        }
                    }
                    self.changed(widget);
                }
            }
            Part::Button(index) => self.press_button(index),
            Part::Action(widget) => {
                let id = self.form.widgets[widget].id;
                self.dialog.on_action(&mut self.form, id);
                self.refresh_look();
                self.kick();
            }
            Part::Refresh(widget, item) => {
                if self.can_refresh(widget, item) {
                    let id = self.form.widgets[widget].id;
                    self.dialog.on_item_action(&mut self.form, id, item);
                    self.refresh_look();
                    self.kick();
                }
            }
            Part::Reveal(widget) => {
                if let Kind::Field { field, .. } = &mut self.form.widgets[widget].kind {
                    field.toggle_reveal();
                }
                self.kick();
            }
            Part::Color(widget) => {
                // One popup open at a time.
                self.toggle_popup(widget);
                self.hide_tip();
                self.set_hover(None);
                self.kick();
            }
            Part::Expander(widget) => {
                if let Kind::Expander { open, .. } = &mut self.form.widgets[widget].kind {
                    *open = !*open;
                }
                self.hide_tip();
                self.set_hover(None);
                self.kick();
            }
            Part::ColorPreset(widget, at) => {
                if let Some(&color) = picker_swatches().get(at) {
                    self.set_picked(widget, color);
                }
            }
            Part::ColorModel(widget) => {
                if let Kind::Color {
                    model, values, rgb, ..
                } = &mut self.form.widgets[widget].kind
                {
                    *model = model.next();
                    *values = model.from_rgb(*rgb, [0.0; 3]);
                    color_space::remember_model(*model);
                }
                self.sync_thumbs(widget, true);
                self.kick();
            }
            Part::ColorPick(widget) => {
                self.set_hover(None);
                self.pressed = None;
                if let Some(color) = super::eyedropper::pick(self.hwnd) {
                    self.set_picked(widget, color);
                }
                self.kick();
            }
            Part::ColorCopy(widget) => {
                if let Kind::Color { rgb, .. } = self.form.widgets[widget].kind {
                    if super::context_menu::copy_text(self.hwnd, &format!("#{rgb:06X}")).is_ok() {
                        self.copied = Some(Instant::now());
                        color_space::remember_color(rgb);
                    }
                }
                self.kick();
            }
            Part::ColorPaste(widget) => {
                match super::context_menu::paste_text(self.hwnd)
                    .as_deref()
                    .and_then(color_space::parse)
                {
                    Some(color) => self.set_picked(widget, color),
                    None => self.form.set_error(Some(
                        "The clipboard has no color code. Copy one like #5600D8 first.".into(),
                    )),
                }
                self.kick();
            }
            Part::ColorSlider(..) | Part::Popup => {}
            Part::Slider(_) => {}
            Part::Page(page) => self.show_page(page),
            Part::Field(widget) => self.set_focus(Some(Part::Field(widget)), true),
            Part::Thumb | Part::Track => {}
        }
        self.invalidate();
    }

    fn is_shortcut(&self, widget: usize) -> bool {
        matches!(
            self.form.widgets.get(widget).map(|widget| &widget.kind),
            Some(Kind::Field { shortcut: true, .. })
        )
    }

    fn field_mut(&mut self, widget: usize) -> Option<&mut TextField> {
        match &mut self.form.widgets.get_mut(widget)?.kind {
            Kind::Field { field, .. } => Some(field),
            _ => None,
        }
    }

    fn field_position(&self, arranged: &Arranged, widget: usize, x: f32) -> usize {
        let (Some(rect), Some(Widget { kind, .. })) = (
            arranged.boxes.get(widget).map(|b| b.control),
            self.form.widgets.get(widget),
        ) else {
            return 0;
        };
        let Kind::Field { field, scroll, .. } = kind else {
            return 0;
        };
        let Some(layout) = self
            .gfx
            .layout(&field.shown(), Font::Body, 100_000.0, FIELD_H)
        else {
            return 0;
        };
        paint::position_at(&layout, x - rect.l - 10.0 + scroll, FIELD_H / 2.0)
    }

    fn on_mouse_move(&mut self, x: f32, y: f32) {
        if !self.tracking {
            let mut event = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: self.hwnd,
                dwHoverTime: 0,
            };
            unsafe {
                TrackMouseEvent(&mut event);
            }
            self.tracking = true;
        }
        let arranged = self.arranged();
        match self.drag {
            Drag::Text(widget) => {
                let position = self.field_position(&arranged, widget, x);
                if let Some(field) = self.field_mut(widget) {
                    field.move_to(position, true);
                }
                self.caret_on = true;
                self.invalidate();
            }
            Drag::Slider(widget) => self.slide(&arranged, widget, x),
            Drag::Color { widget, channel } => {
                self.slide_color(&arranged, widget, channel, x, false)
            }
            Drag::Thumb {
                start_y,
                start_scroll,
            } => {
                if let Some((track, thumb)) = arranged.scrollbar {
                    let travel = (track.height() - thumb.height()).max(1.0);
                    let range = self.max_scroll(&arranged);
                    self.scroll_to(start_scroll + (y - start_y) * range / travel, false);
                }
            }
            Drag::Row {
                widget,
                item,
                grab,
                start_y,
                moving,
                ..
            } => {
                let content_y = y + self.scroll_now(&arranged);
                let moving = moving || (content_y - start_y).abs() > 4.0;
                self.drag = Drag::Row {
                    widget,
                    item,
                    grab,
                    start_y,
                    y: content_y,
                    moving,
                };
                if moving {
                    self.set_hover(None);
                    self.invalidate();
                }
            }
            Drag::None => {
                let part = self.hit(&arranged, x, y);
                self.set_hover(part);
                self.rest_on(&arranged, x, y);
            }
        }
    }

    /// Follows which label the pointer rests on, for its tip.
    fn rest_on(&mut self, arranged: &Arranged, x: f32, y: f32) {
        let inside =
            arranged.popup.is_none() && y >= TITLE_H && y < arranged.view_h && x >= arranged.left;
        let content_y = y + self.scroll_now(arranged);
        let over = inside
            .then(|| {
                arranged
                    .tips
                    .iter()
                    .find(|(rect, _)| rect.contains(x, content_y))
                    .map(|(_, widget)| *widget)
            })
            .flatten();
        if over == self.tip.map(|(widget, _)| widget) {
            return;
        }
        self.hide_tip();
        if let Some(widget) = over {
            self.tip = Some((widget, Instant::now()));
            unsafe {
                SetTimer(
                    self.hwnd,
                    TIMER_TIP,
                    TIP_DELAY.as_millis() as u32 + 20,
                    None,
                );
            }
        }
    }

    fn hide_tip(&mut self) {
        if let Some((_, since)) = self.tip.take() {
            if since.elapsed() >= TIP_DELAY {
                self.invalidate();
            }
        }
    }

    /// Moves entry `from` of a reorderable list to `to`.
    fn move_row(&mut self, widget: usize, from: usize, to: usize) {
        if let Some(Widget {
            kind: Kind::Order { items },
            ..
        }) = self.form.widgets.get_mut(widget)
        {
            if from < items.len() && to < items.len() && from != to {
                let moved = items.remove(from);
                items.insert(to, moved);
                self.focus = Some(Part::Check(widget, to));
                self.changed(widget);
            }
        }
    }

    /// Closes an open color picker. True when one was open.
    fn close_picker(&mut self) -> bool {
        let mut closed = false;
        for widget in &mut self.form.widgets {
            match &mut widget.kind {
                Kind::Color {
                    open, rgb, start, ..
                } => {
                    if *open && *rgb != *start {
                        color_space::remember_color(*rgb);
                    }
                    closed |= *open;
                    *open = false;
                }
                Kind::Qr { open, .. } => {
                    closed |= *open;
                    *open = false;
                }
                _ => {}
            }
        }
        if closed {
            self.set_hover(None);
            self.kick();
        }
        closed
    }

    fn on_mouse_down(&mut self, x: f32, y: f32, double: bool) {
        self.hide_tip();
        let arranged = self.arranged();
        let part = self.hit(&arranged, x, y);
        let in_picker = matches!(
            part,
            Some(
                Part::ColorSlider(..)
                    | Part::ColorModel(_)
                    | Part::ColorPick(_)
                    | Part::ColorCopy(_)
                    | Part::ColorPaste(_)
                    | Part::ColorPreset(..)
                    | Part::Popup
                    | Part::Color(_)
            )
        );
        if arranged.popup.is_some() && !in_picker && self.close_picker() {
            self.pressed = None;
            return;
        }
        self.pressed = part;
        unsafe {
            SetCapture(self.hwnd);
        }
        match part {
            Some(Part::Field(widget)) => {
                let position = self.field_position(&arranged, widget, x);
                let shift = key_down(VK_SHIFT);
                if let Some(field) = self.field_mut(widget) {
                    if double {
                        field.select_word_at(position);
                    } else {
                        field.move_to(position, shift);
                    }
                }
                self.set_focus(Some(Part::Field(widget)), false);
                self.drag = if double {
                    Drag::None
                } else {
                    Drag::Text(widget)
                };
            }
            Some(Part::Check(widget, item))
                if matches!(self.form.widgets[widget].kind, Kind::Order { .. }) =>
            {
                let content_y = y + self.scroll_now(&arranged);
                let row = arranged.boxes[widget].items[item];
                self.drag = Drag::Row {
                    widget,
                    item,
                    grab: content_y - row.t,
                    start_y: content_y,
                    y: content_y,
                    moving: false,
                };
                self.focus = Some(Part::Check(widget, item));
                self.focus_visible = false;
            }
            Some(Part::Slider(widget)) => {
                self.drag = Drag::Slider(widget);
                self.focus = part;
                self.focus_visible = false;
                self.slide(&arranged, widget, x);
            }
            Some(Part::ColorSlider(widget, channel)) => {
                self.drag = Drag::Color { widget, channel };
                self.focus = part;
                self.focus_visible = false;
                // A click glides the thumb there; dragging then follows.
                self.slide_color(&arranged, widget, channel, x, true);
            }
            Some(Part::Thumb) => {
                self.drag = Drag::Thumb {
                    start_y: y,
                    start_scroll: self.scroll_now(&arranged),
                };
            }
            Some(Part::Track) => {
                if let Some((_, thumb)) = arranged.scrollbar {
                    let page = arranged.view_h - 40.0;
                    let delta = if y < thumb.t { -page } else { page };
                    self.scroll_to(self.scroll.target() + delta, true);
                }
            }
            Some(part) => {
                self.focus = Some(part.stop());
                self.focus_visible = false;
            }
            None => {}
        }
        self.invalidate();
    }

    fn on_mouse_up(&mut self, x: f32, y: f32) {
        let pressed = self.pressed.take();
        let drag = std::mem::replace(&mut self.drag, Drag::None);
        unsafe {
            ReleaseCapture();
        }
        if let Drag::Row {
            widget,
            item,
            grab,
            y: content_y,
            moving: true,
            ..
        } = drag
        {
            let arranged = self.arranged();
            let rows = &arranged.boxes[widget].items;
            let slot = drop_slot(rows[0].t, content_y - grab, rows.len());
            self.move_row(widget, item, slot);
            self.invalidate();
            return;
        }
        if let (Drag::Color { widget, .. }, Some(_)) = (drag, self.preview_due) {
            // Let go: the main window gets the last color now.
            self.flush_preview(widget);
        }
        let dragging = !matches!(drag, Drag::None | Drag::Row { .. });
        if dragging {
            self.invalidate();
            return;
        }
        let arranged = self.arranged();
        let part = self.hit(&arranged, x, y);
        if part.is_some() && part == pressed && !matches!(part, Some(Part::Field(_))) {
            self.activate(part.unwrap_or(Part::Track));
        }
        self.invalidate();
    }

    fn on_wheel(&mut self, delta: i16) {
        let amount = -(delta as f32 / 120.0) * 72.0;
        self.scroll_to(self.scroll.target() + amount, true);
    }

    fn on_key(&mut self, key: u16) {
        let shift = key_down(VK_SHIFT);
        let control = key_down(VK_CONTROL);
        let pages = self.form.pages.len();
        if pages > 1 && control && (key == VK_TAB || key == VK_PRIOR || key == VK_NEXT) {
            let back = shift || key == VK_PRIOR;
            let page = self.form.current_page();
            self.show_page(if back {
                (page + pages - 1) % pages
            } else {
                (page + 1) % pages
            });
            return;
        }
        if self.focus == Some(Part::Page(0)) && pages > 1 {
            let page = self.form.current_page();
            match key {
                VK_UP | VK_LEFT => {
                    self.show_page(page.saturating_sub(1));
                    self.focus = Some(Part::Page(0));
                    self.focus_visible = true;
                    return;
                }
                VK_DOWN | VK_RIGHT => {
                    self.show_page((page + 1).min(pages - 1));
                    self.focus = Some(Part::Page(0));
                    self.focus_visible = true;
                    return;
                }
                _ => {}
            }
        }
        if key == VK_TAB {
            self.move_focus(shift);
            return;
        }
        if key == VK_ESCAPE {
            // Esc closes the color picker first, then the window.
            if !self.close_picker() {
                self.finish();
            }
            return;
        }
        let focus = self.focus;
        // A shortcut box that stopped listening starts again on Space or
        // Enter; its text is never edited by hand.
        if let Some(Part::Field(widget)) = focus {
            if self.is_shortcut(widget) {
                if key == VK_SPACE || key == VK_RETURN {
                    self.start_recording(widget);
                }
                return;
            }
        }
        if key == VK_RETURN {
            match focus {
                Some(Part::Button(index)) => self.press_button(index),
                Some(
                    Part::Add(_)
                    | Part::Action(_)
                    | Part::Expander(_)
                    | Part::Refresh(..)
                    | Part::Color(_)
                    | Part::ColorPreset(..)
                    | Part::ColorModel(_)
                    | Part::ColorPick(_)
                    | Part::ColorCopy(_)
                    | Part::ColorPaste(_)
                    | Part::Reveal(_),
                ) => self.activate(focus.unwrap_or(Part::Track)),
                _ => self.press_primary(),
            }
            return;
        }
        match focus {
            Some(Part::Field(widget)) => {
                let mut edited = false;
                let hwnd = self.hwnd;
                let Some(field) = self.field_mut(widget) else {
                    return;
                };
                match key {
                    VK_LEFT => field.move_left(shift, control),
                    VK_RIGHT => field.move_right(shift, control),
                    VK_HOME => field.home(shift),
                    VK_END => field.end(shift),
                    VK_BACK => edited = field.backspace(control),
                    VK_DELETE => edited = field.delete(control),
                    _ if control && key == b'A' as u16 => field.select_all(),
                    _ if control && key == b'C' as u16 => {
                        if field.has_selection() {
                            let _ = context_menu::copy_text(hwnd, &field.selected_text());
                        }
                    }
                    _ if control && key == b'X' as u16 => {
                        if field.has_selection() {
                            let _ = context_menu::copy_text(hwnd, &field.selected_text());
                            edited = field.delete_selection();
                        }
                    }
                    _ if control && key == b'V' as u16 => {
                        if let Some(text) = context_menu::paste_text(hwnd) {
                            let line = text.lines().next().unwrap_or("").to_string();
                            edited = field.insert(&line);
                        }
                    }
                    VK_UP => {
                        self.move_focus(true);
                        return;
                    }
                    VK_DOWN => {
                        self.move_focus(false);
                        return;
                    }
                    _ => return,
                }
                self.caret_on = true;
                if edited {
                    self.changed(widget);
                }
                self.invalidate();
            }
            Some(Part::Segment(widget, _)) => {
                let id = self.form.widgets[widget].id;
                let selected = self.form.selected(id);
                let count = match &self.form.widgets[widget].kind {
                    Kind::Segments { options, .. } => options.len(),
                    _ => 0,
                };
                let next = match key {
                    VK_LEFT | VK_UP => selected.saturating_sub(1),
                    VK_RIGHT | VK_DOWN => (selected + 1).min(count.saturating_sub(1)),
                    VK_HOME => 0,
                    VK_END => count.saturating_sub(1),
                    _ => return,
                };
                self.focus_visible = true;
                self.activate(Part::Segment(widget, next));
            }
            Some(Part::Check(widget, item))
                if control
                    && (key == VK_UP || key == VK_DOWN)
                    && matches!(self.form.widgets[widget].kind, Kind::Order { .. }) =>
            {
                let to = if key == VK_UP {
                    item.saturating_sub(1)
                } else {
                    item + 1
                };
                self.focus_visible = true;
                self.move_row(widget, item, to);
            }
            Some(Part::ColorSlider(widget, channel)) => {
                let by = match key {
                    VK_LEFT => -1.0,
                    VK_RIGHT => 1.0,
                    VK_NEXT => -10.0,
                    VK_PRIOR => 10.0,
                    VK_HOME => -1000.0,
                    VK_END => 1000.0,
                    VK_UP => return self.move_focus(true),
                    VK_DOWN => return self.move_focus(false),
                    _ => return,
                };
                self.focus_visible = true;
                self.nudge_color(widget, channel, if shift { by * 10.0 } else { by });
            }
            Some(Part::Slider(widget)) => {
                let id = self.form.widgets[widget].id;
                let value = self.form.value(id) as i32;
                let to = match key {
                    VK_LEFT => value - 1,
                    VK_RIGHT => value + 1,
                    VK_NEXT => value - 10,
                    VK_PRIOR => value + 10,
                    VK_HOME => 1,
                    VK_END => 100,
                    VK_UP => return self.move_focus(true),
                    VK_DOWN => return self.move_focus(false),
                    _ => return,
                };
                self.focus_visible = true;
                self.set_slider(widget, to.clamp(1, 100) as u32);
            }
            Some(
                part @ (Part::Switch(_)
                | Part::Check(..)
                | Part::Button(_)
                | Part::Add(_)
                | Part::Action(_)
                | Part::Expander(_)
                | Part::Color(_)
                | Part::ColorPreset(..)
                | Part::ColorModel(_)
                | Part::ColorPick(_)
                | Part::ColorCopy(_)
                | Part::ColorPaste(_)
                | Part::Reveal(_)
                | Part::Refresh(..)),
            ) => match key {
                VK_SPACE => {
                    self.focus_visible = true;
                    self.activate(part);
                }
                VK_UP | VK_LEFT => self.move_focus(true),
                VK_DOWN | VK_RIGHT => self.move_focus(false),
                _ => self.scroll_keys(key),
            },
            Some(part @ Part::Remove(..)) => match key {
                VK_SPACE | VK_DELETE => {
                    self.focus_visible = true;
                    self.activate(part);
                }
                VK_UP => self.move_focus(true),
                VK_DOWN => self.move_focus(false),
                _ => self.scroll_keys(key),
            },
            _ => match key {
                VK_UP | VK_DOWN | VK_PRIOR | VK_NEXT | VK_HOME | VK_END => self.scroll_keys(key),
                _ => {}
            },
        }
    }

    fn scroll_keys(&mut self, key: u16) {
        let arranged = self.arranged();
        let page = arranged.view_h - 40.0;
        let target = self.scroll.target();
        let next = match key {
            VK_PRIOR => target - page,
            VK_NEXT => target + page,
            VK_HOME if self.focus.is_none() => 0.0,
            VK_END if self.focus.is_none() => f32::MAX,
            _ => return,
        };
        self.scroll_to(next, true);
    }

    fn on_char(&mut self, unit: u16) {
        if unit < 0x20 || unit == 0x7f {
            return;
        }
        if let Some(Part::Field(widget)) = self.focus {
            if self.is_shortcut(widget) {
                return;
            }
            let numeric = matches!(
                self.form.widgets.get(widget).map(|widget| &widget.kind),
                Some(Kind::Field { numeric: true, .. })
            );
            if numeric && !(b'0' as u16..=b'9' as u16).contains(&unit) {
                return;
            }
            if let Some(field) = self.field_mut(widget) {
                field.insert_units(&[unit]);
            }
            self.caret_on = true;
            self.changed(widget);
        }
    }

    /// Keeps each field's caret inside its visible box.
    fn scroll_fields(&mut self, arranged: &Arranged) {
        for (index, widget) in self.form.widgets.iter_mut().enumerate() {
            let Kind::Field { field, scroll, .. } = &mut widget.kind else {
                continue;
            };
            let eye = if field.is_secret() { 34.0 } else { 0.0 };
            let width = arranged.boxes[index].control.width() - 20.0 - eye;
            let Some(layout) = self
                .gfx
                .layout(&field.shown(), Font::Body, 100_000.0, FIELD_H)
            else {
                continue;
            };
            let caret = paint::caret_offset(&layout, field.caret());
            let end = paint::caret_offset(&layout, field.wide().len());
            if caret - *scroll > width {
                *scroll = caret - width;
            }
            if caret < *scroll {
                *scroll = caret;
            }
            *scroll = scroll.min((end - width).max(0.0)).max(0.0);
        }
    }

    fn draw(&mut self, rt: &ID2D1RenderTarget, opaque: bool) -> windows::core::Result<()> {
        let arranged = self.arranged();
        // A page that got shorter (or a taller window) pulls the scroll back,
        // so drawing and clicks agree on where everything is.
        let max = self.max_scroll(&arranged);
        if self.scroll.target() > max {
            self.scroll.jump(max);
        }
        self.scroll_fields(&arranged);
        let now = Instant::now();
        let view = View {
            pal: self.now_palette(now),
            arranged: &arranged,
            form: &self.form,
            scroll: self
                .scroll
                .value(now)
                .clamp(0.0, self.max_scroll(&arranged)),
            fades: &self.fades,
            hover: self.hover,
            pressed: self.pressed,
            focus: self.focus,
            focus_visible: self.focus_visible,
            recording: self.recording.as_ref().map(|recording| {
                let names = hotkey::held_names(&recording.held);
                // Names can merge two held keys (both Shifts); pop by the
                // newest key's time.
                let newest = recording.since.last().copied();
                (recording.widget, names, newest, recording.started)
            }),
            committed: self.committed,
            caret: self.caret_on && self.active && !opaque,
            opaque: opaque || effective_material(self.look.material) == Material::Solid,
            now,
            title: &self.title,
            icons: &self.icons,
            drag: self.drag,
            tip: self
                .tip
                .filter(|(_, since)| since.elapsed() >= TIP_DELAY)
                .map(|(widget, _)| widget),
            popup_in: match self.popup_since {
                Some(at) if self.form.animate => anim::ease_out(
                    (now.duration_since(at).as_secs_f32() / POPUP_IN.as_secs_f32()).min(1.0),
                ),
                _ => 1.0,
            },
            thumbs: [
                self.thumbs[0].value(now),
                self.thumbs[1].value(now),
                self.thumbs[2].value(now),
            ],
            swatch: (self.swatch_from, self.swatch_fade.value(now)),
            copied: self
                .copied
                .is_some_and(|at| now.duration_since(at) < COPIED_FOR),
            caption_hole: (!opaque && effective_material(self.look.material) == Material::Solid)
                .then(|| {
                    let (w, h) = super::caption_bounds(self.hwnd);
                    Rect::new(arranged.width - w, 0.0, arranged.width, h)
                }),
        };
        let Some(painter) = Painter::new(rt, &self.gfx) else {
            return Ok(());
        };
        unsafe {
            rt.BeginDraw();
            rt.SetTextAntialiasMode(
                windows::Win32::Graphics::Direct2D::D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE,
            );
            view.paint(&painter);
            rt.EndDraw(None, None)
        }
    }

    fn render(&mut self, hdc: windows_sys::Win32::Graphics::Gdi::HDC) {
        let (width, height) = client_size(self.hwnd);
        let mut canvas = std::mem::take(&mut self.canvas);
        let d2d = self.gfx.d2d.clone();
        let keep = self.anim_timer;
        let presented = canvas.present(&d2d, hdc, width, height, keep, |rt| self.draw(rt, false));
        self.canvas = canvas;
        if let canvas::Presented::Lost = presented {
            self.invalidate();
        }
    }

    fn capture(&mut self, path: &std::path::Path) -> Result<(), String> {
        let (width, height) = client_size(self.hwnd);
        self.form.settle();
        self.fades.settle();
        self.scroll.settle();
        self.theme.settle();
        self.theme_from = None;
        let d2d = self.gfx.d2d.clone();
        let wic = self.gfx.wic.clone();
        canvas::capture_png(
            &d2d,
            &wic,
            width.max(0) as u32,
            height.max(0) as u32,
            path,
            |rt| self.draw(rt, true),
        )
    }
}

fn key_down(key: u16) -> bool {
    unsafe { GetKeyState(key as i32) < 0 }
}

fn effective_material(material: Material) -> Material {
    if theme::backdrop_supported() {
        material
    } else {
        Material::Solid
    }
}

// ----- Painting ---------------------------------------------------------------

enum IconLook<'a> {
    Drive(&'a str),
    Folder(&'a str),
}

struct View<'a> {
    pal: Palette,
    arranged: &'a Arranged,
    form: &'a Form,
    scroll: f32,
    fades: &'a Fades<Part>,
    hover: Option<Part>,
    pressed: Option<Part>,
    focus: Option<Part>,
    focus_visible: bool,
    /// The listening shortcut box: its widget, the held keys' labels, when
    /// the newest went down, and when listening began.
    recording: Option<(usize, Vec<String>, Option<Instant>, Instant)>,
    committed: Option<(usize, Instant)>,
    caret: bool,
    opaque: bool,
    now: Instant,
    title: &'a str,
    icons: &'a std::cell::RefCell<IconCache>,
    /// Pixels to leave see-through so Windows' close button shows (solid
    /// background only).
    caption_hole: Option<Rect>,
    drag: Drag,
    /// The widget whose tip shows.
    tip: Option<usize>,
    /// The popup sliding in, 0 to 1.
    popup_in: f32,
    /// The picker's thumbs, 0 to 1 along their tracks.
    thumbs: [f32; 3],
    /// The picker's swatch fading from an earlier color, and how far.
    swatch: (u32, f32),
    /// The code was just copied.
    copied: bool,
}

impl View<'_> {
    fn level(&self, part: Part) -> f32 {
        if self.pressed == Some(part) && self.hover == Some(part) {
            return 2.0;
        }
        self.fades.level(part, self.now)
    }

    fn at(&self, rect: Rect) -> Rect {
        shift(rect, -self.scroll)
    }

    fn focused(&self, part: Part) -> bool {
        self.focus_visible && self.focus == Some(part.stop())
    }

    fn ring(&self, p: &Painter, rect: Rect, radius: f32) {
        p.stroke_round(rect.inset(-3.0, -3.0), radius + 3.0, self.pal.accent, 2.0);
    }

    /// A shortcut box: its keys as key caps, or a pulsing prompt while it
    /// listens.
    fn paint_shortcut(&self, p: &Painter, index: usize, rect: Rect, field: &TextField) {
        let pal = &self.pal;
        let listening = self
            .recording
            .as_ref()
            .filter(|(widget, ..)| *widget == index);
        let (names, newest): (Vec<String>, Option<Instant>) = match listening {
            Some((_, held, newest, _)) if !held.is_empty() => (held.clone(), *newest),
            Some(_) => (Vec::new(), None),
            None => {
                let text = field.text();
                let names = if text.trim().is_empty() {
                    Vec::new()
                } else {
                    text.split(" + ").map(str::to_string).collect()
                };
                (names, None)
            }
        };
        let level = self.level(Part::Field(index)).min(1.0);
        p.fill_round(rect, 8.0, pal.text.fade(0.035 + 0.02 * level));
        if let Some((_, _, _, started)) = listening {
            let t = self.now.duration_since(*started).as_secs_f32();
            let pulse = 0.55 + 0.45 * (t * std::f32::consts::TAU / 1.4).cos();
            p.stroke_round(rect, 8.0, pal.accent.fade(pulse), 1.6);
            p.stroke_round(
                rect.inset(-3.0, -3.0),
                11.0,
                pal.accent.fade(0.18 * pulse),
                3.0,
            );
        } else {
            p.stroke_round(rect, 8.0, pal.line.mix(pal.text2, 0.3 * level), 1.0);
        }
        if let Some((widget, at)) = self.committed {
            let age = self.now.duration_since(at).as_secs_f32() / GLOW.as_secs_f32();
            if widget == index && age < 1.0 {
                let fade = 1.0 - age;
                p.stroke_round(
                    rect.inset(-2.0 - 4.0 * age, -2.0 - 4.0 * age),
                    10.0 + 4.0 * age,
                    pal.accent.fade(0.6 * fade),
                    2.0,
                );
            }
        }
        let mid = rect.t + rect.height() / 2.0;
        if names.is_empty() {
            let (text, color) = match listening {
                Some((_, _, _, started)) => {
                    let t = self.now.duration_since(*started).as_secs_f32();
                    let pulse = 0.65 + 0.35 * (t * std::f32::consts::TAU / 1.4).cos();
                    ("Press any keys together", pal.accent.fade(pulse))
                }
                None => ("None. Click to choose keys", pal.text2.fade(0.7)),
            };
            p.text(
                text,
                Font::Body,
                Rect::new(rect.l + 12.0, rect.t, rect.r - 12.0, rect.b),
                color,
            );
        } else {
            let mut x = rect.l + 8.0;
            let count = names.len();
            for (at, name) in names.iter().enumerate() {
                let width = p.gfx.measure_width(name, Font::Small).ceil() + 20.0;
                // The newest key pops in.
                let scale = match newest {
                    Some(when) if at + 1 == count => {
                        let t = (self.now.duration_since(when).as_secs_f32() / 0.14).min(1.0);
                        0.8 + 0.2 * anim::ease_out(t)
                    }
                    _ => 1.0,
                };
                let (w, h) = (width * scale, 24.0 * scale);
                let left = x + (width - w) / 2.0;
                let cap = Rect::new(left, mid - h / 2.0 - 1.0, left + w, mid + h / 2.0 - 1.0);
                // A key cap: a darker base under a lighter top.
                p.fill_round(
                    Rect::new(cap.l, cap.t + 2.5, cap.r, cap.b + 2.5),
                    6.0,
                    pal.text.fade(0.16),
                );
                p.fill_round(cap, 6.0, pal.opaque_base().mix(pal.text, 0.12));
                p.stroke_round(cap, 6.0, pal.text.fade(0.12), 1.0);
                let text_w = p.gfx.measure_width(name, Font::Small);
                let tx = cap.l + (cap.width() - text_w) / 2.0;
                p.text(
                    name,
                    Font::Small,
                    Rect::new(tx, cap.t, cap.r + 4.0, cap.b),
                    pal.text,
                );
                x += width + 6.0;
            }
        }
        if listening.is_some() {
            let hint = "Esc cancels";
            let w = p.gfx.measure_width(hint, Font::Small).ceil();
            p.text(
                hint,
                Font::Small,
                Rect::new(rect.r - 12.0 - w, rect.t, rect.r - 8.0, rect.b),
                pal.text2,
            );
        }
    }

    fn paint(&self, p: &Painter) {
        let pal = &self.pal;
        let a = self.arranged;
        let client = Rect::new(0.0, 0.0, a.width, a.height);
        if self.opaque && pal.base.a < 1.0 {
            p.clear(pal.opaque_base());
        } else {
            p.clear(pal.base);
        }
        p.fill(client, pal.tint);
        self.paint_chrome(p);

        p.clip(Rect::new(a.left, TITLE_H, a.width, a.view_h));
        if let Some((title, _)) = self.form.pages.get(self.form.page) {
            p.text(title, Font::Title, self.at(a.page_title), pal.text);
        }
        for &card in &a.cards {
            let card = self.at(card);
            p.fill_round(card, 12.0, pal.card);
            p.stroke_round(card, 12.0, pal.line, 1.0);
        }
        for &divider in &a.dividers {
            p.fill(self.at(divider), pal.line);
        }
        for (index, widget) in self.form.widgets.iter().enumerate() {
            if !a.shown.get(index).copied().unwrap_or(false) {
                continue;
            }
            self.paint_widget(p, index, widget);
        }
        if let Some(panel) = a.popup {
            let open = self.form.widgets.iter().position(|widget| {
                matches!(
                    widget.kind,
                    Kind::Color { open: true, .. } | Kind::Qr { open: true, .. }
                )
            });
            if let (Some(index), Some(b)) = (open, open.and_then(|index| a.boxes.get(index))) {
                let panel = self.at(panel);
                // Slides down into place, revealed from the top.
                let t = self.popup_in;
                let reveal = Rect::new(
                    panel.l - 20.0,
                    panel.t - 20.0,
                    panel.r + 20.0,
                    panel.t + (panel.height() + 40.0) * t,
                );
                p.clip(reveal);
                // A soft shadow, then an opaque panel.
                for (grow, alpha) in [(10.0, 0.06), (6.0, 0.08), (3.0, 0.10)] {
                    let shadow = Rect::new(
                        panel.l - grow,
                        panel.t - grow + 6.0,
                        panel.r + grow,
                        panel.b + grow + 6.0,
                    );
                    p.fill_round(shadow, 14.0 + grow, Color::hex(0).alpha(alpha));
                }
                p.fill_round(panel, 12.0, pal.ground);
                p.fill_round(panel, 12.0, pal.card);
                p.stroke_round(panel, 12.0, pal.line.mix(pal.text2, 0.3), 1.0);
                match &self.form.widgets[index].kind {
                    Kind::Color {
                        rgb, model, values, ..
                    } => self.paint_picker(p, index, *rgb, *model, *values, b),
                    Kind::Qr {
                        size,
                        modules,
                        waiting,
                        ..
                    } => self.paint_qr(p, *size, modules, waiting, b),
                    _ => {}
                }
                // Again over the content, so the border runs all the way
                // around, the picker's swatch included.
                p.stroke_round(panel, 12.0, pal.line.mix(pal.text2, 0.3), 1.0);
                p.unclip();
            }
        }
        self.paint_tip(p);
        p.unclip();

        if let Some((track, thumb)) = a.scrollbar {
            let level = if self.pressed == Some(Part::Thumb) {
                1.0
            } else {
                self.level(Part::Thumb)
                    .max(self.level(Part::Track))
                    .min(1.0)
            };
            if level > 0.0 {
                p.fill_round(track, 4.0, pal.hover.fade(level));
            }
            let thumb = Rect::new(thumb.l + 3.0 * (1.0 - level), thumb.t, thumb.r, thumb.b);
            p.fill_round(thumb, 3.0, pal.scrollbar);
        }

        // Footer.
        p.fill(
            Rect::new(a.left, a.view_h, a.width, a.view_h + 1.0),
            pal.line,
        );
        if let Some(error) = &self.form.error {
            p.text(error, Font::Body, a.error, pal.danger);
        }
        for (index, button) in self.form.buttons.iter().enumerate() {
            if button.hidden {
                continue;
            }
            let rect = a.buttons[index];
            let part = Part::Button(index);
            let level = self.level(part);
            match button.style {
                ButtonStyle::Primary => {
                    let fill = if level > 1.0 {
                        pal.accent.mix(Color::hex(0x000000), 0.12)
                    } else {
                        pal.accent.mix(pal.text, 0.10 * level)
                    };
                    p.fill_round(rect, 10.0, fill);
                    p.text(&button.label, Font::Button, rect, pal.on_accent);
                }
                ButtonStyle::Secondary => {
                    if level > 0.0 {
                        p.fill_round(rect, 10.0, pal.hover.alpha(pal.hover.a * level));
                    }
                    p.stroke_round(rect, 10.0, pal.line, 1.0);
                    p.text(&button.label, Font::ButtonLight, rect, pal.text);
                }
                ButtonStyle::Quiet => {
                    if level > 0.0 {
                        p.fill_round(rect, 10.0, pal.hover.alpha(pal.hover.a * level));
                    }
                    p.text(&button.label, Font::ButtonLight, rect, pal.text2);
                }
            }
            if self.focused(part) {
                self.ring(p, rect, 10.0);
            }
        }
        if let Some(hole) = self.caption_hole {
            p.clip(hole);
            p.clear(Color::hex(0).alpha(0.0));
            p.unclip();
        }
    }

    /// Draws a shell icon: a drive's own icon, or the folder icon.
    fn icon(&self, p: &Painter, look: IconLook, rect: Rect) {
        let mut icons = self.icons.borrow_mut();
        let (index, size) = match look {
            IconLook::Drive(root) => (icons.index_for_scope(root), IconSize::Large),
            IconLook::Folder(path) => {
                let trimmed = path.trim_end_matches('\\');
                if trimmed.len() == 2 && trimmed.ends_with(':') {
                    (icons.index_for_scope(trimmed), IconSize::Small)
                } else {
                    (icons.index_for("", true), IconSize::Small)
                }
            }
        };
        if let Some(icon) = icons.bitmap(&p.gfx.wic, p.rt, index, size, false) {
            p.bitmap(&icon.bitmap, rect, None, 1.0);
        }
    }

    fn check_box(&self, p: &Painter, x: f32, cy: f32, on: bool, level: f32) {
        let pal = &self.pal;
        let boxr = Rect::new(x, cy - 9.0, x + 18.0, cy + 9.0);
        if on {
            p.fill_round(boxr, 5.0, pal.accent);
            p.line(x + 4.5, cy + 0.5, x + 7.8, cy + 3.8, pal.on_accent, 1.8);
            p.line(x + 7.8, cy + 3.8, x + 13.5, cy - 3.2, pal.on_accent, 1.8);
        } else {
            p.stroke_round(boxr, 5.0, pal.text2.mix(pal.text, level), 1.4);
        }
    }

    /// Reorderable entries: grip, icon, name, and check box. A dragged entry
    /// floats under the pointer and the others make room.
    fn paint_order(
        &self,
        p: &Painter,
        index: usize,
        items: &[(String, Category, bool)],
        b: &WidgetBox,
    ) {
        let pal = &self.pal;
        let step = ITEM_H + 2.0;
        let dragging = match self.drag {
            Drag::Row {
                widget,
                item,
                grab,
                y,
                moving: true,
                ..
            } if widget == index => Some((item, y - grab)),
            _ => None,
        };
        let slot = dragging.map(|(from, top)| (from, drop_slot(b.items[0].t, top, items.len())));
        let mut lifted = None;
        for (item, (label, category, on)) in items.iter().enumerate() {
            let mut rect = b.items[item];
            if let Some((from, to)) = slot {
                if item == from {
                    let top = dragging.map(|(_, top)| top).unwrap_or(rect.t);
                    lifted = Some((item, Rect::new(rect.l, top, rect.r, top + ITEM_H)));
                    continue;
                }
                if from < item && item <= to {
                    rect = shift(rect, -step);
                } else if to <= item && item < from {
                    rect = shift(rect, step);
                }
            }
            self.paint_order_row(p, index, item, label, *category, *on, self.at(rect), false);
        }
        if let Some((item, rect)) = lifted {
            let (label, category, on) = &items[item];
            let rect = self.at(rect);
            p.fill_round(rect.inset(-1.0, -1.0), 9.0, Color::hex(0).alpha(0.18));
            p.fill_round(rect, 8.0, pal.opaque_base().mix(pal.text, 0.07));
            p.stroke_round(rect, 8.0, pal.accent.fade(0.55), 1.2);
            self.paint_order_row(p, index, item, label, *category, *on, rect, true);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_order_row(
        &self,
        p: &Painter,
        index: usize,
        item: usize,
        label: &str,
        category: Category,
        on: bool,
        row: Rect,
        lifted: bool,
    ) {
        let pal = &self.pal;
        let part = Part::Check(index, item);
        let level = if lifted {
            0.0
        } else {
            self.level(part).min(1.0)
        };
        if level > 0.0 {
            p.fill_round(row, 8.0, pal.hover.fade(level));
        }
        let cy = row.t + row.height() / 2.0;
        // The grip: two columns of three dots.
        for column in 0..2 {
            for dot in 0..3 {
                let (x, y) = (
                    row.l + 10.0 + column as f32 * 4.5,
                    cy - 4.5 + dot as f32 * 4.5,
                );
                p.fill_round(
                    Rect::new(x - 1.1, y - 1.1, x + 1.1, y + 1.1),
                    1.1,
                    pal.text2.fade(0.8),
                );
            }
        }
        let icon = Rect::new(row.l + 26.0, cy - 9.0, row.l + 44.0, cy + 9.0);
        let tint = glyphs::tint(category, pal.accent, pal.dark);
        glyphs::draw(p, category, icon, if on { tint } else { tint.fade(0.45) });
        p.text(
            label,
            Font::Body,
            Rect::new(row.l + 54.0, row.t, row.r - 40.0, row.b),
            if on { pal.text } else { pal.text2 },
        );
        self.check_box(p, row.r - 30.0, cy, on, level);
        if self.focused(part) && !lifted {
            self.ring(p, row, 8.0);
        }
    }

    /// Feather's color picker: a saturation and brightness pad for the
    /// current hue, a hue bar, and ready-made colors.
    fn paint_picker(
        &self,
        p: &Painter,
        index: usize,
        rgb: u32,
        model: Model,
        values: [f32; 3],
        b: &WidgetBox,
    ) {
        let pal = &self.pal;
        let item = |at: usize| b.items.get(at).map(|rect| self.at(*rect));
        let Some(swatch) = item(picker_at::SWATCH) else {
            return;
        };
        // The color itself, large, fading in from the one before.
        let (from, t) = self.swatch;
        let shown = if t < 1.0 {
            Color::hex(from).mix(Color::hex(rgb), t)
        } else {
            Color::hex(rgb)
        };
        p.fill_round(swatch, 12.0, shown);
        p.fill(
            Rect::new(swatch.l, swatch.t + 12.0, swatch.r, swatch.b),
            shown,
        );

        if let Some(hex) = item(picker_at::HEX) {
            p.text(
                &format!("#{rgb:06X}"),
                Font::Body,
                hex,
                pal.text2.mix(pal.text, 0.35),
            );
        }
        if let Some(rect) = item(picker_at::MODEL) {
            let part = Part::ColorModel(index);
            let level = self.level(part).min(1.0);
            if level > 0.0 {
                p.fill_round(rect, 8.0, pal.hover.fade(level * 1.6));
            }
            let text_w = p.gfx.measure_width(model.name(), Font::ButtonLight).ceil();
            let left = rect.r - 22.0 - text_w;
            p.text(
                model.name(),
                Font::ButtonLight,
                Rect::new(left, rect.t, rect.r - 18.0, rect.b),
                pal.text2.mix(pal.text, 0.35 + 0.5 * level),
            );
            let (cx, cy) = (rect.r - 11.0, rect.t + rect.height() / 2.0);
            p.polyline(
                &[(cx - 4.0, cy - 2.0), (cx, cy + 2.0), (cx + 4.0, cy - 2.0)],
                false,
                pal.text2,
                1.5,
            );
            if self.focused(part) {
                self.ring(p, rect, 8.0);
            }
        }
        // Pick from screen, Copy, Paste: an icon and a word each.
        let buttons = [
            (picker_at::PICK, Part::ColorPick(index), "Pick"),
            (
                picker_at::COPY,
                Part::ColorCopy(index),
                if self.copied { "Copied" } else { "Copy" },
            ),
            (picker_at::PASTE, Part::ColorPaste(index), "Paste"),
        ];
        for (at, part, word) in buttons {
            let Some(rect) = item(at) else {
                continue;
            };
            let level = self.level(part).min(1.0);
            p.fill_round(rect, 8.0, pal.text.fade(0.05));
            if level > 0.0 {
                p.fill_round(rect, 8.0, pal.hover.fade(level * 1.6));
            }
            p.stroke_round(rect, 8.0, pal.line, 1.0);
            let done = matches!(part, Part::ColorCopy(_)) && self.copied;
            let ink = if done { pal.accent } else { pal.text };
            let text_w = p.gfx.measure_width(word, Font::Small).ceil();
            let left = rect.l + (rect.width() - (14.0 + 6.0 + text_w)) / 2.0;
            // Windows' own glyphs: eyedropper, copy, paste, check mark.
            let glyph = match part {
                Part::ColorPick(_) => "\u{ef3c}",
                Part::ColorCopy(_) if done => "\u{e73e}",
                Part::ColorCopy(_) => "\u{e8c8}",
                _ => "\u{e77f}",
            };
            p.text(
                glyph,
                Font::Icon,
                Rect::new(left - 3.0, rect.t + 1.0, left + 17.0, rect.b + 1.0),
                ink,
            );
            p.text(
                word,
                Font::Small,
                Rect::new(left + 20.0, rect.t, rect.r, rect.b),
                ink,
            );
            if self.focused(part) {
                self.ring(p, rect, 8.0);
            }
        }
        let ranges = model.ranges();
        for channel in 0..3 {
            let (Some(track), Some(value)) = (
                item(picker_at::TRACKS + channel),
                item(picker_at::VALUES + channel),
            ) else {
                continue;
            };
            let stops: Vec<(f32, Color)> = model
                .track(values, channel)
                .into_iter()
                .map(|(t, color)| (t, Color::hex(color)))
                .collect();
            let radius = track.height() / 2.0;
            p.fill_gradient(track, radius, &stops, false);
            p.stroke_round(track, radius, Color::hex(0xffffff).fade(0.12), 1.0);
            let part = Part::ColorSlider(index, channel);
            let level = self.level(part).min(1.0);
            let span = track.width() - track.height();
            let x = track.l + radius + self.thumbs[channel].clamp(0.0, 1.0) * span;
            let y = track.t + radius;
            let grow = 1.0 + 0.15 * level;
            let size = 7.0 * grow;
            p.circle(x, y + 1.0, size + 2.0, Color::hex(0).alpha(0.28), 1.5);
            p.fill_round(
                Rect::new(x - size, y - size, x + size, y + size),
                size,
                Color::hex(rgb),
            );
            p.circle(x, y, size, Color::hex(0xffffff), 2.0);
            if self.focused(part) {
                self.ring(p, Rect::new(x - 9.0, y - 9.0, x + 9.0, y + 9.0), 9.0);
            }
            let text = format!("{:.0}", values[channel].clamp(0.0, ranges[channel]));
            let text_w = p.gfx.measure_width(&text, Font::Small).ceil();
            p.text(
                &text,
                Font::Small,
                Rect::new(value.r - text_w, value.t, value.r + 2.0, value.b),
                pal.text2.mix(pal.text, 0.35),
            );
        }

        let swatches = picker_swatches();
        let recent = color_space::recent().len();
        if let Some(label) = item(picker_at::LABEL) {
            let title = if recent > 0 {
                "Recent colors"
            } else {
                "Suggested colors"
            };
            p.text(title, Font::Small, label, pal.text2.mix(pal.text, 0.5));
        }
        for (at, color) in swatches.iter().enumerate() {
            let Some(rect) = item(picker_at::CELLS + at) else {
                break;
            };
            let part = Part::ColorPreset(index, at);
            let level = self.level(part).min(1.0);
            let lift = if level > 0.0 { 1.0 * level } else { 0.0 };
            let rect = Rect::new(rect.l - lift, rect.t - lift, rect.r + lift, rect.b + lift);
            p.fill_round(rect, 7.0, Color::hex(*color));
            p.stroke_round(rect, 7.0, pal.text.fade(0.14 + 0.2 * level), 1.0);
            if *color == rgb {
                p.stroke_round(rect.inset(-2.5, -2.5), 9.0, pal.accent, 2.0);
            }
            if self.focused(part) {
                self.ring(p, rect, 7.0);
            }
        }
    }

    /// The round-arrow button on a drive card or a folder row.
    fn paint_refresh(&self, p: &Painter, part: Part, rect: Rect) {
        let pal = &self.pal;
        let level = self.level(part).min(1.0);
        // A quiet round button, so it reads as something to press.
        p.fill_round(
            rect,
            rect.height() / 2.0,
            pal.text.fade(0.05 + 0.07 * level),
        );
        p.refresh(
            rect.l + rect.width() / 2.0,
            rect.t + rect.height() / 2.0,
            5.5,
            pal.text2.mix(pal.accent, level),
            1.6,
        );
        if self.focused(part) {
            self.ring(p, rect, rect.height() / 2.0);
        }
    }

    /// Drive cards: Explorer icon, name, free space with a bar, and a round
    /// check that turns accent when the drive is searched.
    fn paint_drives(&self, p: &Painter, index: usize, items: &[DriveCard], b: &WidgetBox) {
        let pal = &self.pal;
        for (item, card) in items.iter().enumerate() {
            let rect = self.at(b.items[item]);
            let part = Part::Check(index, item);
            let level = self.level(part).min(1.0);
            let fill = if card.on {
                pal.accent.fade(0.10 + 0.04 * level)
            } else {
                pal.text.fade(0.03 + 0.03 * level)
            };
            p.fill_round(rect, 10.0, fill);
            let border = if card.on {
                pal.accent.fade(0.65)
            } else {
                pal.line.mix(pal.text2, 0.3 * level)
            };
            p.stroke_round(rect, 10.0, border, 1.2);
            let cy = rect.t + rect.height() / 2.0;
            self.icon(
                p,
                IconLook::Drive(&card.root),
                Rect::new(rect.l + 14.0, cy - 16.0, rect.l + 46.0, cy + 16.0),
            );
            let text_l = rect.l + 58.0;
            let text_r = rect.r - 40.0;
            p.text(
                &card.label,
                Font::Body,
                Rect::new(text_l, rect.t + 12.0, text_r, rect.t + 32.0),
                pal.text,
            );
            p.text(
                &card.space,
                Font::Small,
                Rect::new(text_l, rect.t + 32.0, text_r, rect.t + 48.0),
                pal.text2,
            );
            let bar = Rect::new(text_l, rect.t + 52.0, text_r, rect.t + 56.0);
            p.fill_round(bar, 2.0, pal.text.fade(0.10));
            let used = card.used.clamp(0.0, 1.0);
            if used > 0.0 {
                let color = if used > 0.9 {
                    pal.danger
                } else if card.on {
                    pal.accent
                } else {
                    pal.text2
                };
                p.fill_round(
                    Rect::new(bar.l, bar.t, bar.l + bar.width() * used, bar.b),
                    2.0,
                    color,
                );
            }
            // A round check at the top right.
            let (cx, ty) = (rect.r - 22.0, rect.t + 22.0);
            let circle = Rect::new(cx - 9.0, ty - 9.0, cx + 9.0, ty + 9.0);
            if card.on {
                p.fill_round(circle, 9.0, pal.accent);
                p.line(cx - 4.0, ty + 0.5, cx - 1.2, ty + 3.3, pal.on_accent, 1.8);
                p.line(cx - 1.2, ty + 3.3, cx + 4.2, ty - 2.8, pal.on_accent, 1.8);
            } else {
                p.stroke_round(circle, 9.0, pal.text2.mix(pal.text, level), 1.4);
            }
            if self.focused(part) {
                self.ring(p, rect, 10.0);
            }
            // Searched drives can be read again on their own.
            if let (true, Some(&again)) = (card.on, b.extra.get(item)) {
                self.paint_refresh(p, Part::Refresh(index, item), self.at(again));
            }
        }
    }

    /// The title strip and, for dialogs with pages, the page list on the
    /// left, styled like the main window's sidebar.
    fn paint_chrome(&self, p: &Painter) {
        let pal = &self.pal;
        let a = self.arranged;
        if a.rail.is_empty() {
            p.text(
                self.title,
                Font::Body,
                Rect::new(PAD + 2.0, 0.0, a.width - 160.0, TITLE_H),
                pal.text2,
            );
            return;
        }
        p.fill(Rect::new(RAIL_W - 1.0, 0.0, RAIL_W, a.height), pal.line);
        {
            let mut icons = self.icons.borrow_mut();
            if let Some(icon) =
                icons.bitmap(&p.gfx.wic, p.rt, icons::APP_ICON, IconSize::Large, false)
            {
                p.bitmap(&icon.bitmap, Rect::new(21.0, 19.0, 43.0, 41.0), None, 1.0);
            }
        }
        p.text(
            self.title,
            Font::Wordmark,
            Rect::new(52.0, 12.0, RAIL_W - 12.0, 48.0),
            pal.text,
        );
        // The highlight slides between pages.
        let t = self
            .form
            .page_pill
            .value(self.now)
            .clamp(0.0, (a.rail.len() - 1) as f32);
        let top = 62.0 + t * (RAIL_ITEM_H + 2.0);
        p.fill_round(
            Rect::new(12.0, top, RAIL_W - 12.0, top + RAIL_ITEM_H),
            8.0,
            pal.nav_selected,
        );
        for (page, rect) in a.rail.iter().enumerate() {
            let (title, icon) = &self.form.pages[page];
            let part = Part::Page(page);
            let level = self.level(part).min(1.0);
            if level > 0.0 && page != self.form.page {
                p.fill_round(*rect, 8.0, pal.hover.fade(level));
            }
            let selected = page == self.form.page;
            let color = if selected { pal.accent } else { pal.text2 };
            // A sub-page sits indented under its page, its icon smaller.
            let (indent, icon_size) = if self.form.sub_pages.contains(&page) {
                (18.0, 14.0)
            } else {
                (0.0, 18.0)
            };
            let icon_t = rect.t + (rect.height() - icon_size) / 2.0;
            glyphs::draw_page(
                p,
                *icon,
                Rect::new(
                    rect.l + 11.0 + indent,
                    icon_t,
                    rect.l + 11.0 + indent + icon_size,
                    icon_t + icon_size,
                ),
                color,
            );
            p.text(
                title,
                Font::Body,
                Rect::new(rect.l + 40.0 + indent, rect.t, rect.r - 6.0, rect.b),
                pal.text,
            );
        }
        if self.focused(Part::Page(0)) {
            let rect = a.rail[self.form.page];
            self.ring(p, rect, 8.0);
        }
    }

    /// A button inside the page.
    fn paint_button(&self, p: &Painter, part: Part, control: Rect, caption: &str) {
        let pal = &self.pal;
        let level = self.level(part);
        let rect = self.at(control);
        p.fill_round(rect, 10.0, pal.text.fade(0.04));
        if level > 0.0 {
            p.fill_round(rect, 10.0, pal.hover.alpha(pal.hover.a * level));
        }
        p.stroke_round(rect, 10.0, pal.line, 1.0);
        p.text(caption, Font::ButtonLight, rect, pal.text);
        if self.focused(part) {
            self.ring(p, rect, 10.0);
        }
    }

    /// The QR popup's code, dark on white whatever the theme so cameras
    /// read it, and the line under it.
    fn paint_qr(&self, p: &Painter, size: usize, modules: &[bool], waiting: &str, b: &WidgetBox) {
        let n = b.items.len();
        let (Some(&code), Some(&caption)) = (
            n.checked_sub(2).and_then(|at| b.items.get(at)),
            b.items.last(),
        ) else {
            return;
        };
        let rect = self.at(code);
        let caption = self.at(caption);
        if size == 0 {
            // Not a blank white square: an outlined place with what to
            // expect, like the internet link still being set up.
            p.stroke_round(rect.inset(1.0, 1.0), 10.0, self.pal.text2.fade(0.5), 1.5);
            let text = if waiting.is_empty() {
                "No address yet. Turn sharing on and save first."
            } else {
                waiting
            };
            p.text(
                "\u{ed14}",
                Font::Icon,
                Rect::new(
                    rect.l,
                    rect.t + rect.height() / 2.0 - 40.0,
                    rect.r,
                    rect.t + rect.height() / 2.0 - 12.0,
                ),
                self.pal.text2,
            );
            p.text(
                text,
                Font::SmallWrap,
                Rect::new(
                    rect.l + 20.0,
                    rect.t + rect.height() / 2.0,
                    rect.r - 20.0,
                    rect.b - 16.0,
                ),
                self.pal.text2,
            );
            return;
        }
        p.fill_round(rect, 10.0, Color::hex(0xffffff));
        let margin = 12.0;
        let cell = (rect.width() - margin * 2.0) / size as f32;
        for (at, on) in modules.iter().enumerate() {
            if *on {
                let (x, y) = ((at % size) as f32, (at / size) as f32);
                let l = rect.l + margin + x * cell;
                let t = rect.t + margin + y * cell;
                // A hair of overlap so no seams show between cells.
                p.fill(
                    Rect::new(l, t, l + cell + 0.4, t + cell + 0.4),
                    Color::hex(0),
                );
            }
        }
        p.text(
            "Scan with your phone's camera",
            Font::Small,
            caption,
            self.pal.text2,
        );
    }

    /// The help of the label the pointer rests on, under it (or over it
    /// near the bottom).
    fn paint_tip(&self, p: &Painter) {
        let pal = &self.pal;
        let a = self.arranged;
        let Some(index) = self.tip else {
            return;
        };
        let (Some(widget), Some(&(anchor, _))) = (
            self.form.widgets.get(index),
            a.tips.iter().find(|(_, widget)| *widget == index),
        ) else {
            return;
        };
        let Some(text) = &widget.tip else {
            return;
        };
        let anchor = self.at(anchor);
        let width = p.gfx.measure_width(text, Font::Small).ceil().min(340.0);
        let height = p.gfx.measure_height(text, Font::SmallWrap, width).ceil();
        let (w, h) = (width + 24.0, height + 16.0);
        let left = (anchor.l).min(a.width - 16.0 - w).max(a.left + 8.0);
        let below = anchor.b + 2.0;
        let top = if below + h > a.view_h - 8.0 {
            anchor.t - 2.0 - h
        } else {
            below
        };
        let panel = Rect::new(left, top, left + w, top + h);
        for (grow, alpha) in [(6.0, 0.06), (3.0, 0.08)] {
            let shadow = Rect::new(
                panel.l - grow,
                panel.t - grow + 4.0,
                panel.r + grow,
                panel.b + grow + 4.0,
            );
            p.fill_round(shadow, 10.0 + grow, Color::hex(0).alpha(alpha));
        }
        p.fill_round(panel, 8.0, pal.ground);
        p.fill_round(panel, 8.0, pal.card);
        p.stroke_round(panel, 8.0, pal.line.mix(pal.text2, 0.3), 1.0);
        p.text(
            text,
            Font::SmallWrap,
            Rect::new(panel.l + 12.0, panel.t + 8.0, panel.r - 12.0, panel.b - 8.0),
            pal.text,
        );
    }

    fn paint_label(&self, p: &Painter, widget: &Widget, b: &WidgetBox) {
        if !widget.label.is_empty() {
            p.text(&widget.label, Font::Body, self.at(b.label), self.pal.text);
        }
        if let (Some(detail), Some(rect)) = (&widget.detail, b.detail) {
            p.text(detail, Font::SmallWrap, self.at(rect), self.pal.text2);
        }
    }

    fn paint_widget(&self, p: &Painter, index: usize, widget: &Widget) {
        let pal = &self.pal;
        let b = &self.arranged.boxes[index];
        match &widget.kind {
            Kind::Heading => {
                p.text(&widget.label, Font::Heading, self.at(b.label), pal.text);
            }
            Kind::Paragraph => {
                p.text(&widget.label, Font::BodyWrap, self.at(b.label), pal.text2);
            }
            Kind::Switch { knob, .. } => {
                let part = Part::Switch(index);
                let level = self.level(part).min(1.0);
                self.paint_label(p, widget, b);
                let rect = self.at(b.control);
                let t = knob.value(self.now);
                let radius = rect.height() / 2.0;
                p.fill_round(rect, radius, pal.accent.fade(t));
                p.stroke_round(
                    rect,
                    radius,
                    pal.text2.mix(pal.text, level).fade(1.0 - t),
                    1.4,
                );
                let knob_r = 5.5 + 1.5 * t + level * 0.5;
                let cx = rect.l + radius + t * (rect.width() - radius * 2.0);
                let cy = rect.t + radius;
                p.fill_round(
                    Rect::new(cx - knob_r, cy - knob_r, cx + knob_r, cy + knob_r),
                    knob_r,
                    pal.text2.mix(pal.on_accent, t),
                );
                if self.focused(part) {
                    self.ring(p, rect, radius);
                }
            }
            Kind::Segments {
                options,
                selected,
                pill,
            } => {
                self.paint_label(p, widget, b);
                let control = self.at(b.control);
                p.fill_round(control, 9.0, pal.hover.alpha(pal.hover.a * 1.6));
                // The highlight slides between options.
                let t = pill.value(self.now).clamp(0.0, (options.len() - 1) as f32);
                let low = t.floor() as usize;
                let high = (low + 1).min(options.len() - 1);
                let frac = t - low as f32;
                let (a, z) = (self.at(b.items[low]), self.at(b.items[high]));
                let lerp = |x: f32, y: f32| x + (y - x) * frac;
                let highlight = Rect::new(lerp(a.l, z.l), a.t, lerp(a.r, z.r), a.b);
                p.fill_round(highlight, 7.0, pal.card.mix(pal.accent, 0.0));
                p.fill_round(highlight, 7.0, pal.nav_selected);
                p.stroke_round(highlight, 7.0, pal.accent.fade(0.45), 1.0);
                for (option, choice) in options.iter().enumerate() {
                    let rect = self.at(b.items[option]);
                    let level = self.level(Part::Segment(index, option)).min(1.0);
                    if option != *selected && level > 0.0 {
                        p.fill_round(rect, 7.0, pal.hover.fade(level));
                    }
                    let color = if option == *selected {
                        pal.text
                    } else {
                        pal.text2.mix(pal.text, level)
                    };
                    let mut text_rect = rect;
                    if let Some(swatch) = choice.swatch {
                        let label_w = p.gfx.measure_width(&choice.label, Font::ButtonLight);
                        let start = rect.l + (rect.width() - label_w - 18.0) / 2.0;
                        let cy = rect.t + rect.height() / 2.0;
                        p.fill_round(
                            Rect::new(start, cy - 5.0, start + 10.0, cy + 5.0),
                            5.0,
                            swatch,
                        );
                        text_rect =
                            Rect::new(start + 18.0, rect.t, start + 18.0 + label_w + 2.0, rect.b);
                        p.text(&choice.label, Font::Body, text_rect, color);
                    } else {
                        p.text(&choice.label, Font::ButtonLight, text_rect, color);
                    }
                }
                if self.focused(Part::Segment(index, 0)) {
                    self.ring(p, control, 9.0);
                }
            }
            Kind::Field {
                field,
                cue,
                scroll,
                shortcut,
                ..
            } => {
                self.paint_label(p, widget, b);
                let part = Part::Field(index);
                let rect = self.at(b.control);
                if *shortcut {
                    self.paint_shortcut(p, index, rect, field);
                    return;
                }
                let has_focus = self.focus == Some(part);
                let level = self.level(part).min(1.0);
                p.fill_round(rect, 8.0, pal.text.fade(0.035 + 0.02 * level));
                if has_focus {
                    p.stroke_round(rect, 8.0, pal.accent, 1.5);
                } else {
                    p.stroke_round(rect, 8.0, pal.line.mix(pal.text2, 0.3 * level), 1.0);
                }
                // A password's eye button: open while it shows, crossed out
                // while it hides.
                let mut text_r = rect.r - 8.0;
                if let (true, Some(&eye)) = (field.is_secret(), b.extra.first()) {
                    let eye = self.at(eye);
                    text_r = eye.l - 2.0;
                    let part = Part::Reveal(index);
                    let level = self.level(part).min(1.0);
                    if level > 0.0 {
                        p.fill_round(eye, 7.0, pal.hover.fade(level * 1.6));
                    }
                    let color = if field.is_revealed() {
                        pal.accent
                    } else {
                        pal.text2.mix(pal.text, level)
                    };
                    let (cx, cy) = (eye.l + eye.width() / 2.0, eye.t + eye.height() / 2.0);
                    let curve = |sign: f32| -> Vec<(f32, f32)> {
                        (0..=12)
                            .map(|step| {
                                let t = step as f32 / 12.0;
                                let x = cx - 8.0 + 16.0 * t;
                                let y = cy + sign * 5.5 * (std::f32::consts::PI * t).sin();
                                (x, y)
                            })
                            .collect()
                    };
                    p.polyline(&curve(-1.0), false, color, 1.4);
                    p.polyline(&curve(1.0), false, color, 1.4);
                    p.circle(cx, cy, 2.4, color, 1.4);
                    if !field.is_revealed() {
                        p.line(cx - 7.0, cy + 6.0, cx + 7.0, cy - 6.0, color, 1.4);
                    }
                    if self.focused(part) {
                        self.ring(p, eye, 7.0);
                    }
                }
                let inner = Rect::new(rect.l + 8.0, rect.t, text_r, rect.b);
                p.clip(inner);
                let origin = rect.l + 10.0 - scroll;
                let layout = p.gfx.layout(&field.shown(), Font::Body, 100_000.0, FIELD_H);
                if let Some(layout) = &layout {
                    if has_focus {
                        let (start, end) = field.selection();
                        for (left, right) in paint::selection_spans(layout, start, end) {
                            p.fill(
                                Rect::new(
                                    origin + left,
                                    rect.t + 8.0,
                                    origin + right,
                                    rect.b - 8.0,
                                ),
                                pal.accent.fade(0.28),
                            );
                        }
                    }
                }
                if field.is_empty() {
                    p.text(
                        cue,
                        Font::Body,
                        Rect::new(rect.l + 10.0, rect.t, rect.r - 10.0, rect.b),
                        pal.text2.fade(0.6),
                    );
                } else if let Some(layout) = &layout {
                    p.text_layout(layout, origin, rect.t, pal.text);
                }
                if has_focus && self.caret && !*shortcut {
                    if let Some(layout) = &layout {
                        let x = (origin + paint::caret_offset(layout, field.caret())).round() + 0.5;
                        p.line(x, rect.t + 9.0, x, rect.b - 9.0, pal.accent, 1.5);
                    }
                }
                // The accent border already marks the focused field.
                p.unclip();
            }
            Kind::Checks { items } => {
                self.paint_label(p, widget, b);
                for (item, (label, on)) in items.iter().enumerate() {
                    let part = Part::Check(index, item);
                    let row = self.at(b.items[item]);
                    let level = self.level(part).min(1.0);
                    if level > 0.0 {
                        p.fill_round(row, 8.0, pal.hover.fade(level));
                    }
                    let cy = row.t + row.height() / 2.0;
                    let x = row.l + 8.0;
                    let boxr = Rect::new(x, cy - 9.0, x + 18.0, cy + 9.0);
                    if *on {
                        p.fill_round(boxr, 5.0, pal.accent);
                        p.line(x + 4.5, cy + 0.5, x + 7.8, cy + 3.8, pal.on_accent, 1.8);
                        p.line(x + 7.8, cy + 3.8, x + 13.5, cy - 3.2, pal.on_accent, 1.8);
                    } else {
                        p.stroke_round(boxr, 5.0, pal.text2.mix(pal.text, level), 1.4);
                    }
                    p.text(
                        label,
                        Font::Body,
                        Rect::new(x + 30.0, row.t, row.r - 8.0, row.b),
                        pal.text,
                    );
                    if self.focused(part) {
                        self.ring(p, row, 8.0);
                    }
                }
            }
            Kind::Slider { value, caption } => {
                self.paint_label(p, widget, b);
                let part = Part::Slider(index);
                let level = self.level(part).min(1.0);
                let held = matches!(self.drag, Drag::Slider(at) if at == index);
                let words = caption(*value);
                let text = self.at(b.items[0]);
                let words_w = p.gfx.measure_width(&words, Font::Small).ceil() + 2.0;
                p.text(
                    &words,
                    Font::Small,
                    Rect::new(text.r - words_w, text.t, text.r, text.b),
                    pal.text2
                        .mix(pal.text, if held { 1.0 } else { level * 0.5 }),
                );
                let track = self.at(b.items[1]);
                let cy = (track.t + track.height() / 2.0).round();
                let t = (*value as f32 - 1.0) / 99.0;
                let cx = track.l + track.width() * t;
                let groove = Rect::new(track.l - 3.0, cy - 2.0, track.r + 3.0, cy + 2.0);
                p.fill_round(groove, 2.0, pal.text.fade(0.14));
                p.fill_round(Rect::new(groove.l, groove.t, cx, groove.b), 2.0, pal.accent);
                let r = if held { 7.0 } else { 8.0 + level * 0.8 };
                p.fill_round(
                    Rect::new(cx - r - 1.0, cy - r - 1.0, cx + r + 1.0, cy + r + 1.0),
                    r + 1.0,
                    Color::hex(0).alpha(0.16),
                );
                p.fill_round(Rect::new(cx - r, cy - r, cx + r, cy + r), r, pal.on_accent);
                let dot = if held { 3.6 } else { 4.4 + level * 0.8 };
                p.fill_round(
                    Rect::new(cx - dot, cy - dot, cx + dot, cy + dot),
                    dot,
                    pal.accent,
                );
                if self.focused(part) {
                    self.ring(p, self.at(b.control), 8.0);
                }
            }
            Kind::Status { dot } => {
                let label = self.at(b.label);
                let mid = label.t + 10.0;
                p.fill_round(
                    Rect::new(label.l - 16.0, mid - 4.0, label.l - 8.0, mid + 4.0),
                    4.0,
                    Color::hex(*dot),
                );
                p.text(&widget.label, Font::BodyBold, label, pal.text);
                if let (Some(detail), Some(rect)) = (&widget.detail, b.detail) {
                    p.text(detail, Font::SmallWrap, self.at(rect), pal.text2);
                }
            }
            Kind::Qr {
                icon: Some(icon),
                text,
                waiting,
                copied,
                ..
            } => {
                let (Some(&mark), Some(&field), Some(&info), Some(&copy)) = (
                    b.items.first(),
                    b.items.get(1),
                    b.items.get(2),
                    b.items.get(3),
                ) else {
                    return;
                };
                p.text(&icon.to_string(), Font::Icon, self.at(mark), pal.text2);
                p.text(&widget.label, Font::Body, self.at(b.label), pal.text);
                let field = self.at(field);
                p.fill_round(field, 8.0, pal.text.fade(0.05));
                let (shown, ink) = if text.is_empty() {
                    (waiting.as_str(), pal.text2)
                } else {
                    (text.strip_prefix("https://").unwrap_or(text), pal.text)
                };
                p.text(shown, Font::Small, field.inset(10.0, 0.0), ink);
                // Icon buttons: info (hover only), copy, QR.
                let square = |part: Option<Part>, rect: Rect, glyph: &str| {
                    let rect = self.at(rect);
                    let level = part.map_or(0.0, |part| self.level(part));
                    if level > 0.0 {
                        p.fill_round(rect, 8.0, pal.hover.alpha(pal.hover.a * level));
                    }
                    p.stroke_round(rect, 8.0, pal.line, 1.0);
                    p.text(glyph, Font::Icon, rect, pal.text);
                    if part.is_some_and(|part| self.focused(part)) {
                        self.ring(p, rect, 8.0);
                    }
                };
                if widget.tip.is_some() {
                    square(None, info, "\u{e946}");
                }
                square(
                    Some(Part::Action(index)),
                    copy,
                    if *copied { "\u{e73e}" } else { "\u{e8c8}" },
                );
                square(Some(Part::Color(index)), b.control, "\u{ed14}");
            }
            Kind::Qr { caption, .. } => {
                self.paint_label(p, widget, b);
                self.paint_button(p, Part::Color(index), b.control, caption);
            }
            Kind::Expander { open, .. } => {
                let part = Part::Expander(index);
                let level = self.level(part).min(1.0);
                if level > 0.0 {
                    let row = self.at(Rect::new(
                        b.label.l - ROW_X + 6.0,
                        b.control.t - 12.0,
                        b.control.r + ROW_X - 6.0,
                        b.control.b + 12.0,
                    ));
                    p.fill_round(row, 8.0, pal.hover.fade(level));
                }
                self.paint_label(p, widget, b);
                let c = self.at(b.control);
                let (cx, cy) = (c.l + c.width() / 2.0, c.t + c.height() / 2.0);
                let d = if *open { -1.0 } else { 1.0 };
                p.polyline(
                    &[
                        (cx - 5.0, cy - 2.5 * d),
                        (cx, cy + 2.5 * d),
                        (cx + 5.0, cy - 2.5 * d),
                    ],
                    false,
                    pal.text2.mix(pal.text, level),
                    1.6,
                );
                if self.focused(part) {
                    let row = self.at(Rect::new(
                        b.label.l - ROW_X + 6.0,
                        b.control.t - 12.0,
                        b.control.r + ROW_X - 6.0,
                        b.control.b + 12.0,
                    ));
                    self.ring(p, row, 8.0);
                }
            }
            Kind::Color { rgb, .. } => {
                self.paint_label(p, widget, b);
                let part = Part::Color(index);
                let level = self.level(part);
                let rect = self.at(b.control);
                p.fill_round(rect, 10.0, pal.text.fade(0.04));
                if level > 0.0 {
                    p.fill_round(rect, 10.0, pal.hover.alpha(pal.hover.a * level));
                }
                p.stroke_round(rect, 10.0, pal.line, 1.0);
                let cy = rect.t + rect.height() / 2.0;
                let swatch = Rect::new(rect.l + 9.0, cy - 10.0, rect.l + 29.0, cy + 10.0);
                p.fill_round(swatch, 6.0, Color::hex(*rgb));
                p.stroke_round(swatch, 6.0, pal.text.fade(0.25), 1.0);
                p.text(
                    &format!("#{rgb:06X}"),
                    Font::ButtonLight,
                    Rect::new(swatch.r + 4.0, rect.t, rect.r - 4.0, rect.b),
                    pal.text,
                );
                if self.focused(part) {
                    self.ring(p, rect, 10.0);
                }
            }
            Kind::Action { caption } => {
                self.paint_label(p, widget, b);
                self.paint_button(p, Part::Action(index), b.control, caption);
            }
            Kind::Order { items } => {
                self.paint_label(p, widget, b);
                self.paint_order(p, index, items, b);
            }
            Kind::Drives { items } => {
                self.paint_label(p, widget, b);
                self.paint_drives(p, index, items, b);
            }
            Kind::Paths {
                items,
                empty,
                refresh,
            } => {
                self.paint_label(p, widget, b);
                for (item, path) in items.iter().enumerate() {
                    let row = self.at(b.items[item]);
                    let remove = self.at(b.extra[item]);
                    let part = Part::Remove(index, item);
                    let level = self.level(part).min(1.0);
                    let cy = row.t + row.height() / 2.0;
                    self.icon(
                        p,
                        IconLook::Folder(path),
                        Rect::new(row.l + 8.0, cy - 8.0, row.l + 24.0, cy + 8.0),
                    );
                    p.text(
                        path,
                        Font::Body,
                        Rect::new(
                            row.l + 34.0,
                            row.t,
                            remove.l - if *refresh { 38.0 } else { 8.0 },
                            row.b,
                        ),
                        pal.text,
                    );
                    if *refresh {
                        self.paint_refresh(p, Part::Refresh(index, item), refresh_beside(remove));
                    }
                    if level > 0.0 {
                        p.fill_round(remove, 13.0, pal.hover.fade(level * 1.6));
                    }
                    let (cx, cy) = (
                        remove.l + remove.width() / 2.0,
                        remove.t + remove.height() / 2.0,
                    );
                    let color = pal.text2.mix(pal.danger, level);
                    p.line(cx - 4.5, cy - 4.5, cx + 4.5, cy + 4.5, color, 1.5);
                    p.line(cx + 4.5, cy - 4.5, cx - 4.5, cy + 4.5, color, 1.5);
                    if self.focused(part) {
                        self.ring(p, remove, 13.0);
                    }
                }
                if items.is_empty() {
                    p.text(empty, Font::Body, self.at(b.control), pal.text2);
                }
                if let Some(&add) = b.extra.last() {
                    let add = self.at(add);
                    let part = Part::Add(index);
                    let level = self.level(part).min(1.0);
                    if level > 0.0 {
                        p.fill_round(add, 8.0, pal.hover.fade(level));
                    }
                    let (cx, cy) = (add.l + 18.0, add.t + add.height() / 2.0);
                    p.line(cx - 5.5, cy, cx + 5.5, cy, pal.accent, 1.6);
                    p.line(cx, cy - 5.5, cx, cy + 5.5, pal.accent, 1.6);
                    p.text(
                        "Add a folder",
                        Font::ButtonLight,
                        Rect::new(add.l + 32.0, add.t, add.r, add.b),
                        pal.accent,
                    );
                    if self.focused(part) {
                        self.ring(p, add, 8.0);
                    }
                }
            }
            Kind::Syntax { rows } => {
                for (row, (code, text)) in rows.iter().enumerate() {
                    let code_rect = self.at(b.items[row]);
                    let code_w = p.gfx.measure_width(code, Font::Code).ceil();
                    let chip = Rect::new(
                        code_rect.l,
                        code_rect.t,
                        (code_rect.l + code_w + 14.0).min(code_rect.r - 8.0),
                        code_rect.b,
                    );
                    p.fill_round(chip, 6.0, pal.hover.alpha(pal.hover.a * 1.8));
                    p.text(
                        code,
                        Font::Code,
                        Rect::new(chip.l + 7.0, chip.t, chip.r - 4.0, chip.b),
                        pal.text,
                    );
                    p.text(text, Font::BodyWrap, self.at(b.extra[row]), pal.text2);
                }
            }
        }
    }
}

// ----- Window plumbing ----------------------------------------------------------

const FORM_CLASS: windows_sys::core::PCWSTR = w!("FeatherFormWindow");
static REGISTERED: Once = Once::new();
static REGISTER_OK: AtomicBool = AtomicBool::new(false);

fn register_class() -> Result<(), String> {
    REGISTERED.call_once(|| {
        let hinstance = unsafe { GetModuleHandleW(std::ptr::null()) };
        let class = WNDCLASSW {
            style: CS_DBLCLKS,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: super::tray::app_icon(0),
            hCursor: unsafe { LoadCursorW(std::ptr::null_mut(), IDC_ARROW) },
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: FORM_CLASS,
        };
        REGISTER_OK.store(unsafe { RegisterClassW(&class) } != 0, Ordering::SeqCst);
    });
    if REGISTER_OK.load(Ordering::SeqCst) {
        Ok(())
    } else {
        Err("could not register the dialog window class".into())
    }
}

unsafe fn host_ref<'a>(hwnd: HWND) -> Option<&'a mut Host> {
    let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Host;
    (!pointer.is_null()).then(|| &mut *pointer)
}

/// Asks every dialog that `owner` has open on this thread to close, as its
/// close button would (a dialog with a result to give cancels).
pub fn close_owned_by(owner: HWND) {
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumThreadWindows, GetClassNameW, GetWindow, GW_OWNER,
    };

    unsafe extern "system" fn each(hwnd: HWND, owner: LPARAM) -> i32 {
        let mut class = [0u16; 32];
        let len = GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32).max(0) as usize;
        if GetWindow(hwnd, GW_OWNER) as LPARAM == owner
            && String::from_utf16_lossy(&class[..len]) == "FeatherFormWindow"
        {
            PostMessageW(hwnd, WM_CLOSE, 0, 0);
        }
        1
    }

    unsafe {
        EnumThreadWindows(GetCurrentThreadId(), Some(each), owner as LPARAM);
    }
}

fn lparam_point(lparam: LPARAM) -> (f32, f32) {
    let x = (lparam & 0xffff) as u16 as i16 as f32;
    let y = ((lparam >> 16) & 0xffff) as u16 as i16 as f32;
    (x, y)
}

/// A dialog window that is open but not yet running its modal loop.
pub struct FormWindow {
    hwnd: HWND,
    parent: HWND,
}

impl FormWindow {
    /// Creates the window. `width` and `height` are the client size.
    pub fn open(
        parent: HWND,
        title: &str,
        width: i32,
        height: i32,
        look: Look,
        form: Form,
        dialog: Box<dyn FormDialog>,
    ) -> Result<Self, String> {
        register_class()?;
        let gfx = Gfx::new()?;
        let style = WS_POPUP | WS_CAPTION | WS_SYSMENU;
        let exstyle = WS_EX_DLGMODALFRAME;
        let mut frame = RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        };
        unsafe {
            AdjustWindowRectEx(&mut frame, style, 0, exstyle);
        }
        let title_text = title;
        let title = to_wide(title);
        let hwnd = unsafe {
            CreateWindowExW(
                exstyle,
                FORM_CLASS,
                title.as_ptr(),
                style,
                0,
                0,
                frame.right - frame.left,
                frame.bottom - frame.top,
                parent,
                std::ptr::null_mut(),
                GetModuleHandleW(std::ptr::null()),
                std::ptr::null(),
            )
        };
        if hwnd.is_null() {
            return Err("could not create the dialog window".into());
        }
        let host = Box::new(Host {
            hwnd,
            gfx,
            canvas: Canvas::default(),
            look,
            palette: look.palette(),
            theme: Tween::new(1.0),
            theme_from: None,
            form,
            dialog,
            hover: None,
            pressed: None,
            fades: Fades::default(),
            focus: None,
            focus_visible: false,
            scroll: Tween::new(0.0),
            drag: Drag::None,
            caret_on: true,
            active: false,
            tracking: false,
            anim_timer: false,
            done: false,
            outcome: None,
            recording: None,
            committed: None,
            title: title_text.to_string(),
            icons: std::cell::RefCell::new(IconCache::default()),
            tip: None,
            popup_since: None,
            thumbs: [Tween::new(0.0), Tween::new(0.0), Tween::new(0.0)],
            swatch_from: 0,
            swatch_fade: Tween::new(1.0),
            copied: None,
            preview_due: None,
            preview_at: None,
        });
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(host) as isize);
            // Drop the standard caption now that WM_NCCALCSIZE removes it;
            // Windows still draws the close button over the title strip.
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                0,
                0,
                0,
                0,
                SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
            theme::apply_window_effects(hwnd, look.dark, effective_material(look.material));
            center(
                parent,
                hwnd,
                frame.right - frame.left,
                frame.bottom - frame.top,
            );
            SetTimer(
                hwnd,
                TIMER_CARET,
                GetCaretBlinkTime().clamp(200, 2000),
                None,
            );
            SetTimer(hwnd, TIMER_TICK, 1000, None);
            if let Some(host) = host_ref(hwnd) {
                host.apply_focus_request();
            }
        }
        Ok(Self { hwnd, parent })
    }

    fn show(&self) {
        unsafe {
            ShowWindow(self.hwnd, SW_SHOW);
            UpdateWindow(self.hwnd);
            SetForegroundWindow(self.hwnd);
            SetFocus(self.hwnd);
        }
    }

    /// Runs the dialog until it is accepted or dismissed.
    pub fn run_modal(mut self) -> Option<Box<dyn Any>> {
        unsafe {
            EnableWindow(self.parent, 0);
        }
        self.show();
        let mut message = MSG::default();
        loop {
            let done = unsafe { host_ref(self.hwnd).map(|host| host.done).unwrap_or(true) };
            if done {
                break;
            }
            if !frames::next_message(&mut message) {
                break;
            }
            unsafe {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        let outcome = unsafe { host_ref(self.hwnd).and_then(|host| host.outcome.take()) };
        unsafe {
            EnableWindow(self.parent, 1);
            SetForegroundWindow(self.parent);
        }
        self.close();
        outcome
    }

    /// Switches a dialog with pages to `page`.
    pub fn set_page(&mut self, page: usize) {
        unsafe {
            if let Some(host) = host_ref(self.hwnd) {
                host.form.set_page(page);
                host.form.settle();
            }
        }
    }

    /// Opens the popup of row `id` (a color picker or a QR code), or
    /// unfolds an expander, for screenshots.
    pub fn open_color(&mut self, id: Id) {
        unsafe {
            if let Some(host) = host_ref(self.hwnd) {
                let Some(index) = host.form.widgets.iter().position(|w| w.id == id) else {
                    return;
                };
                match &mut host.form.widgets[index].kind {
                    Kind::Color { open: false, .. } | Kind::Qr { open: false, .. } => {
                        host.toggle_popup(index);
                        host.popup_since = None;
                        for thumb in &mut host.thumbs {
                            thumb.settle();
                        }
                    }
                    Kind::Expander { open, .. } => *open = true,
                    _ => {}
                }
            }
        }
    }

    /// Shows the tip of `id`'s label, for screenshots.
    pub fn show_tip(&mut self, id: Id) {
        unsafe {
            if let Some(host) = host_ref(self.hwnd) {
                if let Some(index) = host.form.widgets.iter().position(|w| w.id == id) {
                    host.tip = Some((index, Instant::now() - TIP_DELAY * 2));
                }
            }
        }
    }

    /// Shows the window, renders it into a PNG, and closes it.
    pub fn capture(self, path: &std::path::Path) -> Result<(), String> {
        self.capture_at(path, 0.0)
    }

    /// Captures the window scrolled down by `scroll` pixels.
    pub fn capture_at(self, path: &std::path::Path, scroll: f32) -> Result<(), String> {
        self.capture_keys(path, scroll, None)
    }

    /// Drags row `from` of the reorderable list `id` onto row `to` with the
    /// mouse, then captures the window. Returns the list's labels after.
    /// Clicks the middle of `id`'s control like a person would (press,
    /// a pause while the window keeps running, release) and returns the
    /// switches that changed and what has focus. A check for clicks landing
    /// where they are aimed.
    pub fn probe_click(mut self, page: usize, scroll: f32, id: Id) -> Result<String, String> {
        self.show();
        self.set_page(page);
        pump();
        let report = unsafe {
            let host = host_ref(self.hwnd).ok_or("dialog closed")?;
            // Raw, so a scroll left past the end of the page is tried too.
            host.scroll.jump(scroll);
            let widget = host
                .form
                .widgets
                .iter()
                .position(|w| w.id == id)
                .ok_or("no such widget")?;
            let arranged = host.arranged();
            let control = arranged.boxes[widget].control;
            let at_scroll = host.scroll_now(&arranged);
            let (x, y) = (
                control.l + 20.0,
                control.t + control.height() / 2.0 - at_scroll,
            );
            let switches = |host: &Host| -> Vec<(Id, bool)> {
                host.form
                    .widgets
                    .iter()
                    .filter_map(|w| match w.kind {
                        Kind::Switch { on, .. } => Some((w.id, on)),
                        _ => None,
                    })
                    .collect()
            };
            let before = switches(host);
            host.on_mouse_down(x, y, false);
            let started = Instant::now();
            while started.elapsed() < Duration::from_millis(150) {
                pump();
                std::thread::sleep(Duration::from_millis(10));
            }
            let host = host_ref(self.hwnd).ok_or("dialog closed")?;
            host.on_mouse_up(x, y);
            let after = switches(host);
            let changed: Vec<String> = before
                .iter()
                .zip(&after)
                .filter(|(a, b)| a.1 != b.1)
                .map(|(a, _)| a.0.to_string())
                .collect();
            format!(
                "clicked {id} at {x},{y} (scroll {at_scroll} -> {}); switches changed: {:?}; focus: {:?}",
                host.scroll.value(Instant::now()),
                changed,
                host.focus
            )
        };
        self.close();
        pump();
        Ok(report)
    }

    pub fn capture_drag(
        mut self,
        path: &std::path::Path,
        id: Id,
        from: usize,
        to: usize,
    ) -> Result<Vec<String>, String> {
        self.show();
        pump();
        let result = unsafe {
            host_ref(self.hwnd)
                .map(|host| {
                    let widget = host
                        .form
                        .widgets
                        .iter()
                        .position(|w| w.id == id)
                        .ok_or("no such list")?;
                    let arranged = host.arranged();
                    let rows = arranged.boxes[widget].items.clone();
                    let scroll = host.scroll.value(Instant::now());
                    let x = rows[from].l + 60.0;
                    let y0 = rows[from].t + rows[from].height() / 2.0 - scroll;
                    let y1 = rows[to].t + rows[to].height() / 2.0 - scroll;
                    host.on_mouse_down(x, y0, false);
                    for step in 1..=8 {
                        host.on_mouse_move(x, y0 + (y1 - y0) * step as f32 / 8.0);
                    }
                    host.on_mouse_up(x, y1);
                    let labels = match &host.form.widgets[widget].kind {
                        Kind::Order { items } => {
                            items.iter().map(|(label, _, _)| label.clone()).collect()
                        }
                        _ => Vec::new(),
                    };
                    host.capture(path)?;
                    Ok(labels)
                })
                .unwrap_or_else(|| Err("dialog closed".into()))
        };
        self.close();
        pump();
        result
    }

    /// Captures it with the shortcut box `id` listening and `held` keys down,
    /// set directly so no keyboard hook is installed.
    pub fn capture_keys(
        mut self,
        path: &std::path::Path,
        scroll: f32,
        keys: Option<(Id, &[u32])>,
    ) -> Result<(), String> {
        self.show();
        pump();
        let result = unsafe {
            host_ref(self.hwnd)
                .map(|host| {
                    host.scroll_to(scroll, false);
                    if let Some((id, held)) = keys {
                        if let Some(widget) = host.form.widgets.iter().position(|w| w.id == id) {
                            let then = Instant::now() - Duration::from_secs(1);
                            host.focus = Some(Part::Field(widget));
                            host.recording = Some(Recording {
                                widget,
                                held: held.to_vec(),
                                since: held.iter().map(|_| then).collect(),
                                peak: held.to_vec(),
                                started: then,
                            });
                        }
                    }
                    host.capture(path)
                })
                .unwrap_or_else(|| Err("dialog closed".into()))
        };
        self.close();
        pump();
        result
    }

    fn close(&mut self) {
        if !self.hwnd.is_null() {
            unsafe {
                if IsWindow(self.hwnd) != 0 {
                    DestroyWindow(self.hwnd);
                }
            }
            self.hwnd = std::ptr::null_mut();
        }
    }
}

impl Drop for FormWindow {
    fn drop(&mut self) {
        self.close();
    }
}

fn pump() {
    let mut message = MSG::default();
    let started = Instant::now();
    while started.elapsed() < Duration::from_millis(50)
        && unsafe { PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) } > 0
    {
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

unsafe fn center(parent: HWND, hwnd: HWND, width: i32, height: i32) {
    let mut rect = RECT::default();
    if parent.is_null() || GetWindowRect(parent, &mut rect) == 0 {
        rect = RECT {
            left: 0,
            top: 0,
            right: GetSystemMetrics(SM_CXSCREEN),
            bottom: GetSystemMetrics(SM_CYSCREEN),
        };
    }
    let x = rect.left + ((rect.right - rect.left) - width) / 2;
    let y = rect.top + ((rect.bottom - rect.top) - height) / 2;
    SetWindowPos(
        hwnd,
        std::ptr::null_mut(),
        x.max(0),
        y.max(0),
        0,
        0,
        SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
    );
}

unsafe extern "system" fn wndproc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_ERASEBKGND {
        return 1;
    }
    // Windows draws and hit-tests the close button over the title strip.
    let mut dwm_result: LRESULT = 0;
    if windows_sys::Win32::Graphics::Dwm::DwmDefWindowProc(
        hwnd,
        message,
        wparam,
        lparam,
        &mut dwm_result,
    ) != 0
    {
        return dwm_result;
    }
    if message == WM_NCCALCSIZE && wparam != 0 {
        // Keep the borders, drop the caption.
        let params = &mut *(lparam as *mut NCCALCSIZE_PARAMS);
        let top = params.rgrc[0].top;
        let result = DefWindowProcW(hwnd, message, wparam, lparam);
        params.rgrc[0].top = top;
        return result;
    }
    let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Host;
    if pointer.is_null() {
        return DefWindowProcW(hwnd, message, wparam, lparam);
    }
    let host = &mut *pointer;
    match message {
        WM_NCHITTEST => {
            let result = DefWindowProcW(hwnd, message, wparam, lparam);
            if result != HTCLIENT as LRESULT {
                return result;
            }
            let mut point = windows_sys::Win32::Foundation::POINT {
                x: (lparam & 0xffff) as i16 as i32,
                y: ((lparam >> 16) & 0xffff) as i16 as i32,
            };
            windows_sys::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut point);
            let (x, y) = (point.x as f32, point.y as f32);
            let arranged = host.arranged();
            let over_rail_header = !arranged.rail.is_empty() && x < RAIL_W && y < 56.0;
            if (y < TITLE_H || over_rail_header) && host.hit(&arranged, x, y).is_none() {
                return HTCAPTION as LRESULT;
            }
            HTCLIENT as LRESULT
        }
        WM_PAINT => {
            let mut paint = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut paint);
            host.render(hdc);
            EndPaint(hwnd, &paint);
            0
        }
        WM_SIZE => {
            host.invalidate();
            0
        }
        WM_TIMER => {
            match wparam {
                TIMER_TICK => {
                    if host.dialog.on_tick(&mut host.form) {
                        host.invalidate();
                    }
                }
                TIMER_TIP => {
                    KillTimer(hwnd, TIMER_TIP);
                    host.invalidate();
                }
                TIMER_CARET if host.active && matches!(host.focus, Some(Part::Field(_))) => {
                    host.caret_on = !host.caret_on;
                    host.invalidate();
                }
                _ => {}
            }
            0
        }
        WM_SETFOCUS => {
            host.active = true;
            host.caret_on = true;
            host.invalidate();
            0
        }
        WM_KILLFOCUS => {
            host.active = false;
            host.stop_recording();
            host.invalidate();
            0
        }
        keyhook::WM_RECORD_KEY => {
            host.on_record_key(wparam as u32, lparam != 0);
            0
        }
        WM_SETCURSOR if (lparam & 0xffff) as u32 == HTCLIENT => {
            let mut point = windows_sys::Win32::Foundation::POINT::default();
            windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut point);
            windows_sys::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut point);
            let arranged = host.arranged();
            let over_field = matches!(
                host.hit(&arranged, point.x as f32, point.y as f32),
                Some(Part::Field(_))
            );
            let cursor = if over_field || matches!(host.drag, Drag::Text(_)) {
                IDC_IBEAM
            } else {
                IDC_ARROW
            };
            SetCursor(LoadCursorW(std::ptr::null_mut(), cursor));
            1
        }
        WM_MOUSEMOVE => {
            let (x, y) = lparam_point(lparam);
            host.on_mouse_move(x, y);
            0
        }
        WM_MOUSELEAVE => {
            host.tracking = false;
            host.hide_tip();
            if host.drag == Drag::None {
                host.set_hover(None);
            }
            0
        }
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
            let (x, y) = lparam_point(lparam);
            host.on_mouse_down(x, y, message == WM_LBUTTONDBLCLK);
            0
        }
        WM_LBUTTONUP => {
            let (x, y) = lparam_point(lparam);
            host.on_mouse_up(x, y);
            0
        }
        WM_MOUSEWHEEL => {
            host.hide_tip();
            host.on_wheel(((wparam >> 16) & 0xffff) as u16 as i16);
            0
        }
        WM_KEYDOWN => {
            host.on_key(wparam as u16);
            0
        }
        WM_CHAR => {
            host.on_char(wparam as u16);
            0
        }
        WM_CLOSE => {
            host.finish();
            0
        }
        WM_FORM_DONE => 0,
        frames::WM_ANIM_FRAME => {
            host.on_anim_frame();
            0
        }
        WM_NCDESTROY => {
            KillTimer(hwnd, TIMER_CARET);
            KillTimer(hwnd, TIMER_TICK);
            KillTimer(hwnd, TIMER_TIP);
            frames::stop(hwnd);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            drop(Box::from_raw(pointer));
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Form {
        let mut form = Form::new();
        form.heading("Section");
        form.switch("a", "Switch", Some("Detail"), true);
        form.segments(
            "b",
            "Choice",
            None,
            vec![option("One"), option("Two"), option("Three")],
            1,
        );
        form.field("c", "Field", None, "value", "cue", false);
        form.checks("d", "Checks", vec![("x".into(), true), ("y".into(), false)]);
        form.paths("e", "Paths", None, vec![r"C:\Temp".into()]);
        form.button("ok", "Save", ButtonStyle::Primary);
        form.button("cancel", "Cancel", ButtonStyle::Secondary);
        form.button("reset", "Reset", ButtonStyle::Quiet);
        form
    }

    #[test]
    fn values_round_trip_by_id() {
        let mut form = sample();
        assert!(form.is_on("a"));
        form.set_on("a", false);
        assert!(!form.is_on("a"));
        assert_eq!(form.selected("b"), 1);
        form.set_selected("b", 9);
        assert_eq!(form.selected("b"), 2, "clamped to the last option");
        assert_eq!(form.text("c"), "value");
        form.set_text("c", "new");
        assert_eq!(form.text("c"), "new");
        assert_eq!(form.checked("d"), vec![true, false]);
        assert_eq!(form.path_items("e"), vec![r"C:\Temp".to_string()]);
        assert_eq!(form.text("missing"), "");
    }

    #[test]
    fn layout_keeps_rows_in_cards_and_buttons_in_the_footer() {
        let gfx = Gfx::new().expect("graphics");
        let form = sample();
        let arranged = arrange(&form, &gfx, 600.0, 500.0, 0.0);
        assert_eq!(arranged.cards.len(), 1);
        let card = arranged.cards[0];
        for (rect, part) in &arranged.parts {
            assert!(
                rect.l >= card.l && rect.r <= card.r,
                "{part:?} {rect:?} {card:?}"
            );
        }
        // Primary at the far right, secondary to its left, quiet at the left.
        let (save, cancel, reset) = (
            arranged.buttons[0],
            arranged.buttons[1],
            arranged.buttons[2],
        );
        assert!(save.r <= 600.0 - PAD + 0.5);
        assert!(cancel.r <= save.l);
        assert!(reset.r <= cancel.l);
        assert!(save.t >= arranged.view_h);
    }

    #[test]
    fn segmented_controls_are_one_keyboard_stop() {
        assert_eq!(Part::Segment(3, 2).stop(), Part::Segment(3, 0));
        assert_eq!(Part::Check(1, 2).stop(), Part::Check(1, 2));
    }
}
