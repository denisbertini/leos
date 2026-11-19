//! The leos operations: archive / restore / status / verify / purge / janitor.
//!
//! Unit lifecycle (per checkpoint directory holding the completion marker):
//!
//!   pending -> copied to <dest>/<unit>.incomplete/ -> remote sizes verified
//!   against a manifest (per-file size + content adler32) -> sealed by atomic
//!   metadata rename -> manifest uploaded as .archived LAST -> mirrored
//!   locally as the scratch-side deletion license.
//!
//! Every step is idempotent and crash-safe: reruns resume into the same
//! .incomplete; the janitor sweeps abandoned ones; purge needs the license
//! verified on both sides.

use crate::args::Args;
use crate::backend::{is_remote, Backend, R};
use crate::claims::{self, CLAIMS_DIR};
use crate::core::{
    self, discover, epoch_now, human_bytes, log, parallel, record_append, records_read,
    records_write, walk_unit, Lock, Manifest, INCOMPLETE_SUFFIX, LICENSE_NAME, MARKER_DEFAULT,
    STATE_DIR,
};
use std::path::{Path, PathBuf};

struct Cfg {
    backend: Backend,
    jobs: usize,
    hash: bool,
    dry: bool,
    marker: String,
    seal_rename: bool,
    allow_unverified: bool,
}

fn cfg(a: &Args, ref_path: &str, streams_default: usize) -> R<Cfg> {
    let streams = a.usize("streams", streams_default)?;
    let backend = Backend::from_args(
        a.opt("backend").unwrap_or("auto"),
        ref_path,
        streams,
        a.opt("extra-args").unwrap_or(""),
        token(a).as_deref(),
    )?;
    let seal_rename = match a.opt("seal").unwrap_or("auto") {
        "auto" => {
            let ok = backend.probe_dirs(ref_path);
            log(&format!(
                "seal mode: {}",
                if ok { "rename (atomic)" } else { "marker-only (rename unsupported)" }
            ));
            ok
        }
        "rename" => true,
        "marker" => false,
        other => return Err(format!("--seal must be auto|rename|marker, got '{other}'")),
    };
    Ok(Cfg {
        backend,
        jobs: a.usize("jobs", 8)?,
        hash: !a.flag("no-checksum"),
        dry: a.flag("dry-run"),
        marker: a.opt("marker").unwrap_or(MARKER_DEFAULT).to_string(),
        seal_rename,
        allow_unverified: a.flag("allow-unverified"),
    })
}

// ------------------------------------------------------------------ archive

/// capability token precedence: --token, $LEOS_TOKEN, $EOSAUTHZ
fn token(a: &Args) -> Option<String> {
    if let Some(t) = a.opt("token") {
        return Some(t.to_string());
    }
    for var in ["LEOS_TOKEN", "EOSAUTHZ"] {
        if let Ok(t) = std::env::var(var) {
            if !t.is_empty() {
                return Some(t);
            }
        }
    }
    None
}

/// Auth-class errors are systemic: a token that is missing or dead on one
/// file will be dead on every remaining file. Continuing only burns planning.
fn auth_suspect(e: &str) -> bool {
    let l = e.to_lowercase();
    [
        "permission denied",
        "public access",
        "unauthenticat",
        "authz",
        "token",
        "access denied",
        "authoriz",
        "[3010]",
        "sfs_err_auth",
    ]
    .iter()
    .any(|k| l.contains(k))
}

/// One authenticated write+delete at the destination before any hashing:
/// missing tokens, dead tokens and unwritable trees fail in ~1 s, not after
/// a TiB of planning.
fn preflight_write(c: &Cfg, dst: &str) -> R<()> {
    let probe = Backend::join(dst, ".leos-writeprobe");
    let tmp = std::env::temp_dir().join(format!("leos-probe-{}", std::process::id()));
    std::fs::write(&tmp, b"leos preflight").map_err(|e| e.to_string())?;
    let r = c
        .backend
        .copy_to(&tmp, &probe)
        .and_then(|()| match c.backend.size(&probe) {
            Some(got) if got == 14 => Ok(()),
            Some(got) => Err(format!("write probe has {got} bytes, expected 14")),
            None => Ok(()), // stat-unsupported stores: the copy itself proved auth
        });
    let _ = c.backend.remove(&probe);
    let _ = std::fs::remove_file(&tmp);
    r.map_err(|e| {
        format!(
            "preflight write to {probe} failed: {e}\n\
             check: $EOSAUTHZ minted and exported in THIS shell (echo ${{#EOSAUTHZ}}), \
             xrdcp/eos on PATH ($PATH), token not expired, tree path exists. \
             Nothing was transferred."
        )
    })
}


