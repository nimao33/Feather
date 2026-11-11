//! Path comparison helpers shared by query scope and index selection.
//!
//! Windows compares drives and backslash paths; Linux uses one tree rooted at
//! `/` with forward slashes. Matching stays case-insensitive on both, like
//! Everything, while the display path keeps the original case.

/// The separator native paths use.
pub const SEP: char = if cfg!(windows) { '\\' } else { '/' };

/// The root path an index is built under when no prefix is stored.
pub const fn default_prefix() -> &'static str {
    if cfg!(windows) {
        "C:\\"
    } else {
        "/"
    }
}

/// Lowercases a path, uses the native separator, and removes a trailing
/// separator except from a root (`C:\` or `/`).
#[cfg(windows)]
pub fn normalize_path(input: &str) -> String {
    let mut path = input.trim().replace('/', "\\").to_lowercase();
    while path.len() > 3 && path.ends_with('\\') {
        path.pop();
    }
    if path.len() == 3 && path.as_bytes()[1] == b':' && path.ends_with('\\') {
        path.pop();
    }
    path
}

#[cfg(not(windows))]
pub fn normalize_path(input: &str) -> String {
    let lower = input.trim().to_lowercase();
    let mut path = String::with_capacity(lower.len());
    let mut last_slash = false;
    for ch in lower.chars() {
        if ch == '/' {
            if !last_slash {
                path.push('/');
            }
            last_slash = true;
        } else {
            path.push(ch);
            last_slash = false;
        }
    }
    while path.len() > 1 && path.ends_with('/') {
        path.pop();
    }
    path
}

/// True when `path` is the same as `scope` or lives below it. The separator
/// check stops `C:\Foobar` from matching a scope of `C:\Foo`.
pub fn path_within(path: &str, scope: &str) -> bool {
    let path = normalize_path(path);
    let scope = normalize_path(scope);
    if scope.is_empty() {
        return true;
    }
    if path == scope {
        return true;
    }
    if scope == "/" {
        return path.starts_with('/');
    }
    match path.strip_prefix(&scope) {
        Some(rest) => rest.starts_with(SEP),
        None => false,
    }
}

/// A path is usable as a search scope only when it is absolute. Relative
/// paths would silently depend on the current directory.
#[cfg(windows)]
pub fn is_absolute_path(input: &str) -> bool {
    let path = input.trim();
    let bytes = path.as_bytes();
    if bytes.len() >= 3 && bytes[1] == b':' && (bytes[2] == b'\\' || bytes[2] == b'/') {
        return true;
    }
    path.starts_with("\\\\") || path.starts_with("//")
}

#[cfg(not(windows))]
pub fn is_absolute_path(input: &str) -> bool {
    input.trim().starts_with('/')
}

/// True when any path component equals `name`, ignoring case. Used for
/// folder-name exclusions such as `node_modules` or `WinSxS`.
#[cfg(windows)]
pub fn path_has_component(path: &str, name: &str) -> bool {
    path.split(['\\', '/'])
        .any(|part| part.eq_ignore_ascii_case(name))
}

#[cfg(not(windows))]
pub fn path_has_component(path: &str, name: &str) -> bool {
    path.split('/').any(|part| part.eq_ignore_ascii_case(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_keeps_drive_roots_short() {
        assert_eq!(normalize_path(r"C:\Users\Me\"), r"c:\users\me");
        assert_eq!(normalize_path("C:/Users/Me"), r"c:\users\me");
        assert_eq!(normalize_path(r"C:\"), r"c:");
        assert_eq!(normalize_path(r"\\server\share\"), r"\\server\share");
    }

    #[test]
    fn scopes_do_not_match_partial_names() {
        assert!(path_within(r"C:\Foo\Bar.txt", r"C:\Foo"));
        assert!(path_within(r"C:\Foo", r"C:\Foo"));
        assert!(!path_within(r"C:\Foobar\Bar.txt", r"C:\Foo"));
        assert!(path_within(r"C:\Foo\Bar.txt", r"C:\"));
        assert!(path_within(r"D:\Anything", "d:"));
        assert!(!path_within(r"C:\Foo", r"D:\Foo"));
    }

    #[test]
    fn absolute_paths_need_a_drive_or_unc_root() {
        assert!(is_absolute_path(r"C:\Users"));
        assert!(is_absolute_path("D:/Games"));
        assert!(is_absolute_path(r"\\server\share"));
        assert!(!is_absolute_path(r"Users\Me"));
        assert!(!is_absolute_path(""));
    }

    #[test]
    fn folder_names_match_whole_components_only() {
        assert!(path_has_component(
            r"C:\Work\app\node_modules\lib\index.js",
            "node_modules"
        ));
        assert!(path_has_component(r"D:\Windows\WinSxS\x86", "winsxs"));
        assert!(!path_has_component(
            r"C:\Work\node_modules_backup\lib",
            "node_modules"
        ));
        assert!(!path_has_component(r"C:\Work\app", "node_modules"));
    }
}
