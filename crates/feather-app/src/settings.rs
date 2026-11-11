//! Per-user settings and their small on-disk file.
//!
//! Settings live in `%APPDATA%\Feather\settings.ini`, a key=value file. The
//! same struct feeds the Settings window and the running UI, so adding a
//! setting means adding one field here and reading it where it matters.

use std::fmt::Write as _;
use std::fs;
use std::io::{ErrorKind, Write as _};
use std::path::{Path, PathBuf};

use feather_core::{is_absolute_path, normalize_path, MatchMode};

use crate::filters::{parse_extensions, Category, FilterSet, KindFilter, PRESET_CATEGORIES};
use crate::text;

#[cfg(windows)]
use crate::ui::hotkey;

/// The shortcut parser is Windows-specific (virtual-key codes). On Linux the
/// settings file keeps the same text form; the X11 backend normalizes it when
/// it registers the shortcut.
#[cfg(not(windows))]
mod hotkey {
    pub const DEFAULT: &str = "Ctrl + Space";
    pub const OFF: &str = "None";

    pub fn normalize(text: &str) -> Result<String, String> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(DEFAULT.to_string());
        }
        if text.eq_ignore_ascii_case("none") || text.eq_ignore_ascii_case("off") {
            return Ok(OFF.to_string());
        }
        let mut parts = Vec::new();
        for part in text
            .split('+')
            .map(str::trim)
            .filter(|part| !part.is_empty())
        {
            let canonical = match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => "Ctrl",
                "alt" => "Alt",
                "shift" => "Shift",
                "win" | "super" | "meta" => "Win",
                "space" => "Space",
                other if other.len() == 1 => {
                    let upper = other.to_ascii_uppercase();
                    parts.push(upper);
                    continue;
                }
                _ => part,
            };
            parts.push(canonical.to_string());
        }
        if parts.is_empty() {
            return Err("a shortcut needs at least one key".into());
        }
        Ok(parts.join(" + "))
    }
}

/// How much of the index a search may list; 100 lists every result.
pub const DEFAULT_RESULTS_PERCENT: u32 = 100;
pub const DEFAULT_SEARCH_DELAY_MS: u64 = 25;
pub const MAX_SEARCH_DELAY_MS: u64 = 2_000;
const SETTINGS_FILE: &str = "settings.ini";
const MIN_WINDOW_WIDTH: i32 = 640;
const MIN_WINDOW_HEIGHT: i32 = 360;
const MAX_WINDOW_SIZE: i32 = 16_384;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OpenMode {
    #[default]
    File,
    ContainingFolder,
}

impl OpenMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::ContainingFolder => "folder",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "file" | "item" | "open" => Some(Self::File),
            "folder" | "containing_folder" | "containing-folder" => Some(Self::ContainingFolder),
            _ => None,
        }
    }
}

/// How soon the index follows changes on disk.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AutoUpdate {
    /// A few seconds after things settle.
    #[default]
    Live,
    Minutes15,
    Hourly,
    /// Only when asked.
    Off,
}

impl AutoUpdate {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Minutes15 => "15min",
            Self::Hourly => "hourly",
            Self::Off => "off",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "live" | "auto" | "on" => Some(Self::Live),
            "15min" | "15m" | "15" => Some(Self::Minutes15),
            "hourly" | "hour" | "1h" | "60min" => Some(Self::Hourly),
            "off" | "never" | "manual" => Some(Self::Off),
            _ => None,
        }
    }

    /// The least time between two automatic rebuilds; `None` means never.
    pub fn wait(self) -> Option<std::time::Duration> {
        let seconds = match self {
            Self::Live => 0,
            Self::Minutes15 => 15 * 60,
            Self::Hourly => 60 * 60,
            Self::Off => return None,
        };
        Some(std::time::Duration::from_secs(seconds))
    }
}

/// A setting with a few named choices, saved as a short word. The first
/// choice is the default.
macro_rules! choice {
    ($(#[$meta:meta])* pub enum $name:ident { $($variant:ident => $word:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub enum $name {
            $($variant),+
        }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),+];

            pub fn as_str(self) -> &'static str {
                match self {
                    $($name::$variant => $word),+
                }
            }

            pub fn parse(value: &str) -> Option<Self> {
                let value = value.trim();
                Self::ALL
                    .iter()
                    .copied()
                    .find(|choice| choice.as_str().eq_ignore_ascii_case(value))
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::ALL[0]
            }
        }
    };
}

choice! {
    /// How file sizes are written in the results and the details pane.
    pub enum SizeFormat {
        Auto => "auto",
        Kb => "kb",
        Mb => "mb",
        Gb => "gb",
    }
}

choice! {
    /// How a background image covers the window.
    pub enum BackgroundFit {
        Fill => "fill",
        Fit => "fit",
        Stretch => "stretch",
        Center => "center",
        Tile => "tile",
    }
}

choice! {
    /// What the web server lets people do.
    pub enum HttpMode {
        Off => "off",
        Download => "download",
        Search => "search",
        Full => "full",
    }
}

choice! {
    /// What the FTP server lets people do.
    pub enum FtpMode {
        Off => "off",
        Read => "read",
        Full => "full",
    }
}

choice! {
    /// Who may connect to the servers.
    pub enum ShareAccess {
        Local => "local",
        Network => "network",
        Internet => "internet",
    }
}

choice! {
    /// How the web server is reached from the internet.
    pub enum Tunnel {
        Cloudflare => "cloudflare",
        Ngrok => "ngrok",
        Direct => "direct",
    }
}

/// The colors of a theme the person made, as `0xRRGGBB`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ThemeColors {
    pub background: u32,
    pub panel: u32,
    pub text: u32,
    pub text2: u32,
    pub accent: u32,
}

impl Default for ThemeColors {
    /// Starts from the dark Ember palette.
    fn default() -> Self {
        Self {
            background: 0x181614,
            panel: 0x272320,
            text: 0xf0ece6,
            text2: 0xa39a8f,
            accent: 0xf2a33a,
        }
    }
}

impl ThemeColors {
    fn as_text(&self) -> String {
        [
            self.background,
            self.panel,
            self.text,
            self.text2,
            self.accent,
        ]
        .iter()
        .map(|rgb| format!("#{rgb:06x}"))
        .collect::<Vec<_>>()
        .join(",")
    }

    fn parse(value: &str) -> Option<Self> {
        let colors: Vec<u32> = value
            .split(',')
            .map(|part| u32::from_str_radix(part.trim().trim_start_matches('#'), 16).ok())
            .collect::<Option<Vec<_>>>()?;
        if colors.len() != 5 || colors.iter().any(|rgb| *rgb > 0xff_ffff) {
            return None;
        }
        Some(Self {
            background: colors[0],
            panel: colors[1],
            text: colors[2],
            text2: colors[3],
            accent: colors[4],
        })
    }
}

pub const DEFAULT_HTTP_PORT: u16 = 8443;
pub const DEFAULT_FTP_PORT: u16 = 2121;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SortMode {
    #[default]
    Ordinal,
    Natural,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DarkMode {
    Light,
    #[default]
    FollowSystem,
    Full,
}

/// The window backdrop. Mica and Acrylic need Windows 11 22H2 or later; older
/// builds draw the solid look instead.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Material {
    Mica,
    #[default]
    Acrylic,
    Solid,
}

