// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

//! The working directory for temporary files.
//!
//! In production we run as a Kubernetes job whose working directory is an
//! ephemeral disk. Kubernetes may kill and restart the container at any
//! time, for example when a node fails, so the directory may still hold
//! files from a run that never finished. Each signal gets its own
//! subdirectory, such as `node-density/`, so that several signals can
//! share a working directory, even concurrently. Inside it, every
//! temporary file lives in `tmp/`, which is wiped when a run starts and
//! removed when it ends. Temporary files are
//! ordinary named files, so disk usage shows up where Kubernetes measures
//! it for `sizeLimit`.
//!
//! An exclusive lock on `lock` in the signal's subdirectory keeps two
//! concurrent runs of the same signal from wiping each other's files. The operating system releases the lock when
//! the process dies, however it dies.

use anyhow::{Context, Result, bail};
use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

const LOCK: &str = "lock";
const TMP: &str = "tmp";

pub struct Workdir {
    tmp: PathBuf,
    _lock: File,
}

impl Workdir {
    /// Takes over the subdirectory `signal` of `workdir`, creating it if
    /// needed. Fails if another process is computing the same signal there.
    pub fn open(workdir: &Path, signal: &str) -> Result<Workdir> {
        let dir = &workdir.join(signal);
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let lock_path = dir.join(LOCK);
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("opening {}", lock_path.display()))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                bail!("{} is in use by another process", dir.display())
            }
            Err(TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("locking {}", lock_path.display()));
            }
        }
        let tmp = dir.join(TMP);
        match std::fs::remove_dir_all(&tmp) {
            Ok(()) => eprintln!("removed leftovers of an earlier run in {}", tmp.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing {}", tmp.display())),
        }
        std::fs::create_dir(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        Ok(Workdir { tmp, _lock: lock })
    }

    /// Directory for temporary files.
    pub fn tmp(&self) -> &Path {
        &self.tmp
    }
}

impl Drop for Workdir {
    fn drop(&mut self) {
        // Best effort; the next run cleans up anyway.
        let _ = std::fs::remove_dir_all(&self.tmp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_and_removes_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("work");
        let w = Workdir::open(&root, "node-density").unwrap();
        assert!(w.tmp().is_dir());
        assert_eq!(w.tmp(), root.join("node-density/tmp"));
        let tmp = w.tmp().to_path_buf();
        drop(w);
        assert!(!tmp.exists());
    }

    #[test]
    fn wipes_leftovers_but_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().join("node-density").join(TMP);
        std::fs::create_dir_all(tmp.join("sub")).unwrap();
        std::fs::write(tmp.join("sub/run-17"), b"stale").unwrap();
        std::fs::write(dir.path().join("planet.osm.pbf"), b"keep").unwrap();
        let w = Workdir::open(dir.path(), "node-density").unwrap();
        assert_eq!(std::fs::read_dir(w.tmp()).unwrap().count(), 0);
        assert_eq!(
            std::fs::read(dir.path().join("planet.osm.pbf")).unwrap(),
            b"keep"
        );
    }

    #[test]
    fn refuses_concurrent_use() {
        let dir = tempfile::tempdir().unwrap();
        let first = Workdir::open(dir.path(), "node-density").unwrap();
        let err = Workdir::open(dir.path(), "node-density")
            .err()
            .expect("second open should fail");
        assert!(err.to_string().contains("in use"), "{err}");
        // Another signal can share the working directory.
        Workdir::open(dir.path(), "other-signal").unwrap();
        drop(first);
        Workdir::open(dir.path(), "node-density").unwrap();
    }
}
