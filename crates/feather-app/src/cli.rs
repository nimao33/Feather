//! Command line surface for the app, the screenshot harness, and tests.
//!
//! The modern surface is subcommands (`search`, `index`, `config`) while the
//! original flag form stays available for the screenshot and benchmark
//! scripts.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

#[cfg(windows)]
use feather_core::mft;
use feather_core::scan::{library_dir_name, scan, ScanOptions, ScanReport};
use feather_core::synthetic::generate;
use feather_core::{IndexSet, Query};

use crate::search::SearchOptions;
use crate::settings;
#[cfg(windows)]
use crate::ui;

#[derive(Clone, Debug)]
pub enum Command {
    Open,
    Search,
    IndexScan {
        roots: Vec<PathBuf>,
    },
    IndexSynthetic {
        prefix: Option<String>,
    },
    IndexStatus,
    Config(ConfigCommand),
    /// Feather's window as a local page (the window on Linux).
    Window,
    /// Stops the window started by `Window`.
    Quit,
    /// Adds the applications-menu entry, and autostart with `--autostart`.
    Install {
        autostart: bool,
    },
    Uninstall,
}

#[derive(Clone, Debug)]
pub enum ConfigCommand {
    List,
    Get(String),
    Set(String, String),
    Path,
}

#[derive(Clone, Debug)]
#[cfg_attr(not(windows), allow(dead_code))]
pub struct Args {
    pub index: PathBuf,
    pub entries: u32,
    pub entries_explicit: bool,
    pub query: String,
    pub folder: Option<String>,
    pub limit: usize,
    pub json: bool,
    pub follow_links: bool,
    pub flat_index: bool,
    /// Walk folders even where the NTFS reader could be used.
    pub walk: bool,
    pub exclude_names: Vec<String>,
    pub exclude_paths: Vec<String>,
    /// Re-run the scan with admin rights so NTFS drives can be read fast.
    pub elevate: bool,
    pub all: bool,
    pub screenshot: Option<PathBuf>,
    pub ui_test: bool,
    /// `--memory`: report memory use after common steps.
    pub memory: bool,
    /// Start hidden in the tray (used at sign-in).
    pub background: bool,
    pub screenshots: PathBuf,
    pub gen_index: bool,
    pub headless: bool,
    pub bench: bool,
    pub iterations: usize,
    pub help: bool,
    pub version: bool,
    pub command: Option<Command>,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            index: default_index(),
            entries: 200_000,
            entries_explicit: false,
            query: String::new(),
            folder: None,
            limit: 100,
            json: false,
            follow_links: false,
            flat_index: false,
            walk: false,
            exclude_names: Vec::new(),
            exclude_paths: Vec::new(),
            elevate: false,
            all: false,
            screenshot: None,
            ui_test: false,
            memory: false,
            background: false,
            screenshots: std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join("target")
                .join("ui-screenshots"),
            gen_index: false,
            headless: false,
            bench: false,
            iterations: 5,
            help: false,
            version: false,
            command: None,
        }
    }
}

fn default_index() -> PathBuf {
    #[cfg(windows)]
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        if !local.is_empty() {
            return PathBuf::from(local).join("Feather").join("index");
        }
    }
    #[cfg(not(windows))]
    {
        if let Some(data) = std::env::var_os("XDG_DATA_HOME") {
            if !data.is_empty() {
                return PathBuf::from(data).join("feather").join("index");
            }
        }
        if let Some(home) = std::env::var_os("HOME") {
            if !home.is_empty() {
                return PathBuf::from(home)
                    .join(".local")
                    .join("share")
                    .join("feather")
                    .join("index");
            }
        }
    }
    std::env::temp_dir().join("feather-index")
}

fn take_value(args: &[String], index: &mut usize, name: &str) -> Result<String, String> {
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or_else(|| format!("missing value for {name}"))
}

fn parse_count(value: String, name: &str) -> Result<u32, String> {
    value
        .parse()
        .map_err(|_| format!("{name} must be a positive number"))
}

pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let args: Vec<String> = args.into_iter().collect();
    if args.is_empty() {
        return Ok(Args::default());
    }
    match args[0].as_str() {
        "search" => parse_search(&args[1..]),
        "open" => parse_open(&args[1..]),
        "index" => parse_index(&args[1..]),
        "config" => parse_config(&args[1..]),
        "window" => Ok(Args {
            command: Some(Command::Window),
            background: args[1..].iter().any(|arg| arg == "--background"),
            ..Args::default()
        }),
        "quit" => Ok(Args {
            command: Some(Command::Quit),
            ..Args::default()
        }),
        "install" => Ok(Args {
            command: Some(Command::Install {
                autostart: args[1..].iter().any(|arg| arg == "--autostart"),
            }),
            ..Args::default()
        }),
        "uninstall" => Ok(Args {
            command: Some(Command::Uninstall),
            ..Args::default()
        }),
        "help" | "--help" | "-h" => Ok(Args {
            help: true,
            ..Args::default()
        }),
        "version" | "--version" | "-V" => Ok(Args {
            version: true,
            ..Args::default()
        }),
        _ => parse_legacy(&args),
    }
}

fn parse_open(args: &[String]) -> Result<Args, String> {
    let mut parsed = Args {
        command: Some(Command::Open),
        ..Args::default()
    };
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--index" => {
                parsed.index = crate::library::library_of(PathBuf::from(take_value(
                    args, &mut index, "--index",
                )?))
            }
            "--query" => parsed.query = take_value(args, &mut index, "--query")?,
            other => return Err(format!("unknown argument: {other}")),
        }
        index += 1;
    }
    Ok(parsed)
}

fn parse_search(args: &[String]) -> Result<Args, String> {
    let mut parsed = Args {
        command: Some(Command::Search),
        limit: 100,
        ..Args::default()
    };
    let mut terms = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--index" => {
                parsed.index = crate::library::library_of(PathBuf::from(take_value(
                    args, &mut index, "--index",
                )?))
            }
            "--limit" => {
                parsed.limit = take_value(args, &mut index, "--limit")?
                    .parse()
                    .map_err(|_| "--limit must be a whole number".to_string())?;
            }
            "--folder" => parsed.folder = Some(take_value(args, &mut index, "--folder")?),
            "--json" => parsed.json = true,
            "--all" => parsed.all = true,
            "--" => {
                terms.extend(args[index + 1..].iter().cloned());
                break;
            }
            other if other.starts_with("--") => {
                return Err(format!("unknown argument: {other}"));
            }
            term => terms.push(term.to_string()),
        }
        index += 1;
    }
    if terms.is_empty() {
        return Err("search needs at least one query term".into());
    }
    parsed.query = terms.join(" ");
    Ok(parsed)
}

fn parse_index(args: &[String]) -> Result<Args, String> {
    let Some(subcommand) = args.first() else {
        return Err("index needs a subcommand: scan, synthetic, or status".into());
    };
    let rest = &args[1..];
    match subcommand.as_str() {
        "scan" => parse_index_scan(rest),
        "synthetic" => parse_index_synthetic(rest),
        "status" => parse_index_status(rest),
        other => Err(format!("unknown index subcommand: {other}")),
    }
}

fn parse_index_scan(args: &[String]) -> Result<Args, String> {
    let mut parsed = Args {
        command: Some(Command::IndexScan { roots: Vec::new() }),
        ..Args::default()
    };
    let mut roots = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--index" => {
                parsed.index = crate::library::library_of(PathBuf::from(take_value(
                    args, &mut index, "--index",
                )?))
            }
            "--root" => roots.push(PathBuf::from(take_value(args, &mut index, "--root")?)),
            "--follow-links" => parsed.follow_links = true,
            "--flat" => parsed.flat_index = true,
            "--walk" => parsed.walk = true,
            "--elevate" => parsed.elevate = true,
            "--exclude-name" => {
                parsed
                    .exclude_names
                    .push(take_value(args, &mut index, "--exclude-name")?)
            }
            "--exclude-path" => {
                parsed
                    .exclude_paths
                    .push(take_value(args, &mut index, "--exclude-path")?)
            }
            other => return Err(format!("unknown argument: {other}")),
        }
        index += 1;
    }
    if roots.is_empty() {
        return Err("index scan needs at least one --root PATH".into());
    }
    parsed.command = Some(Command::IndexScan { roots });
    Ok(parsed)
}