impl Material {
    pub const ALL: [Material; 3] = [Material::Mica, Material::Acrylic, Material::Solid];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mica => "mica",
            Self::Acrylic => "acrylic",
            Self::Solid => "solid",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Mica => "Mica (heavy blur)",
            Self::Acrylic => "Acrylic (light blur)",
            Self::Solid => "Solid (no blur)",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "mica" => Some(Self::Mica),
            "acrylic" => Some(Self::Acrylic),
            "solid" | "none" => Some(Self::Solid),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Accent {
    #[default]
    Saffron,
    Cobalt,
    Teal,
    Iris,
}

impl Accent {
    pub const ALL: [Accent; 4] = [Accent::Saffron, Accent::Cobalt, Accent::Teal, Accent::Iris];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Saffron => "saffron",
            Self::Cobalt => "cobalt",
            Self::Teal => "teal",
            Self::Iris => "iris",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Saffron => "Saffron",
            Self::Cobalt => "Cobalt",
            Self::Teal => "Teal",
            Self::Iris => "Iris",
        }
    }

    /// Also accepts the names used before the Ember palette, so older
    /// settings files keep loading without a warning.
    fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        let legacy = match value.to_ascii_lowercase().as_str() {
            "green" | "amber" => Some(Self::Saffron),
            "blue" => Some(Self::Cobalt),
            "plum" => Some(Self::Iris),
            _ => None,
        };
        legacy.or_else(|| {
            Self::ALL
                .into_iter()
                .find(|accent| accent.as_str().eq_ignore_ascii_case(value))
        })
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum NetworkAccess {
    #[default]
    None,
    ManualUpdate,
    Automatic,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowPlacement {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub maximized: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Settings {
    /// 1 to 100: the share of all indexed items one search may list.
    pub results_percent: u32,
    pub search_delay_ms: u64,
    pub open_mode: OpenMode,
    pub auto_update: AutoUpdate,
    pub show_all_when_empty: bool,
    pub remember_filters: bool,
    pub remember_scope: bool,
    pub remember_window: bool,
    pub always_on_top: bool,
    pub show_hidden: bool,
    pub show_system: bool,
    pub use_shell_context_menu: bool,
    pub default_category: Category,
    pub start_at_login: bool,
    pub close_to_tray: bool,
    pub tray_icon_visible: bool,
    pub hotkey: String,
    pub start_menu_shortcut: bool,
    pub desktop_shortcut: bool,
    pub filters: FilterSet,
    pub excluded_volumes: Vec<String>,
    pub excluded_folders: Vec<String>,
    /// Folders and drives the in-app indexer scans.
    pub index_roots: Vec<String>,
    /// False until the first-run setup has been completed or skipped.
    pub setup_done: bool,
    /// The sidebar's kinds in display order, each shown or hidden.
    pub sidebar_tabs: Vec<(Category, bool)>,
    /// The sidebar folded down to its icons.
    pub sidebar_collapsed: bool,
    pub scan_excluded_paths: Vec<String>,
    pub scan_excluded_names: Vec<String>,
    pub index_location: String,
    pub rescan_policy: String,
    pub admin_only: bool,
    pub sort_mode: SortMode,
    pub folders_first: bool,
    pub dark_mode: DarkMode,
    pub material: Material,
    pub accent: Accent,
    pub network_access: NetworkAccess,
    pub scope: Option<String>,
    pub window: Option<WindowPlacement>,
    pub size_format: SizeFormat,
    /// The details pane on the right of the results.
    pub details_pane: bool,
    /// Paint with `theme_colors` instead of the built-in palette.
    pub custom_theme: bool,
    pub theme_colors: ThemeColors,
    /// A copy of the chosen picture in Feather's folder; empty for none.
    pub background_image: String,
    pub background_fit: BackgroundFit,
    /// 0 to 100: how strongly the picture shows through.
    pub background_strength: u32,
    pub http_mode: HttpMode,
    pub ftp_mode: FtpMode,
    pub share_access: ShareAccess,
    pub tunnel: Tunnel,
    pub http_port: u16,
    pub ftp_port: u16,
    pub share_user: String,
    /// An Argon2 hash of the sharing password; empty until one is set.
    pub share_password: String,
    pub share_folders: Vec<String>,
    /// Lets old FTP apps connect without TLS.
    pub ftp_plain: bool,
    /// The ngrok authtoken or Cloudflare tunnel token, encrypted for this
    /// Windows account.
    pub tunnel_token: String,
    /// The sharing password itself, encrypted for this Windows account, so
    /// Settings can show it again.
    pub share_password_saved: String,
    /// Refuse weak sharing passwords.
    pub share_strong_password: bool,
    /// Encrypt every request and reply in the page itself, so not even a
    /// tunnel can read them.
    pub share_e2e: bool,
    /// ngrok's static domain, so the internet address never changes.
    pub tunnel_domain: String,
}

impl Settings {
    /// True when either server should run.
    pub fn sharing_on(&self) -> bool {
        self.http_mode != HttpMode::Off || self.ftp_mode != FtpMode::Off
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            results_percent: DEFAULT_RESULTS_PERCENT,
            search_delay_ms: DEFAULT_SEARCH_DELAY_MS,
            open_mode: OpenMode::File,
            auto_update: AutoUpdate::Live,
            show_all_when_empty: true,
            remember_filters: false,
            remember_scope: false,
            remember_window: true,
            always_on_top: false,
            show_hidden: true,
            show_system: true,
            use_shell_context_menu: true,
            default_category: Category::Everything,
            start_at_login: false,
            close_to_tray: true,
            tray_icon_visible: false,
            hotkey: hotkey::DEFAULT.to_string(),
            filters: FilterSet::default(),
            excluded_volumes: Vec::new(),
            excluded_folders: Vec::new(),
            index_roots: Vec::new(),
            setup_done: false,
            sidebar_tabs: default_sidebar_tabs(),
            sidebar_collapsed: false,
            scan_excluded_paths: Vec::new(),
            scan_excluded_names: vec![
                "System Volume Information".into(),
                "$Recycle.Bin".into(),
                "WinSxS".into(),
                "node_modules".into(),
                ".git".into(),
            ],
            index_location: String::new(),
            rescan_policy: "manual".into(),
            admin_only: false,
            sort_mode: SortMode::Ordinal,
            folders_first: false,
            dark_mode: DarkMode::FollowSystem,
            material: Material::Acrylic,
            accent: Accent::Saffron,
            network_access: NetworkAccess::None,
            start_menu_shortcut: true,
            desktop_shortcut: true,
            scope: None,
            window: None,
            size_format: SizeFormat::Auto,
            details_pane: true,
            custom_theme: false,
            theme_colors: ThemeColors::default(),
            background_image: String::new(),
            background_fit: BackgroundFit::Fill,
            background_strength: 60,
            http_mode: HttpMode::Off,
            ftp_mode: FtpMode::Off,
            share_access: ShareAccess::Network,
            tunnel: Tunnel::Cloudflare,
            http_port: DEFAULT_HTTP_PORT,
            ftp_port: DEFAULT_FTP_PORT,
            share_user: "feather".into(),
            share_password: String::new(),
            share_folders: Vec::new(),
            ftp_plain: false,
            tunnel_token: String::new(),
            share_password_saved: String::new(),
            share_strong_password: false,
            share_e2e: false,
            tunnel_domain: String::new(),
        }
    }
}

impl Settings {
    /// The most results one search lists when the index holds `items`.
    pub fn result_cap(&self, items: u64) -> usize {
        result_cap(self.results_percent, items)
    }

    pub fn normalize(&mut self) {
        self.results_percent = self.results_percent.clamp(1, 100);
        self.search_delay_ms = self.search_delay_ms.min(MAX_SEARCH_DELAY_MS);
        self.excluded_volumes = normalize_list(std::mem::take(&mut self.excluded_volumes));
        self.excluded_folders = normalize_list(std::mem::take(&mut self.excluded_folders));
        // Whole drives keep their backslash: `C:` alone is not a full path
        // and used to be dropped the next time settings loaded.
        self.index_roots =
            with_drive_backslash(normalize_list(std::mem::take(&mut self.index_roots)));
        self.sidebar_tabs = normalize_tabs(std::mem::take(&mut self.sidebar_tabs));
        self.background_strength = self.background_strength.min(100);
        self.share_folders =
            with_drive_backslash(normalize_list(std::mem::take(&mut self.share_folders)));
        if self.http_port == 0 {
            self.http_port = DEFAULT_HTTP_PORT;
        }
        if self.ftp_port == 0 {
            self.ftp_port = DEFAULT_FTP_PORT;
        }
        self.share_user = self.share_user.trim().to_string();
        self.scope = self
            .scope
            .take()
            .map(trim_scope)
            .filter(|scope| !scope.is_empty());
        if let Some(window) = &mut self.window {
            window.width = window.width.clamp(MIN_WINDOW_WIDTH, MAX_WINDOW_SIZE);
            window.height = window.height.clamp(MIN_WINDOW_HEIGHT, MAX_WINDOW_SIZE);
        }
    }

    pub fn to_text(&self) -> String {
        let mut text = String::from("; Feather settings. Delete this file to restore defaults.\n");
        let _ = writeln!(text, "version=2");
        let _ = writeln!(text, "results_percent={}", self.results_percent);
        let _ = writeln!(text, "search_delay_ms={}", self.search_delay_ms);
        let _ = writeln!(text, "open_mode={}", self.open_mode.as_str());
        let _ = writeln!(text, "auto_update={}", self.auto_update.as_str());
        let _ = writeln!(
            text,
            "show_all_when_empty={}",
            bool_value(self.show_all_when_empty)
        );
        let _ = writeln!(
            text,
            "remember_filters={}",
            bool_value(self.remember_filters)
        );
        let _ = writeln!(text, "remember_scope={}", bool_value(self.remember_scope));
        let _ = writeln!(text, "remember_window={}", bool_value(self.remember_window));
        let _ = writeln!(text, "always_on_top={}", bool_value(self.always_on_top));
        let _ = writeln!(text, "show_hidden={}", bool_value(self.show_hidden));
        let _ = writeln!(text, "show_system={}", bool_value(self.show_system));
        let _ = writeln!(
            text,
            "use_shell_context_menu={}",
            bool_value(self.use_shell_context_menu)
        );
        let _ = writeln!(
            text,
            "default_category={}",
            self.default_category.label().to_ascii_lowercase()
        );
        let _ = writeln!(text, "start_at_login={}", bool_value(self.start_at_login));
        let _ = writeln!(text, "close_to_tray={}", bool_value(self.close_to_tray));
        let _ = writeln!(
            text,
            "tray_icon_visible={}",
            bool_value(self.tray_icon_visible)
        );
        let _ = writeln!(text, "hotkey={}", self.hotkey);
        let _ = writeln!(text, "excluded_volumes={}", self.excluded_volumes.join(";"));
        let _ = writeln!(text, "excluded_folders={}", self.excluded_folders.join(";"));
        let _ = writeln!(text, "index_roots={}", self.index_roots.join(";"));
        let _ = writeln!(text, "setup_done={}", bool_value(self.setup_done));
        let tabs: Vec<String> = self
            .sidebar_tabs
            .iter()
            .map(|(category, shown)| {
                let name = category.label().to_ascii_lowercase();
                if *shown {
                    name
                } else {
                    format!("-{name}")
                }
            })
            .collect();
        let _ = writeln!(text, "sidebar_tabs={}", tabs.join(";"));
        let _ = writeln!(
            text,
            "sidebar_collapsed={}",
            bool_value(self.sidebar_collapsed)
        );
        let _ = writeln!(
            text,
            "scan_excluded_paths={}",
            self.scan_excluded_paths.join(";")
        );
        let _ = writeln!(
            text,
            "scan_excluded_names={}",
            self.scan_excluded_names.join(";")
        );
        let _ = writeln!(text, "index_location={}", self.index_location);
        let _ = writeln!(text, "rescan_policy={}", self.rescan_policy);
        let _ = writeln!(text, "admin_only={}", bool_value(self.admin_only));
        let _ = writeln!(text, "sort_mode={}", sort_mode_name(self.sort_mode));
        let _ = writeln!(text, "folders_first={}", bool_value(self.folders_first));
        let _ = writeln!(text, "dark_mode={}", dark_mode_name(self.dark_mode));
        let _ = writeln!(text, "material={}", self.material.as_str());
        let _ = writeln!(text, "accent={}", self.accent.as_str());
        let _ = writeln!(
            text,
            "network_access={}",
            network_access_name(self.network_access)
        );
        if self.remember_scope {
            if let Some(scope) = &self.scope {
                let _ = writeln!(text, "scope={scope}");
            }
        }
        let _ = writeln!(text, "filter_kind={}", kind_name(self.filters.kind));
        let _ = writeln!(
            text,
            "filter_extensions={}",
            self.filters.extensions.join(";")
        );
        let _ = writeln!(
            text,
            "filter_min_size={}",
            self.filters
                .min_size
                .map(|value| value.to_string())
                .unwrap_or_default()
        );
        let _ = writeln!(
            text,
            "filter_max_size={}",
            self.filters
                .max_size
                .map(|value| value.to_string())
                .unwrap_or_default()
        );
        let _ = writeln!(text, "filter_match={}", self.filters.match_mode.as_str());
        let _ = writeln!(text, "size_format={}", self.size_format.as_str());
        let _ = writeln!(text, "details_pane={}", bool_value(self.details_pane));
        let _ = writeln!(text, "custom_theme={}", bool_value(self.custom_theme));
        let _ = writeln!(text, "theme_colors={}", self.theme_colors.as_text());
        let _ = writeln!(text, "background_image={}", self.background_image);
        let _ = writeln!(text, "background_fit={}", self.background_fit.as_str());
        let _ = writeln!(text, "background_strength={}", self.background_strength);
        let _ = writeln!(text, "http_mode={}", self.http_mode.as_str());
        let _ = writeln!(text, "ftp_mode={}", self.ftp_mode.as_str());
        let _ = writeln!(text, "share_access={}", self.share_access.as_str());
        let _ = writeln!(text, "tunnel={}", self.tunnel.as_str());
        let _ = writeln!(text, "http_port={}", self.http_port);
        let _ = writeln!(text, "ftp_port={}", self.ftp_port);
        let _ = writeln!(text, "share_user={}", self.share_user);
        let _ = writeln!(text, "share_password={}", self.share_password);
        let _ = writeln!(text, "share_folders={}", self.share_folders.join(";"));
        let _ = writeln!(text, "ftp_plain={}", bool_value(self.ftp_plain));
        let _ = writeln!(text, "tunnel_token={}", self.tunnel_token);
        let _ = writeln!(text, "share_password_saved={}", self.share_password_saved);
        let _ = writeln!(
            text,
            "share_strong_password={}",
            bool_value(self.share_strong_password)
        );
        let _ = writeln!(text, "share_e2e={}", bool_value(self.share_e2e));
        let _ = writeln!(text, "tunnel_domain={}", self.tunnel_domain);
        if let Some(window) = self.window {
            let _ = writeln!(
                text,
                "window={},{},{},{},{}",
                window.x,
                window.y,
                window.width,
                window.height,
                bool_value(window.maximized)
            );
        }
        text
    }

    pub fn save(&self) -> Result<(), String> {
        let path = file_path().ok_or_else(|| "APPDATA is not set".to_string())?;
        self.save_to(&path)
    }

    pub fn save_to(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)
                    .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
            }
        }
        let temp = path.with_extension(format!("tmp-{}", std::process::id()));
        {
            let mut file = fs::File::create(&temp)
                .map_err(|error| format!("could not write {}: {error}", temp.display()))?;
            file.write_all(self.to_text().as_bytes())
                .map_err(|error| format!("could not write {}: {error}", temp.display()))?;
            file.sync_all()
                .map_err(|error| format!("could not flush {}: {error}", temp.display()))?;
        }
        if let Err(error) = fs::rename(&temp, path) {
            let _ = fs::remove_file(&temp);
            return Err(format!("could not replace {}: {error}", path.display()));
        }
        Ok(())
    }
}

pub struct LoadedSettings {
    pub settings: Settings,
    pub warnings: Vec<String>,
}

#[cfg(windows)]
pub fn file_path() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    if appdata.is_empty() {
        return None;
    }
    Some(PathBuf::from(appdata).join("Feather").join(SETTINGS_FILE))
}

#[cfg(not(windows))]
pub fn file_path() -> Option<PathBuf> {
    {
        if let Some(config) = std::env::var_os("XDG_CONFIG_HOME") {
            if !config.is_empty() {
                return Some(PathBuf::from(config).join("feather").join(SETTINGS_FILE));
            }
        }
        let home = std::env::var_os("HOME")?;
        if home.is_empty() {
            return None;
        }
        Some(
            PathBuf::from(home)
                .join(".config")
                .join("feather")
                .join(SETTINGS_FILE),
        )
    }
}

pub fn load() -> LoadedSettings {
    match file_path() {
        Some(path) => load_from(&path),
        None => LoadedSettings {
            settings: Settings::default(),
            warnings: vec![
                "the configuration directory is not set; settings will not be saved".into(),
            ],
        },
    }
}

pub fn load_from(path: &Path) -> LoadedSettings {
    match fs::read_to_string(path) {
        Ok(text) => parse(&text),
        Err(error) if error.kind() == ErrorKind::NotFound => LoadedSettings {
            settings: Settings::default(),
            warnings: Vec::new(),
        },
        Err(error) => LoadedSettings {
            settings: Settings::default(),
            warnings: vec![format!("could not read {}: {error}", path.display())],
        },
    }
}

