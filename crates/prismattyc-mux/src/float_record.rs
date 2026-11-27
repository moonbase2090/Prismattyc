//! Client overlay placements stored beside a Space directory.
//!
//! The server session file stays tiled. This record is keyed by the space,
//! the session name, and the pane's place in that session. Pane ids restart
//! when the server does, so they are not part of the record.

use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::layout_file::validate_layout_name;

const FLOAT_PANES_FILE: &str = "float-panes.json";
static FLOAT_WRITE_ID: AtomicU64 = AtomicU64::new(0);

/// One overlay. `window_slot` is the window's index in the session.
/// `pane_slot` is the pane's depth-first index in that window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedFloat {
    pub space: String,
    pub session: String,
    pub window_slot: u32,
    pub pane_slot: u32,
    pub col: u32,
    pub row: u32,
    pub cols: u32,
    pub rows: u32,
}

/// Every overlay remembered for the spaces in one directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SavedFloats {
    #[serde(default)]
    pub floats: Vec<SavedFloat>,
}

impl SavedFloat {
    fn same_place(&self, other: &Self) -> bool {
        self.space == other.space
            && self.session == other.session
            && self.window_slot == other.window_slot
            && self.pane_slot == other.pane_slot
    }
}

impl SavedFloats {
    /// The overlay at this place, if one was saved.
    #[must_use]
    pub fn find(
        &self,
        space: &str,
        session: &str,
        window_slot: u32,
        pane_slot: u32,
    ) -> Option<&SavedFloat> {
        self.floats.iter().find(|saved| {
            saved.space == space
                && saved.session == session
                && saved.window_slot == window_slot
                && saved.pane_slot == pane_slot
        })
    }

    /// Insert `float`, or replace the record that names the same place.
    pub fn upsert(&mut self, float: SavedFloat) {
        if let Some(existing) = self
            .floats
            .iter_mut()
            .find(|saved| saved.same_place(&float))
        {
            *existing = float;
        } else {
            self.floats.push(float);
        }
    }
}

/// `{dir}/float-panes.json`. This is not a space file.
#[must_use]
pub fn float_panes_path(dir: &Path) -> PathBuf {
    dir.join(FLOAT_PANES_FILE)
}

/// Read the overlay file. A missing file is an empty record.
pub fn load_floats(dir: &Path) -> Result<SavedFloats> {
    let path = float_panes_path(dir);
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(SavedFloats::default());
        }
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let parsed: SavedFloats =
        serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    for float in &parsed.floats {
        validate_float(float)?;
    }
    Ok(parsed)
}

/// Replace the overlay file. Validates every record before writing.
pub fn save_floats(dir: &Path, floats: &SavedFloats) -> Result<PathBuf> {
    for float in &floats.floats {
        validate_float(float)?;
    }
    crate::private_fs::create_dir(dir).with_context(|| format!("create {}", dir.display()))?;
    let path = float_panes_path(dir);
    let mut bytes = serde_json::to_vec_pretty(floats).context("serialize float record")?;
    bytes.push(b'\n');
    write_atomic(dir, &path, &bytes)?;
    Ok(path)
}

fn validate_float(float: &SavedFloat) -> Result<()> {
    validate_layout_name(&float.space).context("float space name")?;
    validate_layout_name(&float.session).context("float session name")?;
    if float.cols == 0 || float.rows == 0 {
        bail!("a float record needs a size");
    }
    Ok(())
}

fn write_atomic(dir: &Path, path: &Path, bytes: &[u8]) -> Result<()> {
    let nonce = FLOAT_WRITE_ID.fetch_add(1, Ordering::Relaxed);
    let temporary = dir.join(format!(
        ".{FLOAT_PANES_FILE}.{}.{nonce}.tmp",
        std::process::id()
    ));
    let result = (|| -> Result<()> {
        let mut file = crate::private_fs::open(
            &temporary,
            fs::OpenOptions::new().write(true).create_new(true),
        )
        .with_context(|| format!("write {}", temporary.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("write {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("sync {}", temporary.display()))?;
        drop(file);
        crate::platform::replace_file(&temporary, path)
            .with_context(|| format!("replace {}", path.display()))?;
        crate::platform::sync_directory(dir)
            .with_context(|| format!("sync directory {}", dir.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn sample(pane_slot: u32, col: u32) -> SavedFloat {
        SavedFloat {
            space: "work".to_string(),
            session: "default".to_string(),
            window_slot: 0,
            pane_slot,
            col,
            row: 4,
            cols: 30,
            rows: 8,
        }
    }

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "prism-float-record-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn float_record_round_trips_by_place_and_a_missing_file_is_empty() {
        let dir = scratch();
        assert_eq!(load_floats(&dir).unwrap(), SavedFloats::default());

        let mut rejected = SavedFloats::default();
        let mut nameless = sample(1, 10);
        nameless.session.clear();
        rejected.upsert(nameless);
        assert!(save_floats(&dir, &rejected).is_err());
        assert!(!float_panes_path(&dir).exists());

        let mut saved = SavedFloats::default();
        saved.upsert(sample(1, 10));
        saved.upsert(sample(1, 12));
        saved.upsert(sample(2, 3));
        assert_eq!(saved.floats.len(), 2, "the same place replaces");
        assert_eq!(saved.find("work", "default", 0, 1).unwrap().col, 12);

        let path = save_floats(&dir, &saved).unwrap();
        assert_eq!(path, dir.join("float-panes.json"));
        let loaded = load_floats(&dir).unwrap();
        assert_eq!(loaded, saved);
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("pane_id"), "the place is not a pane id");
        let value = serde_json::to_value(loaded.find("work", "default", 0, 1).unwrap()).unwrap();
        let keys: BTreeSet<_> = value.as_object().unwrap().keys().cloned().collect();
        assert_eq!(
            keys,
            BTreeSet::from([
                "space".to_string(),
                "session".to_string(),
                "window_slot".to_string(),
                "pane_slot".to_string(),
                "col".to_string(),
                "row".to_string(),
                "cols".to_string(),
                "rows".to_string(),
            ])
        );
        let listed = crate::layout_file::list_spaces(&dir).unwrap();
        assert!(
            listed.iter().all(|entry| entry.name != "float-panes"),
            "the overlay file is not a space"
        );

        fs::write(&path, b"{}\n").unwrap();
        let empty_object = load_floats(&dir).unwrap();
        assert!(empty_object.floats.is_empty());
        fs::write(&path, b"not json\n").unwrap();
        assert!(load_floats(&dir).is_err(), "a broken file is not dropped");
        fs::write(
            &path,
            b"{\"floats\":[{\"space\":\"work\",\"session\":\"default\",\"window_slot\":0,\"pane_slot\":1,\"col\":1,\"row\":1,\"cols\":0,\"rows\":8}]}\n",
        )
        .unwrap();
        assert!(load_floats(&dir).is_err(), "a zero size is refused");
        let _ = fs::remove_dir_all(&dir);
    }
}
