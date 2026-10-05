//! A deliberately tiny logger: leveled lines to stderr and, optionally, a
//! size-capped file. No dependencies, no per-call allocation beyond the line.

use std::fmt::Arguments;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    /// Accepts the same names the Python bot's `LOG_LEVEL` did.
    pub fn parse(s: &str) -> Option<Level> {
        match s.trim().to_ascii_uppercase().as_str() {
            "DEBUG" => Some(Level::Debug),
            "INFO" => Some(Level::Info),
            "WARNING" | "WARN" => Some(Level::Warn),
            "ERROR" | "CRITICAL" => Some(Level::Error),
            _ => None,
        }
    }

    fn tag(self) -> &'static str {
        match self {
            Level::Debug => "DEBUG",
            Level::Info => "INFO ",
            Level::Warn => "WARN ",
            Level::Error => "ERROR",
        }
    }
}

/// Rotate to `<name>.1` once the file passes this size. Two files, ~2 MB max.
const MAX_LOG_BYTES: u64 = 1_000_000;

struct FileSink {
    path: PathBuf,
    file: File,
    written: u64,
}

struct Sink {
    min: Level,
    file: Option<FileSink>,
}

static SINK: OnceLock<Mutex<Sink>> = OnceLock::new();

/// Installs the process-wide logger. Calling it twice keeps the first setup.
pub fn init(min: Level, log_file: Option<PathBuf>) {
    let file = log_file.and_then(open_sink);
    let _ = SINK.set(Mutex::new(Sink { min, file }));
}

fn open_sink(path: PathBuf) -> Option<FileSink> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).ok()?;
    }
    let file = OpenOptions::new().create(true).append(true).open(&path).ok()?;
    let written = file.metadata().map(|m| m.len()).unwrap_or(0);
    Some(FileSink { path, file, written })
}

fn rotate(sink: &mut FileSink) {
    let mut backup = sink.path.clone().into_os_string();
    backup.push(".1");
    let _ = fs::rename(&sink.path, PathBuf::from(backup));
    if let Ok(file) = OpenOptions::new().create(true).append(true).open(&sink.path) {
        sink.file = file;
        sink.written = 0;
    }
}

pub fn log(level: Level, target: &str, args: Arguments<'_>) {
    let sink = SINK.get_or_init(|| Mutex::new(Sink { min: Level::Info, file: None }));
    let mut sink = sink.lock().unwrap_or_else(|e| e.into_inner());
    if level < sink.min {
        return;
    }
    let target = target.strip_prefix("pryxea::").unwrap_or(target);
    let line = format!("{} {} {}: {}\n", timestamp(), level.tag(), target, args);
    let _ = std::io::stderr().write_all(line.as_bytes());
    if let Some(file) = sink.file.as_mut() {
        if file.written > MAX_LOG_BYTES {
            rotate(file);
        }
        if file.file.write_all(line.as_bytes()).is_ok() {
            file.written += line.len() as u64;
        }
    }
}

fn timestamp() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (y, mo, d, h, mi, s) = civil(secs);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

/// Unix seconds -> UTC calendar fields (Howard Hinnant's days-from-civil, inverted).
fn civil(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day, (rem / 3600) as u32, ((rem % 3600) / 60) as u32, (rem % 60) as u32)
}

#[macro_export]
macro_rules! debug {
    ($($t:tt)*) => { $crate::logging::log($crate::logging::Level::Debug, module_path!(), format_args!($($t)*)) };
}
#[macro_export]
macro_rules! info {
    ($($t:tt)*) => { $crate::logging::log($crate::logging::Level::Info, module_path!(), format_args!($($t)*)) };
}
#[macro_export]
macro_rules! warn {
    ($($t:tt)*) => { $crate::logging::log($crate::logging::Level::Warn, module_path!(), format_args!($($t)*)) };
}
#[macro_export]
macro_rules! error {
    ($($t:tt)*) => { $crate::logging::log($crate::logging::Level::Error, module_path!(), format_args!($($t)*)) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_matches_known_dates() {
        assert_eq!(civil(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(civil(1_700_000_000), (2023, 11, 14, 22, 13, 20));
        // Leap day.
        assert_eq!(civil(1_709_164_800), (2024, 2, 29, 0, 0, 0));
    }

    #[test]
    fn level_names_match_the_python_bot() {
        assert_eq!(Level::parse("warning"), Some(Level::Warn));
        assert_eq!(Level::parse(" CRITICAL "), Some(Level::Error));
        assert_eq!(Level::parse("loud"), None);
        assert!(Level::Debug < Level::Error);
    }
}
