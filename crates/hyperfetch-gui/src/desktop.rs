//! What ties the app to the desktop: how it was started (`--minimized`, `--data-dir`, an
//! `endo-dl:` link), its data folder and log file, launch at startup and the `endo-dl:` link,
//! sleeping or shutting down once the queue is done, and the schedule's clock. Whatever changes
//! the system goes through the [`Writer`] or the [`Power`] the app holds, which tests replace;
//! in tests [`write`] and [`power`] refuse.

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hyperfetch_core::history::redact_text;

use crate::settings::Schedule;

/// The size at which the log file is renamed `endo.log.1`.
pub const LOG_LIMIT: u64 = 5 << 20;
/// How long the countdown before sleeping or shutting down lasts.
pub const COUNTDOWN: Duration = Duration::from_secs(30);

/// How the app was started.
#[derive(Debug, Default, PartialEq)]
pub struct Launch {
    /// Hidden in the tray: `--minimized`, or started by an `endo-dl:` link.
    pub minimized: bool,
    /// The data folder `--data-dir` names.
    pub data_dir: Option<PathBuf>,
}

impl Launch {
    /// Reads the arguments after the program's name; any other is ignored, and an `endo-dl:`
    /// link only starts the app: its text is never read, nor anything after it.
    pub fn parse(args: impl IntoIterator<Item = OsString>) -> Self {
        let mut launch = Self::default();
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            let text = arg.to_string_lossy();
            let link = text.get(..8).is_some_and(|scheme| scheme.eq_ignore_ascii_case("endo-dl:"));
            if text == "--minimized" || link {
                launch.minimized = true;
            } else if text == "--data-dir" {
                launch.data_dir = args.next().map(PathBuf::from);
            }
            // The link comes last (see `protocol_entries`): what follows is its own text, split
            // where it held a `"`, and could otherwise name a data folder.
            if link {
                break;
            }
        }
        launch
    }
}

/// Whether this copy registers the `endo-dl:` link: not a portable one, nor one started with
/// `ENDO_HISTORY_PATH` set (a test or development run), as their data is elsewhere. Asked before
/// `main` sets that variable for a portable copy.
pub fn takes_links(portable: bool) -> bool {
    !portable && std::env::var_os("ENDO_HISTORY_PATH").is_none()
}

/// Locks the data folder `dir` for this app (its `instance.lock`, while the file is open), so
/// that two started at once never both run on it; None while another copy holds it.
pub fn lock_instance(dir: &Path) -> io::Result<Option<File>> {
    std::fs::create_dir_all(dir)?;
    let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(dir.join("instance.lock"))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(e),
    }
}

/// A data folder other than the usual one: `given` (`--data-dir`), else `data` next to the
/// program `exe` when a file named `portable` is there. `main` makes it the history's and the
/// download archive's, which the settings, queue and logs sit next to.
pub fn portable_dir(given: Option<PathBuf>, exe: Option<&Path>) -> Option<PathBuf> {
    let beside = || exe?.parent().filter(|dir| dir.join("portable").is_file()).map(|dir| dir.join("data"));
    given.or_else(beside).map(|dir| std::path::absolute(&dir).unwrap_or(dir))
}

/// A short tag of the data folder `dir`, which `/ping` tells, so that a copy of the app with a
/// folder of its own (a portable one) is not taken for this one: FNV-1a of its path.
pub fn data_tag(dir: &Path) -> String {
    let path = dir.to_string_lossy();
    let path = if cfg!(windows) { path.to_lowercase() } else { path.into_owned() };
    let hash = path.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3));
    format!("{:08x}", hash >> 32)
}

/// Something registered for the user: a value under HKEY_CURRENT_USER (`name` None for the
/// key's default value), or a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
    Registry { key: String, name: Option<String> },
    File(PathBuf),
}

/// Writes a value at a place, or removes it (None): [`write`] in the app, a stand-in in tests.
pub type Writer = Box<dyn Fn(&Place, Option<&str>) -> io::Result<()>>;

/// Puts the computer to sleep or shuts it down: [`power`] in the app, a stand-in in tests.
pub type Power = Box<dyn Fn(WhenDone) -> io::Result<()>>;

