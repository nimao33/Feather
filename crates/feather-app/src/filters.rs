//! Filter state shared by the search path and the Filters dialog.

use feather_core::MatchMode;

use crate::text::human_size;
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum KindFilter {
    #[default]
    Any,
    Files,
    Folders,
}

/// A one-click filter preset. The preset writes the normal kind and extension
/// fields, so the query parser and the Filters dialog remain the single source
/// of truth. `Custom` is derived, never selected from the preset list.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Category {
    #[default]
    Everything,
    Apps,
    Files,
    Folders,
    Documents,
    Images,
    Video,
    Audio,
    Archives,
    Executables,
    Code,
    Custom,
}

pub const PRESET_CATEGORIES: [Category; 11] = [
    Category::Everything,
    Category::Apps,
    Category::Files,
    Category::Folders,
    Category::Documents,
    Category::Images,
    Category::Video,
    Category::Audio,
    Category::Archives,
    Category::Executables,
    Category::Code,
];

impl Category {
    pub fn label(self) -> &'static str {
        match self {
            Self::Everything => "Everything",
            Self::Apps => "Apps",
            Self::Files => "Files",
            Self::Folders => "Folders",
            Self::Documents => "Documents",
            Self::Images => "Images",
            Self::Video => "Video",
            Self::Audio => "Audio",
            Self::Archives => "Archives",
            Self::Executables => "Executables",
            Self::Code => "Code",
            Self::Custom => "Custom",
        }
    }

    pub fn extensions(self) -> &'static [&'static str] {
        match self {
            // Start menu and desktop shortcuts, programs, and ClickOnce apps.
            Self::Apps => &["lnk", "exe", "appref-ms"],
            Self::Documents => &[
                "doc", "docx", "docm", "odt", "rtf", "txt", "md", "pdf", "xls", "xlsx", "xlsm",
                "ods", "csv", "ppt", "pptx", "pps", "ppsx", "odp", "epub",
            ],
            Self::Images => &[
                "jpg", "jpeg", "png", "gif", "bmp", "webp", "tif", "tiff", "svg", "ico", "heic",
                "raw", "cr2", "nef", "arw",
            ],
            Self::Video => &[
                "mp4", "mkv", "avi", "mov", "wmv", "webm", "m4v", "flv", "mpg", "mpeg", "ts", "3gp",
            ],
            Self::Audio => &[
                "mp3", "wav", "flac", "aac", "ogg", "m4a", "wma", "opus", "aiff", "mid", "midi",
            ],
            Self::Archives => &[
                "zip", "rar", "7z", "tar", "gz", "tgz", "bz2", "xz", "iso", "cab", "tar.gz",
            ],
            Self::Executables => &[
                "exe", "msi", "msix", "appx", "bat", "cmd", "com", "scr", "ps1",
            ],
            Self::Code => &[
                "rs", "py", "js", "ts", "jsx", "tsx", "c", "h", "cpp", "hpp", "cc", "cs", "java",
                "kt", "kts", "go", "rb", "php", "swift", "lua", "sh", "html", "htm", "css", "scss",
                "json", "xml", "yaml", "yml", "toml", "sql", "vue", "svelte",
            ],
            _ => &[],
        }
    }

    pub fn apply(self, filters: &mut FilterSet) {
        match self {
            Self::Everything => {
                filters.kind = KindFilter::Any;
                filters.extensions.clear();
            }
            Self::Files => {
                filters.kind = KindFilter::Files;
                filters.extensions.clear();
            }
            Self::Folders => {
                filters.kind = KindFilter::Folders;
                filters.extensions.clear();
            }
            Self::Custom => {}
            _ => {
                filters.kind = KindFilter::Files;
                filters.extensions = self
                    .extensions()
                    .iter()
                    .map(|extension| (*extension).to_string())
                    .collect();
            }
        }
    }

    pub fn detect(filters: &FilterSet) -> Self {
        if filters.kind == KindFilter::Any && filters.extensions.is_empty() {
            return Self::Everything;
        }
        if filters.extensions.is_empty() {
            return match filters.kind {
                KindFilter::Files => Self::Files,
                KindFilter::Folders => Self::Folders,
                KindFilter::Any => Self::Everything,
            };
        }
        for category in PRESET_CATEGORIES {
            let extensions = category.extensions();
            if category == Self::Everything
                || category == Self::Files
                || category == Self::Folders
                || filters.kind != KindFilter::Files
                || extensions.len() != filters.extensions.len()
            {
                continue;
            }
            let matches = filters
                .extensions
                .iter()
                .all(|extension| extensions.iter().any(|preset| preset == extension));
            if matches {
                return category;
            }
        }
        Self::Custom
    }

    pub fn index(self) -> usize {
        PRESET_CATEGORIES
            .iter()
            .position(|category| *category == self)
            .unwrap_or(PRESET_CATEGORIES.len())
    }

    /// Parses the names written to `settings.ini`. `Custom` is derived from a
    /// filter set, never restored as a preset.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "everything" | "all" => Some(Self::Everything),
            "apps" | "app" | "applications" | "programs" => Some(Self::Apps),
            "files" | "file" => Some(Self::Files),
            "folders" | "folder" => Some(Self::Folders),
            "documents" | "document" => Some(Self::Documents),
            "images" | "image" => Some(Self::Images),
            "video" | "videos" => Some(Self::Video),
            "audio" => Some(Self::Audio),
            "archives" | "archive" => Some(Self::Archives),
            "executables" | "executable" => Some(Self::Executables),
            "code" => Some(Self::Code),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FilterSet {
    pub kind: KindFilter,
    pub extensions: Vec<String>,
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    pub match_mode: MatchMode,
}