fn parse_index_synthetic(args: &[String]) -> Result<Args, String> {
    let mut parsed = Args {
        command: Some(Command::IndexSynthetic { prefix: None }),
        ..Args::default()
    };
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--index" => {
                parsed.index = crate::library::library_of(PathBuf::from(take_value(
                    args, &mut index, "--index",
                )?))
            }
            "--entries" => {
                parsed.entries =
                    parse_count(take_value(args, &mut index, "--entries")?, "--entries")?;
                parsed.entries_explicit = true;
            }
            "--prefix" => {
                let prefix = take_value(args, &mut index, "--prefix")?;
                parsed.command = Some(Command::IndexSynthetic {
                    prefix: Some(prefix),
                });
            }
            other => return Err(format!("unknown argument: {other}")),
        }
        index += 1;
    }
    Ok(parsed)
}

fn parse_index_status(args: &[String]) -> Result<Args, String> {
    let mut parsed = Args {
        command: Some(Command::IndexStatus),
        ..Args::default()
    };
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--index" => {
                parsed.index = crate::library::library_of(PathBuf::from(take_value(
                    args, &mut index, "--index",
                )?))
            }
            "--json" => parsed.json = true,
            other => return Err(format!("unknown argument: {other}")),
        }
        index += 1;
    }
    Ok(parsed)
}

fn parse_config(args: &[String]) -> Result<Args, String> {
    let Some(subcommand) = args.first() else {
        return Err("config needs a subcommand: list, get, set, or path".into());
    };
    let command = match subcommand.as_str() {
        "list" => {
            if args.len() > 1 {
                return Err("config list takes no arguments".into());
            }
            ConfigCommand::List
        }
        "get" => {
            let key = args
                .get(1)
                .ok_or_else(|| "config get needs a key".to_string())?;
            ConfigCommand::Get(key.clone())
        }
        "set" => {
            let key = args
                .get(1)
                .ok_or_else(|| "config set needs a key and value".to_string())?;
            let value = args.get(2).ok_or_else(|| {
                "config set needs a value; quote values that contain spaces".to_string()
            })?;
            if args.len() > 3 {
                return Err("config set takes one value; quote values that contain spaces".into());
            }
            ConfigCommand::Set(key.clone(), value.clone())
        }
        "path" => {
            if args.len() > 1 {
                return Err("config path takes no arguments".into());
            }
            ConfigCommand::Path
        }
        other => return Err(format!("unknown config subcommand: {other}")),
    };
    Ok(Args {
        command: Some(Command::Config(command)),
        ..Args::default()
    })
}

fn parse_legacy(args: &[String]) -> Result<Args, String> {
    let mut parsed = Args::default();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--index" => {
                parsed.index = crate::library::library_of(PathBuf::from(take_value(
                    args, &mut index, "--index",
                )?))
            }
            "--entries" => {
                parsed.entries =
                    parse_count(take_value(args, &mut index, "--entries")?, "--entries")?;
                parsed.entries_explicit = true;
            }
            "--query" => parsed.query = take_value(args, &mut index, "--query")?,
            "--folder" => parsed.folder = Some(take_value(args, &mut index, "--folder")?),
            "--limit" => {
                parsed.limit = take_value(args, &mut index, "--limit")?
                    .parse()
                    .map_err(|_| "--limit must be a whole number".to_string())?;
            }
            "--json" => parsed.json = true,
            "--follow-links" => parsed.follow_links = true,
            "--flat" => parsed.flat_index = true,
            "--screenshot" => {
                parsed.screenshot =
                    Some(PathBuf::from(take_value(args, &mut index, "--screenshot")?))
            }
            "--ui-test" => parsed.ui_test = true,
            "--memory" => parsed.memory = true,
            "--background" => parsed.background = true,
            "--screenshots" => {
                parsed.screenshots = PathBuf::from(take_value(args, &mut index, "--screenshots")?)
            }
            "--gen-index" => parsed.gen_index = true,
            "--headless" => parsed.headless = true,
            "--bench" => parsed.bench = true,
            "--iterations" => {
                parsed.iterations = take_value(args, &mut index, "--iterations")?
                    .parse()
                    .map_err(|_| "--iterations must be a positive number".to_string())?;
                if parsed.iterations == 0 {
                    return Err("--iterations must be at least 1".into());
                }
            }
            "--help" | "-h" => parsed.help = true,
            "--version" | "-V" => parsed.version = true,
            other => return Err(format!("unknown argument: {other}")),
        }
        index += 1;
    }
    Ok(parsed)
}

