use crate::date::{parse_date_filter, Clock, DateRange};
use crate::entry::EntryView;
use crate::index::VolumeIndex;
use crate::path::{is_absolute_path, normalize_path, path_within};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    File,
    Folder,
}

/// Where a typed word may appear in a name. Set per query with `match:` or
/// from the Filters window; it applies to every plain name term.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MatchMode {
    #[default]
    Anywhere,
    StartsWith,
    EndsWith,
}

impl MatchMode {
    pub const ALL: [MatchMode; 3] = [
        MatchMode::Anywhere,
        MatchMode::StartsWith,
        MatchMode::EndsWith,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Anywhere => "Anywhere",
            Self::StartsWith => "Starts with",
            Self::EndsWith => "Ends with",
        }
    }

    /// The name written to settings and accepted after `match:`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Anywhere => "anywhere",
            Self::StartsWith => "starts",
            Self::EndsWith => "ends",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "anywhere" | "contains" | "any" => Some(Self::Anywhere),
            "starts" | "start" | "starts_with" | "startswith" | "prefix" => Some(Self::StartsWith),
            "ends" | "end" | "ends_with" | "endswith" | "suffix" => Some(Self::EndsWith),
            _ => None,
        }
    }
}

/// A compiled name term: a literal carrying its Boyer–Moore–Horspool skip
/// table, so searching a million names does not prepare the pattern a million
/// times, or a wildcard pattern whose characters are parsed once.
#[derive(Clone, Debug)]
enum Needle {
    Literal(Literal),
    Pattern(Vec<char>),
}

#[derive(Clone, Debug)]
struct Literal {
    text: String,
    /// How far to advance after a mismatch on the last byte of a window.
    /// Boxed so a compiled query's terms stay small.
    skip: Box<[u16; 256]>,
}

impl Literal {
    fn new(text: String) -> Self {
        let bytes = text.as_bytes();
        let len = bytes.len().min(u16::MAX as usize);
        let mut skip = Box::new([len as u16; 256]);
        if len > 1 {
            for (index, byte) in bytes[..len - 1].iter().enumerate() {
                skip[*byte as usize] = (len - 1 - index) as u16;
            }
        }
        Self { text, skip }
    }

    #[inline]
    fn find(&self, haystack: &str) -> Option<usize> {
        let needle = self.text.as_bytes();
        let len = needle.len();
        if len == 0 {
            return Some(0);
        }
        let haystack = haystack.as_bytes();
        if len > haystack.len() {
            return None;
        }
        let last = len - 1;
        let mut at = 0usize;
        while at + len <= haystack.len() {
            if haystack[at + last] == needle[last] && haystack[at..at + last] == needle[..last] {
                return Some(at);
            }
            at += self.skip[haystack[at + last] as usize] as usize;
        }
        None
    }

    /// The first occurrence in an ASCII name, ignoring case and without
    /// allocating a lowercased copy.
    fn find_ascii(&self, haystack: &str) -> Option<usize> {
        let needle = self.text.as_bytes();
        let len = needle.len();
        if len == 0 {
            return Some(0);
        }
        let haystack = haystack.as_bytes();
        if len > haystack.len() {
            return None;
        }
        let last = len - 1;
        let mut at = 0usize;
        while at + len <= haystack.len() {
            let end = haystack[at + last].to_ascii_lowercase();
            if end == needle[last] && haystack[at..at + last].eq_ignore_ascii_case(&needle[..last])
            {
                return Some(at);
            }
            at += self.skip[end as usize] as usize;
        }
        None
    }

    /// Every non-overlapping occurrence in an ASCII name, appended as byte
    /// ranges. ASCII bytes are UTF-16 code units, so the ranges are ready for
    /// DirectWrite.
    fn find_all_ascii(&self, haystack: &str, spans: &mut Vec<(usize, usize)>) {
        let len = self.text.len();
        if len == 0 {
            return;
        }
        let mut from = 0usize;
        while from + len <= haystack.len() {
            let Some(found) = self.find_ascii(&haystack[from..]) else {
                break;
            };
            let start = from + found;
            spans.push((start, start + len));
            from = start + len;
        }
    }

    /// True when an ASCII name begins or ends with the term, ignoring case.
    fn edge_ascii(&self, name: &str, mode: MatchMode) -> bool {
        let len = self.text.len();
        if len > name.len() {
            return false;
        }
        let bytes = name.as_bytes();
        match mode {
            MatchMode::StartsWith => bytes[..len].eq_ignore_ascii_case(self.text.as_bytes()),
            MatchMode::EndsWith => {
                bytes[name.len() - len..].eq_ignore_ascii_case(self.text.as_bytes())
            }
            MatchMode::Anywhere => true,
        }
    }

    #[inline]
    fn matches(&self, name: &str, mode: MatchMode) -> bool {
        match mode {
            MatchMode::Anywhere => self.find(name).is_some(),
            MatchMode::StartsWith => name.starts_with(self.text.as_str()),
            MatchMode::EndsWith => name.ends_with(self.text.as_str()),
        }
    }
}

impl Needle {
    #[inline]
    fn matches(&self, name: &str, mode: MatchMode) -> bool {
        match self {
            Needle::Literal(literal) => literal.matches(name, mode),
            // Wildcards are explicit about the whole name, so they ignore the
            // match mode: `*.pdf` already means "ends with .pdf".
            Needle::Pattern(chars) => glob_match_chars(chars, name),
        }
    }
}

/// Longer literals reject more names per compare, so plain terms are tried
/// longest first and wildcard patterns last.
fn order_terms(terms: &mut [Needle]) {
    terms.sort_by(|a, b| match (a, b) {
        (Needle::Literal(a), Needle::Literal(b)) => b.text.len().cmp(&a.text.len()),
        (Needle::Literal(_), Needle::Pattern(_)) => std::cmp::Ordering::Less,
        (Needle::Pattern(_), Needle::Literal(_)) => std::cmp::Ordering::Greater,
        (Needle::Pattern(_), Needle::Pattern(_)) => std::cmp::Ordering::Equal,
    });
}

/// Typing a full folder path, like a file browser's address bar: `C:\Users\`
/// lists what is directly inside, and `C:\Users\ad` also lists the items
/// in `C:\Users` whose names contain `ad`.
#[derive(Clone, Debug)]
pub struct Browse {
    /// The whole typed path, normalized: its children are listed.
    folder: String,
    /// The folder the last part is typed in, when the path does not end in
    /// a backslash.
    parent: Option<String>,
    /// The last part, matched against names in `parent`.
    name: Needle,
}

/// Where a browse query lands in one volume, as folder file references.
#[derive(Clone, Copy, Debug, Default)]
pub struct VolumeFilter {
    folder: Option<u64>,
    parent: Option<u64>,
    active: bool,
}

impl VolumeFilter {
    /// False when the query browses a folder this volume does not have.
    pub fn may_match(&self) -> bool {
        !self.active || self.folder.is_some() || self.parent.is_some()
    }
}