/// Bytes read from disk by direct children of this process (xrdcp,
/// eos cp) via /proc: linux, same-uid, no privileges. The transfer
/// oracle that makes fleet progress live even when the store reports
/// nothing about files still open.
fn children_read_bytes() -> u64 {
    let me = std::process::id();
    let mut total = 0u64;
    let rd = match std::fs::read_dir("/proc") {
        Ok(rd) => rd,
        Err(_) => return 0,
    };
    for e in rd.flatten() {
        let pid: u32 = match e.file_name().to_string_lossy().parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        let stat = match std::fs::read(format!("/proc/{pid}/stat")) {
            Ok(s) => s,
            Err(_) => continue,
        };
        // ppid is the field after the ')' of comm (comm may hold spaces)
        let s = String::from_utf8_lossy(&stat);
        let ppid: u32 = match s
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().nth(1))
            .and_then(|p| p.parse().ok())
        {
            Some(p) => p,
            None => continue,
        };
        if ppid != me {
            continue;
        }
        if let Ok(io) = std::fs::read_to_string(format!("/proc/{pid}/io")) {
            for line in io.lines() {
                // rchar: bytes via read() syscalls, page-cache included
                // -- read_bytes counts only block I/O and would be zero
                // for exactly the cached reads a mover thrives on
                if let Some(v) = line.strip_prefix("rchar:") {
                    total += v.trim().parse().unwrap_or(0);
                }
            }
        }
    }
    total
}

pub fn run_archive(a: &Args) -> R<()> {
    let src = PathBuf::from(a.pos1("lustre-path")?);
    if !src.is_dir() {
        return Err(format!("{} is not a directory", src.display()));
    }
    let dst = a.dest()?;
    let c = cfg(a, &dst, 4)?;
    let state = src.join(STATE_DIR);
    // --claim: peer data-mover nodes share this tree; whole-tree
    // exclusion would exclude the fleet. Coordination moves to per-unit
    // O_EXCL claims under .leos/claims (atomic on the Lustre MDT), and
    // sealing stays the identical marker-chain path per unit.
    let claims = if a.flag("claim") { Some(state.join(CLAIMS_DIR)) } else { None };
    let _lock = if claims.is_some() { None } else { Some(Lock::acquire(&state)?) };
    let watch: Option<u64> = match a.opt("watch") {
        Some(s) => Some(s.parse().map_err(|_| "--watch needs seconds".to_string())?),
        None => None,
    };
    let budget: Option<u64> = match a.opt("max-gib") {
        Some(s) => Some(
            s.parse::<u64>()
                .map_err(|_| "--max-gib needs an integer".to_string())?
                * (1 << 30),
        ),
        None => None,
    };
    if !c.dry && crate::backend::is_remote(&dst) {
        log("preflight: authenticated write probe at destination...");
        preflight_write(&c, &dst)?;
    }
    loop {
        let units = discover(&src, &c.marker);
        let pending: Vec<_> = units.into_iter().filter(|u| !u.licensed).collect();
        if pending.is_empty() {
            log("nothing pending");
        }
        let mut left = budget;
        let mut failed = 0usize;
        let mut peer_claimed = 0usize;
        for u in &pending {
            if left == Some(0) {
                log("--max-gib budget reached; stopping this pass");
                break;
            }
            // fleet mode: win the unit or step aside for whoever holds it
            let claim_g = match &claims {
                Some(dir) => match claims::Guard::try_acquire(
                    dir,
                    &format!("{}.unit", u.name),
                    claims::STALE_SECS,
                )? {
                    Some(g) => Some(g),
                    None => {
                        peer_claimed += 1;
                        continue;
                    }
                },
                None => None,
            };
            match archive_unit(&c, &dst, &state, u, claim_g.as_ref()) {
                Ok(bytes) => {
                    if let Some(b) = left.as_mut() {
                        *b = b.saturating_sub(bytes);
                    }
                }
                Err(e) => {
                    failed += 1;
                    log(&format!("FAILED {}: {e}", u.name));
                    if auth_suspect(&e) {
                        return Err(format!(
                            "aborting the pass: authentication-class failure -- every \
                             remaining unit would fail identically. Re-mint/re-export the \
                             token, then rerun the same command (resumes where it stopped). \
                             Original error: {e}"
                        ));
                    }
                    if a.flag("stop-on-error") {
                        return Err(format!("aborted on first failure: {e}"));
                    }
                }
            }
        }
        if peer_claimed > 0 {
            log(&format!("{peer_claimed} unit(s) held by peer claims (skipped)"));
        }
        match watch {
            Some(secs) => {
                if failed > 0 && a.flag("stop-on-error") {
                    return Err(format!("{failed} unit(s) failed this pass"));
                }
                std::thread::sleep(std::time::Duration::from_secs(secs));
            }
            None => {
                return if failed > 0 {
                    Err(format!("{failed} unit(s) failed"))
                } else {
                    Ok(())
                };
            }
        }
    }
}

