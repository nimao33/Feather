//! The admin helper: one elevated copy of Feather per session that reads
//! NTFS master file tables for the app.
//!
//! Reading `$MFT` needs admin rights. Instead of asking with a UAC prompt on
//! every refresh, the app starts `feather index serve` elevated once (one
//! prompt) and keeps it for the session. The app owns a named pipe; the
//! helper connects, reads one request per line, and answers with the folder
//! tree as a byte stream (`Tree::encode`). The helper never writes files and
//! exits when the pipe closes, so nothing stays elevated after Feather quits.
//!
//! A declined prompt is remembered for the session; refreshes then use the
//! slower folder walk without asking again.

use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::os::windows::io::FromRawHandle;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use feather_core::mft;
use feather_core::scan::{ScanOptions, Tree};
use windows_sys::Win32::Foundation::{
    CloseHandle, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, OPEN_EXISTING, PIPE_ACCESS_DUPLEX};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_WAIT,
};

const FILE_FLAG_FIRST_PIPE_INSTANCE: u32 = 0x0008_0000;
const BUFFER: u32 = 1 << 20;

struct Session {
    reader: BufReader<File>,
    writer: BufWriter<File>,
}

#[derive(Default)]
struct Helper {
    session: Option<Session>,
    /// The person said no to the prompt this session.
    declined: bool,
}

fn helper() -> &'static Mutex<Helper> {
    static HELPER: OnceLock<Mutex<Helper>> = OnceLock::new();
    HELPER.get_or_init(|| Mutex::new(Helper::default()))
}

/// True when this session already has the helper or may still ask for it.
pub fn available() -> bool {
    helper()
        .lock()
        .map(|helper| !helper.declined)
        .unwrap_or(false)
}

/// Reads `root` through the helper, starting it (one UAC prompt) if needed.
/// Fails with `PermissionDenied` when the prompt is declined.
pub fn read(root: &Path, options: &ScanOptions) -> io::Result<Tree> {
    let mut guard = helper()
        .lock()
        .map_err(|_| io::Error::other("the helper lock is poisoned"))?;
    if guard.declined {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "admin rights were declined",
        ));
    }
    for attempt in 0..2 {
        if guard.session.is_none() {
            match start() {
                Ok(session) => guard.session = Some(session),
                Err(error) => {
                    if error.kind() == io::ErrorKind::PermissionDenied {
                        guard.declined = true;
                    }
                    return Err(error);
                }
            }
        }
        let session = guard.session.as_mut().expect("session started");
        match request(session, root, options) {
            Ok(tree) => return Ok(tree),
            // The helper went away (killed, crashed): start a new one once.
            Err(error) if attempt == 0 && error.kind() == io::ErrorKind::BrokenPipe => {
                guard.session = None;
            }
            Err(error) => {
                if matches!(
                    error.kind(),
                    io::ErrorKind::BrokenPipe | io::ErrorKind::UnexpectedEof
                ) {
                    guard.session = None;
                }
                return Err(error);
            }
        }
    }
    Err(io::Error::other("the admin helper stopped answering"))
}

fn request(session: &mut Session, root: &Path, options: &ScanOptions) -> io::Result<Tree> {
    let join = |items: &[String]| items.join("|");
    writeln!(
        session.writer,
        "scan\t{}\t{}\t{}",
        root.to_string_lossy(),
        join(&options.excluded_names),
        join(&options.excluded_paths)
    )
    .map_err(broken)?;
    session.writer.flush().map_err(broken)?;
    let mut status = [0u8; 1];
    session.reader.read_exact(&mut status).map_err(broken)?;
    if status[0] == 0 {
        return Tree::decode(&mut session.reader);
    }
    let mut len = [0u8; 4];
    session.reader.read_exact(&mut len)?;
    let mut message = vec![0u8; u32::from_le_bytes(len).min(4096) as usize];
    session.reader.read_exact(&mut message)?;
    let kind = match status[0] {
        2 => io::ErrorKind::Unsupported,
        3 => io::ErrorKind::NotFound,
        _ => io::ErrorKind::Other,
    };
    Err(io::Error::new(
        kind,
        String::from_utf8_lossy(&message).into_owned(),
    ))
}

fn broken(error: io::Error) -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, error)
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Creates the pipe, starts the elevated helper, and waits for it.
fn start() -> io::Result<Session> {
    let (name, pipe) = open_pipe()?;
    if let Err(error) = launch(&name) {
        unsafe { CloseHandle(pipe) };
        return Err(error);
    }
    accept(&name, pipe, Duration::from_secs(20))
}

/// A new pipe with a name nobody can guess, owned by this process.
fn open_pipe() -> io::Result<(String, windows_sys::Win32::Foundation::HANDLE)> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let name = format!(r"\\.\pipe\feather-{}-{nanos:x}", std::process::id());
    let pipe = unsafe {
        CreateNamedPipeW(
            wide(&name).as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            BUFFER,
            BUFFER,
            0,
            std::ptr::null(),
        )
    };
    if pipe == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok((name, pipe))
}