/// Where launch at startup is registered for the program `exe`, started hidden, and what: the
/// Run value on Windows, a LaunchAgent in `root` (`~/Library`) on macOS, an autostart entry in
/// `root` (the config folder, `~/.config`) on Linux.
// shortcut: a `--data-dir` given on the command line is not carried over (a `portable` file is);
// add it to the command when someone needs that.
pub fn startup_entry(exe: &Path, root: &Path) -> (Place, String) {
    if cfg!(windows) {
        let key = r"Software\Microsoft\Windows\CurrentVersion\Run".to_string();
        let place = Place::Registry { key, name: Some("EndosUnifiedDownloader".to_string()) };
        return (place, format!("\"{}\" --minimized", exe.display()));
    }
    let exe = exe.to_string_lossy();
    if cfg!(target_os = "macos") {
        let label = "io.github.adityasharma0101911.endos-unified-downloader";
        let exe = exe.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
        let plist = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\">\n<dict>\n\
             \t<key>Label</key>\n\t<string>{label}</string>\n\
             \t<key>ProgramArguments</key>\n\t<array>\n\t\t<string>{exe}</string>\n\t\t<string>--minimized</string>\n\t</array>\n\
             \t<key>RunAtLoad</key>\n\t<true/>\n\
             </dict>\n</plist>\n"
        );
        return (Place::File(root.join("LaunchAgents").join(format!("{label}.plist"))), plist);
    }
    let entry = format!("[Desktop Entry]\nType=Application\nName=Endo's Unified Downloader\nExec={} --minimized\n", exec_arg(&exe));
    (Place::File(root.join("autostart").join("endos-unified-downloader.desktop")), entry)
}

/// `arg` as one argument of a desktop entry's Exec line: quoted, `"`, `` ` ``, `$` and `\`
/// escaped inside the quotes, then `\` escaped again as the file's strings take it, `%` doubled.
fn exec_arg(arg: &str) -> String {
    let mut quoted = String::from("\"");
    for c in arg.chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('"');
    quoted.replace('\\', "\\\\").replace('%', "%%")
}

/// Where launch at startup goes on macOS (`~/Library`) and Linux (`$XDG_CONFIG_HOME`, else
/// `~/.config`); Windows keeps it in the registry.
pub fn startup_root() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    if cfg!(target_os = "macos") {
        return home.join("Library");
    }
    std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| home.join(".config"))
}

/// The registry values that make `endo-dl:` links start the program `exe` hidden (Windows); the
/// link only starts it (see [`Launch::parse`]). On macOS and Linux the app's Info.plist and
/// .desktop file register it.
pub fn protocol_entries(exe: &Path) -> Vec<(Place, String)> {
    let key = r"Software\Classes\endo-dl";
    let at = |key: String, name: Option<&str>| Place::Registry { key, name: name.map(str::to_string) };
    vec![
        (at(key.to_string(), None), "URL:Endo's Unified Downloader".to_string()),
        (at(key.to_string(), Some("URL Protocol")), String::new()),
        (at(format!(r"{key}\shell\open\command"), None), format!("\"{}\" --minimized \"%1\"", exe.display())),
    ]
}

/// Writes `value` at `place` on this computer, or removes it (None).
#[cfg(not(test))]
pub fn write(place: &Place, value: Option<&str>) -> io::Result<()> {
    match place {
        Place::File(path) => write_file(path, value),
        #[cfg(windows)]
        Place::Registry { key, name } => write_registry(key, name.as_deref(), value),
        #[cfg(not(windows))]
        Place::Registry { .. } => Err(io::Error::other("there is no registry here")),
    }
}

/// Tests never register anything on the computer they run on.
#[cfg(test)]
pub fn write(_: &Place, _: Option<&str>) -> io::Result<()> {
    Err(io::Error::other("tests never change the system"))
}

/// Writes the file `path` (making its folder), or deletes it (None; one already gone is fine).
pub fn write_file(path: &Path, value: Option<&str>) -> io::Result<()> {
    match value {
        Some(text) => crate::settings::write_atomic(path, text.as_bytes()),
        None => match std::fs::remove_file(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        },
    }
}