fn archive_unit(
    c: &Cfg,
    dst: &str,
    state: &Path,
    u: &core::Unit,
    claim: Option<&claims::Guard>,
) -> R<u64> {
    let t0 = epoch_now();
    // dry-run plans with metadata only: sizes and file lists, no hashing
    let hash_now = c.hash && !c.dry;
    if hash_now {
        log(&format!("planning {}: hashing content (one pass over the bytes)...", u.name));
    }
    let walked = walk_unit(&u.path, hash_now, c.jobs)?;
    let bytes: u64 = walked.entries.iter().map(|e| e.size).sum();
    let dst_unit = Backend::join(dst, &u.name);
    let dst_target =
        if c.seal_rename { format!("{dst_unit}{INCOMPLETE_SUFFIX}") } else { dst_unit.clone() };

    if c.dry {
        log(&format!(
            "DRY {}: {} files, {} -> {}",
            u.name,
            walked.entries.len(),
            human_bytes(bytes),
            dst_target
        ));
        return Ok(0);
    }

    record_append(state, &u.name, &dst_target)?;
    log(&format!(
        "archiving {}: {} files, {} -> {}",
        u.name,
        walked.entries.len(),
        human_bytes(bytes),
        dst_target
    ));

    // 1. remote directories (serial: cheap, avoids mkdir races)
    for d in &walked.dirs {
        c.backend.make_dirs(&Backend::join(&dst_target, d))?;
    }

    // 2. payload, parallel
    let work: Vec<(PathBuf, String, u64)> = walked
        .entries
        .iter()
        .map(|e| (u.path.join(&e.rel), Backend::join(&dst_target, &e.rel), e.size))
        .collect();
    let b = c.backend.clone();
    let allow_unverified = c.allow_unverified;
    // fleet progress: each completed payload file publishes its running
    // byte count into our claim file -> the TUI bar (zero protocol)
    if let Some(g) = claim {
        g.progress(0, bytes);
    }
    let done = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    // intra-file liveness: file-completion alone is too coarse for a
    // 146 GiB data.0, so while copies run, a poller adds the bytes
    // already visible at the destinations (local stat, or store stat
    // where the FS reports open-file growth). Purely opportunistic:
    // completion accounting above remains the authority; the claim
    // takes the max of both views.
    let stop_mon = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let results = std::thread::scope(|scope| {
        let mon = claim.map(|g| {
            let (b2, work2, stop2, total, done_c) = (
                b.clone(),
                work.clone(),
                std::sync::Arc::clone(&stop_mon),
                bytes,
                std::sync::Arc::clone(&done),
            );
            scope.spawn(move || loop {
                if stop2.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                // two independent live views, keep the better one:
                //  * destination sizes (instant on local copy; EOS only
                //    reports an open file's size at close)
                //  * completed files + bytes our own copy-children have
                //    read, metered on THIS node via /proc/<child>/io --
                //    authoritative, needs nothing from the store
                let dest: u64 = work2
                    .iter()
                    .map(|(_, remote, want)| b2.size(remote).unwrap_or(0).min(*want))
                    .sum();
                let kids = done_c.load(std::sync::atomic::Ordering::Relaxed)
                    + children_read_bytes();
                g.progress(dest.max(kids), total);
                std::thread::sleep(std::time::Duration::from_secs(1));
            })
        });
        let res = parallel(work, c.jobs, |_i, (local, remote, want)| {
        let r = b
            .copy_to(&local, &remote)
            .and_then(|()| match b.size(&remote) {
                Some(g) if g == want => Ok(()),
                Some(g) => Err(format!("size mismatch remote {remote}: {g} != {want}")),
                None => {
                    if allow_unverified {
                        Ok(())
                    } else {
                        Err(format!(
                            "cannot stat remote {remote}: verification impossible \
                             (pass --allow-unverified only if you accept that)"
                        ))
                    }
                }
            });
        if r.is_ok() {
            let d = done.fetch_add(want, std::sync::atomic::Ordering::Relaxed) + want;
            if let Some(g) = claim {
                g.progress(d, bytes);
            }
        }
        r
        });
        stop_mon.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(h) = mon {
            let _ = h.join();
        }
        res
    });
    if let Some(e) = results.into_iter().find_map(|r| r.err()) {
        return Err(format!("copy stage failed (left {} for janitor/retry): {e}", dst_target));
    }

    // 3. seal
    if c.seal_rename {
        match c.backend.exists(&dst_unit) {
            Some(true) => {
                // previous seal, or a foreign tree: only proceed if licensed
                if c.backend.exists(&Backend::join(&dst_unit, LICENSE_NAME)) != Some(true) {
                    return Err(format!("{dst_unit} exists without a license -- refusing"));
                }
            }
            _ => {
                c.backend.rename(&dst_target, &dst_unit)?;
            }
        }
    }

    // 4. license uploaded LAST: no license can exist before the data verified
    let manifest = Manifest {
        unit: u.name.clone(),
        sealed: epoch_now(),
        tool: format!(
            "{} streams={} seal={}",
            c.backend.kind.as_str(),
            c.backend.streams,
            if c.seal_rename { "rename" } else { "marker" }
        ),
        entries: walked.entries.clone(),
    };
    let tmp = state.join(format!("{}.license.tmp", u.name));
    std::fs::write(&tmp, manifest.render()).map_err(|e| e.to_string())?;
    let remote_license = Backend::join(&dst_unit, LICENSE_NAME);
    // tag honesty: "refreshed" only when a license actually existed before
    // this upload -- the honest question is store state, not which seal path
    // ran (marker mode has no rename step to infer a first-seal from)
    let refreshed = c.backend.exists(&remote_license) == Some(true);
    c.backend.copy_to(&tmp, &remote_license)?;
    if c.backend.size(&remote_license).map(|s| s == 0).unwrap_or(false) {
        return Err("remote license has zero size".to_string());
    }
    std::fs::copy(&tmp, u.path.join(LICENSE_NAME)).map_err(|e| e.to_string())?;
    std::fs::remove_file(&tmp).ok();

    let dt = (epoch_now() - t0).max(1);
    log(&format!(
        "sealed {} ({} in {}s = {}/s){}",
        u.name,
        human_bytes(bytes),
        dt,
        human_bytes(bytes / dt),
        if refreshed { " [license refreshed]" } else { "" }
    ));
    Ok(bytes)
}

