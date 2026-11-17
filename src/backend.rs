//! Store backends. The EOS wire is subprocesses, never a library: leos
//! orchestrates `eos cp` (EOS console) or `xrdcp`/`xrdfs` (xrootd clients),
//! so site authentication (kinit, tokens) stays with the tools that already
//! work. `Copy` is a local-filesystem backend for testing and dry-runs.
//!
//! Capability tokens (Helios-style): the token is handed to every child as
//! `EOSAUTHZ` (metadata ops: mkdir/mv/rm/file stat authenticate that way) and
//! appended as `?authz=` on copy URLs (data transfer). Reads through
//! `eos cp` with a token are refused: the XRootD client applies authz only
//! to copy *destinations*, never sources (known eos cp_cmd_native.cc bug),
//! so downloads must go through xrdcp with the token on the source URL.

use std::path::Path;
use std::process::Command;

pub type R<T> = Result<T, String>;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    EosCp, // EOS console: `eos cp|mkdir|mv|rm|file stat`, paths /eos/<inst>/...
    Xrdcp, // xrootd clients: `xrdcp`, `xrdfs`; URLs root://host//path
    Copy,  // local filesystem (test / fake-tier)
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::EosCp => "eoscp",
            Kind::Xrdcp => "xrdcp",
            Kind::Copy => "copy",
        }
    }
}

#[derive(Clone)]
pub struct Backend {
    pub kind: Kind,
    pub streams: usize,
    /// literal extra arguments appended to every copy command
    pub extra: Vec<String>,
    /// xrdcp backend: host parsed from root://HOST//path
    host: Option<String>,
    /// capability token (Helios EOS): children get $EOSAUTHZ, copy URLs get
    /// `?authz=`; empty string counts as no token.
    token: Option<String>,
}

pub fn is_remote(path: &str) -> bool {
    path.starts_with("root://") || path.starts_with("/eos/")
}

fn head(s: &str, n: usize) -> String {
    let t = s.trim();
    if t.is_empty() {
        String::new()
    } else if t.len() <= n {
        format!("stderr: {t}")
    } else {
        let mut end = n;
        while end > 0 && !t.is_char_boundary(end) {
            end -= 1; // never slice mid-UTF8
        }
        format!("stderr: {}...", &t[..end])
    }
}

fn err_cmd(what: &str, cmd: &std::process::Output) -> String {
    let code = cmd.status.code().map(|c| c.to_string()).unwrap_or_else(|| "signal".into());
    // keep the HEAD of stderr: real complaints come first, progress/warning
    // summaries last (field lesson: tail-truncation hid the actual error)
    format!("{what}: exit={code} {}", head(&String::from_utf8_lossy(&cmd.stderr), 400))
}

impl Backend {
    /// `--backend auto|eoscp|xrdcp|copy`; auto: root:// -> xrdcp,
    /// /eos/ -> eoscp, anything else -> copy (local path).
    pub fn from_args(
        kind: &str,
        path: &str,
        streams: usize,
        extra: &str,
        token: Option<&str>,
    ) -> R<Backend> {
        let kind = match kind {
            "auto" if path.starts_with("root://") => Kind::Xrdcp,
            "auto" if path.starts_with("/eos/") => Kind::EosCp,
            "auto" => Kind::Copy,
            "eoscp" => Kind::EosCp,
            "xrdcp" => Kind::Xrdcp,
            "copy" => Kind::Copy,
            other => return Err(format!("unknown --backend '{other}' (auto|eoscp|xrdcp|copy)")),
        };
        let host = match kind {
            Kind::Xrdcp => {
                let rest = path
                    .strip_prefix("root://")
                    .ok_or_else(|| format!("xrdcp backend needs root://host//path, got '{path}'"))?;
                let (h, after) = rest.split_once('/').unwrap_or((rest, ""));
                if !after.starts_with('/') {
                    return Err(format!(
                        "xrootd URLs need a double slash before the path: root://{h}//eos/..."
                    ));
                }
                Some(h.to_string())
            }
            _ => None,
        };
        Ok(Backend {
            kind,
            streams,
            extra: extra.split_whitespace().map(|s| s.to_string()).collect(),
            host,
            token: token.filter(|t| !t.is_empty()).map(|s| s.to_string()),
        })
    }

    fn host(&self) -> R<&str> {
        self.host.as_deref().ok_or("internal: missing host".to_string())
    }

    /// every child process carries the token in the environment, which is
    /// how the EOS console authenticates metadata operations (`EOSAUTHZ`)
    fn cmd(&self, prog: &str) -> Command {
        let mut c = Command::new(prog);
        if let Some(t) = &self.token {
            c.env("EOSAUTHZ", t);
        }
        c
    }

