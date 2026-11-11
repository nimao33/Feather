//! Pure text formatting helpers used by the result rows. No Win32 here, so
//! these are unit-testable.

/// Returns the containing folder for a resolved Windows path. The results
/// table shows the folder next to the name, like Everything, instead of
/// repeating the name inside the path column.
pub fn parent_path(path: &str) -> String {
    match path.rsplit_once('\\') {
        Some((parent, _)) if parent.ends_with(':') => format!("{parent}\\"),
        Some((parent, _)) => parent.to_string(),
        None => path.to_string(),
    }
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// A size in the unit chosen in Settings. `Auto` picks the unit by size.
pub fn size_in(bytes: u64, format: crate::settings::SizeFormat) -> String {
    use crate::settings::SizeFormat;
    match format {
        SizeFormat::Auto => human_size(bytes),
        // Like Explorer: whole kilobytes, rounded up, grouped.
        SizeFormat::Kb => format!("{} KB", group(bytes.div_ceil(1024))),
        SizeFormat::Mb => decimal(bytes as f64 / (1024.0 * 1024.0), "MB"),
        SizeFormat::Gb => decimal(bytes as f64 / (1024.0 * 1024.0 * 1024.0), "GB"),
    }
}

/// `value` with two decimals when small, one under 100, none above, the
/// whole part grouped by thousands.
fn decimal(value: f64, unit: &str) -> String {
    let places = if value >= 100.0 {
        0
    } else if value >= 10.0 {
        1
    } else {
        2
    };
    let text = format!("{value:.places$}");
    let separator = thousands_separator();
    // Where thousands are grouped with a dot, decimals take a comma.
    let point = if separator == "." { "," } else { "." };
    match text.split_once('.') {
        Some((whole, fraction)) => {
            format!("{}{point}{fraction} {unit}", group_with(whole, separator))
        }
        None => format!("{} {unit}", group_with(&text, separator)),
    }
}

pub fn extension(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((_, extension)) if !extension.is_empty() => extension,
        _ => "",
    }
}

pub fn type_label(name: &str, is_dir: bool) -> String {
    if is_dir {
        return "Folder".to_string();
    }
    let extension = extension(name);
    if extension.is_empty() {
        "File".to_string()
    } else {
        extension.to_uppercase()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_of_a_file_is_its_folder() {
        assert_eq!(parent_path(r"C:\Users\admin\notes.txt"), r"C:\Users\admin");
        assert_eq!(
            parent_path(r"C:\Users\admin\Documents\report.pdf"),
            r"C:\Users\admin\Documents"
        );
    }

    #[test]
    fn parent_of_a_root_level_item_keeps_the_drive_slash() {
        assert_eq!(parent_path(r"C:\notes.txt"), r"C:\");
        assert_eq!(parent_path(r"C:\Windows"), r"C:\");
    }

    #[test]
    fn parent_of_a_drive_without_separator_is_unchanged() {
        assert_eq!(parent_path("C:"), "C:");
        assert_eq!(parent_path(""), "");
    }

    #[test]
    fn sizes_use_binary_units() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(1024 * 1024), "1.0 MB");
        assert_eq!(human_size(3 * 1024 * 1024 * 1024), "3.0 GB");
        use crate::settings::SizeFormat;
        let sep = thousands_separator();
        assert_eq!(size_in(1, SizeFormat::Kb), "1 KB");
        assert_eq!(size_in(0, SizeFormat::Kb), "0 KB");
        assert_eq!(
            size_in(5 * 1024 * 1024, SizeFormat::Kb),
            format!("5{sep}120 KB")
        );
        assert!(size_in(1536 * 1024, SizeFormat::Mb).starts_with("1"));
        assert!(size_in(1536 * 1024, SizeFormat::Mb).ends_with("50 MB"));
        assert!(size_in(512 * 1024 * 1024, SizeFormat::Gb).ends_with("50 GB"));
        assert_eq!(size_in(1023, SizeFormat::Auto), "1023 B");
    }

    #[test]
    fn types_fall_back_to_file() {
        assert_eq!(type_label("report.pdf", false), "PDF");
        assert_eq!(type_label("README", false), "File");
        assert_eq!(type_label(".gitignore", false), "GITIGNORE");
        assert_eq!(type_label("src", true), "Folder");
        assert_eq!(extension("archive.tar.gz"), "gz");
    }
}

/// The digit-group separator from the Windows region settings (`.` in
/// much of Europe, `,` in the US), or `,` if it cannot be read.
#[cfg(windows)]
pub fn thousands_separator() -> &'static str {
    static SEPARATOR: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    SEPARATOR.get_or_init(|| {
        use windows_sys::Win32::Globalization::{GetLocaleInfoEx, LOCALE_STHOUSAND};
        let mut buffer = [0u16; 8];
        let len = unsafe {
            GetLocaleInfoEx(
                std::ptr::null(),
                LOCALE_STHOUSAND,
                buffer.as_mut_ptr(),
                buffer.len() as i32,
            )
        };
        let text = if len > 1 {
            String::from_utf16_lossy(&buffer[..len as usize - 1])
        } else {
            String::new()
        };
        if text.is_empty() {
            ",".to_string()
        } else {
            text
        }
    })
}

/// Linux groups thousands with a comma; the system locale is not consulted
/// in the hot path.
#[cfg(not(windows))]
pub fn thousands_separator() -> &'static str {
    ","
}

/// Groups a run of digits by thousands with `separator`: `1000500` becomes
/// `1.000.500`.
pub fn group_with(digits: &str, separator: &str) -> String {
    let digits: Vec<char> = digits.chars().filter(char::is_ascii_digit).collect();
    let mut out = String::with_capacity(digits.len() * 2);
    for (index, digit) in digits.iter().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push_str(separator);
        }
        out.push(*digit);
    }
    out
}

/// A number grouped the way this computer writes numbers.
pub fn group(value: u64) -> String {
    group_with(&value.to_string(), thousands_separator())
}

/// The digits of a number typed with any grouping, or `None` if there are
/// none.
pub fn digits_only(text: &str) -> String {
    text.chars().filter(char::is_ascii_digit).collect()
}

#[cfg(test)]
mod grouping_tests {
    use super::*;

    #[test]
    fn numbers_are_grouped_by_thousands() {
        assert_eq!(group_with("1000500", "."), "1.000.500");
        assert_eq!(group_with("1.000", "."), "1.000");
        assert_eq!(group_with("999", ","), "999");
        assert_eq!(group_with("50000", "\u{a0}"), "50\u{a0}000");
        assert_eq!(group_with("", "."), "");
        assert_eq!(digits_only("1.000.500 "), "1000500");
    }
}