/// Sets the string `value` under HKEY_CURRENT_USER\`key` (made if missing), named `name` (None:
/// the key's default value), or deletes that value (None; one already gone is fine).
#[cfg(all(windows, not(test)))]
fn write_registry(key: &str, name: Option<&str>, value: Option<&str>) -> io::Result<()> {
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteKeyValueW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE,
        REG_OPTION_NON_VOLATILE, REG_SZ,
    };
    const NOT_FOUND: u32 = 2;
    let wide = |text: &str| text.encode_utf16().chain([0]).collect::<Vec<u16>>();
    let (key, name) = (wide(key), name.map(wide));
    let name = name.as_ref().map_or(std::ptr::null(), |name| name.as_ptr());
    let status = match value {
        Some(value) => {
            let data = wide(value);
            let mut opened: HKEY = std::ptr::null_mut();
            // SAFETY: the key, name and data are NUL-terminated UTF-16 strings that outlive the
            // calls, `opened` receives the key's handle, which is closed once the value is set.
            unsafe {
                let status = RegCreateKeyExW(
                    HKEY_CURRENT_USER,
                    key.as_ptr(),
                    0,
                    std::ptr::null(),
                    REG_OPTION_NON_VOLATILE,
                    KEY_SET_VALUE,
                    std::ptr::null(),
                    &mut opened,
                    std::ptr::null_mut(),
                );
                if status != 0 {
                    return Err(io::Error::from_raw_os_error(status as i32));
                }
                let status = RegSetValueExW(opened, name, 0, REG_SZ, data.as_ptr().cast(), (data.len() * 2) as u32);
                RegCloseKey(opened);
                status
            }
        }
        // SAFETY: NUL-terminated UTF-16 strings that outlive the call.
        None => match unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name) } {
            NOT_FOUND => 0,
            status => status,
        },
    };
    if status == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(status as i32))
    }
}

/// What happens once the queue is done (see `App::queue_finished`), chosen for this session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum WhenDone {
    #[default]
    Nothing,
    Quit,
    Sleep,
    ShutDown,
}