    pub fn join(base: &str, rel: &str) -> String {
        let b = base.trim_end_matches('/');
        let r = rel.trim_start_matches('/');
        if r.is_empty() {
            b.to_string()
        } else {
            format!("{b}/{r}")
        }
    }

    /// xrdfs takes host + bare path, not URLs: strip root://HOST and
    /// collapse the canonical double slash (root://h//eos/x -> /eos/x).
    pub fn rpath(p: &str) -> &str {
        match p.strip_prefix("root://") {
            Some(rest) => match rest.find('/') {
                Some(i) => {
                    let after = &rest[i..];
                    after.strip_prefix('/').unwrap_or(after)
                }
                None => "/",
            },
            None => p,
        }
    }

    /// full xrootd URL for copy commands, token appended as an authz query
    fn url(&self, path: &str) -> String {
        let base = match self.kind {
            Kind::Xrdcp if path.starts_with("root://") => path.to_string(),
            Kind::Xrdcp => format!("root://{}{}", self.host.as_deref().unwrap_or(""), path),
            _ => path.to_string(),
        };
        match &self.token {
            Some(t) if !base.contains("?authz=") => format!("{base}?authz={t}"),
            _ => base,
        }
    }

    /// Conservative capability probe: can we create and remove directories
    /// at the destination? True => honor atomic rename seals (verified at
    /// seal time; rename failure is a hard error, never a silent downgrade).
    pub fn probe_dirs(&self, dst_root: &str) -> bool {
        let probe = Self::join(dst_root, ".leos-probe");
        if self.make_dirs(&probe).is_err() {
            return false;
        }
        // cleanup must succeed too; `eos rm -f` refuses directories, so
        // escalate to a recursive remove before judging the store capable
        if self.remove(&probe).is_err() || self.exists(&probe) == Some(true) {
            if self.remove_tree(&probe).is_err() || self.exists(&probe) == Some(true) {
                return false;
            }
        }
        true
    }

    pub fn exists(&self, path: &str) -> Option<bool> {
        match self.kind {
            Kind::Copy => Some(Path::new(path).exists()),
            Kind::EosCp => {
                let out = self.cmd("eos").args(["file", "stat", path]).output().ok()?;
                Some(out.status.success())
            }
            Kind::Xrdcp => {
                let h = self.host.as_deref()?;
                let out = self
                    .cmd("xrdfs")
                    .args([h, "stat", Self::rpath(path)])
                    .output()
                    .ok()?;
                Some(out.status.success()
                    && String::from_utf8_lossy(&out.stdout).contains("Status=OK"))
            }
        }
    }

    pub fn size(&self, path: &str) -> Option<u64> {
        match self.kind {
            Kind::Copy => std::fs::metadata(path).ok().map(|m| m.len()),
            Kind::EosCp => {
                let o = self.cmd("eos").args(["file", "stat", path]).output().ok()?;
                if !o.status.success() {
                    return None;
                }
                parse_size(&String::from_utf8_lossy(&o.stdout))
            }
            Kind::Xrdcp => {
                let h = self.host.as_deref()?;
                let o = self
                    .cmd("xrdfs")
                    .args([h, "stat", Self::rpath(path)])
                    .output()
                    .ok()?;
                if !o.status.success() {
                    return None;
                }
                parse_size(&String::from_utf8_lossy(&o.stdout))
            }
        }
    }

    pub fn make_dirs(&self, dir: &str) -> R<()> {
        match self.kind {
            Kind::Copy => {
                std::fs::create_dir_all(dir).map_err(|e| format!("mkdir {}: {e}", dir))
            }
            Kind::EosCp => {
                let o = self.cmd("eos").args(["mkdir", "-p", dir]).output()
                    .map_err(|e| format!("spawn eos mkdir: {e}"))?;
                if o.status.success() || self.exists(dir) == Some(true) {
                    Ok(())
                } else {
                    Err(err_cmd(&format!("eos mkdir {dir}"), &o))
                }
            }
            Kind::Xrdcp => {
                let h = self.host()?;
                let dir = Self::rpath(dir).to_string();
                let o = self.cmd("xrdfs").args([h, "mkdir", "-p", &dir]).output()
                    .map_err(|e| format!("spawn xrdfs mkdir: {e}"))?;
                if o.status.success() || self.exists(&dir) == Some(true) {
                    return Ok(());
                }
                // fallback without -p: create ancestors one by one
                let mut cur = String::new();
                for part in dir.trim_start_matches('/').split('/') {
                    if part.is_empty() {
                        continue;
                    }
                    cur = format!("{cur}/{part}");
                    let _ = self.cmd("xrdfs").args([h, "mkdir", &cur]).output();
                }
                if self.exists(&dir) == Some(true) {
                    Ok(())
                } else {
                    Err(err_cmd(&format!("xrdfs mkdir {dir}"), &o))
                }
            }
        }
    }

