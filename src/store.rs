//! User store: a directory of JSON files, one per user.
//!
//! Each user is `{data_dir}/{index}.json`, where `index` is the BIP-32 child
//! index of their payment address. Indexes are never recycled — otherwise two
//! users could share a funded address — so the next index is derived from
//! the file names present, even for files that fail to parse. The directory
//! holds *job configuration only*; payment accounting is always recomputed
//! from the chain, so losing it never loses money, only jobs.
//!
//! Writes are atomic (temp file + rename) and serialized by a lock; one
//! process (the server) is expected to write, CLI commands only read.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::bip32::{HARDENED, Xpub};
use crate::jobs::Job;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct UserRecord {
    pub telegram_id: i64,
    pub created_at: u64,
    pub job: Option<Job>,
}

#[derive(Default)]
struct Index {
    by_telegram_id: HashMap<i64, u32>,
    next: u32,
}

pub struct Store {
    dir: PathBuf,
    xpub: Xpub,
    index: Mutex<Index>,
}

impl Store {
    /// Open (creating if needed) the data directory and index its users.
    pub fn open(dir: impl Into<PathBuf>, xpub: Xpub) -> Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let mut index = Index::default();
        for i in list_indexes(&dir)? {
            index.next = index.next.max(i + 1);
            match read_record(&dir, i) {
                Ok(Some(rec)) => {
                    if let Some(prev) = index.by_telegram_id.insert(rec.telegram_id, i) {
                        tracing::warn!(
                            telegram_id = rec.telegram_id,
                            "duplicate user files {prev} and {i}; using {i}"
                        );
                    }
                }
                Ok(None) => {}
                Err(e) => tracing::warn!("skipping unreadable user file {i}: {e:#}"),
            }
        }
        Ok(Self {
            dir,
            xpub,
            index: Mutex::new(index),
        })
    }

    /// Payment address of user `index` (EIP-55 checksummed).
    pub fn address(&self, index: u32) -> Result<String> {
        Ok(self.xpub.derive(index)?.address())
    }

    /// The user's record, creating one with the next free index if needed.
    pub fn ensure(&self, telegram_id: i64) -> Result<(u32, UserRecord)> {
        let mut idx = self.index.lock().expect("store index poisoned");
        if let Some(&i) = idx.by_telegram_id.get(&telegram_id)
            && let Some(rec) = self.get(i)?
        {
            return Ok((i, rec));
        }
        let i = idx.next;
        if i >= HARDENED {
            bail!("user index space exhausted");
        }
        let rec = UserRecord {
            telegram_id,
            created_at: crate::unix_now(),
            job: None,
        };
        write_record(&self.dir, i, &rec)?;
        idx.next = i + 1;
        idx.by_telegram_id.insert(telegram_id, i);
        Ok((i, rec))
    }

    /// Replace the job of an existing user.
    pub fn set_job(&self, index: u32, job: Option<Job>) -> Result<UserRecord> {
        let _guard = self.index.lock().expect("store index poisoned");
        let mut rec = self
            .get(index)?
            .with_context(|| format!("no user at index {index}"))?;
        rec.job = job;
        write_record(&self.dir, index, &rec)?;
        Ok(rec)
    }

    pub fn get(&self, index: u32) -> Result<Option<UserRecord>> {
        read_record(&self.dir, index)
    }

    /// All readable users ordered by index; unreadable files are logged and skipped.
    pub fn users(&self) -> Result<Vec<(u32, UserRecord)>> {
        let mut out = Vec::new();
        for i in list_indexes(&self.dir)? {
            match read_record(&self.dir, i) {
                Ok(Some(rec)) => out.push((i, rec)),
                Ok(None) => {}
                Err(e) => tracing::warn!("skipping unreadable user file {i}: {e:#}"),
            }
        }
        Ok(out)
    }
}

fn record_path(dir: &Path, index: u32) -> PathBuf {
    dir.join(format!("{index}.json"))
}

/// Indexes of all `{n}.json` files, ascending.
fn list_indexes(dir: &Path) -> Result<Vec<u32>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("listing {}", dir.display()))? {
        let name = entry?.file_name();
        if let Some(i) = name
            .to_str()
            .and_then(|n| n.strip_suffix(".json"))
            .and_then(|n| n.parse::<u32>().ok())
        {
            out.push(i);
        }
    }
    out.sort_unstable();
    Ok(out)
}

fn read_record(dir: &Path, index: u32) -> Result<Option<UserRecord>> {
    let path = record_path(dir, index);
    match fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s)
            .map(Some)
            .with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn write_record(dir: &Path, index: u32, rec: &UserRecord) -> Result<()> {
    let path = record_path(dir, index);
    let tmp = dir.join(format!(".{index}.json.tmp"));
    let mut f = fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    f.write_all(&serde_json::to_vec_pretty(rec)?)?;
    f.sync_all()?;
    fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bip32::Xprv;

    fn test_xpub() -> Xpub {
        let seed = hex::decode("000102030405060708090a0b0c0d0e0f").unwrap();
        Xprv::from_seed(&seed)
            .unwrap()
            .derive_path(crate::bip32::DEFAULT_ACCOUNT_PATH)
            .unwrap()
            .to_xpub()
    }

    #[test]
    fn ensure_is_stable_per_telegram_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), test_xpub()).unwrap();
        let (i1, _) = store.ensure(111).unwrap();
        let (i2, _) = store.ensure(222).unwrap();
        let (i3, _) = store.ensure(111).unwrap();
        assert_eq!((i1, i2, i3), (0, 1, 0));
        assert_ne!(store.address(i1).unwrap(), store.address(i2).unwrap());

        // A reopened store sees the same users.
        let reopened = Store::open(dir.path(), test_xpub()).unwrap();
        assert_eq!(reopened.ensure(222).unwrap().0, 1);
        assert_eq!(reopened.ensure(333).unwrap().0, 2);
    }

    #[test]
    fn set_job_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), test_xpub()).unwrap();
        let (index, _) = store.ensure(333).unwrap();
        let job: Job = serde_json::from_value(serde_json::json!({
            "name": "t", "kind": "shell", "shell": {"cmd": "/bin/echo"}
        }))
        .unwrap();
        store.set_job(index, Some(job)).unwrap();
        let loaded = store.get(index).unwrap().unwrap();
        assert_eq!(loaded.telegram_id, 333);
        assert_eq!(loaded.job.unwrap().name(), "t");
        assert!(store.set_job(99, None).is_err());
    }

    #[test]
    fn corrupt_files_are_skipped_but_never_reused() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("0.json"), "{not json").unwrap();
        fs::write(dir.path().join("notes.txt"), "ignored").unwrap();
        let store = Store::open(dir.path(), test_xpub()).unwrap();
        assert_eq!(store.users().unwrap().len(), 0);
        assert_eq!(store.ensure(1).unwrap().0, 1, "index 0 stays reserved");
        assert_eq!(store.users().unwrap().len(), 1);
    }
}