impl FilterSet {
    pub fn is_empty(&self) -> bool {
        self.kind == KindFilter::Any
            && self.extensions.is_empty()
            && self.min_size.is_none()
            && self.max_size.is_none()
    }

    pub fn category(&self) -> Category {
        Category::detect(self)
    }

    pub fn active_categories(&self) -> usize {
        let category = self.category();
        let kind_and_extensions = if category == Category::Custom {
            usize::from(self.kind != KindFilter::Any) + usize::from(!self.extensions.is_empty())
        } else if category == Category::Everything {
            0
        } else {
            1
        };
        kind_and_extensions
            + usize::from(self.min_size.is_some() || self.max_size.is_some())
            + usize::from(self.match_mode != MatchMode::Anywhere)
    }

    /// Combines the user's search text with the structured filters. The
    /// existing query parser then becomes the single matching path.
    pub fn compose(&self, text: &str) -> String {
        let mut parts = Vec::new();
        let text = text.trim();
        if !text.is_empty() {
            parts.push(text.to_string());
        }
        match self.kind {
            KindFilter::Files => parts.push("type:file".into()),
            KindFilter::Folders => parts.push("type:folder".into()),
            KindFilter::Any => {}
        }
        if !self.extensions.is_empty() {
            parts.push(format!("ext:{}", self.extensions.join(";")));
        }
        match (self.min_size, self.max_size) {
            (Some(min), Some(max)) => parts.push(format!("size:{min}..{max}")),
            (Some(min), None) => parts.push(format!("size:>={min}")),
            (None, Some(max)) => parts.push(format!("size:<={max}")),
            (None, None) => {}
        }
        if self.match_mode != MatchMode::Anywhere {
            parts.push(format!("match:{}", self.match_mode.as_str()));
        }
        parts.join(" ")
    }

    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        let category = self.category();
        match category {
            Category::Everything => {}
            Category::Custom => {
                match self.kind {
                    KindFilter::Any => {}
                    KindFilter::Files => parts.push("Files".to_string()),
                    KindFilter::Folders => parts.push("Folders".to_string()),
                }
                if !self.extensions.is_empty() {
                    parts.push(self.extensions.join(", "));
                }
            }
            _ => parts.push(category.label().to_string()),
        }
        if let Some(min) = self.min_size {
            parts.push(format!(">= {}", human_size(min)));
        }
        if let Some(max) = self.max_size {
            parts.push(format!("<= {}", human_size(max)));
        }
        if self.match_mode != MatchMode::Anywhere {
            parts.push(self.match_mode.label().to_string());
        }
        parts.join(" | ")
    }
}