fn print_help() {
    println!(
        "feather - low-memory file search\n\
         \n\
         usage:\n\
           feather [open] [--index DIR] [--query TEXT]\n\
           feather search [--index DIR] [--limit N] [--json] [--all] QUERY...\n\
           feather index scan --root PATH [--root PATH ...] [--index DIR] [--follow-links] [--flat] [--walk] [--elevate] [--exclude-name NAME] [--exclude-path PATH]\n\
           feather index synthetic [--index DIR] [--entries N] [--prefix PATH]\n\
           feather index status [--index DIR] [--json]\n\
           feather config list | get KEY | set KEY VALUE | path\n\
           feather window [--background]   the window as a local page (the default on Linux)\n\
           feather quit                    stop the window started by `feather window`\n\
           feather install [--autostart]   add Feather to the applications menu (Linux)\n\
           feather uninstall               remove the menu and autostart entries\n\
         \n\
         developer commands:\n\
           feather --gen-index --index DIR [--entries N]\n\
           feather --headless --index DIR [--query TEXT] [--limit N] [--json]\n\
           feather --bench [--iterations N] [--index DIR]\n\
           feather --screenshot FILE [--index DIR] [--query TEXT]\n\
           feather --ui-test [--screenshots DIR] [--index DIR]
feather --memory --index DIR\n\
         \n\
         The default index is %LOCALAPPDATA%\\Feather\\index. A library directory\n\
         holds one subdirectory per volume; a single index directory also works."
    );
}

fn index_exists(path: &Path) -> bool {
    if path.join("manifest.bin").is_file() {
        return true;
    }
    std::fs::read_dir(path)
        .map(|entries| {
            entries.flatten().any(|entry| {
                let path = entry.path();
                path.is_dir() && path.join("manifest.bin").is_file()
            })
        })
        .unwrap_or(false)
}

fn generate_index(args: &Args) -> Result<(), String> {
    let report = generate(&args.index, args.entries, r"C:\FeatherBench")
        .map_err(|error| format!("could not generate index: {error}"))?;
    println!(
        "generated {} entries ({} folders, {} files) in {:.2}s at {}",
        report.entries,
        report.dirs,
        report.files,
        report.elapsed.as_secs_f64(),
        args.index.display()
    );
    Ok(())
}

fn ensure_index(args: &Args, allow_generate: bool) -> Result<(), String> {
    if index_exists(&args.index) {
        return Ok(());
    }
    // Demo data only goes into a new or empty folder: a real index that is
    // half written or being cleaned up must never be overwritten.
    let empty = std::fs::read_dir(&args.index).map_or(true, |mut entries| entries.next().is_none());
    if allow_generate && empty {
        return generate_index(args);
    }
    Err(format!(
        "no index at {}. Run `feather index scan --root C:\\` first, or use `feather index synthetic` for demo data.",
        args.index.display()
    ))
}

fn open_set(args: &Args) -> Result<Arc<IndexSet>, String> {
    IndexSet::open(&args.index)
        .map(Arc::new)
        .map_err(|error| format!("could not open index: {error}"))
}

/// On Windows the installer runs `feather uninstall` while removing Feather,
/// to take away what the app made itself: starting at sign-in and its
/// shortcuts. On Linux it removes the menu and autostart entries.
#[cfg(windows)]
fn uninstall() -> Result<(), String> {
    ui::forget_installation();
    Ok(())
}

#[cfg(not(windows))]
fn uninstall() -> Result<(), String> {
    crate::local::desktop::set_menu_entry(false)?;
    crate::local::desktop::set_autostart(false)?;
    println!("Feather's menu and autostart entries are gone.");
    Ok(())
}

pub fn run(args: impl IntoIterator<Item = String>) -> i32 {
    match run_inner(args) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}