// ------------------------------------------------------------------- status

pub fn run_status(a: &Args) -> R<()> {
    let src = PathBuf::from(a.pos1("path")?);
    let marker = a.opt("marker").unwrap_or(MARKER_DEFAULT).to_string();
    let remote = a.opt("dest").map(|s| s.to_string()).or_else(|| std::env::var("LEOS_DEST").ok());
    let units = discover(&src, &marker);
    let mut pend = (0usize, 0u64);
    let mut lic = (0usize, 0u64);
    let mut mismatch = 0usize;
    let backend = if a.flag("remote") {
        let r = remote.as_deref().ok_or("--remote needs --dest or $LEOS_DEST")?;
        Some(Backend::from_args(a.opt("backend").unwrap_or("auto"), r, 1, "", token(a).as_deref())?)
    } else {
        None
    };
    for u in &units {
        let w = walk_unit(&u.path, false, 1).map_err(|e| e.to_string())?;
        let bytes: u64 = w.entries.iter().map(|e| e.size).sum();
        if u.licensed {
            lic.0 += 1;
            lic.1 += bytes;
            if let (Some(b), Some(r)) = (&backend, &remote) {
                let lic_remote = Backend::join(&Backend::join(r, &u.name), LICENSE_NAME);
                if b.exists(&lic_remote) != Some(true) {
                    mismatch += 1;
                    println!("LICENSE-NO-REMOTE  {}  (do NOT purge; investigate)", u.name);
                }
            }
        } else {
            pend.0 += 1;
            pend.1 += bytes;
            println!(
                "PENDING  {}  {}  (age {}s)",
                u.name,
                human_bytes(bytes),
                epoch_now().saturating_sub(u.mtime)
            );
        }
    }
    if units.is_empty() {
        println!(
            "no checkpoints found under {} (need '{marker}'; wrong <path>?)",
            src.display()
        );
    }
    println!(
        "units: {} pending ({}), {} licensed ({}), {} license-without-remote",
        pend.0,
        human_bytes(pend.1),
        lic.0,
        human_bytes(lic.1),
        mismatch
    );
    Ok(())
}