impl WhenDone {
    /// The choices, as the queue page names them.
    pub const CHOICES: [(Self, &'static str); 4] =
        [(Self::Nothing, "Do nothing"), (Self::Quit, "Quit the app"), (Self::Sleep, "Sleep"), (Self::ShutDown, "Shut down")];

    pub fn name(self) -> &'static str {
        Self::CHOICES.iter().find(|(choice, _)| *choice == self).map_or("", |(_, name)| name)
    }
}

/// Puts the computer to sleep or shuts it down, as `action` says (the others do nothing).
#[cfg(not(test))]
pub fn power(action: WhenDone) -> io::Result<()> {
    use std::process::Command;
    let run = |command: &mut Command| -> io::Result<()> {
        let status = command.status()?;
        match status.success() {
            true => Ok(()),
            false => Err(io::Error::other(format!("{} ended with {}", command.get_program().to_string_lossy(), status))),
        }
    };
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        match action {
            WhenDone::Sleep => {
                // SAFETY: takes three flags (no hibernation, no forcing, wake-up events kept).
                if unsafe { windows_sys::Win32::System::Power::SetSuspendState(0, 0, 0) } == 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            }
            WhenDone::ShutDown => {
                let system = std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
                // CREATE_NO_WINDOW: no console flashes up.
                run(Command::new(system.join("System32").join("shutdown.exe")).args(["/s", "/t", "0"]).creation_flags(0x0800_0000))
            }
            _ => Ok(()),
        }
    }
    #[cfg(target_os = "macos")]
    match action {
        WhenDone::Sleep => run(Command::new("/usr/bin/pmset").arg("sleepnow")),
        WhenDone::ShutDown => run(Command::new("/usr/bin/osascript").args(["-e", "tell app \"System Events\" to shut down"])),
        _ => Ok(()),
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    match action {
        WhenDone::Sleep => run(Command::new("systemctl").arg("suspend")),
        WhenDone::ShutDown => run(Command::new("systemctl").arg("poweroff")),
        _ => Ok(()),
    }
}

/// Tests never put the computer they run on to sleep or shut it down.
#[cfg(test)]
pub fn power(_: WhenDone) -> io::Result<()> {
    Err(io::Error::other("tests never sleep or shut down the computer"))
}

/// Minutes after midnight of a time written "H:MM" or "HH:MM"; None for anything else.
pub fn minute_of(text: &str) -> Option<u16> {
    let (hours, minutes) = text.trim().split_once(':')?;
    let digits = |s: &str, len: std::ops::RangeInclusive<usize>| len.contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(hours, 1..=2) || !digits(minutes, 2..=2) {
        return None;
    }
    let (hours, minutes) = (hours.parse::<u16>().ok()?, minutes.parse::<u16>().ok()?);
    (hours < 24 && minutes < 60).then_some(hours * 60 + minutes)
}

/// What the schedule does as the local time moves from `last` to `now` (the day of the week,
/// Monday 0, and the minute of the day): `Some(true)` starts the queue, at its start time on a
/// ticked day; `Some(false)` pauses it, at its stop time on any day (so that a night begun on a
/// ticked day ends). When both came, the later wins. A first check (no `last`: at launch, or the
/// schedule just turned on) starts the queue inside the time it runs (see `inside`), whose
/// start may have come while the app was not running; else it takes only the minute it is on.
/// A minute is never taken twice.
pub fn schedule_due(schedule: &Schedule, last: Option<(u8, u16)>, now: (u8, u16)) -> Option<bool> {
    if !schedule.enabled {
        return None;
    }
    let from = match last {
        Some((day, minute)) if day == now.0 => minute + 1,
        // A new day: since its midnight.
        Some(_) => 0,
        None if inside(schedule, now) => return Some(true),
        None => now.1,
    };
    let came = |at: &Option<String>| at.as_deref().and_then(minute_of).filter(|at| (from..=now.1).contains(at));
    let start = came(&schedule.start).filter(|_| schedule.days[usize::from(now.0) % 7]);
    match (start, came(&schedule.stop)) {
        (Some(start), Some(stop)) => Some(start > stop),
        (start, stop) => start.map(|_| true).or(stop.map(|_| false)),
    }
}

/// Whether `now` is between the start time of a ticked day and the stop time after it (that
/// day's end when there is none), a night running into the next day.
fn inside(schedule: &Schedule, (day, minute): (u8, u16)) -> bool {
    let Some(start) = schedule.start.as_deref().and_then(minute_of) else { return false };
    let ticked = |day: u8| schedule.days[usize::from(day) % 7];
    match schedule.stop.as_deref().and_then(minute_of) {
        Some(stop) if stop < start => (minute >= start && ticked(day)) || (minute < stop && ticked((day + 6) % 7)),
        Some(stop) => (start..stop).contains(&minute) && ticked(day),
        None => minute >= start && ticked(day),
    }
}

/// The local day of the week (Monday 0) and minute of the day.
#[cfg(windows)]
pub fn local_now() -> (u8, u16) {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    // SAFETY: fills the struct it is given.
    let time = unsafe {
        let mut time = std::mem::zeroed::<SYSTEMTIME>();
        windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut time);
        time
    };
    (((time.wDayOfWeek + 6) % 7) as u8, time.wHour * 60 + time.wMinute)
}

/// The local day of the week (Monday 0) and minute of the day.
#[cfg(unix)]
pub fn local_now() -> (u8, u16) {
    // SAFETY: `time` reads the clock, and `localtime_r` fills the struct it is given from it.
    let tm = unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm = std::mem::zeroed::<libc::tm>();
        libc::localtime_r(&now, &mut tm);
        tm
    };
    (((tm.tm_wday + 6) % 7) as u8, (tm.tm_hour * 60 + tm.tm_min) as u16)
}

/// How long until the next minute begins, when the schedule is checked again.
pub fn until_next_minute() -> Duration {
    let seconds = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    Duration::from_secs(60 - seconds % 60)
}