pub fn parse_extensions(input: &str) -> Result<Vec<String>, String> {
    let mut extensions = Vec::new();
    for part in input.split([';', ',', ' ']) {
        let part = part.trim().trim_start_matches('.').to_lowercase();
        if part.is_empty() {
            continue;
        }
        if !part
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '.' || ch == '_' || ch == '-')
        {
            return Err(format!("invalid extension: {part}"));
        }
        extensions.push(part);
    }
    extensions.sort();
    extensions.dedup();
    Ok(extensions)
}

pub fn parse_size_input(input: &str) -> Result<Option<u64>, String> {
    let input = input.trim();
    if input.is_empty() {
        return Ok(None);
    }
    feather_core::query::parse_size_value(input)
        .map(Some)
        .ok_or_else(|| format!("invalid size: {input}"))
}

/// Formats an existing size without rounding, so opening and applying the
/// dialog cannot silently change the filter. Whole units stay readable;
/// anything else is shown in exact bytes.
pub fn size_input_value(bytes: u64) -> String {
    const UNITS: [(&str, u64); 4] = [
        ("TB", 1024u64 * 1024 * 1024 * 1024),
        ("GB", 1024u64 * 1024 * 1024),
        ("MB", 1024u64 * 1024),
        ("KB", 1024),
    ];
    for (unit, factor) in UNITS {
        if bytes >= factor && bytes.is_multiple_of(factor) {
            return format!("{} {unit}", bytes / factor);
        }
    }
    bytes.to_string()
}

#[cfg(windows)]
mod dialog {
    use super::*;
    use crate::ui::form::{option, Action, ButtonStyle, Form, FormDialog, FormWindow, Id, Look};
    use std::any::Any;
    use std::path::Path;
    use windows_sys::Win32::Foundation::HWND;
    const KIND: Id = "kind";
    const EXTENSIONS: Id = "extensions";
    const MIN_SIZE: Id = "min_size";
    const MAX_SIZE: Id = "max_size";
    const MATCH: Id = "match";
    const APPLY: Id = "apply";
    const RESET: Id = "reset";
    const CANCEL: Id = "cancel";

    const KINDS: [KindFilter; 3] = [KindFilter::Any, KindFilter::Files, KindFilter::Folders];
    const MATCHES: [MatchMode; 3] = [
        MatchMode::Anywhere,
        MatchMode::StartsWith,
        MatchMode::EndsWith,
    ];

    struct FiltersDialog {
        result: Option<FilterSet>,
    }

    impl FiltersDialog {
        fn apply(&mut self, form: &mut Form) -> Action {
            let kind = KINDS[form.selected(KIND).min(KINDS.len() - 1)];
            let match_mode = MATCHES[form.selected(MATCH).min(MATCHES.len() - 1)];
            let fail = |form: &mut Form, field: Id, error: String| {
                form.set_error(Some(error));
                form.focus(field);
                Action::Stay
            };
            let extensions = match parse_extensions(&form.text(EXTENSIONS)) {
                Ok(extensions) => extensions,
                Err(error) => return fail(form, EXTENSIONS, error),
            };
            let min_size = match parse_size_input(&form.text(MIN_SIZE)) {
                Ok(size) => size,
                Err(error) => return fail(form, MIN_SIZE, error),
            };
            let max_size = match parse_size_input(&form.text(MAX_SIZE)) {
                Ok(size) => size,
                Err(error) => return fail(form, MAX_SIZE, error),
            };
            if let (Some(min), Some(max)) = (min_size, max_size) {
                if min > max {
                    return fail(
                        form,
                        MIN_SIZE,
                        "The minimum size is larger than the maximum size.".into(),
                    );
                }
            }
            form.set_error(None);
            self.result = Some(FilterSet {
                kind,
                extensions,
                min_size,
                max_size,
                match_mode,
            });
            Action::Accept
        }
    }

    impl FormDialog for FiltersDialog {
        fn on_button(&mut self, form: &mut Form, id: Id) -> Action {
            match id {
                APPLY => self.apply(form),
                RESET => {
                    form.set_selected(KIND, 0);
                    form.set_selected(MATCH, 0);
                    form.set_text(EXTENSIONS, "");
                    form.set_text(MIN_SIZE, "");
                    form.set_text(MAX_SIZE, "");
                    form.set_error(None);
                    Action::Stay
                }
                _ => Action::Cancel,
            }
        }