fn run_inner(args: impl IntoIterator<Item = String>) -> Result<(), String> {
    let args: Vec<String> = args.into_iter().collect();
    // The app's admin helper (started elevated by the app itself).
    #[cfg(windows)]
    if let [index, serve, flag, pipe] = args.as_slice() {
        if index == "index" && serve == "serve" && flag == "--pipe" {
            return ui::serve_helper(pipe).map_err(|error| format!("helper: {error}"));
        }
    }
    let parsed = parse(args)?;
    if parsed.help {
        print_help();
        return Ok(());
    }
    if parsed.version {
        println!("feather {}", feather_core::version());
        return Ok(());
    }
    if parsed.bench {
        ensure_index(&parsed, true)?;
        let set = open_set(&parsed)?;
        return run_bench(&set, parsed.iterations);
    }

    match &parsed.command {
        Some(Command::Config(command)) => run_config(command),
        Some(Command::IndexScan { roots }) => run_index_scan(&parsed, roots),
        Some(Command::IndexSynthetic { prefix }) => {
            let prefix = prefix.as_deref().unwrap_or_else(|| default_bench_prefix());
            let report = generate(&parsed.index, parsed.entries, prefix)
                .map_err(|error| format!("could not generate index: {error}"))?;
            println!(
                "entries={} folders={} files={} bytes={} elapsed_ms={}",
                report.entries,
                report.dirs,
                report.files,
                report.bytes,
                report.elapsed.as_millis()
            );
            Ok(())
        }
        Some(Command::IndexStatus) => run_index_status(&parsed),
        Some(Command::Window) => crate::local::run(parsed.index.clone(), parsed.background),
        Some(Command::Quit) => {
            #[cfg(windows)]
            let running = ui::quit_running_copies();
            #[cfg(not(windows))]
            let running = crate::local::quit();
            if running {
                Ok(())
            } else {
                Err("Feather isn't running.".into())
            }
        }
        Some(Command::Install { autostart }) => {
            crate::local::desktop::set_menu_entry(true)?;
            if *autostart {
                crate::local::desktop::set_autostart(true)?;
            }
            println!("Feather is in your applications menu.");
            Ok(())
        }
        Some(Command::Uninstall) => uninstall(),
        Some(Command::Search) => {
            ensure_index(&parsed, false)?;
            let set = open_set(&parsed)?;
            run_search(&set, &parsed)
        }
        Some(Command::Open) | None => {
            #[cfg(windows)]
            {
                // The test harnesses run on generated demo data.
                if parsed.memory {
                    ensure_index(&parsed, false)?;
                    return ui::run_memory_report(open_set(&parsed)?);
                }
                if parsed.ui_test || parsed.screenshot.is_some() {
                    ensure_index(&parsed, true)?;
                    let set = open_set(&parsed)?;
                    if let Some(path) = &parsed.screenshot {
                        return ui::run_screenshot(set, path, &parsed.query);
                    }
                    return ui::run_scenarios(set, &parsed.screenshots);
                }
                // The app itself starts empty and indexes what the person picks
                // in first-run setup or Settings. One copy runs at a time; a
                // second launch brings the first one forward (from the tray).
                if ui::activate_running() {
                    return Ok(());
                }
                ui::apply_pending_index(&parsed.index);
                let active = ui::active_library(&parsed.index);
                let set = if index_exists(&active) {
                    IndexSet::open(&active)
                        .map(Arc::new)
                        .unwrap_or_else(|error| {
                            eprintln!("could not open index: {error}");
                            Arc::new(IndexSet::empty())
                        })
                } else {
                    Arc::new(IndexSet::empty())
                };
                let code = ui::run_ui(set, parsed.index.clone(), parsed.background);
                if code == 0 {
                    Ok(())
                } else {
                    Err(format!("UI exited with code {code}"))
                }
            }
            // Elsewhere the window is a local page in an app window of the
            // browser, which works on every desktop.
            #[cfg(not(windows))]
            {
                crate::local::run(parsed.index.clone(), parsed.background)
            }
        }
    }
}