/// The log file (`logs/endo.log` in the data folder): what is logged goes there too, its links
/// without their secrets (see `redact_text`), and once it reaches its limit it is renamed
/// `endo.log.1`, replacing the one before, so the two take at most twice the limit.
pub struct LogFile {
    path: PathBuf,
    limit: u64,
    /// The file open for appending, and its size.
    file: Option<(File, u64)>,
}

impl LogFile {
    pub fn new(path: PathBuf, limit: u64) -> Self {
        Self { path, limit, file: None }
    }
}

impl Write for LogFile {
    /// Takes one event's text (the formatter writes each in one go).
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let text = redact_text(&String::from_utf8_lossy(buf));
        let size = match &self.file {
            Some((_, size)) => *size,
            None => std::fs::metadata(&self.path).map_or(0, |file| file.len()),
        };
        if size > 0 && size + text.len() as u64 > self.limit {
            self.file = None;
            let mut old = self.path.clone().into_os_string();
            old.push(".1");
            std::fs::rename(&self.path, old)?;
        }
        if self.file.is_none() {
            if let Some(dir) = self.path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let file = OpenOptions::new().create(true).append(true).open(&self.path)?;
            let size = file.metadata()?.len();
            self.file = Some((file, size));
        }
        if let Some((file, size)) = &mut self.file {
            file.write_all(text.as_bytes())?;
            *size += text.len() as u64;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.as_mut().map_or(Ok(()), |(file, _)| file.flush())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launches_are_read_from_the_command_line() {
        let parse = |args: &[&str]| Launch::parse(args.iter().map(OsString::from));
        assert_eq!(parse(&[]), Launch::default());
        assert_eq!(parse(&["--minimized"]), Launch { minimized: true, data_dir: None });
        // A link only starts the app, hidden; whatever it carries is not read.
        assert!(parse(&["--minimized", "endo-dl://add?url=https://x.example/a.iso"]).minimized);
        assert!(parse(&["ENDO-DL:open"]).minimized);
        assert_eq!(parse(&["--data-dir", "D:\\Endo data", "--other"]), Launch { minimized: false, data_dir: Some("D:\\Endo data".into()) });
        assert_eq!(parse(&["--data-dir"]).data_dir, None);
        // A `"` in a link splits it into more arguments: none of them is read.
        assert_eq!(parse(&["--minimized", "endo-dl:x", "--data-dir", "D:\\x"]), Launch { minimized: true, data_dir: None });
    }

    /// A portable copy, or a run with a history file of its own (as tests have), leaves the
    /// `endo-dl:` link to the installed app.
    #[test]
    fn only_the_installed_app_takes_endo_dl_links() {
        crate::tests::isolate_history();
        assert!(!takes_links(true));
        assert!(!takes_links(false), "ENDO_HISTORY_PATH is set");
    }

    /// One app holds a data folder at a time; once it ends, the next one may.
    #[test]
    fn one_app_holds_a_data_folder() {
        let dir = tempfile::tempdir().unwrap();
        let first = lock_instance(&dir.path().join("data")).unwrap().expect("the folder is free");
        assert!(lock_instance(&dir.path().join("data")).unwrap().is_none(), "the first holds it");
        drop(first);
        assert!(lock_instance(&dir.path().join("data")).unwrap().is_some(), "free again");
    }

    #[test]
    fn a_portable_copy_keeps_its_data_next_to_it() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("app.exe");
        assert_eq!(portable_dir(None, Some(&exe)), None);
        std::fs::write(dir.path().join("portable"), "").unwrap();
        assert_eq!(portable_dir(None, Some(&exe)), Some(dir.path().join("data")));
        let given = dir.path().join("elsewhere");
        assert_eq!(portable_dir(Some(given.clone()), Some(&exe)), Some(given.clone()), "--data-dir wins");
        assert_eq!(portable_dir(Some(given.clone()), None), Some(given));
        assert!(portable_dir(Some("relative".into()), None).unwrap().is_absolute());
    }

    #[test]
    fn data_folders_have_short_distinct_tags() {
        let (a, b) = (data_tag(Path::new("/home/u/.local/share/EndosUnifiedDownloader")), data_tag(Path::new("/media/usb/Endo/data")));
        assert_eq!((a.len(), b.len()), (8, 8));
        assert_ne!(a, b);
        assert_eq!(a, data_tag(Path::new("/home/u/.local/share/EndosUnifiedDownloader")), "the same folder, the same tag");
    }

