//! Colors, the light/dark decision, and the window backdrop material.

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::Graphics::Dwm::{
    DwmExtendFrameIntoClientArea, DwmSetWindowAttribute, DWMSBT_MAINWINDOW, DWMSBT_NONE,
    DWMSBT_TRANSIENTWINDOW, DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_USE_IMMERSIVE_DARK_MODE,
};
use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows_sys::Win32::UI::Controls::MARGINS;

use super::gfx::Color;
use crate::settings::{Accent, DarkMode, Material, ThemeColors};

/// Every color the main window paints with.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub dark: bool,
    /// Painted first under everything. Transparent when a backdrop material
    /// shows through; opaque for the solid material and screenshots.
    pub base: Color,
    /// A wash over the backdrop that gives the window its warm cast.
    pub tint: Color,
    pub text: Color,
    pub text2: Color,
    /// The results column surface.
    pub card: Color,
    pub line: Color,
    pub hover: Color,
    pub selected: Color,
    pub nav_selected: Color,
    pub accent: Color,
    pub on_accent: Color,
    pub scrollbar: Color,
    /// Error text.
    pub danger: Color,
    /// The opaque background used where no backdrop shows, as in
    /// screenshots.
    pub ground: Color,
}

impl Palette {
    /// The Ember palette: warm charcoal in dark, warm stone in light.
    pub fn new(dark: bool, accent: Accent, material: Material) -> Self {
        let solid = material == Material::Solid;
        let accent_fill = accent_color(accent, dark);
        if dark {
            let base = Color::hex(0x181614);
            Self {
                dark,
                base: if solid { base } else { base.alpha(0.0) },
                tint: if solid {
                    base
                } else {
                    Color::hex(0x181512).alpha(0.62)
                },
                text: Color::hex(0xf0ece6),
                text2: Color::hex(0xa39a8f),
                card: if solid {
                    Color::hex(0x272320)
                } else {
                    Color::hex(0xfff4e6).alpha(0.06)
                },
                line: Color::hex(0xfff4e6).alpha(0.09),
                hover: Color::hex(0xfff4e6).alpha(0.05),
                selected: accent_fill.alpha(0.20),
                nav_selected: accent_fill.alpha(0.16),
                accent: accent_fill,
                on_accent: Color::hex(0x1d1a16),
                scrollbar: Color::hex(0xfff4e6).alpha(0.26),
                danger: Color::hex(0xff9b8f),
                ground: Color::hex(0x1c1916),
            }
        } else {
            let base = Color::hex(0xf1efec);
            Self {
                dark,
                base: if solid { base } else { base.alpha(0.0) },
                tint: if solid {
                    base
                } else {
                    Color::hex(0xf3efe9).alpha(0.55)
                },
                text: Color::hex(0x221f1b),
                text2: Color::hex(0x6e675e),
                card: if solid {
                    Color::hex(0xffffff)
                } else {
                    Color::hex(0xffffff).alpha(0.74)
                },
                line: Color::hex(0x221f1b).alpha(0.10),
                hover: Color::hex(0x221f1b).alpha(0.05),
                selected: accent_fill.alpha(0.13),
                nav_selected: accent_fill.alpha(0.12),
                accent: accent_fill,
                on_accent: Color::hex(0xffffff),
                scrollbar: Color::hex(0x221f1b).alpha(0.3),
                danger: Color::hex(0xb42318),
                ground: Color::hex(0xe9e5df),
            }
        }
    }

    /// A palette from the colors the person picked. Light or dark follows
    /// the background, so icons and hover shades still fit.
    pub fn custom(colors: &ThemeColors, material: Material) -> Self {
        let solid = material == Material::Solid;
        let background = Color::hex(colors.background);
        let panel = Color::hex(colors.panel);
        let text = Color::hex(colors.text);
        let accent = Color::hex(colors.accent);
        let dark = background.luminance() < 0.4;
        Self {
            dark,
            base: if solid {
                background
            } else {
                background.alpha(0.0)
            },
            tint: if solid {
                background
            } else {
                background.alpha(0.72)
            },
            text,
            text2: Color::hex(colors.text2),
            card: if solid { panel } else { panel.alpha(0.62) },
            line: text.alpha(0.10),
            hover: text.alpha(0.06),
            selected: accent.alpha(if dark { 0.22 } else { 0.16 }),
            nav_selected: accent.alpha(if dark { 0.18 } else { 0.14 }),
            accent,
            on_accent: if accent.luminance() > 0.4 {
                Color::hex(0x1d1a16)
            } else {
                Color::hex(0xffffff)
            },
            scrollbar: text.alpha(0.28),
            danger: if dark {
                Color::hex(0xff9b8f)
            } else {
                Color::hex(0xb42318)
            },
            ground: background,
        }
    }