    pub fn copy_to(&self, local: &Path, remote: &str) -> R<()> {
        match self.kind {
            Kind::Copy => {
                if let Some(p) = Path::new(remote).parent() {
                    std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
                }
                std::fs::copy(local, remote).map(|_| ()).map_err(|e| e.to_string())
            }
            Kind::EosCp => {
                // EOS >= 5.5 clients apply $EOSAUTHZ to the SOURCE argument
                // as well, corrupting local paths ("/tmp/f?&authz=...") on
                // uploads. Console copies therefore run WITHOUT the env
                // token and carry it solely as ?authz= on the destination --
                // the site-demonstrated form. Metadata ops keep the env.
                let mut c = self.cmd("eos");
                c.env_remove("EOSAUTHZ");
                c.args(["cp", "-n", "--streams"]);
                c.arg(self.streams.to_string());
                c.args(&self.extra);
                c.arg(local).arg(self.url(remote));
                let o = c.output().map_err(|e| format!("spawn eos cp: {e}"))?;
                if o.status.success() { Ok(()) } else { Err(err_cmd("eos cp", &o)) }
            }
            Kind::Xrdcp => {
                let mut c = self.cmd("xrdcp");
                c.args(["-f", "--streams"]);
                c.arg(self.streams.to_string());
                c.args(&self.extra);
                c.arg(local).arg(self.url(remote));
                let o = c.output().map_err(|e| format!("spawn xrdcp: {e}"))?;
                if o.status.success() { Ok(()) } else { Err(err_cmd("xrdcp", &o)) }
            }
        }
    }

    pub fn copy_from(&self, remote: &str, local: &Path) -> R<()> {
        match self.kind {
            Kind::Copy => {
                if let Some(p) = local.parent() {
                    std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
                }
                std::fs::copy(remote, local).map(|_| ()).map_err(|e| e.to_string())
            }
            Kind::EosCp => {
                if self.token.is_some() {
                    return Err(
                        "reading with a capability token through `eos cp` is unsupported: the \
                         XRootD client applies authz only to copy destinations, never sources \
                         (known eos bug). Address the unit as root://<mgm-host>//eos/... instead \
                         -- that auto-selects the xrdcp backend, which puts the token on the \
                         source URL."
                            .to_string(),
                    );
                }
                let mut c = self.cmd("eos");
                c.args(["cp", "-n", "--streams"]);
                c.arg(self.streams.to_string());
                c.args(&self.extra);
                c.arg(remote).arg(local);
                let o = c.output().map_err(|e| format!("spawn eos cp: {e}"))?;
                if o.status.success() { Ok(()) } else { Err(err_cmd("eos cp (get)", &o)) }
            }
            Kind::Xrdcp => {
                let mut c = self.cmd("xrdcp");
                c.args(["-f", "--streams"]);
                c.arg(self.streams.to_string());
                c.args(&self.extra);
                c.arg(self.url(remote)).arg(local);
                let o = c.output().map_err(|e| format!("spawn xrdcp: {e}"))?;
                if o.status.success() { Ok(()) } else { Err(err_cmd("xrdcp (get)", &o)) }
            }
        }
    }

    pub fn rename(&self, from: &str, to: &str) -> R<()> {
        match self.kind {
            Kind::Copy => {
                if Path::new(to).exists() {
                    return Err(format!("seal target already exists: {to}"));
                }
                std::fs::rename(from, to).map_err(|e| format!("rename: {e}"))
            }
            Kind::EosCp => {
                let o = self.cmd("eos").args(["mv", from, to]).output()
                    .map_err(|e| format!("spawn eos mv: {e}"))?;
                if o.status.success() { Ok(()) } else { Err(err_cmd("eos mv", &o)) }
            }
            Kind::Xrdcp => {
                let h = self.host()?;
                let (from, to) = (Self::rpath(from), Self::rpath(to));
                let o = self.cmd("xrdfs").args([h, "mv", from, to]).output()
                    .map_err(|e| format!("spawn xrdfs mv: {e}"))?;
                if o.status.success() { Ok(()) } else { Err(err_cmd("xrdfs mv", &o)) }
            }
        }
    }

