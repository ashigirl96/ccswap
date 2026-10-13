//! Concurrent engines share `autoswitch_state.json` only through
//! read-modify-write under `.autoswitch_state.lock`.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::error::Result;
use crate::locks::{STORE_LOCK_TIMEOUT, StoreLock};
use crate::settings::atomic_write_json;

pub const STATE_FILENAME: &str = "autoswitch_state.json";
pub const STATE_SCHEMA_VERSION: i64 = 1;

pub type State = Map<String, Value>;

#[derive(Debug, Clone)]
pub struct StateFile {
    path: PathBuf,
}

impl StateFile {
    pub fn new(path: PathBuf) -> Self {
        StateFile { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn lock_path(&self) -> PathBuf {
        self.path
            .parent()
            .unwrap_or(Path::new("."))
            .join(".autoswitch_state.lock")
    }

    pub fn lock(&self) -> Result<StoreLock> {
        StoreLock::acquire(&self.lock_path(), STORE_LOCK_TIMEOUT)
    }

    /// Unreadable or not an object reads as empty.
    pub fn read(&self) -> State {
        std::fs::read(&self.path)
            .ok()
            .and_then(|b| String::from_utf8(b).ok())
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| match v {
                Value::Object(m) => Some(m),
                _ => None,
            })
            .unwrap_or_default()
    }

    pub fn write(&self, state: &State) -> Result<()> {
        atomic_write_json(&self.path, &Value::Object(state.clone()))
    }

    /// Read-modify-write under the state lock; returns the written state.
    /// Must not be called while another lock is held.
    pub fn mutate(&self, mutator: impl FnOnce(&mut State)) -> Result<State> {
        let _lock = self.lock()?;
        let mut state = self.read();
        state.insert("schemaVersion".into(), STATE_SCHEMA_VERSION.into());
        mutator(&mut state);
        self.write(&state)?;
        Ok(state)
    }
}