        fn take_outcome(&mut self) -> Option<Box<dyn Any>> {
            self.result
                .take()
                .map(|filters| Box::new(filters) as Box<dyn Any>)
        }
    }

    fn build(initial: &FilterSet) -> Form {
        let mut form = Form::new();
        form.heading("What to show");
        form.segments(
            KIND,
            "Kind",
            None,
            vec![option("All items"), option("Files"), option("Folders")],
            KINDS
                .iter()
                .position(|kind| *kind == initial.kind)
                .unwrap_or(0),
        );
        form.field(
            EXTENSIONS,
            "Extensions",
            Some("Separate with commas. Leave empty for every extension."),
            &initial.extensions.join(", "),
            "pdf, docx, txt",
            true,
        );
        form.heading("Typed words match");
        form.segments(
            MATCH,
            "Match",
            Some("Where a word from the search box may appear in the name."),
            vec![
                option("Anywhere"),
                option("Starts with"),
                option("Ends with"),
            ],
            MATCHES
                .iter()
                .position(|mode| *mode == initial.match_mode)
                .unwrap_or(0),
        );
        form.heading("Size");
        // Exact values avoid changing an existing filter just by opening and
        // applying the window; KB, MB, and GB values can still be typed.
        form.field(
            MIN_SIZE,
            "At least",
            Some("Like 500 KB or 2 GB. Blank means no limit."),
            &initial.min_size.map(size_input_value).unwrap_or_default(),
            "No minimum",
            false,
        );
        form.field(
            MAX_SIZE,
            "At most",
            None,
            &initial.max_size.map(size_input_value).unwrap_or_default(),
            "No maximum",
            false,
        );
        form.button(RESET, "Reset", ButtonStyle::Quiet);
        form.button(CANCEL, "Cancel", ButtonStyle::Secondary);
        form.button(APPLY, "Apply", ButtonStyle::Primary);
        form.focus(EXTENSIONS);
        form
    }

    fn open(parent: HWND, initial: &FilterSet, look: Look) -> Result<FormWindow, String> {
        FormWindow::open(
            parent,
            "Filters",
            540,
            600,
            look,
            build(initial),
            Box::new(FiltersDialog { result: None }),
        )
    }

    pub fn show(parent: HWND, initial: FilterSet, look: Look) -> Result<Option<FilterSet>, String> {
        let outcome = open(parent, &initial, look)?.run_modal();
        Ok(outcome.and_then(|boxed| boxed.downcast::<FilterSet>().ok().map(|boxed| *boxed)))
    }

    pub fn capture(
        parent: HWND,
        initial: FilterSet,
        look: Look,
        path: &Path,
    ) -> Result<(), String> {
        open(parent, &initial, look)?.capture(path)
    }
}

