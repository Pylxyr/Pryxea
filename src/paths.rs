//! Where things live. One writable home folder holds `.env`, `data/` and
//! `logs/`; it survives updates and needs no admin rights.

use std::path::{Path, PathBuf};

pub const APP_DIR_NAME: &str = "Pryxea";

#[derive(Debug, Clone)]
pub struct Dirs {
    pub home: PathBuf,
    pub env_file: PathBuf,
    pub data: PathBuf,
    pub logs: PathBuf,
}

impl Dirs {
    pub fn at(home: impl Into<PathBuf>) -> Dirs {
        let home = home.into();
        Dirs {
            env_file: home.join(".env"),
            data: home.join("data"),
            logs: home.join("logs"),
            home,
        }
    }

    /// `PRYXEA_HOME` wins; otherwise the platform's per-user app-data folder.
    pub fn resolve() -> Dirs {
        Dirs::at(home_dir(|k| std::env::var(k).ok()))
    }

    pub fn create(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.data)?;
        std::fs::create_dir_all(&self.logs)
    }
}

fn nonempty(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.trim().is_empty())
}

pub fn home_dir(get: impl Fn(&str) -> Option<String>) -> PathBuf {
    if let Some(p) = nonempty(get("PRYXEA_HOME")) {
        return PathBuf::from(p);
    }
    if cfg!(windows) {
        if let Some(base) = nonempty(get("APPDATA")) {
            return Path::new(&base).join(APP_DIR_NAME);
        }
    } else if cfg!(target_os = "macos") {
        if let Some(home) = nonempty(get("HOME")) {
            return Path::new(&home).join("Library/Application Support").join(APP_DIR_NAME);
        }
    } else {
        if let Some(base) = nonempty(get("XDG_DATA_HOME")) {
            return Path::new(&base).join("pryxea");
        }
        if let Some(home) = nonempty(get("HOME")) {
            return Path::new(&home).join(".local/share/pryxea");
        }
    }
    PathBuf::from(".")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_wins() {
        let p = home_dir(|k| (k == "PRYXEA_HOME").then(|| "/tmp/x".to_string()));
        assert_eq!(p, PathBuf::from("/tmp/x"));
    }

    #[test]
    fn layout_is_home_relative() {
        let d = Dirs::at("/h");
        assert_eq!(d.env_file, PathBuf::from("/h/.env"));
        assert_eq!(d.data, PathBuf::from("/h/data"));
        assert_eq!(d.logs, PathBuf::from("/h/logs"));
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn linux_prefers_xdg_then_home() {
        let p = home_dir(|k| (k == "XDG_DATA_HOME").then(|| "/x".to_string()));
        assert_eq!(p, PathBuf::from("/x/pryxea"));
        let p = home_dir(|k| (k == "HOME").then(|| "/home/u".to_string()));
        assert_eq!(p, PathBuf::from("/home/u/.local/share/pryxea"));
    }
}