// ------------------------------------------------------------------- verify

pub fn run_verify(a: &Args) -> R<()> {
    let path = a.pos1("path")?.to_string();
    let marker = a.opt("marker").unwrap_or(MARKER_DEFAULT).to_string();
    let round_trip = a.flag("round-trip");
    // --remote forces the store-side check on any backend (tests a staged
    // directory as the archive tier; on real EOS use root:// or /eos/ paths).
    if is_remote(&path) || a.flag("remote") {
        return verify_remote(a, &path, round_trip);
    }
    // local: one unit or a whole scratch tree
    let src = PathBuf::from(&path);
    if !src.is_dir() {
        return Err(format!("{} is not a directory (remote path? give root://... or /eos/...)", src.display()));
    }
    let units = discover(&src, &marker);
    if units.is_empty() {
        return Err(format!("no checkpoints found under {} (need '{marker}')", src.display()));
    }
    let mut bad = 0usize;
    for u in &units {
        if !u.licensed {
            println!("UNLICENSED {} (never archived -- nothing to verify)", u.name);
            continue;
        }
        match std::fs::read_to_string(u.path.join(LICENSE_NAME))
            .map_err(|e| e.to_string())
            .and_then(|s| Manifest::parse(&s))
            .and_then(|m| m.verify_local(&u.path, true))
        {
            Ok(()) => println!("OK       {}", u.name),
            Err(e) => {
                bad += 1;
                println!("BROKEN   {}: {e}", u.name);
            }
        }
    }
    if bad > 0 {
        return Err(format!("{bad} unit(s) fail verification"));
    }
    Ok(())
}