    /// Blends two palettes for the theme cross-fade. Icons and other
    /// light/dark choices follow `to`.
    pub fn mix(from: &Self, to: &Self, t: f32) -> Self {
        Self {
            dark: to.dark,
            base: from.base.mix(to.base, t),
            tint: from.tint.mix(to.tint, t),
            text: from.text.mix(to.text, t),
            text2: from.text2.mix(to.text2, t),
            card: from.card.mix(to.card, t),
            line: from.line.mix(to.line, t),
            hover: from.hover.mix(to.hover, t),
            selected: from.selected.mix(to.selected, t),
            nav_selected: from.nav_selected.mix(to.nav_selected, t),
            accent: from.accent.mix(to.accent, t),
            on_accent: from.on_accent.mix(to.on_accent, t),
            scrollbar: from.scrollbar.mix(to.scrollbar, t),
            danger: from.danger.mix(to.danger, t),
            ground: from.ground.mix(to.ground, t),
        }
    }

    /// The opaque color used where no backdrop exists, such as screenshots.
    pub fn opaque_base(&self) -> Color {
        self.ground
    }

    /// The selected result row: a wash of the accent color.
    pub fn row_selected(&self) -> Color {
        self.selected
    }
}

/// The accent fill. Dark mode uses a lighter shade, paired with dark text,
/// so buttons keep their contrast on the charcoal background.
pub fn accent_color(accent: Accent, dark: bool) -> Color {
    let (light, dark_shade) = match accent {
        Accent::Saffron => (0xa85a06, 0xf0a43a),
        Accent::Cobalt => (0x2f57c9, 0x7b9cf5),
        Accent::Teal => (0x0d7a6d, 0x3cc7b4),
        Accent::Iris => (0x5b47d6, 0x9a8cff),
    };
    Color::hex(if dark { dark_shade } else { light })
}

/// Resolves the dark-mode setting against the Windows app theme.
pub fn resolve_dark(mode: DarkMode) -> bool {
    match mode {
        DarkMode::Light => false,
        DarkMode::Full => true,
        DarkMode::FollowSystem => system_prefers_dark(),
    }
}

/// Reads `AppsUseLightTheme` from the personalization key. Missing values
/// mean light, which is the Windows default.
pub fn system_prefers_dark() -> bool {
    let key: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize\0"
        .encode_utf16()
        .collect();
    let value: Vec<u16> = "AppsUseLightTheme\0".encode_utf16().collect();
    let mut data: u32 = 1;
    let mut size = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            &mut data as *mut u32 as *mut _,
            &mut size,
        )
    };
    status == 0 && data == 0
}

/// Applies the dark title-bar hint and the system backdrop. The whole client
/// area is treated as glass so translucent pixels show the backdrop. Older
/// Windows builds ignore the backdrop attribute; the window then shows the
/// opaque base color painted by the renderer.
pub fn apply_window_effects(hwnd: HWND, dark: bool, material: Material) {
    let margins = MARGINS {
        cxLeftWidth: -1,
        cxRightWidth: -1,
        cyTopHeight: -1,
        cyBottomHeight: -1,
    };
    let dark_flag: i32 = dark as i32;
    let backdrop = match material {
        Material::Mica => DWMSBT_MAINWINDOW,
        Material::Acrylic => DWMSBT_TRANSIENTWINDOW,
        Material::Solid => DWMSBT_NONE,
    };
    unsafe {
        DwmExtendFrameIntoClientArea(hwnd, &margins);
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
            &dark_flag as *const i32 as *const _,
            std::mem::size_of::<i32>() as u32,
        );
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_SYSTEMBACKDROP_TYPE as u32,
            &backdrop as *const i32 as *const _,
            std::mem::size_of::<i32>() as u32,
        );
        // With no backdrop, the frame color shows behind Windows' caption
        // buttons; match it to the solid background. Otherwise use the
        // default so the backdrop shows.
        const DWMWA_CAPTION_COLOR: u32 = 35;
        const DWMWA_COLOR_DEFAULT: u32 = 0xffff_ffff;
        let caption: u32 = if material == Material::Solid {
            let base = Palette::new(dark, Accent::Saffron, Material::Solid).base;
            let channel = |value: f32| (value * 255.0).round() as u32;
            channel(base.r) | (channel(base.g) << 8) | (channel(base.b) << 16)
        } else {
            DWMWA_COLOR_DEFAULT
        };
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_CAPTION_COLOR,
            &caption as *const u32 as *const _,
            std::mem::size_of::<u32>() as u32,
        );
    }
}

