//! Lock-free multi-node claiming on Lustre: O_CREAT|O_EXCL (`create_new`)
//! claim files with a heartbeat, stale-claim stealing, and a host-aware
//! ownership check. This is the only coordination leos needs across
//! data-mover nodes -- no MPI, no coordinator, still zero dependencies.
//!
//! Protocol:
//!   * `try_acquire` atomically creates `<dir>/<name>` holding
//!     `host:pid:epoch`. Exactly one node across the cluster wins;
//!     the losers get AlreadyExists and move on.
//!   * A heartbeat thread rewrites the file (mtime + fresh epoch) every
//!     HEARTBEAT_SECS while the guard is alive.
//!   * A claim is stealable when its heartbeat is older than `stale`
//!     seconds, or when it names a pid on THIS host that no longer
//!     exists (same-node crash: provable death, no waiting). A claim on
//!     ANOTHER host can only be judged by heartbeat age -- we cannot
//!     read a foreign /proc. This also fixes the pre-claim bug where a
//!     shared-tree lock was checked against the local /proc only.
//!   * Dropping the guard removes the claim (clean release) so the
//!     queue drains; a dead holder's file stays and is reaped+retried.
//!
//! Claim files double as the cluster feed for `leos tui`: every live
//! claim names its owner and last beat, no extra protocol needed.

use std::fs::{self, File, FileTimes, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Where claim files live under the scratch tree's state dir. MUST be on
/// Lustre: the MDT answers O_CREAT|O_EXCL atomically in microseconds;
/// no claim file ever touches an EOS FUSE mount.
pub const CLAIMS_DIR: &str = "claims";
/// A silent heartbeat older than this is presumed dead: stealable.
pub const STALE_SECS: u64 = 120;
const HEARTBEAT_SECS: u64 = 30;

pub fn host() -> String {
    fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".into())
}

fn owner_line() -> String {
    format!("{}:{}:{}", host(), std::process::id(), epoch_now())
}

fn epoch_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn mtime_age(path: &Path) -> Option<u64> {
    fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .map(|d| d.as_secs())
}

/// Parse `host:pid:epoch`; None for foreign/legacy formats (mtime rule
/// then applies alone).
fn parse_owner(text: &str) -> Option<(String, u32, u64)> {
    let mut it = text.trim().split(':');
    let h = it.next()?.to_string();
    let p = it.next()?.parse().ok()?;
    let e = it.next().unwrap_or("0").parse().unwrap_or(0);
    Some((h, p, e))
}

fn is_stale(path: &Path, stale: u64) -> bool {
    // provably dead on this node: immediate steal, do not wait out the
    // heartbeat window (Slurm-killed leader case)
    if let Ok(text) = fs::read_to_string(path) {
        if let Some((h, pid, _)) = parse_owner(text.trim()) {
            if h == host() && !Path::new(&format!("/proc/{pid}")).exists() {
                return true;
            }
        }
    }
    mtime_age(path).map(|age| age > stale).unwrap_or(true) // unreadable: aged out
}

pub struct Guard {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    hb: Option<thread::JoinHandle<()>>,
}

impl Guard {
    /// Claim `name` in `dir`; Ok(None) = a live claim is held elsewhere.
    /// Steals exactly once, then retries the exclusive create (a rival
    /// stealer can win that retry -- which is the whole point).
    pub fn try_acquire(dir: &Path, name: &str, stale: u64) -> Result<Option<Guard>, String> {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let path = dir.join(name);
        for attempt in 0..2 {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(f) => {
                    Self::beat(&f);
                    let stop = Arc::new(AtomicBool::new(false));
                    let hb = {
                        let (stop, p) = (Arc::clone(&stop), path.clone());
                        thread::spawn(move || {
                            // 500ms ticks so Drop::join is never a 30 s wait
                            let mut since = Duration::ZERO;
                            let step = Duration::from_millis(500);
                            while !stop.load(Ordering::Relaxed) {
                                thread::sleep(step);
                                since += step;
                                if since >= Duration::from_secs(HEARTBEAT_SECS) {
                                    since = Duration::ZERO;
                                    if let Ok(f) = fs::OpenOptions::new()
                                        .write(true)
                                        .truncate(true)
                                        .open(&p)
                                    {
                                        Self::beat(&f);
                                    }
                                }
                            }
                        })
                    };
                    return Ok(Some(Guard { path, stop, hb: Some(hb) }));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if attempt == 0 && is_stale(&path, stale) {
                        let _ = fs::remove_file(&path);
                        continue; // one race-safe retry
                    }
                    return Ok(None);
                }
                Err(e) => return Err(format!("claim {}: {e}", path.display())),
            }
        }
        Ok(None)
    }

    /// Who holds this claim (for error messages and the TUI).
    pub fn owner_of(dir: &Path, name: &str) -> String {
        fs::read_to_string(dir.join(name))
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "?".into())
    }

    /// mtime + content refresh, cache-coherent proof of liveness: the
    /// 8-byte rewrite cannot be served from our page cache alone on
    /// Lustre's MDT path, same argument as bbstage's probe sentinel.
    fn beat(mut f: &File) {
        // &File implements Write: mut-binding of the *reference* enables it
        let _ = f.write_all(owner_line().as_bytes());
        let _ = f.sync_all();
        let _ = f.set_times(FileTimes::new().set_modified(SystemTime::now()));
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.hb.take() {
            let _ = h.join();
        }
        let _ = fs::remove_file(&self.path);
    }
}