fn verify_remote(a: &Args, remote_unit: &str, round_trip: bool) -> R<()> {
    let c = cfg(a, remote_unit, 8)?;
    let tmp = std::env::temp_dir().join(format!("leos-verify-{}", std::process::id()));
    let license_local = tmp.join(LICENSE_NAME);
    std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
    let result = (|| -> R<()> {
        c.backend
            .copy_from(&Backend::join(remote_unit, LICENSE_NAME), &license_local)?;
        let manifest = Manifest::parse(&std::fs::read_to_string(&license_local).map_err(|e| e.to_string())?)?;
        log(&format!(
            "verifying {} ({} files, {})",
            manifest.unit,
            manifest.entries.len(),
            human_bytes(manifest.total())
        ));
        if round_trip {
            // full end-to-end: download, re-hash, discard
            let work = manifest
                .entries
                .iter()
                .map(|e| (e.rel.clone(), e.size, e.adler))
                .collect();
            let b = c.backend.clone();
            let results = parallel(work, c.jobs, |_i, (rel, want_size, want_adler)| {
                let local = tmp.join(&rel);
                b.copy_from(&Backend::join(remote_unit, &rel), &local)?;
                manifest1(&local, want_size)?;
                let got = crate::adler32::hash_file(&local)?;
                if got != want_adler {
                    return Err(format!(
                        "checksum mismatch {rel}: EOS copy has {:08x}, manifest {:08x}",
                        got, want_adler
                    ));
                }
                Ok(())
            });
            if let Some(e) = results.into_iter().find_map(|r| r.err()) {
                return Err(format!("round-trip verification failed: {e}"));
            }
        } else {
            let work: Vec<(String, u64)> =
                manifest.entries.iter().map(|e| (e.rel.clone(), e.size)).collect();
            let b = c.backend.clone();
            let results = parallel(work, c.jobs, move |_i, (rel, want)| {
                let r = Backend::join(remote_unit, &rel);
                match b.size(&r) {
                    Some(g) if g == want => Ok(()),
                    Some(g) => Err(format!("size mismatch {r}: {g} != {want}")),
                    None => Err(format!("cannot stat {r}")),
                }
            });
            if let Some(e) = results.into_iter().find_map(|r| r.err()) {
                return Err(format!("remote verification failed: {e}"));
            }
        }
        println!("OK       {} (remote verified{})", manifest.unit, if round_trip { ", round-trip" } else { "" });
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&tmp);
    result
}

fn manifest1(p: &Path, want: u64) -> R<()> {
    let got = std::fs::metadata(p).map_err(|e| format!("{}: {e}", p.display()))?.len();
    if got != want {
        return Err(format!("{}: downloaded {got} bytes, manifest says {want}", p.display()));
    }
    Ok(())
}

// ------------------------------------------------------------------- restore

pub fn run_restore(a: &Args) -> R<()> {
    let remote_unit = a.pos1("eos-path")?.to_string();
    if !is_remote(&remote_unit) && !a.flag("remote") {
        return Err(format!(
            "<eos-path> must be remote (root://host//path or /eos/...); got '{remote_unit}' \
             (to restore FROM a local copy of the archive, copy the tree yourself)"
        ));
    }
    let to = PathBuf::from(a.opt("dest").unwrap_or("."));
    if !to.is_dir() {
        std::fs::create_dir_all(&to).map_err(|e| format!("--dest {}: {e}", to.display()))?;
    }
    let c = cfg(a, &remote_unit, 8)?;
    let name = remote_unit
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("cannot derive unit name from '{remote_unit}'"))?
        .to_string();

    let t0 = epoch_now();
    let work = to.join(format!("{name}{INCOMPLETE_SUFFIX}"));
    if work.exists() {
        std::fs::remove_dir_all(&work).map_err(|e| e.to_string())?; // stale partial
    }
    std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;

    let result = (|| -> R<u64> {
        // 1. license drives everything: no manifest, no restore
        let license_local = work.join(LICENSE_NAME);
        log(&format!("fetching license of {name}"));
        c.backend.copy_from(&Backend::join(&remote_unit, LICENSE_NAME), &license_local)?;
        let manifest =
            Manifest::parse(&std::fs::read_to_string(&license_local).map_err(|e| e.to_string())?)?;
        let bytes = manifest.total();
        log(&format!(
            "restoring {} ({} files, {}) -> {}/{}",
            name,
            manifest.entries.len(),
            human_bytes(bytes),
            to.display(),
            name
        ));
        if c.dry {
            for e in &manifest.entries {
                println!("DRY  {}  {}", e.rel, human_bytes(e.size));
            }
            let _ = std::fs::remove_dir_all(&work);
            return Ok(bytes);
        }

        // 2. payload, parallel (size gate per file during download)
        let items: Vec<(String, u64)> =
            manifest.entries.iter().map(|e| (e.rel.clone(), e.size)).collect();
        let b = c.backend.clone();
        let wref = work.clone();
        let results = parallel(items, c.jobs, move |_i, (rel, want)| {
            let local = wref.join(&rel);
            b.copy_from(&Backend::join(&remote_unit, &rel), &local)?;
            manifest1(&local, want)
        });
        if let Some(e) = results.into_iter().find_map(|r| r.err()) {
            return Err(format!("download failed: {e}"));
        }

        // 3. full manifest re-hash: the smoke test IS the verification
        manifest.verify_local(&work, true)?;

        // 4. atomic placement
        let dest = to.join(&name);
        if dest.exists() {
            return Err(format!(
                "destination {} already exists (remove it or choose another --dest)",
                dest.display()
            ));
        }
        std::fs::rename(&work, &dest).map_err(|e| e.to_string())?;
        Ok(bytes)
    })();
    let bytes = match result {
        Ok(b) => b,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&work);
            return Err(e);
        }
    };
    if !c.dry {
        let dt = (epoch_now() - t0).max(1);
        log(&format!(
            "restored {name} in {}s = {}/s; local {} proves content match",
            dt,
            human_bytes(bytes / dt),
            LICENSE_NAME
        ));
    }
    Ok(())
}

