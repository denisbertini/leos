//! Discovery, manifests, the bounded transfer pool, the archiver lock, and
//! the janitor ledger. std only.

use crate::adler32;
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub const MARKER_DEFAULT: &str = "bbstage.final"; // engine: checkpoint complete on the store
pub const LICENSE_NAME: &str = ".archived"; // leos: uploaded LAST; the deletion license
pub const STATE_DIR: &str = ".leos";
pub const INCOMPLETE_SUFFIX: &str = ".incomplete";

pub fn epoch_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn log(msg: &str) {
    eprintln!("[{}] {}", epoch_now(), msg);
}

pub fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{b} {}", UNITS[0])
    } else {
        format!("{:.2} {}", v, UNITS[u])
    }
}

// ---------------------------------------------------------------- discovery

#[derive(Debug, Clone)]
pub struct Unit {
    pub name: String,
    pub path: PathBuf,
    pub licensed: bool,
    pub mtime: u64,
}

/// Is `dir` a checkpoint unit (holds the completion marker)?
pub fn is_unit(dir: &Path, marker: &str) -> bool {
    dir.join(marker).is_file()
}

/// Every unit at or under `root`; no descent inside a unit; oldest first.
pub fn discover(root: &Path, marker: &str) -> Vec<Unit> {
    let mut out = Vec::new();
    if root.is_dir() && is_unit(root, marker) {
        // the root itself is the unit
        let (name, mtime) = unit_meta(root);
        out.push(Unit {
            name,
            licensed: root.join(LICENSE_NAME).is_file(),
            path: root.to_path_buf(),
            mtime,
        });
    } else {
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let rd = match std::fs::read_dir(&dir) {
                Ok(rd) => rd,
                Err(_) => continue,
            };
            let mut found_marker = false;
            for ent in rd.flatten() {
                let name = ent.file_name().to_string_lossy().to_string();
                let ft = match ent.file_type() {
                    Ok(ft) => ft,
                    Err(_) => continue,
                };
                if name == STATE_DIR {
                    continue;
                }
                if ft.is_file() && name == marker {
                    found_marker = true;
                } else if ft.is_dir() && !name.starts_with('.') {
                    stack.push(ent.path());
                }
            }
            if found_marker {
                let (name, mtime) = unit_meta(&dir);
                out.push(Unit {
                    name,
                    licensed: dir.join(LICENSE_NAME).is_file(),
                    path: dir,
                    mtime,
                });
            }
        }
    }
    out.sort_by_key(|u| u.mtime);
    out
}

fn unit_meta(dir: &Path) -> (String, u64) {
    let name = dir.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let mtime = std::fs::metadata(dir)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    (name, mtime)
}

// ------------------------------------------------------------ manifest data

#[derive(Debug, Clone)]
pub struct Entry {
    pub rel: String,
    pub size: u64,
    pub adler: u32,
}

pub struct Manifest {
    pub unit: String,
    pub sealed: u64,
    pub tool: String,
    pub entries: Vec<Entry>,
}

impl Manifest {
    pub fn total(&self) -> u64 {
        self.entries.iter().map(|e| e.size).sum()
    }