#[derive(Clone, Debug, Default)]
pub struct Query {
    terms: Vec<Needle>,
    negated: Vec<Needle>,
    /// Terms with a backslash, matched against the whole path.
    path_terms: Vec<Needle>,
    negated_path_terms: Vec<Needle>,
    browse: Option<Browse>,
    extensions: Vec<String>,
    kind: Option<Kind>,
    min_size: Option<u64>,
    max_size: Option<u64>,
    /// Modification-date bounds from `dm:`/`date:`, already ANDed together
    /// and converted to FILETIMEs.
    dates: Option<DateRange>,
    scopes: Vec<String>,
    negated_scopes: Vec<String>,
    match_mode: MatchMode,
    warnings: Vec<String>,
    raw: String,
}

impl Query {
    /// Parses a query, reading date filters against the system clock and
    /// time zone.
    pub fn parse(input: &str) -> Self {
        Self::parse_with(input, None)
    }

    /// Parses a query with date filters read against `clock`.
    pub fn parse_at(input: &str, clock: &Clock) -> Self {
        Self::parse_with(input, Some(*clock))
    }

    fn parse_with(input: &str, mut clock: Option<Clock>) -> Self {
        let mut query = Query {
            raw: input.to_string(),
            ..Query::default()
        };

        for token in tokenize(input) {
            let (negative, body) = match token.strip_prefix('!') {
                Some(rest) => (true, rest.to_string()),
                None => (false, token),
            };
            let lower = body.to_lowercase();
            if lower.is_empty() {
                continue;
            }

            if let Some(value) = lower
                .strip_prefix("match:")
                .or_else(|| lower.strip_prefix("mode:"))
            {
                if negative {
                    query
                        .warnings
                        .push("negated match modes are not supported".into());
                } else {
                    match MatchMode::parse(value) {
                        Some(mode) => query.match_mode = mode,
                        None => query.warnings.push(format!("invalid match mode: {value}")),
                    }
                }
                continue;
            }

            if let Some(value) = lower.strip_prefix("ext:") {
                if negative {
                    query
                        .warnings
                        .push("negated extensions are not supported yet".into());
                    continue;
                }
                for part in value.split([';', ',']) {
                    let part = part.trim().trim_start_matches('.');
                    if part.is_empty() {
                        continue;
                    }
                    if !part
                        .chars()
                        .all(|c| c.is_alphanumeric() || c == '.' || c == '_' || c == '-')
                    {
                        query.warnings.push(format!("invalid extension: {part}"));
                        continue;
                    }
                    query.extensions.push(format!(".{part}"));
                }
                continue;
            }

            if let Some(value) = lower.strip_prefix("type:") {
                if negative {
                    query
                        .warnings
                        .push("negated types are not supported yet".into());
                    continue;
                }
                match value {
                    "file" | "files" => query.kind = Some(Kind::File),
                    "folder" | "folders" | "dir" | "directory" => query.kind = Some(Kind::Folder),
                    _ => query.warnings.push(format!("invalid type: {value}")),
                }
                continue;
            }

            if let Some(value) = lower.strip_prefix("size:") {
                if negative {
                    query
                        .warnings
                        .push("negated sizes are not supported yet".into());
                    continue;
                }
                match parse_size_filter(value) {
                    Some((min, max)) => {
                        // Multiple size tokens are ANDed, so keep the
                        // strongest lower bound and the strongest upper bound.
                        if let Some(min) = min {
                            query.min_size =
                                Some(query.min_size.map_or(min, |current| current.max(min)));
                        }
                        if let Some(max) = max {
                            query.max_size =
                                Some(query.max_size.map_or(max, |current| current.min(max)));
                        }
                    }
                    None => query.warnings.push(format!("invalid size: {value}")),
                }
                continue;
            }

            if let Some(value) = lower
                .strip_prefix("in:")
                .or_else(|| lower.strip_prefix("path:"))
                .or_else(|| lower.strip_prefix("folder:"))
            {
                let value = value.trim().trim_matches('"');
                if value.is_empty() {
                    query.warnings.push("empty search scope".into());
                } else if negative {
                    query.negated_scopes.push(normalize_path(value));
                } else {
                    query.scopes.push(normalize_path(value));
                }
                continue;
            }

            if let Some(value) = lower
                .strip_prefix("dm:")
                .or_else(|| lower.strip_prefix("date:"))
            {
                if negative {
                    query
                        .warnings
                        .push("negated date filters are not supported yet".into());
                    continue;
                }
                // The clock is read once per query, and only when needed.
                let clock = clock.get_or_insert_with(Clock::system);
                match parse_date_filter(value, clock) {
                    Some(range) => {
                        query.dates = Some(match query.dates {
                            Some(current) => current.intersect(range),
                            None => range,
                        });
                    }
                    None => query.warnings.push(format!("invalid date: {value}")),
                }
                continue;
            }

            // A backslash (or slash) can't be in a name, so the term is
            // about the path, like in Everything.
            if lower.contains('\\') || lower.contains('/') {
                let lower = lower.replace('/', "\\");
                if !negative && query.browse.is_none() && is_absolute_path(&lower) {
                    query.browse = Some(Browse::new(&lower));
                } else if negative {
                    query.negated_path_terms.push(needle_for(lower));
                } else {
                    query.path_terms.push(needle_for(lower));
                }
                continue;
            }

            let needle = needle_for(lower);
            if negative {
                query.negated.push(needle);
            } else {
                query.terms.push(needle);
            }
        }

        // With other words, a typed folder is where to look, recursively:
        // `C:\Users\ report` finds report anywhere under C:\Users.
        if !query.terms.is_empty() {
            if let Some(browse) = query.browse.take() {
                query.scopes.push(browse.folder);
            }
        }
        order_terms(&mut query.terms);
        order_terms(&mut query.negated);
        query
    }

    /// Resolves a browsed folder in one volume. Cheap: a few binary searches.
    pub fn volume_filter(&self, volume: &VolumeIndex) -> VolumeFilter {
        let Some(browse) = &self.browse else {
            return VolumeFilter::default();
        };
        let frn = |path: &str| volume.find_folder(path).map(|id| volume.entry(id).frn);
        VolumeFilter {
            folder: frn(&browse.folder),
            parent: browse.parent.as_deref().and_then(frn),
            active: true,
        }
    }

    /// The browse condition for an entry already accepted by `matches`.
    pub fn matches_folder(&self, filter: &VolumeFilter, entry: &EntryView<'_>) -> bool {
        self.matches_folder_parts(filter, entry.parent_frn, entry.name_search)
    }

    /// The browse condition from the fields a name filter already read.
    #[inline]
    pub fn matches_folder_parts(
        &self,
        filter: &VolumeFilter,
        parent_frn: u64,
        name_search: &str,
    ) -> bool {
        let Some(browse) = &self.browse else {
            return true;
        };
        filter.folder == Some(parent_frn)
            || (filter.parent == Some(parent_frn)
                && browse.name.matches(name_search, MatchMode::Anywhere))
    }