fn run_search(set: &IndexSet, args: &Args) -> Result<(), String> {
    let mut text = args.query.clone();
    if let Some(folder) = &args.folder {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(&format!("in:\"{folder}\""));
    }
    let query = Query::parse(&text);
    for warning in query.warnings() {
        eprintln!("warning: {warning}");
    }

    let loaded = settings::load();
    let mut options = SearchOptions {
        show_hidden: loaded.settings.show_hidden,
        show_system: loaded.settings.show_system,
        excluded_paths: loaded.settings.excluded_folders.clone(),
        volume_mask: set.volume_mask(&loaded.settings.excluded_volumes),
    }
    .for_index(set);
    if args.all {
        options.show_hidden = true;
        options.show_system = true;
        options.excluded_paths.clear();
        options.volume_mask = set.volume_mask(&[]);
    }

    let limit = if args.limit == 0 {
        usize::MAX
    } else {
        args.limit
    };
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    let mut count = 0usize;
    let mut capped = false;
    let needs_path = query.needs_path() || !options.excluded_paths.is_empty();

    'volumes: for (volume_index, volume) in set.volumes().iter().enumerate() {
        if options.volume_mask & (1u64 << volume_index) == 0 {
            continue;
        }
        let filter = query.volume_filter(volume);
        if !filter.may_match() {
            continue;
        }
        let mut paths = feather_core::PathCheck::new(&query);
        for id in volume.iter_ids() {
            let entry = volume.entry(id);
            if !options.show_hidden && entry.flags & 2 != 0 {
                continue;
            }
            if !options.show_system && entry.flags & 4 != 0 {
                continue;
            }
            if !query.matches(entry.name_search, &entry) || !query.matches_folder(&filter, &entry) {
                continue;
            }

            let reference = feather_core::EntryRef::new(volume_index, id);
            if options.excluded_paths.is_empty()
                && query.needs_path()
                && !paths.matches(volume, entry.parent_frn, entry.name_search, || {
                    set.resolve_path(reference)
                })
            {
                continue;
            }
            let mut resolved = None;
            if needs_path {
                let reference = feather_core::EntryRef::new(volume_index, id);
                let path = match set.resolve_path(reference) {
                    Some(path) => path,
                    None => continue,
                };
                if !query.matches_path(&path)
                    || options
                        .excluded_paths
                        .iter()
                        .any(|excluded| feather_core::path_within(&path, excluded))
                {
                    continue;
                }
                resolved = Some(path);
            }
            let path = match resolved {
                Some(path) => path,
                None => {
                    let reference = feather_core::EntryRef::new(volume_index, id);
                    match set.resolve_path(reference) {
                        Some(path) => path,
                        None => continue,
                    }
                }
            };
            count += 1;
            if args.json {
                let kind = if entry.is_dir() { "folder" } else { "file" };
                let _ = writeln!(
                    output,
                    "{{\"path\":\"{}\",\"size\":{},\"modified\":{},\"kind\":\"{}\",\"volume\":\"{}\"}}",
                    json_escape(&path),
                    entry.size,
                    entry.mtime,
                    kind,
                    json_escape(&set.volumes()[volume_index].prefix)
                );
            } else {
                let _ = writeln!(output, "{path}");
            }
            if count >= limit {
                capped = true;
                break 'volumes;
            }
        }
    }
    if !args.json {
        let suffix = if capped { "+" } else { "" };
        let _ = writeln!(output, "matches={count}{suffix}");
    }
    let _ = output.flush();
    Ok(())
}

fn json_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 8);
    for ch in value.chars() {
        match ch {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            ch if (ch as u32) < 0x20 => escaped.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => escaped.push(ch),
        }
    }
    escaped
}

fn print_scan_report(report: &ScanReport) {
    println!(
        "root={} entries={} folders={} files={} skipped={} errors={} bytes={} elapsed_ms={}{}",
        report.root,
        report.entries,
        report.dirs,
        report.files,
        report.skipped,
        report.errors,
        report.bytes,
        report.elapsed.as_millis(),
        if report.truncated {
            " truncated=true"
        } else {
            ""
        }
    );
}

fn default_bench_prefix() -> &'static str {
    if cfg!(windows) {
        r"C:\FeatherBench"
    } else {
        "/FeatherBench"
    }
}