pub fn parse(text: &str) -> LoadedSettings {
    let mut settings = Settings::default();
    let mut warnings = Vec::new();

    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim().trim_start_matches('\u{feff}');
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            warnings.push(format!("line {}: missing '='", index + 1));
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        let invalid = || format!("line {}: invalid {key} value: {value}", index + 1);

        match key {
            "version" => {}
            // A fixed row limit from older versions; the slider replaced it.
            "max_results" => {}
            "results_percent" => match value.parse::<u32>() {
                Ok(number) => {
                    if !(1..=100).contains(&number) {
                        warnings.push(invalid());
                    }
                    settings.results_percent = number;
                }
                Err(_) => warnings.push(invalid()),
            },
            "search_delay_ms" => match value.parse::<u64>() {
                Ok(number) => {
                    if number > MAX_SEARCH_DELAY_MS {
                        warnings.push(invalid());
                    }
                    settings.search_delay_ms = number;
                }
                Err(_) => warnings.push(invalid()),
            },
            "open_mode" => match OpenMode::parse(value) {
                Some(mode) => settings.open_mode = mode,
                None => warnings.push(invalid()),
            },
            "auto_update" => match AutoUpdate::parse(value) {
                Some(mode) => settings.auto_update = mode,
                None => warnings.push(invalid()),
            },
            "show_all_when_empty" | "show_all_on_launch" => match parse_bool(value) {
                Some(flag) => settings.show_all_when_empty = flag,
                None => warnings.push(invalid()),
            },
            "remember_filters" => match parse_bool(value) {
                Some(flag) => settings.remember_filters = flag,
                None => warnings.push(invalid()),
            },
            "remember_scope" => match parse_bool(value) {
                Some(flag) => settings.remember_scope = flag,
                None => warnings.push(invalid()),
            },
            "remember_window" => match parse_bool(value) {
                Some(flag) => settings.remember_window = flag,
                None => warnings.push(invalid()),
            },
            "always_on_top" => match parse_bool(value) {
                Some(flag) => settings.always_on_top = flag,
                None => warnings.push(invalid()),
            },
            "show_hidden" => match parse_bool(value) {
                Some(flag) => settings.show_hidden = flag,
                None => warnings.push(invalid()),
            },
            "show_system" => match parse_bool(value) {
                Some(flag) => settings.show_system = flag,
                None => warnings.push(invalid()),
            },
            "use_shell_context_menu" => match parse_bool(value) {
                Some(flag) => settings.use_shell_context_menu = flag,
                None => warnings.push(invalid()),
            },
            "excluded_volumes" => {
                settings.excluded_volumes = parse_volume_list(value);
            }
            "excluded_folders" => match parse_folder_list(value) {
                Ok(folders) => settings.excluded_folders = folders,
                Err(_) => warnings.push(invalid()),
            },
            "index_roots" => match parse_folder_list(value) {
                Ok(roots) => settings.index_roots = with_drive_backslash(roots),
                Err(_) => warnings.push(invalid()),
            },
            "setup_done" => match parse_bool(value) {
                Some(flag) => settings.setup_done = flag,
                None => warnings.push(invalid()),
            },
            "sidebar_tabs" => {
                let mut tabs = Vec::new();
                for part in value
                    .split(';')
                    .map(str::trim)
                    .filter(|part| !part.is_empty())
                {
                    let (shown, name) = match part.strip_prefix('-') {
                        Some(name) => (false, name),
                        None => (true, part),
                    };
                    match Category::parse(name) {
                        Some(category) => tabs.push((category, shown)),
                        None => warnings.push(invalid()),
                    }
                }
                settings.sidebar_tabs = normalize_tabs(tabs);
            }
            "sidebar_collapsed" => match parse_bool(value) {
                Some(flag) => settings.sidebar_collapsed = flag,
                None => warnings.push(invalid()),
            },
            "scan_excluded_paths" => {
                settings.scan_excluded_paths = parse_volume_list(value);
            }
            "scan_excluded_names" => {
                settings.scan_excluded_names = parse_volume_list(value);
            }
            "index_location" => {
                settings.index_location = value.trim().to_string();
            }
            "rescan_policy" => {
                settings.rescan_policy = value.trim().to_string();
            }
            "admin_only" => match parse_bool(value) {
                Some(flag) => settings.admin_only = flag,
                None => warnings.push(invalid()),
            },
            "default_category" => match Category::parse(value) {
                Some(category) => settings.default_category = category,
                None => warnings.push(invalid()),
            },
            "start_at_login" => match parse_bool(value) {
                Some(flag) => settings.start_at_login = flag,
                None => warnings.push(invalid()),
            },
            "close_to_tray" => match parse_bool(value) {
                Some(flag) => settings.close_to_tray = flag,
                None => warnings.push(invalid()),
            },
            "tray_icon_visible" => match parse_bool(value) {
                Some(flag) => settings.tray_icon_visible = flag,
                None => warnings.push(invalid()),
            },
            // Empty means the default, so files from before the shortcut
            // existed get Ctrl+Space.
            "hotkey" if value.trim().is_empty() => settings.hotkey = hotkey::DEFAULT.to_string(),
            "hotkey" => match hotkey::normalize(value) {
                Ok(text) => settings.hotkey = text,
                Err(_) => warnings.push(invalid()),
            },
            "sort_mode" => match parse_sort_mode(value) {
                Some(mode) => settings.sort_mode = mode,
                None => warnings.push(invalid()),
            },
            "folders_first" => match parse_bool(value) {
                Some(flag) => settings.folders_first = flag,
                None => warnings.push(invalid()),
            },
            "dark_mode" => match parse_dark_mode(value) {
                Some(mode) => settings.dark_mode = mode,
                None => warnings.push(invalid()),
            },
            "material" => match Material::parse(value) {
                Some(material) => settings.material = material,
                None => warnings.push(invalid()),
            },
            "accent" => match Accent::parse(value) {
                Some(accent) => settings.accent = accent,
                None => warnings.push(invalid()),
            },
            "network_access" => match parse_network_access(value) {
                Some(mode) => settings.network_access = mode,
                None => warnings.push(invalid()),
            },
            "scope" => {
                let scope = value.trim();
                if scope.is_empty() {
                    settings.scope = None;
                } else if is_absolute_path(scope) {
                    settings.scope = Some(scope.to_string());
                } else {
                    warnings.push(invalid());
                }
            }
            "filter_kind" => match parse_kind(value) {
                Some(kind) => settings.filters.kind = kind,
                None => warnings.push(invalid()),
            },
            "filter_extensions" => match parse_extensions(value) {
                Ok(extensions) => settings.filters.extensions = extensions,
                Err(_) => warnings.push(invalid()),
            },
            "filter_min_size" => match parse_optional_size(value) {
                Some(size) => settings.filters.min_size = size,
                None => warnings.push(invalid()),
            },
            "filter_max_size" => match parse_optional_size(value) {
                Some(size) => settings.filters.max_size = size,
                None => warnings.push(invalid()),
            },
            "filter_match" => match MatchMode::parse(value) {
                Some(mode) => settings.filters.match_mode = mode,
                None => warnings.push(invalid()),
            },
            "window" => match parse_window(value) {
                Some(window) => settings.window = Some(window),
                None => warnings.push(invalid()),
            },
            "size_format" => match SizeFormat::parse(value) {
                Some(format) => settings.size_format = format,
                None => warnings.push(invalid()),
            },
            "details_pane" => match parse_bool(value) {
                Some(flag) => settings.details_pane = flag,
                None => warnings.push(invalid()),
            },
            "custom_theme" => match parse_bool(value) {
                Some(flag) => settings.custom_theme = flag,
                None => warnings.push(invalid()),
            },
            "theme_colors" => match ThemeColors::parse(value) {
                Some(colors) => settings.theme_colors = colors,
                None => warnings.push(invalid()),
            },
            "background_image" => settings.background_image = value.to_string(),
            "background_fit" => match BackgroundFit::parse(value) {
                Some(fit) => settings.background_fit = fit,
                None => warnings.push(invalid()),
            },
            "background_strength" => match value.parse::<u32>() {
                Ok(number) if number <= 100 => settings.background_strength = number,
                _ => warnings.push(invalid()),
            },
            "http_mode" => match HttpMode::parse(value) {
                Some(mode) => settings.http_mode = mode,
                None => warnings.push(invalid()),
            },
            "ftp_mode" => match FtpMode::parse(value) {
                Some(mode) => settings.ftp_mode = mode,
                None => warnings.push(invalid()),
            },
            "share_access" => match ShareAccess::parse(value) {
                Some(access) => settings.share_access = access,
                None => warnings.push(invalid()),
            },
            "tunnel" => match Tunnel::parse(value) {
                Some(tunnel) => settings.tunnel = tunnel,
                None => warnings.push(invalid()),
            },
            "http_port" => match value.parse::<u16>() {
                Ok(port) if port > 0 => settings.http_port = port,
                _ => warnings.push(invalid()),
            },
            "ftp_port" => match value.parse::<u16>() {
                Ok(port) if port > 0 => settings.ftp_port = port,
                _ => warnings.push(invalid()),
            },
            "share_user" => settings.share_user = value.to_string(),
            "share_password" => settings.share_password = value.to_string(),
            "share_folders" => match parse_folder_list(value) {
                Ok(folders) => settings.share_folders = folders,
                Err(_) => warnings.push(invalid()),
            },
            "ftp_plain" => match parse_bool(value) {
                Some(flag) => settings.ftp_plain = flag,
                None => warnings.push(invalid()),
            },
            "tunnel_token" => settings.tunnel_token = value.to_string(),
            "share_password_saved" => settings.share_password_saved = value.to_string(),
            "share_strong_password" => match parse_bool(value) {
                Some(flag) => settings.share_strong_password = flag,
                None => warnings.push(invalid()),
            },
            "tunnel_domain" => settings.tunnel_domain = value.to_string(),
            "share_e2e" => match parse_bool(value) {
                Some(flag) => settings.share_e2e = flag,
                None => warnings.push(invalid()),
            },
            _ => {}
        }
    }

    settings.normalize();
    LoadedSettings { settings, warnings }
}

/// The most results a search lists at `percent` of `items`; 100 is no limit.
pub fn result_cap(percent: u32, items: u64) -> usize {
    if percent >= 100 {
        return usize::MAX;
    }
    let share = (items as u128 * percent.max(1) as u128).div_ceil(100);
    (share as usize).max(1)
}

/// The words above the results slider.
#[cfg_attr(not(windows), allow(dead_code))]
fn results_caption(percent: u32, items: u64) -> String {
    if percent >= 100 {
        return "100% \u{2013} all results".to_string();
    }
    if items == 0 {
        return format!("{percent}%");
    }
    format!(
        "{percent}% \u{2013} up to {}",
        text::group(result_cap(percent, items) as u64)
    )
}

pub fn parse_delay(input: &str) -> Result<u64, String> {
    let value = input
        .trim()
        .parse::<u64>()
        .map_err(|_| "Enter a whole number for the search delay.".to_string())?;
    if value > MAX_SEARCH_DELAY_MS {
        return Err(format!(
            "Search delay must be between 0 and {MAX_SEARCH_DELAY_MS} milliseconds."
        ));
    }
    Ok(value)
}

/// Keys accepted by `feather config`. Kept explicit so a typo is an error
/// instead of a silently ignored line.
pub const SETTING_KEYS: &[&str] = &[
    "results_percent",
    "search_delay_ms",
    "open_mode",
    "auto_update",
    "show_all_when_empty",
    "remember_filters",
    "remember_scope",
    "remember_window",
    "always_on_top",
    "show_hidden",
    "show_system",
    "use_shell_context_menu",
    "default_category",
    "start_at_login",
    "close_to_tray",
    "tray_icon_visible",
    "hotkey",
    "excluded_volumes",
    "excluded_folders",
    "index_roots",
    "setup_done",
    "sidebar_tabs",
    "sidebar_collapsed",
    "scan_excluded_paths",
    "scan_excluded_names",
    "index_location",
    "rescan_policy",
    "admin_only",
    "sort_mode",
    "folders_first",
    "dark_mode",
    "material",
    "accent",
    "network_access",
    "scope",
    "filter_kind",
    "filter_extensions",
    "filter_min_size",
    "filter_max_size",
    "filter_match",
    "window",
    "size_format",
    "details_pane",
    "custom_theme",
    "theme_colors",
    "background_image",
    "background_fit",
    "background_strength",
    "http_mode",
    "ftp_mode",
    "share_access",
    "tunnel",
    "http_port",
    "ftp_port",
    "share_user",
    "share_folders",
    "ftp_plain",
    "share_strong_password",
    "share_e2e",
    "tunnel_domain",
];

/// Applies one key=value pair to an existing settings value. Parsing the
/// composed text reuses the same validation and clamping as the INI loader.
pub fn set_value(settings: &mut Settings, key: &str, value: &str) -> Result<(), String> {
    if !SETTING_KEYS.contains(&key) {
        return Err(format!("unknown setting: {key}"));
    }
    if value.contains('\n') || value.contains('\r') {
        return Err("setting values cannot contain line breaks".into());
    }
    let prefix = format!("{key}=");
    let mut lines: Vec<String> = settings
        .to_text()
        .lines()
        .map(|line| {
            if line.starts_with(&prefix) {
                format!("{key}={value}")
            } else {
                line.to_string()
            }
        })
        .collect();
    if !lines.iter().any(|line| line.starts_with(&prefix)) {
        lines.push(format!("{key}={value}"));
    }
    let parsed = parse(&lines.join("\n"));
    if let Some(warning) = parsed.warnings.iter().find(|warning| warning.contains(key)) {
        return Err(warning.clone());
    }
    *settings = parsed.settings;
    Ok(())
}