/// The color behind Windows' minimize, maximize, and close buttons on a
/// window with a solid background. It has to match what Feather paints
/// around them, which changes with the theme and with what sits there.
/// Draws Windows' own popup menus (Explorer's right-click menu, the tray
/// menu) dark or light to match Feather. Windows offers no documented way;
/// this is the uxtheme entry point Explorer and other apps use
/// (SetPreferredAppMode, ordinal 135, Windows 10 1903 and later), followed
/// by FlushMenuThemes (136) so open menus pick it up. Does nothing where
/// it isn't there.
pub fn set_menu_dark(dark: bool) {
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    type SetPreferredAppMode = unsafe extern "system" fn(i32) -> i32;
    type FlushMenuThemes = unsafe extern "system" fn();
    const FORCE_DARK: i32 = 2;
    const FORCE_LIGHT: i32 = 3;
    unsafe {
        let library = LoadLibraryW(windows_sys::core::w!("uxtheme.dll"));
        if library.is_null() {
            return;
        }
        if let Some(set) = GetProcAddress(library, 135 as *const u8) {
            let set: SetPreferredAppMode = std::mem::transmute(set);
            set(if dark { FORCE_DARK } else { FORCE_LIGHT });
        }
        if let Some(flush) = GetProcAddress(library, 136 as *const u8) {
            let flush: FlushMenuThemes = std::mem::transmute(flush);
            flush();
        }
    }
}

pub fn set_caption_color(hwnd: HWND, color: Color) {
    const DWMWA_CAPTION_COLOR: u32 = 35;
    let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u32;
    let caption: u32 = channel(color.r) | (channel(color.g) << 8) | (channel(color.b) << 16);
    unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_CAPTION_COLOR,
            &caption as *const u32 as *const _,
            std::mem::size_of::<u32>() as u32,
        );
    }
}

/// True when this Windows build supports `DWMWA_SYSTEMBACKDROP_TYPE`
/// (Windows 11 22H2, build 22621, or later).
pub fn backdrop_supported() -> bool {
    windows_build() >= 22621
}

fn windows_build() -> u32 {
    let key: Vec<u16> = "Software\\Microsoft\\Windows NT\\CurrentVersion\0"
        .encode_utf16()
        .collect();
    let value: Vec<u16> = "CurrentBuildNumber\0".encode_utf16().collect();
    let mut buffer = [0u16; 32];
    let mut size = std::mem::size_of_val(&buffer) as u32;
    let status = unsafe {
        RegGetValueW(
            windows_sys::Win32::System::Registry::HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            value.as_ptr(),
            windows_sys::Win32::System::Registry::RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buffer.as_mut_ptr() as *mut _,
            &mut size,
        )
    };
    if status != 0 {
        return 0;
    }
    let end = buffer
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..end])
        .trim()
        .parse()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn solid_material_paints_an_opaque_base() {
        let palette = Palette::new(false, Accent::Saffron, Material::Solid);
        assert_eq!(palette.base.a, 1.0);
        assert_eq!(palette.card.a, 1.0);
        let glass = Palette::new(true, Accent::Cobalt, Material::Mica);
        assert_eq!(glass.base.a, 0.0);
        assert!(glass.card.a < 1.0);
        assert_eq!(glass.accent, accent_color(Accent::Cobalt, true));
    }

    #[test]
    fn explicit_modes_ignore_the_system_theme() {
        assert!(!resolve_dark(DarkMode::Light));
        assert!(resolve_dark(DarkMode::Full));
    }
}