fn run_index_scan(args: &Args, roots: &[PathBuf]) -> Result<(), String> {
    let options = ScanOptions {
        follow_links: args.follow_links,
        excluded_names: args.exclude_names.clone(),
        excluded_paths: args.exclude_paths.clone(),
        ..ScanOptions::default()
    };
    #[cfg(windows)]
    if args.elevate {
        let roots: Vec<String> = roots
            .iter()
            .map(|root| root.to_string_lossy().to_string())
            .collect();
        std::fs::create_dir_all(&args.index)
            .map_err(|error| format!("could not create {}: {error}", args.index.display()))?;
        ui::scan_elevated(&args.index, &roots, &options)
            .map_err(|error| format!("the elevated scan did not finish: {error}"))?;
        println!("elevated scan finished in {}", args.index.display());
        return Ok(());
    }
    // The NTFS reader is used where it can be (admin rights, local NTFS),
    // the folder walk everywhere else.
    #[cfg(windows)]
    let scan = |root: &Path, out: &Path, options: &ScanOptions| {
        if args.walk {
            scan(root, out, options)
        } else {
            mft::scan_any(root, out, options)
        }
    };
    #[cfg(not(windows))]
    let scan = |root: &Path, out: &Path, options: &ScanOptions| scan(root, out, options);
    if roots.len() == 1 && args.flat_index {
        let report = scan(&roots[0], &args.index, &options)
            .map_err(|error| format!("could not scan {}: {error}", roots[0].display()))?;
        print_scan_report(&report);
        return Ok(());
    }
    if args.index.join("manifest.bin").is_file() {
        return Err(format!(
            "{} is a single-volume index; choose an empty library directory or pass --flat",
            args.index.display()
        ));
    }

    std::fs::create_dir_all(&args.index)
        .map_err(|error| format!("could not create {}: {error}", args.index.display()))?;
    for root in roots {
        let target = args.index.join(library_dir_name(root));
        let report = scan(root, &target, &options)
            .map_err(|error| format!("could not scan {}: {error}", root.display()))?;
        print_scan_report(&report);
    }
    Ok(())
}

fn run_index_status(args: &Args) -> Result<(), String> {
    let set = open_set(args)?;
    let infos = set.infos();
    if args.json {
        let volumes: Vec<String> = infos
            .iter()
            .map(|info| {
                format!(
                    "{{\\\"prefix\\\":\\\"{}\\\",\\\"entries\\\":{},\\\"items\\\":{},\\\"generation\\\":{},\\\"source\\\":\\\"{}\\\"}}",
                    json_escape(&info.prefix),
                    info.entry_count,
                    info.item_count(),
                    info.generation,
                    json_escape(&info.source.to_string_lossy())
                )
            })
            .collect();
        println!(
            "{{\\\"items\\\":{},\\\"volumes\\\":[{}]}}",
            set.item_count(),
            volumes.join(",")
        );
        return Ok(());
    }
    for info in &infos {
        println!(
            "{}\t{} items\t{} entries\tgeneration {}\t{}",
            info.prefix,
            info.item_count(),
            info.entry_count,
            info.generation,
            info.source.display()
        );
    }
    println!(
        "total={} items in {} volumes",
        set.item_count(),
        infos.len()
    );
    Ok(())
}

fn run_config(command: &ConfigCommand) -> Result<(), String> {
    let path = settings::file_path().ok_or_else(|| "APPDATA is not set".to_string())?;
    match command {
        ConfigCommand::Path => {
            println!("{}", path.display());
            Ok(())
        }
        ConfigCommand::List => {
            let loaded = settings::load();
            for warning in &loaded.warnings {
                eprintln!("warning: {warning}");
            }
            print!("{}", loaded.settings.to_text());
            Ok(())
        }
        ConfigCommand::Get(key) => {
            let loaded = settings::load();
            match settings::get_value(&loaded.settings, key) {
                Some(value) => {
                    println!("{value}");
                    Ok(())
                }
                None => Err(format!("setting not set: {key}")),
            }
        }
        ConfigCommand::Set(key, value) => {
            let loaded = settings::load();
            let mut updated = loaded.settings;
            settings::set_value(&mut updated, key, value)?;
            updated.save()?;
            println!("{key}={value}");
            Ok(())
        }
    }
}

#[allow(dead_code)]
fn scan_count(set: &IndexSet, query: &Query, limit: usize) -> (u64, bool) {
    let mut count = 0u64;
    let mut capped = false;
    'volumes: for volume in set.volumes() {
        let filter = query.volume_filter(volume);
        if !filter.may_match() {
            continue;
        }
        for id in volume.iter_ids() {
            let entry = volume.entry_summary(id);
            if !query.matches_name(entry.name_search, entry.size, entry.flags)
                || !query.matches_date_of(volume, id)
                || !query.matches_folder_parts(&filter, entry.parent_frn, entry.name_search)
            {
                continue;
            }
            count += 1;
            if count as usize >= limit {
                capped = true;
                break 'volumes;
            }
        }
    }
    (count, capped)
}