pub fn get_value(settings: &Settings, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    settings
        .to_text()
        .lines()
        .find_map(|line| line.strip_prefix(&prefix).map(str::to_string))
}

fn parse_bool(input: &str) -> Option<bool> {
    match input.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn bool_value(value: bool) -> &'static str {
    if value {
        "1"
    } else {
        "0"
    }
}

fn trim_scope(mut scope: String) -> String {
    scope = scope.trim().to_string();
    while scope.len() > 3 && scope.ends_with('\\') {
        scope.pop();
    }
    if scope.len() == 3 && scope.as_bytes()[1] == b':' && scope.ends_with('\\') {
        scope.pop();
    }
    scope
}

fn normalize_list(values: Vec<String>) -> Vec<String> {
    let mut result = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for value in values {
        let value = value.trim().trim_end_matches('\\').to_string();
        if value.is_empty() {
            continue;
        }
        let key = normalize_path(&value);
        if seen.insert(key) {
            result.push(value);
        }
    }
    result
}

pub fn default_sidebar_tabs() -> Vec<(Category, bool)> {
    PRESET_CATEGORIES
        .iter()
        .map(|category| (*category, true))
        .collect()
}

/// Keeps each kind once, in the given order, and adds any kind missing from
/// an older file at the end, shown.
pub fn normalize_tabs(tabs: Vec<(Category, bool)>) -> Vec<(Category, bool)> {
    let mut result: Vec<(Category, bool)> = Vec::new();
    for (category, shown) in tabs {
        if PRESET_CATEGORIES.contains(&category)
            && !result.iter().any(|(known, _)| *known == category)
        {
            result.push((category, shown));
        }
    }
    for category in PRESET_CATEGORIES {
        if !result.iter().any(|(known, _)| *known == category) {
            result.push((category, true));
        }
    }
    result
}

/// `C:` with nothing after it: a whole drive written without its backslash.
fn is_bare_drive(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic()
}

/// Writes whole drives as `C:\`.
fn with_drive_backslash(roots: Vec<String>) -> Vec<String> {
    roots
        .into_iter()
        .map(|root| {
            if is_bare_drive(&root) {
                root + "\\"
            } else {
                root
            }
        })
        .collect()
}

/// A whole drive, such as `C:\` or `C:`.
#[cfg_attr(not(windows), allow(dead_code))]
fn is_drive_root(value: &str) -> bool {
    is_bare_drive(value.trim().trim_end_matches('\\'))
}

fn parse_volume_list(input: &str) -> Vec<String> {
    normalize_list(
        input
            .split(';')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

/// Parses the semicolon-separated form stored in `settings.ini`. Paths with a
/// semicolon are not supported in that file; the Settings dialog uses one path
/// per line and can represent them.
fn parse_folder_list(input: &str) -> Result<Vec<String>, String> {
    let mut folders = Vec::new();
    for value in input.split(';') {
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        if !is_absolute_path(value) && !is_bare_drive(value) {
            return Err(format!("not a full path: {value}"));
        }
        folders.push(value.to_string());
    }
    Ok(normalize_list(folders))
}

/// Parses the one-path-per-line editor in the Settings dialog.
pub fn parse_excluded_folders(input: &str) -> Result<Vec<String>, String> {
    let mut folders = Vec::new();
    for line in input.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if !is_absolute_path(line) && !is_bare_drive(line) {
            return Err(format!("Not a full path: {line}"));
        }
        folders.push(line.to_string());
    }
    Ok(normalize_list(folders))
}

fn parse_optional_size(input: &str) -> Option<Option<u64>> {
    if input.trim().is_empty() {
        return Some(None);
    }
    feather_core::query::parse_size_value(input).map(Some)
}

fn parse_window(input: &str) -> Option<WindowPlacement> {
    let parts: Vec<&str> = input.split(',').map(str::trim).collect();
    if parts.len() != 5 {
        return None;
    }
    Some(WindowPlacement {
        x: parts[0].parse().ok()?,
        y: parts[1].parse().ok()?,
        width: parts[2].parse().ok()?,
        height: parts[3].parse().ok()?,
        maximized: parse_bool(parts[4])?,
    })
}

fn sort_mode_name(mode: SortMode) -> &'static str {
    match mode {
        SortMode::Ordinal => "ordinal",
        SortMode::Natural => "natural",
    }
}

fn parse_sort_mode(value: &str) -> Option<SortMode> {
    match value.trim().to_ascii_lowercase().as_str() {
        "ordinal" => Some(SortMode::Ordinal),
        "natural" => Some(SortMode::Natural),
        _ => None,
    }
}

fn dark_mode_name(mode: DarkMode) -> &'static str {
    match mode {
        DarkMode::Light => "light",
        DarkMode::FollowSystem => "follow",
        DarkMode::Full => "full",
    }
}

fn parse_dark_mode(value: &str) -> Option<DarkMode> {
    match value.trim().to_ascii_lowercase().as_str() {
        "light" => Some(DarkMode::Light),
        "follow" | "followsystem" | "follow_system" | "system" => Some(DarkMode::FollowSystem),
        "full" | "dark" => Some(DarkMode::Full),
        _ => None,
    }
}

fn network_access_name(mode: NetworkAccess) -> &'static str {
    match mode {
        NetworkAccess::None => "none",
        NetworkAccess::ManualUpdate => "manual",
        NetworkAccess::Automatic => "auto",
    }
}

fn parse_network_access(value: &str) -> Option<NetworkAccess> {
    match value.trim().to_ascii_lowercase().as_str() {
        "none" => Some(NetworkAccess::None),
        "manual" | "manualupdate" | "manual_update" => Some(NetworkAccess::ManualUpdate),
        "auto" | "automatic" => Some(NetworkAccess::Automatic),
        _ => None,
    }
}

fn kind_name(kind: KindFilter) -> &'static str {
    match kind {
        KindFilter::Any => "any",
        KindFilter::Files => "files",
        KindFilter::Folders => "folders",
    }
}

fn parse_kind(value: &str) -> Option<KindFilter> {
    match value.trim().to_ascii_lowercase().as_str() {
        "any" | "all" | "everything" => Some(KindFilter::Any),
        "file" | "files" => Some(KindFilter::Files),
        "folder" | "folders" => Some(KindFilter::Folders),
        _ => None,
    }
}

#[cfg(windows)]
mod dialog {
    use super::*;
    use crate::text::{self, human_size};
    use crate::ui::form::{
        option, Action, ButtonStyle, Choice, DriveCard, Form, FormDialog, FormWindow, Id, Look,
    };
    use crate::ui::glyphs::PageIcon;
    use crate::ui::hotkey;
    use crate::ui::indexer;
    use crate::ui::theme::{self, Palette};
    use std::any::Any;
    use windows_sys::Win32::Foundation::HWND;
    const THEME: Id = "theme";
    const BACKDROP: Id = "backdrop";
    const ACCENT: Id = "accent";
    const RESULTS: Id = "results";
    const REINDEX: Id = "reindex";
    const AUTO_UPDATE: Id = "auto_update";
    const SEARCH_DELAY: Id = "search_delay";
    const SHOW_ALL: Id = "show_all";
    const OPEN_MODE: Id = "open_mode";
    const DRIVES: Id = "drives";
    const INDEX_ROOTS: Id = "index_roots";
    const EXCLUDED: Id = "excluded";
    const SHOW_HIDDEN: Id = "show_hidden";
    const SHOW_SYSTEM: Id = "show_system";
    const REMEMBER_FILTERS: Id = "remember_filters";
    const REMEMBER_SCOPE: Id = "remember_scope";
    const REMEMBER_WINDOW: Id = "remember_window";
    const ALWAYS_ON_TOP: Id = "always_on_top";
    const SHELL_MENU: Id = "shell_menu";
    const HOTKEY: Id = "hotkey";
    const CLOSE_TO_TRAY: Id = "close_to_tray";
    const START_AT_LOGIN: Id = "start_at_login";
    const START_MENU: Id = "start_menu_shortcut";
    const SHARE_HTTP: Id = "share_http";
    const HTTP_PORT: Id = "http_port";
    const SHARE_FTP: Id = "share_ftp";
    const FTP_PORT: Id = "ftp_port";
    const FTP_PLAIN: Id = "ftp_plain";
    const SHARE_ACCESS: Id = "share_access";
    const SHARE_TUNNEL: Id = "share_tunnel";
    const TUNNEL_TOKEN: Id = "tunnel_token";
    const SHARE_USER: Id = "share_user";
    const SHARE_PASSWORD: Id = "share_password";
    const SHARE_FOLDERS: Id = "share_folders";
    const SHARE_STATUS: Id = "share_status";
    const SHARE_E2E: Id = "share_e2e";
    const TUNNEL_SETUP: Id = "tunnel_setup";
    const TUNNEL_DOMAIN: Id = "tunnel_domain";
    const FIREWALL: Id = "firewall";
    const SHARE_QR: Id = "share_qr";
    const TUNNEL_SECTION: Id = "tunnel_section";
    const PORTS_SECTION: Id = "ports_section";
    const DESKTOP: Id = "desktop_shortcut";
    const SIDEBAR_TABS: Id = "sidebar_tabs";
    const SIZE_FORMAT: Id = "size_format";
    const CUSTOM_THEME: Id = "custom_theme";
    const THEME_BACKGROUND: Id = "theme_background";
    const THEME_PANEL: Id = "theme_panel";
    const THEME_TEXT: Id = "theme_text";
    const THEME_TEXT2: Id = "theme_text2";
    const THEME_ACCENT: Id = "theme_accent";
    const THEME_COPY: Id = "theme_copy";
    const PICTURE: Id = "picture";
    const PICTURE_CLEAR: Id = "picture_clear";
    const PICTURE_FIT: Id = "picture_fit";
    const PICTURE_STRENGTH: Id = "picture_strength";
    const SAVE: Id = "save";
    const RESTORE: Id = "restore";
    const CANCEL: Id = "cancel";

    /// The pages of the Settings window, in order.
    pub const PAGE_APPEARANCE: usize = 0;
    /// Appearance's sub-page for the custom theme.
    pub const PAGE_THEME: usize = 1;
    /// Places to search, then how searching behaves.
    pub const PAGE_INDEXES: usize = 2;
    pub const PAGE_SHORTCUT: usize = 3;
    pub const PAGE_SIDEBAR: usize = 4;
    /// The web and FTP servers.
    pub const PAGE_SHARING: usize = 6;

    const THEMES: [DarkMode; 3] = [DarkMode::Light, DarkMode::Full, DarkMode::FollowSystem];
    const OPEN_MODES: [OpenMode; 2] = [OpenMode::File, OpenMode::ContainingFolder];
    const AUTO_UPDATES: [AutoUpdate; 4] = [
        AutoUpdate::Live,
        AutoUpdate::Minutes15,
        AutoUpdate::Hourly,
        AutoUpdate::Off,
    ];

    fn index_of<T: PartialEq>(values: &[T], value: &T) -> usize {
        values
            .iter()
            .position(|candidate| candidate == value)
            .unwrap_or(0)
    }

    fn pick<T: Copy>(values: &[T], index: usize) -> T {
        values[index.min(values.len() - 1)]
    }

    /// The picture row's line: the file in use, or how to add one.
    fn picture_detail(path: &str) -> String {
        if path.is_empty() {
            "No picture. Choose one to show it behind Feather.".into()
        } else {
            match Path::new(path).file_name() {
                Some(_) => "A picture is set. It shows while your own colors are on.".into(),
                None => path.to_string(),
            }
        }
    }

    /// The drives offered as cards: every fixed drive, plus any drive already
    /// being searched (a USB drive, say), as `C:\`.
    pub(super) fn drive_choices(roots: &[String]) -> Vec<String> {
        let mut drives = indexer::fixed_drives();
        for root in roots.iter().filter(|root| is_drive_root(root)) {
            let drive = format!(
                "{}\\",
                root.trim().trim_end_matches('\\').to_ascii_uppercase()
            );
            if !drives
                .iter()
                .any(|known| known.eq_ignore_ascii_case(&drive))
            {
                drives.push(drive);
            }
        }
        drives.sort();
        drives
    }

    /// Splits searched places into drive check states and other folders.
    pub(super) fn split_roots(roots: &[String], drives: &[String]) -> (Vec<bool>, Vec<String>) {
        let same = |a: &str, b: &str| {
            a.trim_end_matches('\\')
                .eq_ignore_ascii_case(b.trim_end_matches('\\'))
        };
        let checked = drives
            .iter()
            .map(|drive| roots.iter().any(|root| same(root, drive)))
            .collect();
        let others = roots
            .iter()
            .filter(|root| !is_drive_root(root))
            .cloned()
            .collect();
        (checked, others)
    }

