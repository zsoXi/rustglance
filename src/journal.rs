//! Append-only JSONL record of every call. One file serves as evidence,
//! as timing measurements, and as a script `replay` can run again.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::Value;

use crate::util::lock;

pub struct Journal {
    path: PathBuf,
    file: Mutex<File>,
}

impl Journal {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            file: Mutex::new(file),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends one entry as one line.
    pub fn write(&self, entry: &Value) -> io::Result<()> {
        let mut line = entry.to_string();
        line.push('\n');
        lock(&self.file).write_all(line.as_bytes())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error("cannot read {path:?}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{path:?} line {line} is not JSON")]
    Line {
        path: PathBuf,
        line: usize,
        #[source]
        source: serde_json::Error,
    },
}

/// All entries of a journal or script file; blank lines are skipped.
pub fn read(path: &Path) -> Result<Vec<Value>, ReadError> {
    let text = std::fs::read_to_string(path).map_err(|source| ReadError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    text.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, l)| {
            serde_json::from_str(l).map_err(|source| ReadError::Line {
                path: path.to_path_buf(),
                line: i + 1,
                source,
            })
        })
        .collect()
}