    /// Launch at startup starts the program hidden, from where each system looks.
    #[test]
    fn launch_at_startup_starts_the_program_hidden() {
        let root = Path::new("/r");
        let (place, value) = startup_entry(Path::new("/Apps/Endo & co/endo"), root);
        if cfg!(windows) {
            let name = Some("EndosUnifiedDownloader".to_string());
            assert_eq!(place, Place::Registry { key: r"Software\Microsoft\Windows\CurrentVersion\Run".into(), name });
            assert_eq!(value, "\"/Apps/Endo & co/endo\" --minimized");
        } else if cfg!(target_os = "macos") {
            assert_eq!(place, Place::File(root.join("LaunchAgents/io.github.adityasharma0101911.endos-unified-downloader.plist")));
            assert!(value.contains("<string>/Apps/Endo &amp; co/endo</string>\n\t\t<string>--minimized</string>"), "{value}");
            assert!(value.contains("<key>RunAtLoad</key>\n\t<true/>"), "{value}");
        } else {
            assert_eq!(place, Place::File(root.join("autostart/endos-unified-downloader.desktop")));
            assert!(value.contains("\nExec=\"/Apps/Endo & co/endo\" --minimized\n"), "{value}");
        }
        assert_eq!(exec_arg("/opt/a \"b\"/$x 100%"), "\"/opt/a \\\\\"b\\\\\"/\\\\$x 100%%\"");
    }