    /// A drive card: Explorer's name for it and how full it is.
    fn drive_card(root: &str) -> DriveCard {
        let (space, used) = match indexer::drive_space(root) {
            Some((free, total)) if total > 0 => (
                format!("{} free of {}", human_size(free), human_size(total)),
                1.0 - free as f32 / total as f32,
            ),
            _ => ("Not available".to_string(), 0.0),
        };
        DriveCard {
            root: root.to_string(),
            label: indexer::drive_label(root),
            space,
            used,
            on: false,
        }
    }

    /// Writes every control from a settings value. Used when the window opens
    /// and for Restore defaults.
    fn fill(form: &mut Form, settings: &Settings, drives: &[String]) {
        form.set_selected(THEME, index_of(&THEMES, &settings.dark_mode));
        form.set_selected(BACKDROP, index_of(&Material::ALL, &settings.material));
        form.set_selected(ACCENT, index_of(&Accent::ALL, &settings.accent));
        form.set_selected(
            SIZE_FORMAT,
            index_of(SizeFormat::ALL, &settings.size_format),
        );
        form.set_on(CUSTOM_THEME, settings.custom_theme);
        let colors = settings.theme_colors;
        form.set_color(THEME_BACKGROUND, colors.background);
        form.set_color(THEME_PANEL, colors.panel);
        form.set_color(THEME_TEXT, colors.text);
        form.set_color(THEME_TEXT2, colors.text2);
        form.set_color(THEME_ACCENT, colors.accent);
        form.set_detail(PICTURE, &picture_detail(&settings.background_image));
        form.set_selected(
            PICTURE_FIT,
            index_of(BackgroundFit::ALL, &settings.background_fit),
        );
        form.set_value(PICTURE_STRENGTH, settings.background_strength);
        form.set_selected(SHARE_HTTP, index_of(HttpMode::ALL, &settings.http_mode));
        form.set_selected(SHARE_FTP, index_of(FtpMode::ALL, &settings.ftp_mode));
        form.set_selected(
            SHARE_ACCESS,
            index_of(ShareAccess::ALL, &settings.share_access),
        );
        form.set_selected(SHARE_TUNNEL, index_of(Tunnel::ALL, &settings.tunnel));
        form.set_text(HTTP_PORT, &settings.http_port.to_string());
        form.set_text(FTP_PORT, &settings.ftp_port.to_string());
        form.set_on(FTP_PLAIN, settings.ftp_plain);
        form.set_text(SHARE_USER, &settings.share_user);
        // The password and token show again, decrypted for this account.
        form.set_text(
            SHARE_PASSWORD,
            &crate::share::secret::open(&settings.share_password_saved),
        );
        form.set_text(
            TUNNEL_TOKEN,
            &crate::share::secret::open(&settings.tunnel_token),
        );
        form.set_on(SHARE_E2E, settings.share_e2e);
        form.set_text(TUNNEL_DOMAIN, &settings.tunnel_domain);
        form.set_paths(SHARE_FOLDERS, settings.share_folders.clone());
        form.set_value(RESULTS, settings.results_percent);
        form.set_number(SEARCH_DELAY, settings.search_delay_ms);
        form.set_on(SHOW_ALL, settings.show_all_when_empty);
        form.set_selected(OPEN_MODE, index_of(&OPEN_MODES, &settings.open_mode));
        form.set_selected(AUTO_UPDATE, index_of(&AUTO_UPDATES, &settings.auto_update));
        let (checked, others) = split_roots(&settings.index_roots, drives);
        form.set_checked(DRIVES, &checked);
        form.set_paths(INDEX_ROOTS, others);
        form.set_paths(EXCLUDED, settings.excluded_folders.clone());
        form.set_on(SHOW_HIDDEN, settings.show_hidden);
        form.set_on(SHOW_SYSTEM, settings.show_system);
        form.set_on(REMEMBER_FILTERS, settings.remember_filters);
        form.set_on(REMEMBER_SCOPE, settings.remember_scope);
        form.set_on(REMEMBER_WINDOW, settings.remember_window);
        form.set_on(ALWAYS_ON_TOP, settings.always_on_top);
        form.set_on(SHELL_MENU, settings.use_shell_context_menu);
        let shortcut = if settings.hotkey.eq_ignore_ascii_case(hotkey::OFF) {
            ""
        } else {
            settings.hotkey.as_str()
        };
        form.set_text(HOTKEY, shortcut);
        form.set_on(CLOSE_TO_TRAY, settings.close_to_tray);
        form.set_on(START_AT_LOGIN, settings.start_at_login);
        form.set_on(START_MENU, settings.start_menu_shortcut);
        form.set_on(DESKTOP, settings.desktop_shortcut);
        let shown: Vec<bool> = settings
            .sidebar_tabs
            .iter()
            .map(|(_, shown)| *shown)
            .collect();
        form.set_checked(SIDEBAR_TABS, &shown);
    }

    fn build(initial: &Settings, drives: &[String], items: u64) -> Form {
        let mut form = Form::new();

        form.page("Appearance", PageIcon::Appearance);
        form.heading("Colors");
        form.segments(
            THEME,
            "Theme",
            None,
            vec![option("Light"), option("Dark"), option("Match Windows")],
            0,
        );
        form.segments(
            BACKDROP,
            "Background",
            Some("Mica and Acrylic let your desktop show through, blurred."),
            vec![option("Mica"), option("Acrylic"), option("Solid")],
            0,
        );
        let dark = theme::resolve_dark(initial.dark_mode);
        form.segments(
            ACCENT,
            "Accent color",
            None,
            Accent::ALL
                .iter()
                .map(|accent| Choice {
                    label: accent.label().to_string(),
                    swatch: Some(theme::accent_color(*accent, dark)),
                })
                .collect(),
            0,
        );

        form.heading("Sizes");
        form.segments(
            SIZE_FORMAT,
            "Show file sizes in",
            Some("Auto picks KB, MB, or GB for each file."),
            vec![option("Auto"), option("KB"), option("MB"), option("GB")],
            0,
        );

        form.subpage("Custom theme", PageIcon::Theme);
        form.heading("Your colors");
        form.switch(
            CUSTOM_THEME,
            "Use my own theme",
            Some("Replaces the Light and Dark colors with the ones below."),
            false,
        );
        let colors = initial.theme_colors;
        form.color(THEME_BACKGROUND, "Background", None, colors.background);
        form.color(
            THEME_PANEL,
            "Panels",
            Some("The results list and other cards."),
            colors.panel,
        );
        form.color(THEME_TEXT, "Text", None, colors.text);
        form.color(
            THEME_TEXT2,
            "Secondary text",
            Some("Folders, dates, and hints."),
            colors.text2,
        );
        form.color(
            THEME_ACCENT,
            "Accent",
            Some("Buttons, the selection, and highlights."),
            colors.accent,
        );
        form.action(
            THEME_COPY,
            "Start from the built-in theme",
            Some("Copies the Light or Dark colors and accent chosen on Appearance."),
            "Copy colors",
        );
        form.heading("Background picture");
        form.action(PICTURE, "Picture", None, "Choose...");
        form.action(PICTURE_CLEAR, "Remove the picture", None, "Remove");
        form.segments(
            PICTURE_FIT,
            "Fit",
            None,
            vec![
                option("Fill"),
                option("Fit"),
                option("Stretch"),
                option("Center"),
                option("Tile"),
            ],
            0,
        );
        form.slider(
            PICTURE_STRENGTH,
            "Strength",
            Some("How strongly the picture shows behind the window."),
            initial.background_strength,
            Box::new(|percent| format!("{percent}%")),
        );

        form.page("Indexes", PageIcon::Places);
        form.heading("Drives");
        form.drives(
            DRIVES,
            "",
            Some(
                "Click a drive to search everything on it. The round arrow reads one drive again.",
            ),
            drives.iter().map(|drive| drive_card(drive)).collect(),
        );
        form.heading("Folders");
        form.paths(
            INDEX_ROOTS,
            "More folders to search",
            Some("A network folder, or a folder on a drive you are not searching."),
            Vec::new(),
        );
        form.refreshable(INDEX_ROOTS);
        form.paths(
            EXCLUDED,
            "Folders to leave out",
            Some("Nothing inside these folders shows up in results."),
            Vec::new(),
        );

        form.heading("Results");
        form.slider(
        RESULTS,
        "Results to show",
        Some("How much of everything Feather knows one search may list. 100% shows every result."),
        initial.results_percent,
        Box::new(move |percent| results_caption(percent, items)),
    );
        form.number(
            SEARCH_DELAY,
            "Search delay in milliseconds",
            Some("How long to wait after you type. 0 searches on every keystroke."),
            "25",
        );
        form.switch(
            SHOW_ALL,
            "Show everything when the search box is empty",
            None,
            false,
        );
        form.segments(
            OPEN_MODE,
            "Double-click opens",
            None,
            vec![option("The item"), option("Its folder")],
            0,
        );
        form.heading("Index");
        form.segments(
            AUTO_UPDATE,
            "Update the index",
            Some("How soon new, renamed, and deleted files show up in results."),
            vec![
                option("Right away"),
                option("Every 15 min"),
                option("Every hour"),
                option("Never"),
            ],
            0,
        );
        form.action(
            REINDEX,
            "Rebuild the index",
            Some("Reads every place again. Use it if results look wrong."),
            "Reindex now",
        );
        form.heading("What to show");
        form.switch(
            SHOW_HIDDEN,
            "Hidden items",
            Some("Files and folders marked hidden, like AppData."),
            false,
        );
        form.switch(
            SHOW_SYSTEM,
            "System items",
            Some("Files Windows marks as part of the system."),
            false,
        );

        form.page("Shortcut and tray", PageIcon::Shortcut);
        form.heading("Shortcut");
        form.shortcut(
        HOTKEY,
        "Shortcut to open Feather",
        Some("Works from any app. Click the box and press any keys together, even both Shifts. Backspace turns it off."),
        "",
        "None",
    );
        form.heading("Running in the background");
        form.switch(
            CLOSE_TO_TRAY,
            "Keep Feather running in the tray when closed",
            Some("Closing the window hides it next to the clock, so the shortcut keeps working."),
            false,
        );
        form.switch(
            START_AT_LOGIN,
            "Start Feather when you sign in",
            Some("It starts in the tray, ready for the shortcut."),
            false,
        );
        form.heading("Opening Feather");
        form.switch(
            START_MENU,
            "Show Feather in the Start menu",
            Some("So you can find it by typing Feather after pressing the Windows key."),
            false,
        );
        form.switch(DESKTOP, "Put a shortcut on the desktop", None, false);

        form.page("Sidebar", PageIcon::Sidebar);
        form.heading("Tabs");
        form.order(
            SIDEBAR_TABS,
            "Drag to change the order. Uncheck a tab to hide it.",
            initial
                .sidebar_tabs
                .iter()
                .map(|(category, shown)| (category.label().to_string(), *category, *shown))
                .collect(),
        );

        form.page("Window", PageIcon::Window);
        form.heading("When Feather opens");
        form.switch(
            REMEMBER_FILTERS,
            "Remember filters",
            Some("Start with the filters you used last time."),
            false,
        );
        form.switch(
            REMEMBER_SCOPE,
            "Remember the folder you searched in",
            None,
            false,
        );
        form.switch(
            REMEMBER_WINDOW,
            "Remember window size and position",
            None,
            false,
        );
        form.heading("Behavior");
        form.switch(
            ALWAYS_ON_TOP,
            "Keep Feather above other windows",
            None,
            false,
        );
        form.switch(
            SHELL_MENU,
            "Use the full Windows context menu",
            Some("Right-clicking a result shows commands from other apps too."),
            false,
        );

        form.page("Sharing", PageIcon::Sharing);
        // What sharing is doing comes first; the settings follow. Rows carry
        // labels only, and their help shows on hover.
        form.action(SHARE_STATUS, "Status", None, "Copy address");
        form.qr(SHARE_QR, "Open on your phone", "Show QR code", "");
        form.tip(
            SHARE_QR,
            "Scan it with the phone's camera. The phone warns about Feather's own \
             certificate the first time: choose Advanced, then continue.",
        );

        form.heading("Who can connect");
        form.segments(
            SHARE_ACCESS,
            "Reachable from",
            None,
            vec![
                option("This PC"),
                option("Local network"),
                option("Internet"),
            ],
            0,
        );
        form.tip(
            SHARE_ACCESS,
            "Local network means devices on your Wi-Fi or home network. Internet also \
             lets anyone with the address in, through the tunnel below.",
        );
        form.action(FIREWALL, "Let other devices in", None, "Allow in firewall");
        form.tip(
            FIREWALL,
            "If a phone on the same Wi-Fi can't connect, Windows Firewall is blocking \
             Feather. This allows it; Windows asks for permission.",
        );
        form.expander(TUNNEL_SECTION, "Internet tunnel", 4);
        form.segments(
            SHARE_TUNNEL,
            "Through",
            None,
            vec![option("Cloudflare"), option("ngrok"), option("Direct")],
            0,
        );
        form.tip(
            SHARE_TUNNEL,
            "Cloudflare and ngrok give you a public https address without touching your \
             router. Direct needs a port forwarded on your router.",
        );
        form.action(TUNNEL_SETUP, "Set up and start", None, "Set up tunnel");
        form.tip(
            TUNNEL_SETUP,
            "Installs what the tunnel needs, turns on internet access, and starts sharing.",
        );
        form.field(
            TUNNEL_DOMAIN,
            "Stable address",
            None,
            "",
            "name.ngrok-free.app",
            false,
        );
        form.only_when(TUNNEL_DOMAIN, SHARE_TUNNEL, 1);
        form.tip(
            TUNNEL_DOMAIN,
            "One address that never changes, free with an ngrok account. Copy your \
             domain from dashboard.ngrok.com/domains.",
        );
        form.field(TUNNEL_TOKEN, "Token", None, "", "Optional", false);
        form.secret(TUNNEL_TOKEN);
        form.tip(
            TUNNEL_TOKEN,
            "Your ngrok authtoken, or the token of a tunnel on your own Cloudflare \
             domain (a fixed Cloudflare address). Kept encrypted.",
        );

        form.heading("Sign in");
        form.field(SHARE_USER, "User name", None, "", "feather", false);
        form.field(
            SHARE_PASSWORD,
            "Password",
            None,
            "",
            if initial.share_password.is_empty() || !initial.share_password_saved.is_empty() {
                "No password"
            } else {
                "Set earlier"
            },
            false,
        );
        form.secret(SHARE_PASSWORD);
        form.tip(
            SHARE_PASSWORD,
            "Optional. Without one, anyone who can reach Feather gets in.",
        );
        form.switch(SHARE_E2E, "Encrypt requests end to end", None, false);
        form.tip(
            SHARE_E2E,
            "The page and Feather encrypt every request and reply themselves, so not even \
             Cloudflare or ngrok can read names or files, and the password is never sent.",
        );

        form.heading("What to share");
        form.paths(SHARE_FOLDERS, "Folders", None, Vec::new());
        form.tip(
            SHARE_FOLDERS,
            "Only these folders and what's inside them. Folders left out of the index \
             stay hidden here too.",
        );

        form.heading("What people can do");
        form.segments(
            SHARE_HTTP,
            "In a browser",
            None,
            vec![
                option("Nothing"),
                option("Download"),
                option("Search"),
                option("Everything"),
            ],
            0,
        );
        form.tip(
            SHARE_HTTP,
            "Download opens and saves files. Search adds Feather's search. Everything also \
             lets them upload, rename, and delete; deleted items go to the Recycle Bin.",
        );
        form.segments(
            SHARE_FTP,
            "In an FTP app",
            None,
            vec![option("Nothing"), option("Download"), option("Everything")],
            0,
        );
        form.tip(
            SHARE_FTP,
            "For FileZilla, WinSCP, and the like. Uses FTP over TLS (FTPS).",
        );
        form.expander(PORTS_SECTION, "Ports and older FTP apps", 3);
        form.field(HTTP_PORT, "Web page port", None, "", "8443", false);
        form.field(FTP_PORT, "FTP port", None, "", "2121", false);
        form.switch(FTP_PLAIN, "Allow unencrypted FTP", None, false);
        form.tip(
            FTP_PLAIN,
            "Only for old apps without TLS. Passwords and files then cross the network \
             readable.",
        );

        form.button(RESTORE, "Restore defaults", ButtonStyle::Quiet);
        form.button(CANCEL, "Cancel", ButtonStyle::Secondary);
        form.button(SAVE, "Save", ButtonStyle::Primary);
        fill(&mut form, initial, drives);
        form.settle();
        form
    }