    pub fn render(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!(
            "# leos manifest version=1 unit={} sealed={} tool=\"{}\" files={} bytes={}\n",
            self.unit,
            self.sealed,
            self.tool,
            self.entries.len(),
            self.total()
        ));
        for e in &self.entries {
            s.push_str(&format!("{}\t{}\t{:08x}\n", e.rel, e.size, e.adler));
        }
        s
    }

    pub fn parse(text: &str) -> Result<Manifest, String> {
        let mut unit = String::new();
        let mut sealed = 0u64;
        let mut tool = String::new();
        let mut entries = Vec::new();
        for line in text.lines() {
            if let Some(h) = line.strip_prefix("# leos manifest") {
                for kv in h.split_whitespace() {
                    if let Some(v) = kv.strip_prefix("unit=") {
                        unit = v.to_string();
                    } else if let Some(v) = kv.strip_prefix("sealed=") {
                        sealed = v.parse().unwrap_or(0);
                    } else if let Some(v) = kv.strip_prefix("tool=\"") {
                        tool = v.trim_end_matches('"').to_string();
                    }
                }
                continue;
            }
            if line.starts_with('#') || line.trim().is_empty() {
                continue;
            }
            let cols: Vec<&str> = line.split('\t').collect();
            if cols.len() != 3 {
                return Err(format!("manifest: bad entry line: {line}"));
            }
            let rel = cols[0].to_string();
            if rel.contains(['\t', '\n']) || rel.starts_with('/') || rel.contains("..") {
                return Err(format!("manifest: unsafe path: {rel}"));
            }
            entries.push(Entry {
                rel,
                size: cols[1].parse().map_err(|_| format!("manifest: bad size: {line}"))?,
                adler: u32::from_str_radix(cols[2], 16)
                    .map_err(|_| format!("manifest: bad checksum: {line}"))?,
            });
        }
        if entries.is_empty() {
            return Err("manifest: empty".to_string());
        }
        Ok(Manifest { unit, sealed, tool, entries })
    }

    /// Re-check every listed file under `dir` (sizes always, hashes optional).
    pub fn verify_local(&self, dir: &Path, hash: bool) -> Result<(), String> {
        for e in &self.entries {
            let p = dir.join(&e.rel);
            let meta = std::fs::metadata(&p).map_err(|_| format!("missing {}", p.display()))?;
            if meta.len() != e.size {
                return Err(format!(
                    "size mismatch {}: {} != {}",
                    p.display(),
                    meta.len(),
                    e.size
                ));
            }
            if hash {
                let got = adler32::hash_file(&p)?;
                if got != e.adler {
                    return Err(format!(
                        "checksum mismatch {}: got {:08x}, manifest {:08x}",
                        p.display(),
                        got,
                        e.adler
                    ));
                }
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- file walk

pub struct Walked {
    pub entries: Vec<Entry>,
    pub dirs: Vec<String>, // rel dirs needing remote pre-creation
}

/// Walk a unit, hashing content unless `hash == false`.
pub fn walk_unit(unit_dir: &Path, hash: bool) -> Result<Walked, String> {
    let mut entries = Vec::new();
    let mut stack: Vec<PathBuf> = vec![unit_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for ent in
            std::fs::read_dir(&dir).map_err(|e| format!("walk {}: {e}", dir.display()))?
        {
            let ent = ent.map_err(|e| e.to_string())?;
            let name = ent.file_name().to_string_lossy().to_string();
            let ft = ent.file_type().map_err(|e| e.to_string())?;
            if ft.is_dir() {
                stack.push(ent.path());
                continue;
            }
            if !ft.is_file() {
                return Err(format!(
                    "unsupported entry (symlink/special?): {}",
                    ent.path().display()
                ));
            }
            if name == LICENSE_NAME {
                continue; // never part of the payload
            }
            let rel = ent
                .path()
                .strip_prefix(unit_dir)
                .map_err(|e| e.to_string())?
                .to_string_lossy()
                .replace('\\', "/");
            if rel.contains(['\t', '\n']) {
                return Err(format!("tab/newline in file name unsupported: {rel}"));
            }
            let size = ent.metadata().map_err(|e| e.to_string())?.len();
            let adler = if hash { adler32::hash_file(&ent.path())? } else { 0 };
            entries.push(Entry { rel, size, adler });
        }
    }
    entries.sort_by(|a, b| a.rel.cmp(&b.rel));
    let mut dirs = Vec::new();
    let mut seen = HashSet::new();
    for e in &entries {
        if let Some((parent, _)) = e.rel.rsplit_once('/') {
            let mut acc = String::new();
            for part in parent.split('/') {
                acc = if acc.is_empty() { part.to_string() } else { format!("{acc}/{part}") };
                if seen.insert(acc.clone()) {
                    dirs.push(acc.clone());
                }
            }
        }
    }
    Ok(Walked { entries, dirs })
}

// --------------------------------------------------------------------- pool

/// Run `f` over items with at most `jobs` concurrent threads, order-preserving.
pub fn parallel<T, R, F>(items: Vec<T>, jobs: usize, f: F) -> Vec<R>
where
    T: Send,
    R: Send,
    F: Fn(usize, T) -> R + Sync,
{
    let n = items.len();
    if n == 0 {
        return Vec::new();
    }
    let jobs = jobs.max(1).min(n);
    let queue = Mutex::new(items.into_iter().enumerate().collect::<VecDeque<_>>());
    let mut out: Vec<Option<R>> = (0..n).map(|_| None).collect();
    let outp = Mutex::new(&mut out);
    let fref = &f; // each worker borrows the closure (&F: Send since F: Sync)
    std::thread::scope(|s| {
        for _ in 0..jobs {
            let (q, o, f) = (&queue, &outp, fref);
            s.spawn(move || loop {
                let next = q.lock().unwrap().pop_front();
                match next {
                    None => break,
                    Some((i, it)) => {
                        let r = f(i, it);
                        o.lock().unwrap()[i] = Some(r);
                    }
                }
            });
        }
    });
    out.into_iter().map(|r| r.expect("pool: missing result")).collect()
}

// -------------------------------------------------------------------- lock

pub struct Lock {
    path: PathBuf,
}

impl Lock {
    /// One archiver per scratch tree: create-exclusive pid lock; a lock whose
    /// pid no longer exists is stolen (crash recovery).
    pub fn acquire(state_dir: &Path) -> Result<Lock, String> {
        std::fs::create_dir_all(state_dir).map_err(|e| e.to_string())?;
        let path = state_dir.join("lock");
        for _ in 0..2 {
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(f) => {
                    use std::io::Write;
                    (&f)
                        .write_all(format!("{}\n", std::process::id()).as_bytes())
                        .map_err(|e| e.to_string())?;
                    return Ok(Lock { path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let pid = std::fs::read_to_string(&path)
                        .ok()
                        .and_then(|s| s.trim().parse::<u32>().ok());
                    let alive = pid
                        .map(|p| Path::new(&format!("/proc/{p}")).exists())
                        .unwrap_or(false);
                    if alive {
                        return Err(format!(
                            "another leos holds {} (pid {})",
                            path.display(),
                            pid.unwrap_or(0)
                        ));
                    }
                    let _ = std::fs::remove_file(&path); // stale
                }
                Err(e) => return Err(e.to_string()),
            }
        }
        Err(format!("could not take lock {}", path.display()))
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

// ------------------------------------------------------------- janitor ledger

pub fn record_append(state_dir: &Path, unit: &str, target: &str) -> Result<(), String> {
    use std::io::Write;
    let path = state_dir.join("records.log");
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| e.to_string())?;
    writeln!(f, "{}|{}|{}", epoch_now(), unit, target).map_err(|e| e.to_string())
}

pub fn records_read(state_dir: &Path) -> Vec<(u64, String, String)> {
    let text = match std::fs::read_to_string(state_dir.join("records.log")) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let mut it = line.splitn(3, '|');
        if let (Some(ts), Some(unit), Some(target)) = (it.next(), it.next(), it.next()) {
            if let Ok(ts) = ts.parse() {
                out.push((ts, unit.to_string(), target.to_string()));
            }
        }
    }
    out
}

pub fn records_write(state_dir: &Path, rows: &[(u64, String, String)]) -> Result<(), String> {
    let path = state_dir.join("records.log");
    let tmp = state_dir.join("records.log.tmp");
    let mut text = String::new();
    for (ts, unit, target) in rows {
        text.push_str(&format!("{ts}|{unit}|{target}\n"));
    }
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_roundtrip() {
        let m = Manifest {
            unit: "chk1.bp5".into(),
            sealed: 123,
            tool: "xrdcp streams=8".into(),
            entries: vec![
                Entry { rel: "md".into(), size: 10, adler: 0xdead_beef },
                Entry { rel: "sub/data.0".into(), size: 1 << 40, adler: 1 },
            ],
        };
        let t = m.render();
        let p = Manifest::parse(&t).unwrap();
        assert_eq!(p.unit, "chk1.bp5");
        assert_eq!(p.total(), 10 + (1u64 << 40));
        assert_eq!(p.entries[1].adler, 1);
        assert!(Manifest::parse("# leos manifest version=1\n").is_err());
        assert!(Manifest::parse("a\tb\tc\n").is_err());
        assert!(Manifest::parse("/abs\t1\t00000001\n").is_err());
        assert!(Manifest::parse("../esc\t1\t00000001\n").is_err());
    }

    #[test]
    fn pool_preserves_order_and_bounds_concurrency() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NOW: AtomicUsize = AtomicUsize::new(0);
        static MAX: AtomicUsize = AtomicUsize::new(0);
        let items: Vec<usize> = (0..50).collect();
        let res = parallel(items, 4, |_i, x| {
            let c = NOW.fetch_add(1, Ordering::SeqCst) + 1;
            MAX.fetch_max(c, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_micros(200));
            NOW.fetch_sub(1, Ordering::SeqCst);
            x * 2
        });
        assert_eq!(res, (0..50).map(|x| x * 2).collect::<Vec<_>>());
        assert!(MAX.load(Ordering::SeqCst) <= 4);
    }
}