#[cfg(windows)]
pub use dialog::{capture, show};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_filter_does_not_change_the_query() {
        let filters = FilterSet::default();
        assert!(filters.is_empty());
        assert_eq!(filters.compose("report"), "report");
        assert_eq!(filters.compose(""), "");
        assert_eq!(filters.compose("  report  "), "report");
        assert_eq!(filters.active_categories(), 0);
    }

    #[test]
    fn preset_names_parse_back_from_settings() {
        for category in PRESET_CATEGORIES {
            assert_eq!(Category::parse(category.label()), Some(category));
        }
        assert_eq!(Category::parse("ALL"), Some(Category::Everything));
        assert_eq!(Category::parse("custom"), None);
        assert_eq!(Category::parse("nonsense"), None);
    }

    #[test]
    fn all_filters_compose_into_query_tokens() {
        let filters = FilterSet {
            kind: KindFilter::Files,
            extensions: vec!["docx".into(), "pdf".into()],
            min_size: Some(1024),
            max_size: Some(2048),
            match_mode: MatchMode::Anywhere,
        };
        assert_eq!(
            filters.compose("report"),
            "report type:file ext:docx;pdf size:1024..2048"
        );
        assert_eq!(filters.active_categories(), 3);
        assert_eq!(
            filters.summary(),
            "Files | docx, pdf | >= 1.0 KB | <= 2.0 KB"
        );

        let starting = FilterSet {
            match_mode: MatchMode::StartsWith,
            ..filters.clone()
        };
        assert_eq!(
            starting.compose("report"),
            "report type:file ext:docx;pdf size:1024..2048 match:starts"
        );
        assert_eq!(starting.active_categories(), 4);
        assert!(starting.summary().ends_with("Starts with"));
        let query = feather_core::Query::parse(&starting.compose("report"));
        assert_eq!(query.match_mode(), MatchMode::StartsWith);
        assert!(query.warnings().is_empty(), "{:?}", query.warnings());
    }

    #[test]
    fn size_bounds_compose_as_one_parsable_token() {
        let min_only = FilterSet {
            min_size: Some(1024),
            ..FilterSet::default()
        };
        let max_only = FilterSet {
            max_size: Some(2048),
            ..FilterSet::default()
        };
        assert_eq!(min_only.compose(""), "size:>=1024");
        assert_eq!(max_only.compose(""), "size:<=2048");

        let query = feather_core::Query::parse(&min_only.compose(""));
        assert!(!query.is_empty());
        assert!(query.warnings().is_empty(), "{:?}", query.warnings());
    }

    #[test]
    fn category_presets_round_trip_through_the_filter_set() {
        for category in PRESET_CATEGORIES {
            let mut filters = FilterSet::default();
            category.apply(&mut filters);
            assert_eq!(filters.category(), category, "{category:?}");
            assert_eq!(
                filters.active_categories(),
                usize::from(category != Category::Everything)
            );
        }

        let mut filters = FilterSet {
            kind: KindFilter::Files,
            extensions: vec!["pdf".into(), "xyz".into()],
            min_size: Some(1024),
            max_size: None,
            ..FilterSet::default()
        };
        assert_eq!(filters.category(), Category::Custom);
        assert_eq!(filters.active_categories(), 3);
        assert_eq!(filters.summary(), "Files | pdf, xyz | >= 1.0 KB");

        filters.extensions = vec!["pdf".into()];
        assert_eq!(filters.summary(), "Files | pdf | >= 1.0 KB");
    }

    #[test]
    fn category_summary_uses_the_preset_name() {
        let mut filters = FilterSet::default();
        Category::Documents.apply(&mut filters);
        assert_eq!(filters.summary(), "Documents");
        assert_eq!(filters.category(), Category::Documents);
        assert!(filters
            .compose("")
            .starts_with("type:file ext:doc;docx;docm;odt;rtf;txt;md;pdf"));
    }

    #[test]
    fn extension_input_is_normalized_and_deduplicated() {
        assert_eq!(
            parse_extensions(" PDF, .docx; pdf  tar.gz "),
            Ok(vec![
                "docx".to_string(),
                "pdf".to_string(),
                "tar.gz".to_string()
            ])
        );
        assert!(parse_extensions("*.pdf").is_err());
        assert!(parse_extensions("bad/ext").is_err());
    }

    #[test]
    fn size_input_accepts_units_and_blank() {
        assert_eq!(parse_size_input(""), Ok(None));
        assert_eq!(parse_size_input(" 1mb "), Ok(Some(1024 * 1024)));
        assert_eq!(parse_size_input("2.5gb"), Ok(Some(2_684_354_560)));
        assert!(parse_size_input("nope").is_err());
    }

    #[test]
    fn size_input_values_round_trip_without_rounding() {
        assert_eq!(size_input_value(0), "0");
        assert_eq!(size_input_value(1024), "1 KB");
        assert_eq!(size_input_value(1024 * 1024), "1 MB");
        assert_eq!(size_input_value(2_684_354_560), "2560 MB");
        assert_eq!(size_input_value(123_456_789), "123456789");
        for value in [0, 1024, 1024 * 1024, 2_684_354_560, 123_456_789] {
            assert_eq!(
                parse_size_input(&size_input_value(value)),
                Ok(Some(value)),
                "round trip {value}"
            );
        }
    }
}