    /// Called with the settings as they stand while the person changes them.
    pub type Preview = Box<dyn FnMut(&Settings)>;

    /// What the open Settings window can do to the main window.
    pub type Reindex = Box<dyn FnMut(Option<&str>)>;

    /// Starts sharing with these settings now.
    pub type ApplySharing = Box<dyn FnMut(&Settings)>;
    /// Sharing's status lines, and the address to copy.
    pub type SharingStatus = Box<dyn FnMut() -> (Vec<String>, Option<String>)>;

    #[derive(Default)]
    pub struct Live {
        /// Sees every change as it happens.
        pub preview: Option<Preview>,
        /// Rebuilds the index now: all of it, or just the place named.
        pub reindex: Option<Reindex>,
        /// How many items the index holds, for the results slider.
        pub items: u64,
        /// What sharing is doing now, a line each.
        pub sharing: Vec<String>,
        /// The address to copy, when sharing runs.
        pub address: Option<String>,
        /// Starts sharing with these settings right away (and saves them).
        pub apply_sharing: Option<ApplySharing>,
        /// Sharing's status lines now, and the address to copy.
        pub sharing_status: Option<SharingStatus>,
    }

    struct SettingsDialog {
        initial: Settings,
        drives: Vec<String>,
        result: Option<Settings>,
        preview: Option<Preview>,
        reindex: Option<Reindex>,
        /// The background picture chosen so far.
        picture: String,
        /// The sharing address the Copy button copies.
        address: Option<String>,
        apply_sharing: Option<ApplySharing>,
        sharing_status: Option<SharingStatus>,
        /// The status lines last shown.
        shown_status: Vec<String>,
    }

    impl SettingsDialog {
        /// Does everything for sharing on the internet: picks Internet, turns
        /// the web page on, and starts sharing now, which fetches the tunnel
        /// app when it's missing. The address shows under Status by itself.
        fn set_up_tunnel(&mut self, form: &mut Form) -> String {
            let kind = pick(Tunnel::ALL, form.selected(SHARE_TUNNEL));
            if kind == Tunnel::Ngrok && form.text(TUNNEL_TOKEN).trim().is_empty() {
                // ngrok only works with an account; its token is on this page.
                let _ = std::thread::Builder::new()
                    .name("feather-tunnel-setup".into())
                    .spawn(|| crate::share::tunnel::install(Tunnel::Ngrok));
                let _ = crate::ui::context_menu::open_url(crate::share::tunnel::NGROK_TOKEN_PAGE);
                return "ngrok needs a free account. Sign in on the page that opened, paste \
                        your authtoken into Tunnel token, and press Set up tunnel again."
                    .into();
            }
            form.set_selected(
                SHARE_ACCESS,
                index_of(ShareAccess::ALL, &ShareAccess::Internet),
            );
            if pick(HttpMode::ALL, form.selected(SHARE_HTTP)) == HttpMode::Off {
                form.set_selected(SHARE_HTTP, index_of(HttpMode::ALL, &HttpMode::Download));
            }
            let mut settings = self.collect(form);
            if let Err((field, error)) = self.sharing_into(form, &mut settings) {
                form.focus(field);
                return error;
            }
            let Some(apply) = &mut self.apply_sharing else {
                return "Sharing can't start from here right now. Save instead.".into();
            };
            apply(&settings);
            // What was applied is now what Save compares against.
            self.initial.share_password = settings.share_password.clone();
            self.initial.share_password_saved = settings.share_password_saved.clone();
            self.initial.tunnel_token = settings.tunnel_token.clone();
            match kind {
                Tunnel::Direct => format!(
                    "Sharing directly. Forward port {} on your router to this PC.",
                    settings.http_port
                ),
                _ => "Setting up. The public address shows under Status below in a few \
                      seconds (the first time, after a one-time download)."
                    .into(),
            }
        }

        /// Puts the checked Sharing page into `settings`.
        fn sharing_into(&self, form: &Form, settings: &mut Settings) -> Result<(), (Id, String)> {
            let (http_port, ftp_port, password, token) = self.sharing(form)?;
            settings.http_port = http_port;
            settings.ftp_port = ftp_port;
            if let Some((hash, sealed)) = password {
                settings.share_password = hash;
                settings.share_password_saved = sealed;
            }
            if let Some(token) = token {
                settings.tunnel_token = token;
            }
            Ok(())
        }

        /// Checks the Sharing page. Returns the ports, a new password hash,
        /// and a new sealed token, or the field to fix and why.
        #[allow(clippy::type_complexity)]
        fn sharing(
            &self,
            form: &Form,
        ) -> Result<(u16, u16, Option<(String, String)>, Option<String>), (Id, String)> {
            let port = |id: Id| {
                text::digits_only(&form.text(id))
                    .parse::<u16>()
                    .ok()
                    .filter(|port| *port > 0)
                    .ok_or((id, "Use a port number from 1 to 65535.".to_string()))
            };
            let http_port = port(HTTP_PORT)?;
            let ftp_port = port(FTP_PORT)?;
            let http = pick(HttpMode::ALL, form.selected(SHARE_HTTP));
            let ftp = pick(FtpMode::ALL, form.selected(SHARE_FTP));
            let on = http != HttpMode::Off || ftp != FtpMode::Off;
            if http != HttpMode::Off && ftp != FtpMode::Off && http_port == ftp_port {
                return Err((
                    FTP_PORT,
                    "The web page and FTP need different ports.".into(),
                ));
            }
            let user = form.text(SHARE_USER).trim().to_string();
            let has_password =
                !form.text(SHARE_PASSWORD).is_empty() || !self.initial.share_password.is_empty();
            if on && has_password && user.is_empty() {
                return Err((SHARE_USER, "Choose a user name for signing in.".into()));
            }
            if user.len() > 64
                || user
                    .chars()
                    .any(|c| c.is_whitespace() || c.is_control() || c == ':')
            {
                return Err((
                    SHARE_USER,
                    "Use a user name without spaces or colons.".into(),
                ));
            }
            use crate::share::{auth, secret};
            let typed = form.text(SHARE_PASSWORD);
            let saved = secret::open(&self.initial.share_password_saved);
            let unchanged = !typed.is_empty() && typed == saved;
            // A new password, or one that predates the pepper, is hashed
            // again. An empty box means no password, except for a hash from
            // before passwords were remembered: its password isn't known, so
            // it stays until a new one is typed.
            let legacy = saved.is_empty() && !self.initial.share_password.is_empty();
            let password = if typed.is_empty() {
                if form.is_on(SHARE_E2E) && on {
                    return Err((
                        SHARE_PASSWORD,
                        if legacy {
                            "Type the password again; end-to-end encryption needs it."
                        } else {
                            "End-to-end encryption needs a password."
                        }
                        .into(),
                    ));
                }
                if legacy {
                    None
                } else {
                    Some((String::new(), String::new()))
                }
            } else {
                if unchanged && self.initial.share_password.starts_with("feather2$") {
                    None
                } else {
                    let hash =
                        auth::hash_password(&typed).map_err(|error| (SHARE_PASSWORD, error))?;
                    let sealed = secret::seal(&typed).map_err(|error| (SHARE_PASSWORD, error))?;
                    Some((hash, sealed))
                }
            };
            if on && form.path_items(SHARE_FOLDERS).is_empty() {
                return Err((SHARE_FOLDERS, "Add at least one folder to share.".into()));
            }
            let typed_token = form.text(TUNNEL_TOKEN).trim().to_string();
            let token = if typed_token == secret::open(&self.initial.tunnel_token) {
                None
            } else {
                Some(secret::seal(&typed_token).map_err(|error| (TUNNEL_TOKEN, error))?)
            };
            Ok((http_port, ftp_port, password, token))
        }

        /// Checked drives followed by the other folders.
        fn roots(&self, form: &Form) -> Vec<String> {
            let mut roots: Vec<String> = self
                .drives
                .iter()
                .zip(form.checked(DRIVES))
                .filter(|(_, on)| *on)
                .map(|(drive, _)| drive.clone())
                .collect();
            roots.extend(form.path_items(INDEX_ROOTS));
            roots
        }