    pub fn browse_folder(&self) -> Option<&str> {
        self.browse.as_ref().map(|browse| browse.folder.as_str())
    }

    pub fn raw(&self) -> &str {
        &self.raw
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub fn match_mode(&self) -> MatchMode {
        self.match_mode
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
            && self.negated.is_empty()
            && self.path_terms.is_empty()
            && self.negated_path_terms.is_empty()
            && self.browse.is_none()
            && self.extensions.is_empty()
            && self.kind.is_none()
            && self.min_size.is_none()
            && self.max_size.is_none()
            && self.dates.is_none()
            && self.scopes.is_empty()
            && self.negated_scopes.is_empty()
    }

    pub fn scopes(&self) -> &[String] {
        &self.scopes
    }

    pub fn negated_scopes(&self) -> &[String] {
        &self.negated_scopes
    }

    /// True when every entry matches apart from scopes, so a volume the
    /// scopes cover can be listed whole without looking at its entries.
    pub fn matches_everything(&self) -> bool {
        self.terms.is_empty()
            && self.negated.is_empty()
            && self.path_terms.is_empty()
            && self.negated_path_terms.is_empty()
            && self.browse.is_none()
            && self.extensions.is_empty()
            && self.kind.is_none()
            && self.min_size.is_none()
            && self.max_size.is_none()
            && self.dates.is_none()
    }

    /// True when the query filters by modification date. Matching then needs
    /// each entry's mtime, which [`Query::matches_name`] does not see.
    pub fn has_dates(&self) -> bool {
        self.dates.is_some()
    }

    /// The modification-date bounds, as FILETIMEs: start included, end not.
    pub fn dates(&self) -> Option<DateRange> {
        self.dates
    }

    /// True when `mtime` (a FILETIME) passes the date filters. Folders are
    /// filtered by their own mtime, like files and like Everything does; an
    /// unknown mtime of 0 matches no date filter.
    #[inline]
    pub fn matches_mtime(&self, mtime: u64) -> bool {
        self.dates.is_none_or(|range| range.contains(mtime))
    }

    /// [`Query::matches_mtime`] for entry `id` of `volume`, reading its mtime
    /// only when there is a date filter. Call it beside
    /// [`Query::matches_name`] wherever entries come from summaries.
    #[inline]
    pub fn matches_date_of(&self, volume: &VolumeIndex, id: u32) -> bool {
        match self.dates {
            None => true,
            Some(range) => range.contains(volume.entry_mtime(id)),
        }
    }

    /// True when matching depends on the resolved path. The search worker uses
    /// this to skip path reconstruction for ordinary name queries.
    pub fn needs_path(&self) -> bool {
        !self.scopes.is_empty()
            || !self.negated_scopes.is_empty()
            || !self.path_terms.is_empty()
            || !self.negated_path_terms.is_empty()
    }

    /// How the path conditions apply to a whole volume whose root is
    /// `prefix`: `None` when nothing on it can match, `Some(false)` when
    /// everything on it passes without looking at paths, and `Some(true)`
    /// when each entry's path must be checked. This lets a search limited
    /// to `E:\` skip `C:` entirely and never build a path on `E:`.
    pub fn volume_paths(&self, prefix: &str) -> Option<bool> {
        if prefix.trim().is_empty() {
            return Some(self.needs_path());
        }
        let mut check = !self.path_terms.is_empty() || !self.negated_path_terms.is_empty();
        for scope in &self.scopes {
            if path_within(prefix, scope) {
                // The scope covers the whole volume.
                continue;
            }
            if path_within(scope, prefix) {
                check = true;
            } else {
                return None;
            }
        }
        for scope in &self.negated_scopes {
            if path_within(prefix, scope) {
                return None;
            }
            if path_within(scope, prefix) {
                check = true;
            }
        }
        Some(check)
    }

    pub fn matches_path(&self, path: &str) -> bool {
        if !(self.scopes.iter().all(|scope| path_within(path, scope))
            && !self
                .negated_scopes
                .iter()
                .any(|scope| path_within(path, scope)))
        {
            return false;
        }
        if self.path_terms.is_empty() && self.negated_path_terms.is_empty() {
            return true;
        }
        let lower = path.to_lowercase();
        self.path_terms
            .iter()
            .all(|needle| needle.matches(&lower, MatchMode::Anywhere))
            && !self
                .negated_path_terms
                .iter()
                .any(|needle| needle.matches(&lower, MatchMode::Anywhere))
    }

    pub fn matches(&self, name_lower: &str, entry: &EntryView<'_>) -> bool {
        self.matches_name(name_lower, entry.size, entry.flags) && self.matches_mtime(entry.mtime)
    }

    /// Name matching from the raw fields, without building an [`EntryView`].
    /// This is the hot path: the search worker reads only the metadata that
    /// filters actually use. Date filters are not checked here, since the
    /// summary has no mtime: pair it with [`Query::matches_date_of`].
    #[inline]
    pub fn matches_name(&self, name_lower: &str, size: u64, flags: u16) -> bool {
        let is_dir = flags & 1 != 0;
        match self.kind {
            Some(Kind::File) if is_dir => return false,
            Some(Kind::Folder) if !is_dir => return false,
            _ => {}
        }

        if !self.extensions.is_empty() {
            if is_dir {
                return false;
            }
            if !self
                .extensions
                .iter()
                .any(|extension| name_lower.ends_with(extension.as_str()))
            {
                return false;
            }
        }

        if let Some(min) = self.min_size {
            if size < min {
                return false;
            }
        }
        if let Some(max) = self.max_size {
            if size > max {
                return false;
            }
        }

        let mode = self.match_mode;
        if !self
            .terms
            .iter()
            .all(|needle| needle.matches(name_lower, mode))
        {
            return false;
        }
        if self
            .negated
            .iter()
            .any(|needle| needle.matches(name_lower, mode))
        {
            return false;
        }
        true
    }

    /// True when a name filter is the whole query: no kind, size, extension,
    /// date, or path work is needed. The worker uses this to skip reading
    /// metadata.
    pub fn name_only(&self) -> bool {
        self.kind.is_none()
            && self.extensions.is_empty()
            && self.min_size.is_none()
            && self.max_size.is_none()
            && self.dates.is_none()
            && !self.needs_path()
            && self.browse.is_none()
    }

    /// The ranges of `name`, in UTF-16 code units, that the typed name terms
    /// matched. Only plain literals are highlighted; wildcard terms and names
    /// that are not ASCII are left plain rather than risk misaligned ranges.
    pub fn match_spans(&self, name: &str) -> Vec<(usize, usize)> {
        let mut spans = Vec::new();
        if self.terms.is_empty() || !name.is_ascii() {
            return spans;
        }
        for needle in &self.terms {
            let Needle::Literal(literal) = needle else {
                continue;
            };
            match self.match_mode {
                MatchMode::Anywhere => literal.find_all_ascii(name, &mut spans),
                MatchMode::StartsWith => {
                    if literal.edge_ascii(name, MatchMode::StartsWith) {
                        spans.push((0, literal.text.len()));
                    }
                }
                MatchMode::EndsWith => {
                    if literal.edge_ascii(name, MatchMode::EndsWith) {
                        spans.push((name.len() - literal.text.len(), name.len()));
                    }
                }
            }
        }
        merge_spans(spans)
    }
}

/// Sorts and joins touching or overlapping highlight ranges.
fn merge_spans(mut spans: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    if spans.len() < 2 {
        return spans;
    }
    spans.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(spans.len());
    for (start, end) in spans {
        if start >= end {
            continue;
        }
        if let Some(last) = merged.last_mut() {
            if start <= last.1 {
                last.1 = last.1.max(end);
                continue;
            }
        }
        merged.push((start, end));
    }
    merged
}

fn needle_for(lower: String) -> Needle {
    if lower.contains('*') || lower.contains('?') {
        Needle::Pattern(lower.chars().collect())
    } else {
        Needle::Literal(Literal::new(lower))
    }
}

impl Browse {
    fn new(lower: &str) -> Self {
        let folder = normalize_path(lower);
        if lower.ends_with('\\') {
            return Browse {
                folder,
                parent: None,
                name: Needle::Literal(Literal::new(String::new())),
            };
        }
        let (parent, name) = match lower.rfind('\\') {
            Some(at) => (normalize_path(&lower[..at]), &lower[at + 1..]),
            None => (String::new(), lower),
        };
        // `\\server` alone has no folder above it to look in.
        let parent = (!parent.is_empty() && parent != "\\").then_some(parent);
        Browse {
            folder,
            parent,
            name: needle_for(name.to_string()),
        }
    }
}

fn tokenize(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for ch in input.chars() {
        match ch {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn parse_size_filter(input: &str) -> Option<(Option<u64>, Option<u64>)> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }
    if let Some((left, right)) = input.split_once("..") {
        let min = if left.trim().is_empty() {
            None
        } else {
            Some(parse_size_value(left)?)
        };
        let max = if right.trim().is_empty() {
            None
        } else {
            Some(parse_size_value(right)?)
        };
        return Some((min, max));
    }
    let (operator, rest) = if let Some(rest) = input.strip_prefix(">=") {
        (">=", rest)
    } else if let Some(rest) = input.strip_prefix("<=") {
        ("<=", rest)
    } else if let Some(rest) = input.strip_prefix('>') {
        (">", rest)
    } else if let Some(rest) = input.strip_prefix('<') {
        ("<", rest)
    } else if let Some(rest) = input.strip_prefix('=') {
        ("=", rest)
    } else {
        ("=", input)
    };
    let value = parse_size_value(rest)?;
    Some(match operator {
        ">=" => (Some(value), None),
        ">" => (Some(value.saturating_add(1)), None),
        "<=" => (None, Some(value)),
        "<" => (None, Some(value.saturating_sub(1))),
        _ => (Some(value), Some(value)),
    })
}

pub fn parse_size_value(input: &str) -> Option<u64> {
    let input = input.trim().to_lowercase();
    let split = input
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(input.len());
    if split == 0 {
        return None;
    }
    let number: f64 = input[..split].parse().ok()?;
    let unit = input[split..].trim();
    let multiplier = match unit {
        "" | "b" => 1u64,
        "kb" | "k" => 1024,
        "mb" | "m" => 1024 * 1024,
        "gb" | "g" => 1024 * 1024 * 1024,
        "tb" | "t" => 1024u64 * 1024 * 1024 * 1024,
        _ => return None,
    };
    let value = number * multiplier as f64;
    if !value.is_finite() || value < 0.0 || value > u64::MAX as f64 {
        None
    } else {
        Some(value as u64)
    }
}

pub fn glob_match(pattern: &str, text: &str) -> bool {
    let chars: Vec<char> = pattern.chars().collect();
    glob_match_chars(&chars, text)
}

/// Matches wildcards against whole code points without allocating the text.
fn glob_match_chars(chars: &[char], text: &str) -> bool {
    let mut text_at = text.chars();
    let mut pattern_at = 0usize;
    // Where to resume after a `*`: the pattern position after it and the text
    // position the star is currently trying to end at.
    let mut star: Option<(usize, std::str::Chars<'_>)> = None;

    loop {
        if pattern_at < chars.len() {
            match chars[pattern_at] {
                '*' => {
                    pattern_at += 1;
                    star = Some((pattern_at, text_at.clone()));
                    continue;
                }
                '?' => {
                    if text_at.next().is_some() {
                        pattern_at += 1;
                        continue;
                    }
                }
                wanted => {
                    if text_at.clone().next() == Some(wanted) {
                        text_at.next();
                        pattern_at += 1;
                        continue;
                    }
                }
            }
        } else if text_at.clone().next().is_none() {
            return true;
        }

        match &mut star {
            Some((star_pattern, retry)) => {
                if retry.next().is_none() {
                    return false;
                }
                text_at = retry.clone();
                pattern_at = *star_pattern;
            }
            None => return false,
        }
    }
}

impl Needle {
    fn same(&self, other: &Needle) -> bool {
        match (self, other) {
            (Needle::Literal(a), Needle::Literal(b)) => a.text == b.text,
            (Needle::Pattern(a), Needle::Pattern(b)) => a == b,
            _ => false,
        }
    }

    /// True when every name this term matches also matches `wider`.
    fn within(&self, wider: &Needle, mode: MatchMode) -> bool {
        match (self, wider) {
            (Needle::Literal(narrow), Needle::Literal(wide)) => match mode {
                MatchMode::Anywhere => narrow.text.contains(wide.text.as_str()),
                MatchMode::StartsWith => narrow.text.starts_with(wide.text.as_str()),
                MatchMode::EndsWith => narrow.text.ends_with(wide.text.as_str()),
            },
            _ => self.same(wider),
        }
    }
}

impl Query {
    /// True when everything this query matches, `previous` matched too, as
    /// when a word is typed further: `repo` after `rep`. The results of
    /// `previous` can then be filtered instead of searching the index again.
    pub fn narrows(&self, previous: &Query) -> bool {
        let covered = |narrow: &[Needle], wide: &[Needle], mode: MatchMode| {
            wide.iter()
                .all(|wide| narrow.iter().any(|term| term.within(wide, mode)))
        };
        let kept = |now: &[Needle], before: &[Needle]| {
            before
                .iter()
                .all(|old| now.iter().any(|term| term.same(old)))
        };
        self.browse.is_none()
            && previous.browse.is_none()
            && self.extensions == previous.extensions
            && self.kind == previous.kind
            && self.min_size == previous.min_size
            && self.max_size == previous.max_size
            && self
                .dates
                .unwrap_or(DateRange::ALL)
                .within(&previous.dates.unwrap_or(DateRange::ALL))
            && self.scopes == previous.scopes
            && self.negated_scopes == previous.negated_scopes
            && self.match_mode == previous.match_mode
            && covered(&self.terms, &previous.terms, self.match_mode)
            && covered(&self.path_terms, &previous.path_terms, MatchMode::Anywhere)
            && kept(&self.negated, &previous.negated)
            && kept(&self.negated_path_terms, &previous.negated_path_terms)
    }
}

/// What a folder's path decides for every entry directly inside it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Verdict {
    Reject,
    Accept,
    /// Decided by how the name starts: it must start with the last part of
    /// each `need` term and with none of the `forbid` terms.
    Names {
        need: u32,
        forbid: u32,
    },
    /// Only the whole path can tell.
    Full,
}

fn term_text(needle: &Needle) -> &str {
    match needle {
        Needle::Literal(literal) => &literal.text,
        Needle::Pattern(_) => "",
    }
}

/// A folder's path as far as path terms care: which terms it contains and
/// its last few characters, enough for a term that runs on into a name.
struct Folder {
    found: u32,
    found_negated: u32,
    tail: Box<str>,
    verdict: Verdict,
}

/// [`Query::matches_path`] for one volume, worked out once per folder
/// instead of once per entry. Building every entry's full path makes a
/// query like `docs\report` read millions of paths; entries share folders,
/// and a folder's path alone almost always settles the match.
pub struct PathCheck<'q> {
    query: &'q Query,
    /// Each folder from its parent's state, without building paths: only
    /// for plain path terms. Otherwise each folder's path is built once.
    chained: bool,
    tail_len: usize,
    folders: std::collections::HashMap<u64, Folder>,
}

impl<'q> PathCheck<'q> {
    pub fn new(query: &'q Query) -> Self {
        let literal = |needle: &Needle| matches!(needle, Needle::Literal(_));
        let chained = crate::path::SEP == '\\'
            && query.scopes.is_empty()
            && query.negated_scopes.is_empty()
            && query.path_terms.len() <= 32
            && query.negated_path_terms.len() <= 32
            && query.path_terms.iter().all(literal)
            && query.negated_path_terms.iter().all(literal);
        let tail_len = query
            .path_terms
            .iter()
            .chain(&query.negated_path_terms)
            .map(|needle| match needle {
                Needle::Literal(literal) => literal.text.len(),
                Needle::Pattern(_) => 0,
            })
            .max()
            .unwrap_or(0);
        PathCheck {
            query,
            chained,
            tail_len,
            folders: std::collections::HashMap::new(),
        }
    }