// -------------------------------------------------------------------- purge

pub fn run_purge(a: &Args) -> R<()> {
    let src = PathBuf::from(a.pos1("lustre-path")?);
    let dst = a.dest()?;
    let c = cfg(a, &dst, 1)?;
    let yes = a.flag("yes");
    let units = discover(&src, &c.marker);
    let mut n = 0;
    for u in units.iter().filter(|u| u.licensed) {
        let text = std::fs::read_to_string(u.path.join(LICENSE_NAME)).map_err(|e| e.to_string())?;
        let manifest = Manifest::parse(&text)?;
        if c.hash {
            log(&format!(
                "verifying {} against its license (hashing {})...",
                u.name,
                human_bytes(manifest.total())
            ));
        }
        // license must describe the tree it licenses...
        manifest.verify_local(&u.path, c.hash).map_err(|e| {
            format!("{}: tree does not match its license ({e}) -- NOT purging", u.name)
        })?;
        // ...and must exist on the store
        let remote_license = Backend::join(&Backend::join(&dst, &u.name), LICENSE_NAME);
        if c.backend.exists(&remote_license) != Some(true) {
            return Err(format!("{}: license missing on EOS -- NOT purging", u.name));
        }
        if yes {
            std::fs::remove_dir_all(&u.path).map_err(|e| e.to_string())?;
            log(&format!("purged {}", u.path.display()));
        } else {
            println!("PURGE-READY {} ({} files) -- add --yes to delete", u.path.display(), manifest.entries.len());
        }
        n += 1;
    }
    if n == 0 {
        println!("no licensed checkpoints to purge under {}", src.display());
    }
    Ok(())
}

// ------------------------------------------------------------------ janitor

pub fn run_janitor(a: &Args) -> R<()> {
    let src = PathBuf::from(a.pos1("lustre-root")?);
    let dst = a.dest()?;
    let c = cfg(a, &dst, 1)?;
    let older = a.usize("older-than-hours", 24)? as u64 * 3600;
    let state = src.join(STATE_DIR);
    let now = epoch_now();
    let mut keep = Vec::new();
    for (ts, unit, target) in records_read(&state) {
        if now.saturating_sub(ts) < older {
            keep.push((ts, unit, target));
            continue;
        }
        let final_unit = target.strip_suffix(INCOMPLETE_SUFFIX).unwrap_or(&target).to_string();
        if c.backend.exists(&Backend::join(&final_unit, LICENSE_NAME)) == Some(true) {
            continue; // sealed: done with the record
        }
        if c.dry {
            println!("JANITOR would remove {} (age {}s)", target, now - ts);
            keep.push((ts, unit, target));
        } else {
            match c.backend.remove_tree(&target) {
                Ok(()) => log(&format!("janitor removed {}", target)),
                Err(e) => {
                    log(&format!("janitor cannot remove {}: {e}", target));
                    keep.push((ts, unit, target));
                }
            }
        }
    }
    records_write(&state, &keep)?;
    Ok(())
}

#[cfg(test)]
mod io_meter_tests {
    #[test]
    fn children_bytes_are_metered_live() {
        // a dd child reading /dev/zero: pure syscall reads (rchar grows,
        // read_bytes stays zero) -- exactly the xrdcp signature
        let child = std::process::Command::new("dd")
            .args(["if=/dev/zero", "of=/dev/null", "bs=1M", "count=5000"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(400));
        let seen = super::children_read_bytes();
        let _ = child.wait_with_output();
        assert!(seen > 10 << 20, "meter saw only {seen} bytes from a busy child");
    }
}