/// All claims in `dir`, newest beat first: (claim name, owner, age s).
/// `age > stale` entries are abandoned-but-unreaped, render them red.
pub fn live(dir: &Path, stale: u64) -> Vec<(String, String, u64)> {
    let mut out = Vec::new();
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return out,
    };
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let owner = fs::read_to_string(ent.path())
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "?".into());
        let age = mtime_age(&ent.path()).unwrap_or(stale + 1);
        out.push((name, owner, age));
    }
    out.sort_by_key(|(_, _, age)| *age);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("leos-claims-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn contention_and_release() {
        let d = tmpdir("contend");
        let g = Guard::try_acquire(&d, "u1.unit", STALE_SECS).unwrap();
        assert!(g.is_some(), "first claim must win");
        assert!(Guard::try_acquire(&d, "u1.unit", STALE_SECS).unwrap().is_none());
        drop(g);
        assert!(!d.join("u1.unit").exists(), "drop releases the claim");
        assert!(Guard::try_acquire(&d, "u1.unit", STALE_SECS).unwrap().is_some());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn dead_local_pid_is_stolen_immediately() {
        let d = tmpdir("deadpid");
        // claimed by THIS host, pid 999999: provably dead, no wait
        fs::write(d.join("u2.unit"), format!("{}:999999:0", host())).unwrap();
        assert!(Guard::try_acquire(&d, "u2.unit", STALE_SECS).unwrap().is_some());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn foreign_claim_ages_out_only_by_heartbeat() {
        let d = tmpdir("foreign");
        let p = d.join("u3.unit");
        fs::write(&p, "other-node:7:0").unwrap();
        // fresh heartbeat on another host: NOT stealable, however odd
        assert!(Guard::try_acquire(&d, "u3.unit", STALE_SECS).unwrap().is_none());
        // heartbeat 200 s old with stale=120: steal
        let f = fs::OpenOptions::new().write(true).open(&p).unwrap();
        f.set_times(FileTimes::new().set_modified(
            SystemTime::now() - Duration::from_secs(200),
        ))
        .unwrap();
        drop(f);
        assert!(Guard::try_acquire(&d, "u3.unit", STALE_SECS).unwrap().is_some());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn live_listing() {
        let d = tmpdir("list");
        let g = Guard::try_acquire(&d, "openpmd_100.unit", STALE_SECS).unwrap();
        let v = live(&d, STALE_SECS);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].0, "openpmd_100.unit");
        assert!(v[0].1.starts_with(&host()));
        assert!(v[0].2 <= 2);
        drop(g);
        assert!(live(&d, STALE_SECS).is_empty());
        let _ = fs::remove_dir_all(&d);
    }
}