    /// True when the entry `name_search` inside folder `parent_frn` of
    /// `volume` passes the query's path conditions. `full` builds the
    /// entry's own path, for the few cases the folder can't decide.
    pub fn matches(
        &mut self,
        volume: &VolumeIndex,
        parent_frn: u64,
        name_search: &str,
        full: impl FnOnce() -> Option<String>,
    ) -> bool {
        let verdict = self.verdict(volume, parent_frn);
        match verdict {
            Verdict::Reject => false,
            Verdict::Accept => true,
            Verdict::Names { need, forbid } => {
                let starts = |terms: &[Needle], index: usize| match &terms[index] {
                    Needle::Literal(literal) => {
                        let last = literal.text.rsplit('\\').next().unwrap_or("");
                        name_search.starts_with(last)
                    }
                    Needle::Pattern(_) => false,
                };
                let query = self.query;
                (0..query.path_terms.len())
                    .filter(|index| need & (1 << index) != 0)
                    .all(|index| starts(&query.path_terms, index))
                    && !(0..query.negated_path_terms.len())
                        .filter(|index| forbid & (1 << index) != 0)
                        .any(|index| starts(&query.negated_path_terms, index))
            }
            Verdict::Full => full().is_some_and(|path| self.query.matches_path(&path)),
        }
    }