#[allow(dead_code)]
fn percentile(sorted: &[f64], percent: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = ((percent / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[rank.min(sorted.len() - 1)]
}

fn run_bench(set: &IndexSet, iterations: usize) -> Result<(), String> {
    let cases = [
        ("(empty)", ""),
        ("file0000005", "file0000005"),
        ("starts file", "match:starts file0000005"),
        ("ends .pdf", "match:ends .pdf"),
        ("ext:pdf", "ext:pdf"),
        ("type:folder", "type:folder"),
    ];
    println!(
        "entries={} iterations={} cap={}",
        set.item_count(),
        iterations,
        crate::search::RESULT_CAP
    );
    println!(
        "{:<14} {:>9} {:>9} {:>9} {:>9}",
        "query", "matches", "p50_ms", "p95_ms", "max_ms"
    );

    for (label, text) in cases {
        let query = Query::parse(text);
        let (matches, _) = scan_count(set, &query, crate::search::RESULT_CAP);
        let _ = scan_count(set, &query, crate::search::RESULT_CAP);
        let mut times = Vec::with_capacity(iterations);
        for _ in 0..iterations {
            let started = Instant::now();
            let (counted, _) = scan_count(set, &query, crate::search::RESULT_CAP);
            times.push(started.elapsed().as_secs_f64() * 1000.0);
            debug_assert_eq!(counted, matches);
        }
        times.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let p50 = percentile(&times, 50.0);
        let p95 = percentile(&times, 95.0);
        let max = times.last().copied().unwrap_or(0.0);
        let capped = matches as usize >= crate::search::RESULT_CAP;
        let shown = format!("{}{}", matches, if capped { "+" } else { "" });
        println!("{label:<14} {shown:>9} {p50:>9.2} {p95:>9.2} {max:>9.2}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_handles_bounds() {
        let values = [1.0, 2.0, 3.0, 4.0, 5.0];
        assert_eq!(percentile(&values, 0.0), 1.0);
        assert_eq!(percentile(&values, 50.0), 3.0);
        assert_eq!(percentile(&values, 100.0), 5.0);
        assert_eq!(percentile(&[], 50.0), 0.0);
    }

    #[test]
    fn parses_benchmark_arguments() {
        let args = parse([
            "--bench".to_string(),
            "--iterations".to_string(),
            "7".to_string(),
        ])
        .expect("parse");
        assert!(args.bench);
        assert_eq!(args.iterations, 7);

        let error = parse(["--iterations".to_string(), "0".to_string()]).expect_err("zero");
        assert!(error.contains("at least 1"));
    }

    #[test]
    fn parses_search_subcommand() {
        let args = parse([
            "search".to_string(),
            "--index".to_string(),
            "idx".to_string(),
            "--limit".to_string(),
            "25".to_string(),
            "--json".to_string(),
            "report".to_string(),
            "pdf".to_string(),
        ])
        .expect("parse");
        assert!(matches!(args.command, Some(Command::Search)));
        assert_eq!(args.query, "report pdf");
        assert_eq!(args.limit, 25);
        assert!(args.json);
        assert_eq!(args.index, PathBuf::from("idx"));

        let args = parse([
            "search".to_string(),
            "--folder".to_string(),
            r"C:\Users\Me".to_string(),
            "report".to_string(),
        ])
        .expect("parse");
        assert_eq!(args.folder.as_deref(), Some(r"C:\Users\Me"));
    }

    #[test]
    fn parses_index_and_config_subcommands() {
        let args = parse([
            "index".to_string(),
            "scan".to_string(),
            "--root".to_string(),
            r"C:\".to_string(),
            "--root".to_string(),
            r"D:\".to_string(),
            "--follow-links".to_string(),
        ])
        .expect("parse");
        match args.command {
            Some(Command::IndexScan { roots }) => assert_eq!(roots.len(), 2),
            _ => panic!("expected scan"),
        }
        assert!(args.follow_links);

        let args = parse([
            "config".to_string(),
            "set".to_string(),
            "max_results".to_string(),
            "1000".to_string(),
        ])
        .expect("parse");
        assert!(matches!(
            args.command,
            Some(Command::Config(ConfigCommand::Set(_, _)))
        ));
    }

    #[test]
    fn search_requires_a_term() {
        assert!(parse(["search".to_string()]).is_err());
        assert!(parse(["index".to_string(), "scan".to_string()]).is_err());
    }
}