        /// Everything the form says, except the parts that need checking first;
        /// those are filled in by `save` (or kept from `initial` for previews).
        fn collect(&self, form: &Form) -> Settings {
            let mut settings = self.initial.clone();
            settings.dark_mode = pick(&THEMES, form.selected(THEME));
            settings.material = pick(&Material::ALL, form.selected(BACKDROP));
            settings.accent = pick(&Accent::ALL, form.selected(ACCENT));
            settings.results_percent = form.value(RESULTS);
            if let Ok(value) = parse_delay(&text::digits_only(&form.text(SEARCH_DELAY))) {
                settings.search_delay_ms = value;
            }
            settings.show_all_when_empty = form.is_on(SHOW_ALL);
            settings.open_mode = pick(&OPEN_MODES, form.selected(OPEN_MODE));
            settings.auto_update = pick(&AUTO_UPDATES, form.selected(AUTO_UPDATE));
            if let Ok(folders) = parse_excluded_folders(&form.path_items(EXCLUDED).join("\n")) {
                settings.excluded_folders = folders;
            }
            settings.show_hidden = form.is_on(SHOW_HIDDEN);
            settings.show_system = form.is_on(SHOW_SYSTEM);
            settings.remember_filters = form.is_on(REMEMBER_FILTERS);
            settings.remember_scope = form.is_on(REMEMBER_SCOPE);
            settings.remember_window = form.is_on(REMEMBER_WINDOW);
            settings.always_on_top = form.is_on(ALWAYS_ON_TOP);
            settings.use_shell_context_menu = form.is_on(SHELL_MENU);
            settings.close_to_tray = form.is_on(CLOSE_TO_TRAY);
            settings.start_at_login = form.is_on(START_AT_LOGIN);
            settings.start_menu_shortcut = form.is_on(START_MENU);
            settings.desktop_shortcut = form.is_on(DESKTOP);
            settings.sidebar_tabs = normalize_tabs(form.order_items(SIDEBAR_TABS));
            settings.size_format = pick(SizeFormat::ALL, form.selected(SIZE_FORMAT));
            settings.custom_theme = form.is_on(CUSTOM_THEME);
            settings.theme_colors = ThemeColors {
                background: form.color_value(THEME_BACKGROUND),
                panel: form.color_value(THEME_PANEL),
                text: form.color_value(THEME_TEXT),
                text2: form.color_value(THEME_TEXT2),
                accent: form.color_value(THEME_ACCENT),
            };
            settings.background_image = self.picture.clone();
            settings.background_fit = pick(BackgroundFit::ALL, form.selected(PICTURE_FIT));
            settings.background_strength = form.value(PICTURE_STRENGTH);
            settings.http_mode = pick(HttpMode::ALL, form.selected(SHARE_HTTP));
            settings.ftp_mode = pick(FtpMode::ALL, form.selected(SHARE_FTP));
            settings.share_access = pick(ShareAccess::ALL, form.selected(SHARE_ACCESS));
            settings.tunnel = pick(Tunnel::ALL, form.selected(SHARE_TUNNEL));
            settings.ftp_plain = form.is_on(FTP_PLAIN);
            settings.share_user = form.text(SHARE_USER).trim().to_string();
            settings.share_folders = form.path_items(SHARE_FOLDERS);
            settings.share_strong_password = false;
            settings.share_e2e = form.is_on(SHARE_E2E);
            settings.tunnel_domain = form.text(TUNNEL_DOMAIN).trim().to_string();
            settings
        }

        fn save(&mut self, form: &mut Form) -> Action {
            let fail = |form: &mut Form, field: Id, page: usize, error: String| {
                form.set_page(page);
                form.set_error(Some(error));
                form.focus(field);
                Action::Stay
            };
            let search_delay = match parse_delay(&text::digits_only(&form.text(SEARCH_DELAY))) {
                Ok(value) => value,
                Err(error) => return fail(form, SEARCH_DELAY, PAGE_INDEXES, error),
            };
            let excluded_folders =
                match parse_excluded_folders(&form.path_items(EXCLUDED).join("\n")) {
                    Ok(folders) => folders,
                    Err(error) => {
                        form.set_page(PAGE_INDEXES);
                        form.set_error(Some(error));
                        return Action::Stay;
                    }
                };
            let hotkey = match hotkey::normalize(&form.text(HOTKEY)) {
                Ok(text) => text,
                Err(error) => return fail(form, HOTKEY, PAGE_SHORTCUT, error),
            };
            let index_roots = match parse_excluded_folders(&self.roots(form).join("\n")) {
                Ok(roots) => roots,
                Err(error) => {
                    form.set_page(PAGE_INDEXES);
                    form.set_error(Some(error));
                    return Action::Stay;
                }
            };

            let mut settings = self.collect(form);
            if let Err((field, error)) = self.sharing_into(form, &mut settings) {
                return fail(form, field, PAGE_SHARING, error);
            }
            settings.search_delay_ms = search_delay;
            settings.index_roots = index_roots;
            // Every indexed place is searched; the folder list is the control now.
            settings.excluded_volumes.clear();
            settings.excluded_folders = excluded_folders;
            settings.hotkey = hotkey;
            settings.normalize();
            form.set_error(None);
            self.result = Some(settings);
            Action::Accept
        }
    }

    impl FormDialog for SettingsDialog {
        fn on_button(&mut self, form: &mut Form, id: Id) -> Action {
            match id {
                SAVE => self.save(form),
                RESTORE => {
                    // Keep what is indexed; restoring defaults should not throw
                    // away the index.
                    let defaults = Settings {
                        index_roots: self.roots(form),
                        // The list keeps its order; every tab is shown.
                        sidebar_tabs: form
                            .order_items(SIDEBAR_TABS)
                            .into_iter()
                            .map(|(category, _)| (category, true))
                            .collect(),
                        start_at_login: false,
                        // Shortcuts stay as they are.
                        start_menu_shortcut: form.is_on(START_MENU),
                        desktop_shortcut: form.is_on(DESKTOP),
                        // The sharing password is kept; sharing turns off.
                        share_password: self.initial.share_password.clone(),
                        ..Settings::default()
                    };
                    self.picture.clear();
                    fill(form, &defaults, &self.drives);
                    form.set_error(None);
                    self.on_change(form, RESTORE);
                    Action::Stay
                }
                _ => Action::Cancel,
            }
        }

        fn on_action(&mut self, form: &mut Form, id: Id) {
            match id {
                THEME_COPY => {
                    let dark = theme::resolve_dark(pick(&THEMES, form.selected(THEME)));
                    let accent = pick(&Accent::ALL, form.selected(ACCENT));
                    let palette = Palette::new(dark, accent, Material::Solid);
                    form.set_color(THEME_BACKGROUND, palette.base.to_rgb());
                    form.set_color(THEME_PANEL, palette.card.to_rgb());
                    form.set_color(THEME_TEXT, palette.text.to_rgb());
                    form.set_color(THEME_TEXT2, palette.text2.to_rgb());
                    form.set_color(THEME_ACCENT, palette.accent.to_rgb());
                    form.set_on(CUSTOM_THEME, true);
                    self.on_change(form, id);
                    return;
                }
                PICTURE => {
                    match crate::ui::picker::pick_image(std::ptr::null_mut()) {
                        Ok(Some(path)) => match crate::ui::wallpaper::import(&path) {
                            Ok(copy) => {
                                self.picture = copy;
                                form.set_detail(PICTURE, &picture_detail(&self.picture));
                                form.set_on(CUSTOM_THEME, true);
                            }
                            Err(error) => form.set_detail(PICTURE, &error),
                        },
                        Ok(None) => {}
                        Err(error) => form.set_detail(PICTURE, &error),
                    }
                    self.on_change(form, id);
                    return;
                }
                FIREWALL => {
                    let note = match crate::share::firewall::allow() {
                        Ok(()) => "Done. Phones and computers on your network can connect now."
                            .to_string(),
                        Err(error) => error,
                    };
                    form.set_detail(FIREWALL, &note);
                    return;
                }
                TUNNEL_SETUP => {
                    let note = self.set_up_tunnel(form);
                    form.set_detail(TUNNEL_SETUP, &note);
                    return;
                }
                SHARE_STATUS => {
                    match &self.address {
                        Some(address) => {
                            let copied =
                                crate::ui::context_menu::copy_text(std::ptr::null_mut(), address);
                            let note = match copied {
                                Ok(()) => format!("Copied {address}"),
                                Err(error) => format!("Couldn't copy: {error}"),
                            };
                            form.set_detail(SHARE_STATUS, &note);
                        }
                        None => form.set_detail(
                            SHARE_STATUS,
                            "There's no address yet. Save with sharing on first.",
                        ),
                    }
                    return;
                }
                PICTURE_CLEAR => {
                    self.picture.clear();
                    form.set_detail(PICTURE, &picture_detail(""));
                    self.on_change(form, id);
                    return;
                }
                _ => {}
            }
            if id != REINDEX {
                return;
            }
            match &mut self.reindex {
                Some(reindex) => {
                    reindex(None);
                    form.set_detail(
                        REINDEX,
                        "Rebuilding now. Searches keep working while it runs.",
                    );
                }
                None => form.set_detail(REINDEX, "The index can't be rebuilt right now."),
            }
        }

        /// The round arrow on a drive or folder: read just that place again.
        fn on_item_action(&mut self, form: &mut Form, id: Id, item: usize) {
            let place = match id {
                DRIVES => self.drives.get(item).cloned(),
                INDEX_ROOTS => form.path_items(INDEX_ROOTS).get(item).cloned(),
                _ => None,
            };
            let Some(place) = place else {
                return;
            };
            let key = |path: &str| path.trim().trim_end_matches('\\').to_lowercase();
            let saved = self
                .initial
                .index_roots
                .iter()
                .any(|root| key(root) == key(&place));
            let text = match &mut self.reindex {
                _ if !saved => format!("{place} is new here. Save, and Feather reads it."),
                Some(reindex) => {
                    reindex(Some(&place));
                    format!("Reading {place} again. Searches keep working while it runs.")
                }
                None => "The index can't be rebuilt right now.".to_string(),
            };
            form.set_detail(id, &text);
        }

        /// Shows each change in the main window right away; Cancel puts the
        /// old settings back.
        /// Keeps Status current while Settings is open, so a tunnel's
        /// address appears as soon as it is known.
        fn on_tick(&mut self, form: &mut Form) -> bool {
            let Some(status) = &mut self.sharing_status else {
                return false;
            };
            let (lines, address) = status();
            if lines == self.shown_status {
                return false;
            }
            form.set_detail(SHARE_STATUS, &lines.join("\n"));
            form.set_qr(SHARE_QR, address.as_deref().unwrap_or(""));
            self.shown_status = lines;
            self.address = address;
            true
        }

        fn on_change(&mut self, form: &Form, _id: Id) {
            let settings = self.collect(form);
            if let Some(preview) = &mut self.preview {
                preview(&settings);
            }
        }

        /// Previews the Appearance choices on this window before saving.
        fn look(&self, form: &Form) -> Option<Look> {
            Some(Look {
                dark: theme::resolve_dark(pick(&THEMES, form.selected(THEME))),
                accent: pick(&Accent::ALL, form.selected(ACCENT)),
                material: pick(&Material::ALL, form.selected(BACKDROP)),
            })
        }

        fn take_outcome(&mut self) -> Option<Box<dyn Any>> {
            self.result
                .take()
                .map(|settings| Box::new(settings) as Box<dyn Any>)
        }
    }

    fn open(parent: HWND, initial: Settings, look: Look, live: Live) -> Result<FormWindow, String> {
        let drives = drive_choices(&initial.index_roots);
        let mut form = build(&initial, &drives, live.items);
        form.set_detail(PICTURE, &picture_detail(&initial.background_image));
        let initial_picture = initial.background_image.clone();
        let status = if live.sharing.is_empty() {
            "Off. Add a folder and choose what people can do below.".to_string()
        } else {
            live.sharing.join("\n")
        };
        form.set_detail(SHARE_STATUS, &status);
        form.set_qr(SHARE_QR, live.address.as_deref().unwrap_or(""));
        let dialog = SettingsDialog {
            initial,
            drives,
            result: None,
            preview: live.preview,
            reindex: live.reindex,
            picture: initial_picture,
            address: live.address,
            apply_sharing: live.apply_sharing,
            sharing_status: live.sharing_status,
            shown_status: live.sharing.clone(),
        };
        FormWindow::open(parent, "Settings", 860, 680, look, form, Box::new(dialog))
    }

    /// Shows Settings. `live.preview` sees every change as it happens.
    pub fn show(
        parent: HWND,
        initial: Settings,
        look: Look,
        live: Live,
    ) -> Result<Option<Settings>, String> {
        let outcome = open(parent, initial, look, live)?.run_modal();
        Ok(outcome.and_then(|boxed| boxed.downcast::<Settings>().ok().map(|boxed| *boxed)))
    }

    pub fn capture(parent: HWND, initial: Settings, look: Look, path: &Path) -> Result<(), String> {
        open(parent, initial, look, sample_live())?.capture(path)
    }

    /// A made-up index size, so screenshots show the slider's count.
    fn sample_live() -> Live {
        Live {
            items: 1_284_500,
            sharing: vec!["Open https://192.168.1.20:8443".into()],
            address: Some("https://192.168.1.20:8443".into()),
            ..Live::default()
        }
    }