    fn verdict(&mut self, volume: &VolumeIndex, frn: u64) -> Verdict {
        if let Some(folder) = self.folders.get(&frn) {
            return folder.verdict;
        }
        if !self.chained {
            let verdict = volume
                .lookup_directory_frn(frn)
                .and_then(|id| volume.resolve_path(id))
                .map_or(Verdict::Full, |path| self.query.folder_verdict(&path));
            self.folders.insert(
                frn,
                Folder {
                    found: 0,
                    found_negated: 0,
                    tail: Box::from(""),
                    verdict,
                },
            );
            return verdict;
        }

        // Walk up to the nearest folder already known, then fill in each
        // one on the way back down from its parent.
        let mut chain = Vec::new();
        let mut current = frn;
        let mut known = None;
        for _ in 0..128 {
            if self.folders.contains_key(&current) {
                known = Some(current);
                break;
            }
            let Some(id) = volume.lookup_directory_frn(current) else {
                break;
            };
            let entry = volume.entry(id);
            chain.push(current);
            if entry.id == 0 || entry.parent_frn == entry.frn {
                break;
            }
            current = entry.parent_frn;
        }
        let mut parent = known;
        for &frn in chain.iter().rev() {
            let id = volume.lookup_directory_frn(frn);
            let folder = match (parent, id) {
                (_, None) => self.unknown(),
                (None, Some(id)) => {
                    let entry = volume.entry(id);
                    if entry.id == 0 || entry.parent_frn == entry.frn {
                        // The root: its path is the volume prefix.
                        let root = volume.resolve_path(id).unwrap_or_default().to_lowercase();
                        self.folder(0, 0, "", root.trim_end_matches('\\'))
                    } else {
                        self.unknown()
                    }
                }
                (Some(parent), Some(id)) => {
                    let name = volume.entry(id).name_search;
                    let up = &self.folders[&parent];
                    let (found, negated, tail) = (up.found, up.found_negated, up.tail.clone());
                    if up.verdict == Verdict::Full {
                        self.unknown()
                    } else {
                        self.folder(found, negated, &tail, &format!("\\{name}"))
                    }
                }
            };
            self.folders.insert(frn, folder);
            parent = Some(frn);
        }
        self.folders
            .get(&frn)
            .map_or(Verdict::Full, |folder| folder.verdict)
    }

    fn unknown(&self) -> Folder {
        Folder {
            found: 0,
            found_negated: 0,
            tail: Box::from(""),
            verdict: Verdict::Full,
        }
    }