    #[test]
    fn endo_dl_links_start_the_program_hidden() {
        let entries = protocol_entries(Path::new(r"C:\Apps\endo.exe"));
        let key = |key: &str, name: Option<&str>| Place::Registry { key: key.into(), name: name.map(str::to_string) };
        assert_eq!(
            entries,
            [
                (key(r"Software\Classes\endo-dl", None), "URL:Endo's Unified Downloader".to_string()),
                (key(r"Software\Classes\endo-dl", Some("URL Protocol")), String::new()),
                (key(r"Software\Classes\endo-dl\shell\open\command", None), r#""C:\Apps\endo.exe" --minimized "%1""#.to_string()),
            ]
        );
    }

    #[test]
    fn startup_files_are_written_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("autostart").join("endo.desktop");
        write_file(&file, Some("[Desktop Entry]\n")).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "[Desktop Entry]\n");
        write_file(&file, None).unwrap();
        assert!(!file.exists());
        write_file(&file, None).expect("one already gone is fine");
    }

    #[test]
    fn tests_never_change_the_system() {
        let run = Place::Registry { key: "Software".into(), name: Some("x".into()) };
        assert!(write(&run, Some("x")).is_err());
        for action in [WhenDone::Sleep, WhenDone::ShutDown] {
            assert!(power(action).is_err());
        }
    }

    #[test]
    fn schedule_times_are_hours_and_minutes() {
        assert_eq!(minute_of("09:30"), Some(570));
        assert_eq!(minute_of(" 9:05 "), Some(545));
        assert_eq!(minute_of("23:59"), Some(1439));
        assert_eq!(minute_of("0:00"), Some(0));
        for bad in ["24:00", "9:5", "12:60", "+9:00", "9", "", "ab:cd", "123:00"] {
            assert_eq!(minute_of(bad), None, "{bad}");
        }
    }

    /// The queue starts at the start time on ticked days and pauses at the stop time on any day,
    /// each once, also when a check comes late or the day changed between checks.
    #[test]
    fn the_schedule_starts_and_stops_the_queue_once() {
        let mut days = [false; 7];
        days[0] = true;
        let schedule = Schedule { enabled: true, start: Some("22:00".into()), stop: Some("06:30".into()), days };
        let (monday, tuesday, wednesday) = (0, 1, 2);
        assert_eq!(schedule_due(&schedule, None, (monday, 22 * 60)), Some(true));
        assert_eq!(schedule_due(&schedule, Some((monday, 22 * 60)), (monday, 22 * 60)), None, "a minute is taken once");
        assert_eq!(schedule_due(&schedule, Some((monday, 21 * 60 + 58)), (monday, 22 * 60 + 3)), Some(true), "a late check");
        // A first check (a launch) inside the night starts it, its start having come before.
        assert_eq!(schedule_due(&schedule, None, (monday, 22 * 60 + 1)), Some(true));
        assert_eq!(schedule_due(&schedule, None, (tuesday, 3 * 60)), Some(true), "the night begun on Monday");
        assert_eq!(schedule_due(&schedule, None, (tuesday, 7 * 60)), None, "a first check takes its own minute only");
        assert_eq!(schedule_due(&schedule, None, (tuesday, 6 * 60 + 30)), Some(false));
        assert_eq!(schedule_due(&schedule, None, (wednesday, 3 * 60)), None, "Tuesday's night is not ticked");
        assert_eq!(schedule_due(&schedule, None, (tuesday, 22 * 60)), None, "Tuesday is not ticked");
        let day = Schedule { start: Some("08:00".into()), stop: Some("18:00".into()), days: [true; 7], ..schedule.clone() };
        assert_eq!(schedule_due(&day, None, (tuesday, 12 * 60)), Some(true));
        assert_eq!(schedule_due(&day, None, (tuesday, 19 * 60)), None);
        assert_eq!(schedule_due(&Schedule { stop: None, ..day.clone() }, None, (tuesday, 23 * 60)), Some(true), "no stop: until midnight");
        assert_eq!(schedule_due(&Schedule { start: None, ..day }, None, (tuesday, 12 * 60)), None, "nothing to start");
        assert_eq!(schedule_due(&schedule, Some((monday, 23 * 60)), (tuesday, 6 * 60 + 31)), Some(false), "the night ends on any day");
        assert_eq!(schedule_due(&schedule, Some((tuesday, 6 * 60 + 31)), (tuesday, 7 * 60)), None);
        let both = Schedule { start: Some("06:00".into()), stop: Some("06:10".into()), ..schedule.clone() };
        assert_eq!(schedule_due(&both, Some((monday, 5 * 60)), (monday, 7 * 60)), Some(false), "the later one wins");
        assert_eq!(schedule_due(&Schedule { enabled: false, ..schedule.clone() }, None, (monday, 22 * 60)), None);
        assert_eq!(schedule_due(&Schedule { start: Some("soon".into()), ..schedule }, None, (monday, 22 * 60)), None);
    }

    #[test]
    fn the_local_time_is_a_day_and_a_minute() {
        let (day, minute) = local_now();
        assert!(day < 7 && minute < 24 * 60);
        assert!(until_next_minute() <= Duration::from_secs(60));
    }

    /// The log is written without the secrets of its links and kept under twice its limit: the
    /// file is renamed `.1` (replacing the one before) once it is full.
    #[test]
    fn the_log_is_redacted_and_rotated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs").join("endo.log");
        let mut log = LogFile::new(path.clone(), 100);
        log.write_all(b"fetching https://me:secret@files.example/a.iso\n").unwrap();
        let first = std::fs::read_to_string(&path).unwrap();
        assert!(!first.contains("secret") && first.contains("files.example/a.iso"), "{first}");
        for line in 0..10 {
            log.write_all(format!("line {line} of the log, thirty bytes\n").as_bytes()).unwrap();
        }
        log.flush().unwrap();
        let old = dir.path().join("logs").join("endo.log.1");
        let (now, before) = (std::fs::read_to_string(&path).unwrap(), std::fs::read_to_string(&old).unwrap());
        assert!(now.len() <= 100 && before.len() <= 100, "{now:?} {before:?}");
        assert!(now.ends_with("line 9 of the log, thirty bytes\n"), "{now:?}");
        assert_eq!(std::fs::read_dir(path.parent().unwrap()).unwrap().count(), 2, "one old file is kept");
        // A file already there is appended to, its size counted.
        drop(log);
        let mut log = LogFile::new(path.clone(), 100);
        log.write_all(b"again\n").unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().len() <= 100);
    }
}