    pub fn remove(&self, path: &str) -> R<()> {
        match self.kind {
            Kind::Copy => {
                let p = Path::new(path);
                if !p.exists() {
                    return Ok(());
                }
                if p.is_dir() {
                    std::fs::remove_dir_all(p).map_err(|e| e.to_string())
                } else {
                    std::fs::remove_file(path).map_err(|e| e.to_string())
                }
            }
            Kind::EosCp => {
                let o = self.cmd("eos").args(["rm", "-f", path]).output()
                    .map_err(|e| format!("spawn eos rm: {e}"))?;
                if o.status.success() || self.exists(path) != Some(true) {
                    Ok(())
                } else {
                    Err(err_cmd("eos rm", &o))
                }
            }
            Kind::Xrdcp => {
                let h = self.host()?;
                let _ = self.cmd("xrdfs").args([h, "rm", Self::rpath(path)]).output()
                    .map_err(|e| format!("spawn xrdfs rm: {e}"))?;
                Ok(())
            }
        }
    }

    /// Recursive removal; xrdfs cannot do it portably -> Err (janitor warns).
    pub fn remove_tree(&self, path: &str) -> R<()> {
        match self.kind {
            Kind::Copy => {
                if Path::new(path).exists() {
                    std::fs::remove_dir_all(path).map_err(|e| e.to_string())
                } else {
                    Ok(())
                }
            }
            Kind::EosCp => {
                let o = self.cmd("eos").args(["rm", "-rf", path]).output()
                    .map_err(|e| format!("spawn eos rm -rf: {e}"))?;
                if o.status.success() { Ok(()) } else { Err(err_cmd("eos rm -rf", &o)) }
            }
            Kind::Xrdcp => Err(format!(
                "recursive removal unsupported via xrdfs; delete {path} manually \
                 (xrdfs rm per file) or use --backend eoscp"
            )),
        }
    }
}

/// tolerate `size=NNN`, `size: NNN`, `size NNN` in stat outputs
fn parse_size(s: &str) -> Option<u64> {
    let bytes = s.as_bytes();
    let idx = bytes.windows(4).position(|w| w == b"size")?;
    let mut i = idx + 4;
    while i < bytes.len() && (bytes[i] == b'=' || bytes[i] == b' ' || bytes[i] == b':') {
        i += 1;
    }
    let start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == start {
        return None;
    }
    s[start..i].parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_parse() {
        let b =
            Backend::from_args("auto", "root://eosxyz.cern.ch//eos/xyz/user/u1", 8, "", None)
                .unwrap();
        assert_eq!(b.kind, Kind::Xrdcp);
        assert_eq!(b.host.as_deref(), Some("eosxyz.cern.ch"));
        assert_eq!(Backend::join("root://h//eos/a/b", "c/d.txt"), "root://h//eos/a/b/c/d.txt");
        assert_eq!(Backend::join("/eos/a/b/", "/c"), "/eos/a/b/c");
        assert!(Backend::from_args("auto", "root://h/eos/a", 1, "", None).is_err()); // single slash
    }

    #[test]
    fn token_urls_and_rpath() {
        // full URLs pass through url() unharmed (no prefix doubling)
        let b = Backend::from_args("auto", "root://h//eos/a", 4, "", Some("TOKEN+eq/==")).unwrap();
        assert_eq!(b.url("root://h//eos/a/f1"), "root://h//eos/a/f1?authz=TOKEN+eq/==");
        // bare paths get the host, and the token
        assert_eq!(b.url("/eos/a/f1"), "root://h/eos/a/f1?authz=TOKEN+eq/==");
        // xrdfs wants the bare path, not the URL
        assert_eq!(Backend::rpath("root://h//eos/a/b"), "/eos/a/b");
        assert_eq!(Backend::rpath("/eos/a/b"), "/eos/a/b");
        // no token => no query anywhere
        let b0 = Backend::from_args("auto", "root://h//eos/a", 4, "", None).unwrap();
        assert_eq!(b0.url("root://h//eos/a/f1"), "root://h//eos/a/f1");
        // console reads with a token are refused (eos source-authz bug)
        let e = Backend::from_args("auto", "/eos/a", 4, "", Some("T")).unwrap();
        assert_eq!(e.kind, Kind::EosCp);
        let err = e.copy_from("/eos/a/f1", Path::new("/nonexistent/x")).unwrap_err();
        assert!(err.contains("xrdcp") && err.contains("source"), "{err}");
    }

    #[test]
    fn size_parse() {
        assert_eq!(parse_size("size=12345\nmtime=99"), Some(12345));
        assert_eq!(parse_size("  size : 42 "), Some(42));
        assert_eq!(parse_size("size=abc"), None);
        assert_eq!(parse_size("nothing"), None);
    }
}
