//! A tiny atomic JSON key-value file. Writes go to a temp file in the same
//! folder and are renamed into place, so a crash can't leave a half-written
//! file. Reads come from an in-memory copy that is revalidated against the
//! file's (mtime, size) on every call, so a hand-edit is picked up at once
//! without re-parsing JSON on every chat command.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use serde_json::{Map, Value};

pub type JsonMap = Map<String, Value>;
type StatKey = (Option<SystemTime>, u64);

#[derive(Default)]
struct Cache {
    data: Option<JsonMap>,
    key: Option<StatKey>,
}

pub struct JsonStore {
    path: PathBuf,
    cache: Mutex<Cache>,
    private: bool,
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

impl JsonStore {
    pub fn new(path: impl Into<PathBuf>) -> JsonStore {
        JsonStore { path: path.into(), cache: Mutex::new(Cache::default()), private: false }
    }

    /// For files holding secrets: on unix they are written readable by the owner only.
    pub fn private(mut self) -> JsonStore {
        self.private = true;
        self
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn stat_key(&self) -> Option<StatKey> {
        let meta = fs::metadata(&self.path).ok()?;
        Some((meta.modified().ok(), meta.len()))
    }

    fn load_locked(&self, cache: &mut Cache) -> JsonMap {
        let key = self.stat_key();
        if let (Some(data), true) = (&cache.data, key == cache.key) {
            return data.clone();
        }
        let data = self.read_file();
        cache.data = Some(data.clone());
        // Re-stat after reading, so a write that landed mid-read isn't cached under the old key.
        cache.key = self.stat_key();
        data
    }

    fn read_file(&self) -> JsonMap {
        match fs::read(&self.path) {
            Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
                Ok(Value::Object(map)) => map,
                Ok(_) => {
                    crate::warn!("{} did not contain a JSON object - falling back to defaults.", self.path.display());
                    JsonMap::new()
                }
                Err(e) => {
                    crate::warn!("Couldn't parse {} ({e}) - falling back to defaults.", self.path.display());
                    JsonMap::new()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => JsonMap::new(),
            Err(e) => {
                crate::warn!("Couldn't read {} ({e}) - falling back to defaults.", self.path.display());
                JsonMap::new()
            }
        }
    }

    pub fn read(&self) -> JsonMap {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        self.load_locked(&mut cache)
    }

    pub fn write(&self, data: JsonMap) -> std::io::Result<()> {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        self.write_locked(&mut cache, data)
    }

    /// Read-modify-write under one lock so two concurrent callers can't
    /// clobber each other. `f` returns the map to persist, or `None` to leave
    /// the file untouched. Returns the resulting contents.
    pub fn update(&self, f: impl FnOnce(JsonMap) -> Option<JsonMap>) -> std::io::Result<JsonMap> {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        let current = self.load_locked(&mut cache);
        match f(current.clone()) {
            Some(updated) => {
                self.write_locked(&mut cache, updated.clone())?;
                Ok(updated)
            }
            None => Ok(current),
        }
    }

    fn write_locked(&self, cache: &mut Cache, data: JsonMap) -> std::io::Result<()> {
        let dir = self.path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(dir)?;
        let name = self.path.file_name().and_then(|n| n.to_str()).unwrap_or("store");
        let tmp = dir.join(format!(".{name}.{}.{}.tmp", std::process::id(), TMP_COUNTER.fetch_add(1, Ordering::Relaxed)));
        let result = (|| {
            let mut file = fs::File::create(&tmp)?;
            file.write_all(&serde_json::to_vec_pretty(&data).map_err(std::io::Error::other)?)?;
            file.sync_all()?;
            #[cfg(unix)]
            if self.private {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
            }
            fs::rename(&tmp, &self.path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
            return result;
        }
        cache.data = Some(data);
        cache.key = self.stat_key();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_path(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pryxea-store-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir.join("sub").join("state.json")
    }

    #[test]
    fn missing_file_reads_as_empty_and_write_creates_folders() {
        let path = temp_path("create");
        let store = JsonStore::new(&path);
        assert!(store.read().is_empty());
        let mut m = JsonMap::new();
        m.insert("a".into(), json!(1));
        store.write(m).unwrap();
        assert_eq!(store.read()["a"], json!(1));
        // No temp files left behind.
        let leftovers: Vec<_> = fs::read_dir(path.parent().unwrap()).unwrap().flatten().collect();
        assert_eq!(leftovers.len(), 1);
    }

    #[test]
    fn hand_edits_are_picked_up_without_a_restart() {
        let path = temp_path("edit");
        let store = JsonStore::new(&path);
        store.write(JsonMap::from_iter([("k".to_string(), json!("old"))])).unwrap();
        assert_eq!(store.read()["k"], json!("old"));
        // A different length guarantees a changed stat key even on coarse timestamps.
        fs::write(&path, r#"{"k": "a-much-longer-value"}"#).unwrap();
        assert_eq!(store.read()["k"], json!("a-much-longer-value"));
    }

    #[cfg(unix)]
    #[test]
    fn private_stores_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp_path("private");
        let store = JsonStore::new(&path).private();
        store.write(JsonMap::from_iter([("t".to_string(), json!("secret"))])).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn corrupt_file_falls_back_to_empty() {
        let path = temp_path("corrupt");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{not json").unwrap();
        assert!(JsonStore::new(&path).read().is_empty());
        fs::write(&path, "[1,2,3]").unwrap();
        assert!(JsonStore::new(&path).read().is_empty());
    }

    #[test]
    fn update_is_read_modify_write_and_none_leaves_the_file() {
        let path = temp_path("update");
        let store = JsonStore::new(&path);
        store.update(|mut m| { m.insert("n".into(), json!(1)); Some(m) }).unwrap();
        let after = store.update(|mut m| { m.insert("n".into(), json!(2)); Some(m) }).unwrap();
        assert_eq!(after["n"], json!(2));
        let untouched = store.update(|_| None).unwrap();
        assert_eq!(untouched["n"], json!(2));
        let on_disk: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(on_disk["n"], json!(2));
    }
}