    /// Captures one page of the dialog.
    pub fn capture_page(
        parent: HWND,
        initial: Settings,
        look: Look,
        path: &Path,
        page: usize,
    ) -> Result<(), String> {
        let mut window = open(parent, initial, look, sample_live())?;
        window.set_page(page);
        window.capture(path)
    }

    /// Drags the first sidebar tab down two places, then captures the page.
    /// Returns the order the dialog would save.
    pub fn capture_tab_drag(
        parent: HWND,
        initial: Settings,
        look: Look,
        path: &Path,
    ) -> Result<Vec<String>, String> {
        let mut window = open(parent, initial, look, sample_live())?;
        window.set_page(PAGE_SIDEBAR);
        window.capture_drag(path, SIDEBAR_TABS, 0, 2)
    }

    /// Clicks the password box on Sharing; see `FormWindow::probe_click`.
    pub fn probe_password_click(
        parent: HWND,
        initial: Settings,
        look: Look,
        scroll: f32,
    ) -> Result<String, String> {
        open(parent, initial, look, sample_live())?.probe_click(
            PAGE_SHARING,
            scroll,
            SHARE_PASSWORD,
        )
    }

    /// Captures the custom theme page with the accent's picker open.
    pub fn capture_color_picker(
        parent: HWND,
        initial: Settings,
        look: Look,
        path: &Path,
    ) -> Result<(), String> {
        let mut window = open(parent, initial, look, sample_live())?;
        window.set_page(PAGE_THEME);
        window.open_color(THEME_ACCENT);
        window.capture_at(path, 180.0)
    }

    /// Captures Sharing with its sections unfolded, a tip showing, and,
    /// with `qr`, the phone code open.
    pub fn capture_sharing_open(
        parent: HWND,
        initial: Settings,
        look: Look,
        path: &Path,
        qr: bool,
        scroll: f32,
    ) -> Result<(), String> {
        let mut window = open(parent, initial, look, sample_live())?;
        window.set_page(PAGE_SHARING);
        window.open_color(TUNNEL_SECTION);
        if qr {
            window.open_color(SHARE_QR);
        } else {
            window.show_tip(SHARE_ACCESS);
        }
        window.capture_at(path, scroll)
    }

    /// Captures one page of the dialog scrolled down by `scroll` pixels.
    pub fn capture_page_at(
        parent: HWND,
        initial: Settings,
        look: Look,
        path: &Path,
        page: usize,
        scroll: f32,
    ) -> Result<(), String> {
        let mut window = open(parent, initial, look, sample_live())?;
        window.set_page(page);
        window.capture_at(path, scroll)
    }

    /// Captures the dialog with the shortcut box listening and `held` keys down.
    pub fn capture_recording(
        parent: HWND,
        initial: Settings,
        look: Look,
        path: &Path,
        held: &[u32],
    ) -> Result<(), String> {
        let mut window = open(parent, initial, look, sample_live())?;
        window.set_page(PAGE_SHORTCUT);
        window.capture_keys(path, 0.0, Some((HOTKEY, held)))
    }
}

#[cfg(windows)]
pub use dialog::{
    capture, capture_color_picker, capture_page, capture_page_at, capture_recording,
    capture_sharing_open, capture_tab_drag, probe_password_click, show, Live, Preview,
    PAGE_APPEARANCE, PAGE_INDEXES, PAGE_SHARING, PAGE_SHORTCUT, PAGE_SIDEBAR, PAGE_THEME,
};

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    use super::dialog::{drive_choices, split_roots};
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "feather-settings-{}-{}-{}.ini",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ))
    }

    #[test]
    fn accent_names_from_older_settings_still_load() {
        assert_eq!(Accent::parse("green"), Some(Accent::Saffron));
        assert_eq!(Accent::parse("Amber"), Some(Accent::Saffron));
        assert_eq!(Accent::parse("blue"), Some(Accent::Cobalt));
        assert_eq!(Accent::parse("plum"), Some(Accent::Iris));
        assert_eq!(Accent::parse(" teal "), Some(Accent::Teal));
        assert_eq!(Accent::parse("magenta"), None);
    }

    #[test]
    fn settings_round_trip_through_a_file() {
        let path = temp_path("round-trip");
        let settings = Settings {
            results_percent: 37,
            search_delay_ms: 80,
            open_mode: OpenMode::ContainingFolder,
            auto_update: AutoUpdate::Hourly,
            show_all_when_empty: false,
            remember_filters: true,
            remember_scope: true,
            remember_window: false,
            always_on_top: true,
            show_hidden: false,
            show_system: false,
            use_shell_context_menu: false,
            default_category: Category::Files,
            start_at_login: true,
            close_to_tray: true,
            tray_icon_visible: false,
            hotkey: "Ctrl + Alt + F".into(),
            filters: FilterSet {
                kind: KindFilter::Files,
                extensions: vec!["docx".into(), "pdf".into()],
                min_size: Some(1024),
                max_size: Some(4096),
                match_mode: MatchMode::EndsWith,
            },
            excluded_volumes: vec![r"D:".into(), r"E:\Archive".into()],
            excluded_folders: vec![
                r"C:\Temp\Scratch".into(),
                r"C:\Users\Me\Downloads\Big".into(),
            ],
            index_roots: Vec::new(),
            setup_done: false,
            sidebar_tabs: normalize_tabs(vec![
                (Category::Images, true),
                (Category::Everything, false),
            ]),
            sidebar_collapsed: true,
            scan_excluded_paths: Vec::new(),
            scan_excluded_names: Vec::new(),
            index_location: String::new(),
            rescan_policy: "manual".into(),
            admin_only: false,
            sort_mode: SortMode::Ordinal,
            folders_first: false,
            dark_mode: DarkMode::Full,
            material: Material::Mica,
            accent: Accent::Iris,
            network_access: NetworkAccess::None,
            start_menu_shortcut: true,
            desktop_shortcut: true,
            scope: Some(r"C:\Users\Me\Documents".into()),
            window: Some(WindowPlacement {
                x: 40,
                y: 60,
                width: 1200,
                height: 800,
                maximized: true,
            }),
            size_format: SizeFormat::Mb,
            details_pane: false,
            custom_theme: true,
            theme_colors: ThemeColors {
                background: 0x102030,
                panel: 0x203040,
                text: 0xffffff,
                text2: 0xaabbcc,
                accent: 0x33cc99,
            },
            background_image: r"C:\Users\Me\AppData\Roaming\Feather\backgrounds\sky.png".into(),
            background_fit: BackgroundFit::Tile,
            background_strength: 35,
            http_mode: HttpMode::Search,
            ftp_mode: FtpMode::Read,
            share_access: ShareAccess::Internet,
            tunnel: Tunnel::Ngrok,
            http_port: 9443,
            ftp_port: 2222,
            share_user: "me".into(),
            share_password: "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA".into(),
            share_folders: vec![r"D:\Photos".into()],
            ftp_plain: true,
            tunnel_token: "AQAAAA".into(),
            share_password_saved: "0102".into(),
            share_strong_password: true,
            share_e2e: true,
            tunnel_domain: "quiet-river.ngrok-free.app".into(),
        };

        settings.save_to(&path).expect("save");
        let loaded = load_from(&path);
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        assert_eq!(loaded.settings, settings);

        let mut updated = loaded.settings;
        updated.results_percent = 5;
        updated.save_to(&path).expect("replace existing file");
        let reloaded = load_from(&path);
        assert_eq!(reloaded.settings.results_percent, 5);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn whole_drives_survive_a_reload() {
        // Older versions wrote `C:`, which then failed to load.
        let loaded = parse("index_roots=C:;D:\\;C:\\Users\\Me\n");
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        assert_eq!(
            loaded.settings.index_roots,
            vec![r"C:\", r"D:\", r"C:\Users\Me"]
        );
        let again = parse(&loaded.settings.to_text());
        assert!(again.warnings.is_empty(), "{:?}", again.warnings);
        assert_eq!(again.settings.index_roots, loaded.settings.index_roots);
    }

    #[test]
    #[cfg(windows)]
    fn drives_and_folders_are_split_for_the_dialog() {
        let roots = vec![r"C:\".to_string(), r"E:\Photos".to_string()];
        let drives = vec![r"C:\".to_string(), r"D:\".to_string()];
        let (checked, others) = split_roots(&roots, &drives);
        assert_eq!(checked, vec![true, false]);
        assert_eq!(others, vec![r"E:\Photos"]);
        // A searched drive that is not fixed (a USB drive) is still offered.
        assert!(drive_choices(&[r"Q:\".to_string()]).contains(&r"Q:\".to_string()));
    }

    #[test]
    fn the_shortcut_defaults_to_ctrl_space() {
        assert_eq!(Settings::default().hotkey, "Ctrl + Space");
        assert_eq!(parse("hotkey=\n").settings.hotkey, "Ctrl + Space");
        assert_eq!(parse("hotkey=none\n").settings.hotkey, "None");
        assert_eq!(
            parse("hotkey=alt+shift+k\n").settings.hotkey,
            "Alt + Shift + K"
        );
        assert_eq!(
            parse("hotkey=Ctrl + Shift + Right Shift\n").settings.hotkey,
            "Ctrl + Shift + Right Shift"
        );
        assert!(!parse("hotkey=k\n").warnings.is_empty());
    }

    #[test]
    fn search_locations_round_trip_and_are_normalized() {
        let loaded = parse(
            "\
remember_scope=1
scope=C:\\Users\\Me\\Documents\\
excluded_volumes=d:;D:\\;e:
excluded_folders=C:\\Temp\\Scratch;C:\\Users\\Me\\Downloads\\Big\\
show_hidden=0
show_system=0
use_shell_context_menu=0
",
        );
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        assert!(loaded.settings.remember_scope);
        assert_eq!(
            loaded.settings.scope.as_deref(),
            Some(r"C:\Users\Me\Documents")
        );
        assert_eq!(loaded.settings.excluded_volumes, vec!["d:", "e:"]);
        assert_eq!(
            loaded.settings.excluded_folders,
            vec![r"C:\Temp\Scratch", r"C:\Users\Me\Downloads\Big"]
        );
        assert!(!loaded.settings.show_hidden);
        assert!(!loaded.settings.show_system);
        assert!(!loaded.settings.use_shell_context_menu);

        let text = loaded.settings.to_text();
        let again = parse(&text);
        assert_eq!(again.settings, loaded.settings);
    }

    #[test]
    fn excluded_folder_editor_requires_full_paths() {
        assert_eq!(
            parse_excluded_folders("C:\\Temp\r\n\r\nD:\\Games\\Old\r\n").expect("parse"),
            vec![r"C:\Temp", r"D:\Games\Old"]
        );
        assert!(parse_excluded_folders("Temp").is_err());
    }

    #[test]
    fn invalid_values_fall_back_and_warn() {
        let loaded = parse(
            "\
results_percent=0
search_delay_ms=99999
open_mode=sideways
always_on_top=maybe
filter_kind=sometimes
filter_extensions=*.pdf
window=1,2,3
",
        );
        assert_eq!(loaded.settings.results_percent, 1);
        assert_eq!(loaded.settings.search_delay_ms, MAX_SEARCH_DELAY_MS);
        assert_eq!(loaded.settings.open_mode, OpenMode::File);
        assert!(!loaded.settings.always_on_top);
        assert_eq!(loaded.settings.filters.kind, KindFilter::Any);
        assert!(!loaded.warnings.is_empty());
        assert_eq!(loaded.warnings.len(), 7, "{:?}", loaded.warnings);
    }

    #[test]
    fn comments_blank_lines_and_commas_are_accepted() {
        let loaded = parse(
            "\
; comment
# another

max_results=50,000
results_percent=40
remember_window=yes
window=10,20,1100,700,0
",
        );
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        assert_eq!(loaded.settings.results_percent, 40);
        assert!(loaded.settings.remember_window);
        assert_eq!(
            loaded.settings.window,
            Some(WindowPlacement {
                x: 10,
                y: 20,
                width: 1100,
                height: 700,
                maximized: false,
            })
        );
    }

    #[test]
    fn window_values_are_clamped_to_a_usable_size() {
        let loaded = parse("window=0,0,10,10,0\n");
        let window = loaded.settings.window.expect("window");
        assert_eq!(window.width, MIN_WINDOW_WIDTH);
        assert_eq!(window.height, MIN_WINDOW_HEIGHT);
    }

    #[test]
    fn dialog_parsers_reject_out_of_range_values() {
        assert_eq!(result_cap(100, 1_000), usize::MAX);
        assert_eq!(result_cap(25, 1_000), 250);
        assert_eq!(result_cap(1, 150), 2);
        assert_eq!(result_cap(1, 0), 1);
        assert_eq!(parse_delay("0"), Ok(0));
        assert_eq!(parse_delay("250"), Ok(250));
        assert!(parse_delay("2001").is_err());
    }
}