/// Waits for the helper to connect and say hello.
fn accept(
    name: &str,
    pipe: windows_sys::Win32::Foundation::HANDLE,
    timeout: Duration,
) -> io::Result<Session> {
    // If the helper never connects, connect to the pipe from here after a
    // while so the wait below ends; the request then fails cleanly.
    let watchdog_name = name.to_string();
    let (done, waiting) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        if waiting.recv_timeout(timeout).is_err() {
            let handle = unsafe {
                CreateFileW(
                    wide(&watchdog_name).as_ptr(),
                    GENERIC_READ,
                    0,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                )
            };
            if handle != INVALID_HANDLE_VALUE {
                unsafe { CloseHandle(handle) };
            }
        }
    });
    unsafe {
        ConnectNamedPipe(pipe, std::ptr::null_mut());
    }
    let _ = done.send(());
    let file = unsafe { File::from_raw_handle(pipe as _) };
    let reader = BufReader::with_capacity(BUFFER as usize, file.try_clone()?);
    let mut session = Session {
        reader,
        writer: BufWriter::new(file),
    };
    // The helper says hello first; the watchdog's empty connection does not.
    let mut hello = String::new();
    session.reader.read_line(&mut hello)?;
    if hello.trim() != "feather-helper 1" {
        return Err(io::Error::other("the admin helper did not start"));
    }
    Ok(session)
}

/// Starts `feather index serve --pipe NAME` with admin rights.
fn launch(pipe: &str) -> io::Result<()> {
    use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW};
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;
    const ERROR_CANCELLED: i32 = 1223;
    let exe = std::env::current_exe()?;
    let verb = wide("runas");
    let file = wide(&exe.to_string_lossy());
    let parameters = wide(&format!("index serve --pipe \"{pipe}\""));
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOASYNC;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = parameters.as_ptr();
    info.nShow = SW_HIDE;
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_CANCELLED) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "admin rights were declined",
            ));
        }
        return Err(error);
    }
    Ok(())
}

/// The helper side: connects to the app's pipe and answers requests until
/// the app closes it.
pub fn serve(pipe: &str) -> io::Result<()> {
    if !pipe.starts_with(r"\\.\pipe\feather-") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a Feather pipe",
        ));
    }
    let handle = unsafe {
        CreateFileW(
            wide(pipe).as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            0,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let file = unsafe { File::from_raw_handle(handle as _) };
    let mut reader = BufReader::new(file.try_clone()?);
    let mut writer = BufWriter::with_capacity(BUFFER as usize, file);
    writeln!(writer, "feather-helper 1")?;
    writer.flush()?;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let fields: Vec<&str> = line.trim_end_matches(['\r', '\n']).split('\t').collect();
        let split = |text: &str| -> Vec<String> {
            text.split('|')
                .filter(|item| !item.is_empty())
                .map(str::to_string)
                .collect()
        };
        let result = match fields.as_slice() {
            ["scan", root, names, paths] => {
                let options = ScanOptions {
                    excluded_names: split(names),
                    excluded_paths: split(paths),
                    ..ScanOptions::default()
                };
                mft::read(Path::new(root), &options)
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unknown request",
            )),
        };
        match result {
            Ok(tree) => {
                writer.write_all(&[0])?;
                tree.encode(&mut writer)?;
            }
            Err(error) => {
                let code = match error.kind() {
                    io::ErrorKind::Unsupported => 2,
                    io::ErrorKind::NotFound => 3,
                    _ => 1,
                };
                let message = error.to_string();
                writer.write_all(&[code])?;
                writer.write_all(&(message.len() as u32).to_le_bytes())?;
                writer.write_all(message.as_bytes())?;
            }
        }
        writer.flush()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The app and helper sides talk over a real pipe. Without admin rights
    /// the helper answers with an error, which still crosses the pipe.
    #[test]
    fn requests_and_answers_cross_the_pipe() {
        let (name, pipe) = open_pipe().expect("pipe");
        let helper_name = name.clone();
        let helper = std::thread::spawn(move || serve(&helper_name));
        let mut session = accept(&name, pipe, Duration::from_secs(10)).expect("hello");
        let answer = request(
            &mut session,
            Path::new(r"\server\share"),
            &ScanOptions::default(),
        );
        assert!(answer.is_err());
        // The helper keeps serving after an error.
        let again = request(&mut session, Path::new("Z:\\"), &ScanOptions::default());
        assert!(again.is_err());
        drop(session);
        assert!(helper.join().expect("helper thread").is_ok());
        assert!(serve("not-a-pipe").is_err());
    }
}