    /// The folder whose path is its parent's followed by `rest`.
    fn folder(&self, found: u32, found_negated: u32, tail: &str, rest: &str) -> Folder {
        let joined = format!("{tail}{rest}");
        let mut found = found;
        let mut found_negated = found_negated;
        for (index, term) in self.query.path_terms.iter().enumerate() {
            if joined.contains(term_text(term)) {
                found |= 1 << index;
            }
        }
        for (index, term) in self.query.negated_path_terms.iter().enumerate() {
            if joined.contains(term_text(term)) {
                found_negated |= 1 << index;
            }
        }
        let mut start = joined.len().saturating_sub(self.tail_len);
        while !joined.is_char_boundary(start) {
            start -= 1;
        }
        let tail = &joined[start..];

        let mut verdict = None;
        let mut need = 0u32;
        for (index, term) in self.query.path_terms.iter().enumerate() {
            if found & (1 << index) != 0 {
                continue;
            }
            match term_text(term).rsplit_once('\\') {
                Some((head, _)) if tail.ends_with(head) => need |= 1 << index,
                _ => verdict = Some(Verdict::Reject),
            }
        }
        let mut forbid = 0u32;
        for (index, term) in self.query.negated_path_terms.iter().enumerate() {
            if found_negated & (1 << index) != 0 {
                verdict = Some(Verdict::Reject);
            } else if let Some((head, _)) = term_text(term).rsplit_once('\\') {
                if tail.ends_with(head) {
                    forbid |= 1 << index;
                }
            }
        }
        let verdict = verdict.unwrap_or(if need == 0 && forbid == 0 {
            Verdict::Accept
        } else {
            Verdict::Names { need, forbid }
        });
        Folder {
            found,
            found_negated,
            tail: Box::from(tail),
            verdict,
        }
    }
}

impl Query {
    /// What the path of `folder` decides for the entries directly inside it.
    fn folder_verdict(&self, folder: &str) -> Verdict {
        // Paths join with a backslash only on Windows; a root already ends
        // with one. Both are rare enough to check entry by entry.
        if crate::path::SEP != '\\' || folder.ends_with('\\') {
            return Verdict::Full;
        }
        if self.path_terms.len() > 32 || self.negated_path_terms.len() > 32 {
            return Verdict::Full;
        }
        let lower = folder.to_lowercase();
        let normalized = normalize_path(folder);
        // An entry is inside a scope when its folder is, or when it is the
        // scope itself, which only a name can tell.
        let names_scope = |scope: &str| {
            scope
                .strip_prefix(normalized.as_str())
                .and_then(|rest| rest.strip_prefix('\\'))
                .is_some_and(|rest| !rest.is_empty() && !rest.contains('\\'))
        };
        let mut full = false;
        for scope in &self.scopes {
            if !path_within(&normalized, scope) {
                if names_scope(scope) {
                    full = true;
                } else {
                    return Verdict::Reject;
                }
            }
        }
        for scope in &self.negated_scopes {
            if path_within(&normalized, scope) {
                return Verdict::Reject;
            }
            if names_scope(scope) {
                full = true;
            }
        }
        if full {
            return Verdict::Full;
        }

        // A term with a backslash is found in the folder's path, or runs
        // from its end into the name: the folder ends with the part before
        // the term's last backslash, and the name starts with the rest.
        let mut need = 0u32;
        for (index, term) in self.path_terms.iter().enumerate() {
            let Needle::Literal(literal) = term else {
                return Verdict::Full;
            };
            if lower.contains(literal.text.as_str()) {
                continue;
            }
            match literal.text.rsplit_once('\\') {
                Some((head, _)) if lower.ends_with(head) => need |= 1 << index,
                _ => return Verdict::Reject,
            }
        }
        let mut forbid = 0u32;
        for (index, term) in self.negated_path_terms.iter().enumerate() {
            let Needle::Literal(literal) = term else {
                return Verdict::Full;
            };
            if lower.contains(literal.text.as_str()) {
                return Verdict::Reject;
            }
            if let Some((head, _)) = literal.text.rsplit_once('\\') {
                if lower.ends_with(head) {
                    forbid |= 1 << index;
                }
            }
        }
        if need == 0 && forbid == 0 {
            Verdict::Accept
        } else {
            Verdict::Names { need, forbid }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Uses drive letters and backslashes: Windows paths.
    #[cfg(windows)]
    #[test]
    fn scopes_rule_out_or_cover_whole_volumes() {
        let drive = Query::parse(r#"in:"E:\""#);
        assert_eq!(drive.volume_paths("C:"), None);
        assert_eq!(drive.volume_paths("E:"), Some(false));
        assert_eq!(drive.volume_paths(r"E:\"), Some(false));
        let folder = Query::parse(r#"report in:"E:\Photos""#);
        assert_eq!(folder.volume_paths("C:"), None);
        assert_eq!(folder.volume_paths("E:"), Some(true));
        let outside = Query::parse(r"!in:C:\Windows");
        assert_eq!(outside.volume_paths("E:"), Some(false));
        assert_eq!(outside.volume_paths("C:"), Some(true));
        assert_eq!(Query::parse("cat").volume_paths("C:"), Some(false));
        assert_eq!(Query::parse(r"docs\cat").volume_paths("C:"), Some(true));
    }

    #[test]
    fn tokenizes_quotes() {
        assert_eq!(
            tokenize(r#"foo "bar baz" qux"#),
            vec!["foo", "bar baz", "qux"]
        );
    }

    #[test]
    fn parses_size_filters() {
        assert_eq!(
            parse_size_filter(">10mb"),
            Some((Some(10 * 1024 * 1024 + 1), None))
        );
        assert_eq!(
            parse_size_filter("1kb..2kb"),
            Some((Some(1024), Some(2048)))
        );
        assert_eq!(parse_size_filter("0"), Some((Some(0), Some(0))));
        assert_eq!(parse_size_filter("nope"), None);
    }

    #[test]
    fn size_filters_match_folders_by_their_totals() {
        // A folder's size field holds the total below it, so it is matched
        // like a file of that size (as Everything does); type:file keeps a
        // size filter to files.
        let big = Query::parse("size:>1gb");
        assert!(big.matches_name("photos", 5 << 30, 1));
        assert!(!big.matches_name("empty", 0, 1));
        assert!(big.matches_name("movie.mkv", 2 << 30, 0));
        let files = Query::parse("type:file size:>1gb");
        assert!(!files.matches_name("photos", 5 << 30, 1));
        assert!(files.matches_name("movie.mkv", 2 << 30, 0));
    }

    #[test]
    fn combines_multiple_size_tokens() {
        let query = Query::parse("size:>=1kb size:<=2kb");
        assert_eq!(query.min_size, Some(1024));
        assert_eq!(query.max_size, Some(2048));

        let narrowed = Query::parse("size:1kb..4kb size:>=2kb size:<=3kb");
        assert_eq!(narrowed.min_size, Some(2048));
        assert_eq!(narrowed.max_size, Some(3072));
    }

    // Uses drive letters and backslashes: Windows paths.
    #[cfg(windows)]
    #[test]
    fn parses_search_scopes() {
        let query = Query::parse(r#"report in:"C:\Users\Me\Documents" !in:C:\Temp"#);
        assert_eq!(query.scopes(), &[r"c:\users\me\documents"]);
        assert_eq!(query.negated_scopes(), &[r"c:\temp"]);
        assert!(query.needs_path());
        assert!(query.matches_path(r"C:\Users\Me\Documents\report.pdf"));
        assert!(!query.matches_path(r"C:\Users\Me\Documents2\report.pdf"));
        assert!(!query.matches_path(r"C:\Temp\report.pdf"));
        assert!(!query.is_empty());
    }

    // Uses drive letters and backslashes: Windows paths.
    #[cfg(windows)]
    #[test]
    fn a_typed_folder_lists_its_contents() {
        let query = Query::parse(r"C:\Users\");
        let browse = query.browse.as_ref().unwrap();
        assert_eq!(browse.folder, r"c:\users");
        assert!(browse.parent.is_none());
        assert!(!query.needs_path());

        let partial = Query::parse("c:/users/ad");
        let browse = partial.browse.as_ref().unwrap();
        assert_eq!(browse.folder, r"c:\users\ad");
        assert_eq!(browse.parent.as_deref(), Some(r"c:\users"));
        assert!(browse.name.matches("admin", MatchMode::Anywhere));

        let drive = Query::parse(r"C:\Us");
        assert_eq!(drive.browse.as_ref().unwrap().parent.as_deref(), Some("c:"));
    }

    // Uses drive letters and backslashes: Windows paths.
    #[cfg(windows)]
    #[test]
    fn a_folder_with_words_searches_inside_it() {
        let query = Query::parse(r"C:\Users\ report");
        assert!(query.browse.is_none());
        assert_eq!(query.scopes(), &[r"c:\users"]);
    }

    #[test]
    fn partial_paths_match_anywhere_in_the_path() {
        let query = Query::parse(r"users\me pdf !\temp\");
        assert!(query.needs_path());
        assert!(query.matches_path(r"C:\Users\Me\a.pdf"));
        assert!(!query.matches_path(r"D:\Other\a.pdf"));
        assert!(!query.matches_path(r"C:\Users\Me\Temp\a.pdf"));
    }

    #[test]
    fn glob_matches_codepoints() {
        assert!(glob_match("*.pdf", "report.pdf"));
        assert!(glob_match("file?txt", "file1txt"));
        assert!(!glob_match("file?txt", "file12txt"));
        assert!(glob_match("a*b*c", "aXXbYYc"));
        assert!(glob_match("*", ""));
        assert!(glob_match("*", "anything"));
        assert!(!glob_match("a*", "b"));
        assert!(glob_match("?*", "\u{1F600}x"));
        assert!(glob_match("a*b*c", "a\u{e9}b\u{e9}c"));
    }

    #[test]
    fn match_modes_change_where_a_word_may_appear() {
        let anywhere = Query::parse("report");
        assert_eq!(anywhere.match_mode(), MatchMode::Anywhere);
        assert!(anywhere.matches_name("annual-report.pdf", 0, 0));
        assert!(anywhere.matches_name("report.pdf", 0, 0));

        let starts = Query::parse("match:starts report");
        assert_eq!(starts.match_mode(), MatchMode::StartsWith);
        assert!(starts.matches_name("report.pdf", 0, 0));
        assert!(!starts.matches_name("annual-report.pdf", 0, 0));

        let ends = Query::parse("mode:ends .pdf");
        assert_eq!(ends.match_mode(), MatchMode::EndsWith);
        assert!(ends.matches_name("report.pdf", 0, 0));
        assert!(!ends.matches_name("report.pdf.bak", 0, 0));

        assert_eq!(Query::parse("match:nonsense").warnings().len(), 1);
        assert_eq!(MatchMode::parse("Starts_With"), Some(MatchMode::StartsWith));
        assert_eq!(MatchMode::parse("suffix"), Some(MatchMode::EndsWith));
    }

    #[test]
    fn match_spans_mark_the_matching_text() {
        let query = Query::parse("port pdf");
        assert_eq!(query.match_spans("Report.pdf"), vec![(2, 6), (7, 10)]);

        let starts = Query::parse("match:starts rep");
        assert_eq!(starts.match_spans("report.pdf"), vec![(0, 3)]);
        assert!(starts.match_spans("annual-report.pdf").is_empty());

        let ends = Query::parse("match:ends pdf");
        assert_eq!(ends.match_spans("report.pdf"), vec![(7, 10)]);

        // Overlapping terms merge into one range.
        let overlapping = Query::parse("report port");
        assert_eq!(overlapping.match_spans("report.pdf"), vec![(0, 6)]);

        // Wildcards are not highlighted, and non-ASCII names stay plain.
        assert!(Query::parse("*.pdf").match_spans("report.pdf").is_empty());
        assert!(Query::parse("port")
            .match_spans("r\u{e9}port.pdf")
            .is_empty());
        assert!(Query::parse("").match_spans("report.pdf").is_empty());
    }

    #[test]
    fn folder_verdicts_agree_with_whole_paths() {
        let queries = [
            r"testbox\passwords.txt",
            r"\passwords",
            r"machines\testbox\",
            r"users\admin docs",
            r"virtual machines\test !box\pass",
            r"report in:c:\users\admin\documents",
            r"report !in:c:\users\admin\documents\virtual",
            r"in:c:\users\admin\documents\virtual",
            r"docs\*.txt",
            r"test\box",
        ];
        let folders = [
            r"C:\Users\admin\Documents\Virtual Machines\Testbox",
            r"C:\Users\admin\Documents\Virtual Machines",
            r"C:\Users\admin\Documents",
            r"C:\Users\admin\Docs",
            r"C:\Testbox",
            r"C:\Test",
        ];
        let names = [
            "passwords.txt",
            "testbox",
            "report.pdf",
            "box",
            "other",
            "virtual",
        ];
        for text in queries {
            let query = Query::parse(text);
            for folder in folders {
                let verdict = query.folder_verdict(folder);
                for name in names {
                    let path = format!("{folder}\\{name}");
                    let expected = query.matches_path(&path);
                    let got = match verdict {
                        Verdict::Reject => false,
                        Verdict::Accept => true,
                        Verdict::Full => expected,
                        Verdict::Names { need, forbid } => {
                            let starts = |terms: &[Needle], index: usize| match &terms[index] {
                                Needle::Literal(literal) => {
                                    name.starts_with(literal.text.rsplit('\\').next().unwrap())
                                }
                                Needle::Pattern(_) => false,
                            };
                            (0..query.path_terms.len())
                                .filter(|i| need & (1 << i) != 0)
                                .all(|i| starts(&query.path_terms, i))
                                && !(0..query.negated_path_terms.len())
                                    .filter(|i| forbid & (1 << i) != 0)
                                    .any(|i| starts(&query.negated_path_terms, i))
                        }
                    };
                    assert_eq!(got, expected, "{text} / {path} / {verdict:?}");
                }
            }
        }
    }

    #[test]
    fn typing_further_narrows() {
        let narrows = |now: &str, before: &str| Query::parse(now).narrows(&Query::parse(before));
        assert!(narrows("repo", "rep"));
        assert!(narrows("rep", "rep"));
        assert!(narrows("report pdf", "rep"));
        assert!(narrows("rep !old", "rep"));
        assert!(narrows(r"docs\rep", r"docs\re"));
        assert!(narrows("ext:pdf rep", "ext:pdf re"));
        assert!(!narrows("re", "rep"));
        assert!(!narrows("rep", "rep !old"));
        assert!(!narrows("ext:pdf rep", "rep"));
        assert!(!narrows("abc", "xyz"));
        // A typed drive path browses a folder; only Windows has those.
        #[cfg(windows)]
        assert!(!narrows(r"c:\users\re", r"c:\users\r"));
        assert!(!narrows("*.pd", "*.p"));
        assert!(narrows("match:start report", "match:start rep"));
        assert!(!narrows("match:start xrep", "match:start rep"));
        assert!(narrows("match:end report.pdf", "match:end .pdf"));
    }

    mod dates {
        use super::*;
        use crate::date::filetime_utc;

        /// Wednesday 2026-11-11 14:30 local time, UTC+2.
        fn clock() -> Clock {
            Clock::fixed(filetime_utc(2026, 11, 11, 12, 30), 120)
        }

        fn parse(text: &str) -> Query {
            Query::parse_at(text, &clock())
        }

        /// Local midnight at UTC+2 as a FILETIME.
        fn local(year: i64, month: i64, day: i64) -> u64 {
            filetime_utc(year, month, day, 0, 0) - 2 * 3600 * 10_000_000
        }

        fn bounds(text: &str) -> (u64, u64) {
            let query = parse(text);
            assert!(
                query.warnings().is_empty(),
                "{text}: {:?}",
                query.warnings()
            );
            let range = query
                .dates()
                .unwrap_or_else(|| panic!("{text} has no dates"));
            (range.start, range.end)
        }

        #[test]
        fn every_form_parses() {
            let cases: &[(&str, (u64, u64))] = &[
                ("dm:today", (local(2026, 11, 11), local(2026, 11, 12))),
                ("dm:yesterday", (local(2026, 11, 10), local(2026, 11, 11))),
                ("dm:thisweek", (local(2026, 11, 9), local(2026, 11, 16))),
                ("dm:lastweek", (local(2026, 11, 2), local(2026, 11, 9))),
                ("dm:thismonth", (local(2026, 11, 1), local(2026, 12, 1))),
                ("dm:lastmonth", (local(2026, 10, 1), local(2026, 11, 1))),
                ("dm:thisyear", (local(2026, 1, 1), local(2027, 1, 1))),
                ("dm:lastyear", (local(2025, 1, 1), local(2026, 1, 1))),
                ("dm:last7days", (local(2026, 11, 5), u64::MAX)),
                ("dm:last30days", (local(2026, 10, 13), u64::MAX)),
                ("dm:7days", (local(2026, 11, 5), u64::MAX)),
                ("dm:2hours", (filetime_utc(2026, 11, 11, 10, 30), u64::MAX)),
                (
                    "dm:last24hours",
                    (filetime_utc(2026, 11, 10, 12, 30), u64::MAX),
                ),
                ("dm:2026", (local(2026, 1, 1), local(2027, 1, 1))),
                ("dm:2026-11", (local(2026, 11, 1), local(2026, 12, 1))),
                ("dm:2026-11-11", (local(2026, 11, 11), local(2026, 11, 12))),
                (
                    "dm:2026-01-01..2026-03-31",
                    (local(2026, 1, 1), local(2026, 4, 1)),
                ),
                ("dm:>2026-01-01", (local(2026, 1, 2), u64::MAX)),
                ("dm:>=2026-01-01", (local(2026, 1, 1), u64::MAX)),
                ("dm:<2025", (0, local(2025, 1, 1))),
                ("dm:<=2025", (0, local(2026, 1, 1))),
                ("dm:..2026-06", (0, local(2026, 7, 1))),
                ("dm:2026-06..", (local(2026, 6, 1), u64::MAX)),
                ("date:today", (local(2026, 11, 11), local(2026, 11, 12))),
                ("DM:Today", (local(2026, 11, 11), local(2026, 11, 12))),
            ];
            for (text, expected) in cases {
                assert_eq!(bounds(text), *expected, "{text}");
                let query = parse(text);
                assert!(query.has_dates());
                assert!(!query.is_empty());
                assert!(!query.matches_everything());
                assert!(!query.name_only());
                assert!(!query.needs_path());
            }
        }

        #[test]
        fn bounds_include_the_start_and_exclude_the_end() {
            let query = parse("dm:2026-11-11");
            let (start, end) = (local(2026, 11, 11), local(2026, 11, 12));
            assert!(!query.matches_mtime(start - 1));
            assert!(query.matches_mtime(start));
            assert!(query.matches_mtime(end - 1));
            assert!(!query.matches_mtime(end));
            // Unknown dates never match a date filter...
            assert!(!parse("dm:<2025").matches_mtime(0));
            // ...but do match without one.
            assert!(parse("report").matches_mtime(0));
        }

        #[test]
        fn date_tokens_and_together() {
            assert_eq!(
                bounds("dm:2026 dm:<2026-07 date:>=2026-03"),
                (local(2026, 3, 1), local(2026, 7, 1))
            );
            let disjoint = parse("dm:2025 dm:2026");
            assert!(disjoint.warnings().is_empty());
            assert!(!disjoint.matches_mtime(local(2025, 6, 1)));
            assert!(!disjoint.matches_mtime(local(2026, 6, 1)));
        }

        #[test]
        fn date_filters_combine_with_names() {
            let query = parse("report ext:pdf dm:today");
            let noon = local(2026, 11, 11) + 12 * 3600 * 10_000_000;
            assert!(query.matches_name("report.pdf", 0, 0));
            assert!(query.matches_mtime(noon));
            assert!(!query.matches_mtime(local(2026, 11, 10)));
            assert_eq!(query.match_spans("report.pdf"), vec![(0, 6)]);
        }

        #[test]
        fn invalid_and_negated_dates_warn() {
            for text in [
                "dm:soon",
                "dm:2026-13",
                "date:",
                "dm:2026-02-30",
                "dm:..",
                "dm:>",
            ] {
                let query = parse(text);
                assert_eq!(query.warnings().len(), 1, "{text}");
                assert!(query.warnings()[0].starts_with("invalid date"), "{text}");
                assert!(!query.has_dates(), "{text}");
            }
            let negated = parse("!dm:today");
            assert_eq!(negated.warnings().len(), 1);
            assert!(negated.warnings()[0].contains("negated"));
            assert!(!negated.has_dates());
        }

        #[test]
        fn the_system_clock_is_used_by_default() {
            let query = Query::parse("dm:today");
            assert!(query.warnings().is_empty());
            let now = crate::date::Clock::system().now;
            assert!(query.matches_mtime(now));
        }

        #[test]
        fn narrower_dates_narrow() {
            let narrows = |now: &str, before: &str| parse(now).narrows(&parse(before));
            assert!(narrows("rep dm:today", "rep"));
            assert!(narrows("rep dm:today", "rep dm:thisweek"));
            assert!(narrows("rep dm:today", "re dm:today"));
            assert!(!narrows("rep dm:thisweek", "rep dm:today"));
            assert!(!narrows("rep", "rep dm:today"));
        }
    }
}
