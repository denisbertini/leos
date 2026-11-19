//! leos tui -- read-only dashboard of the Lustre -> EOS archive pipeline.
//!
//! Every pixel here comes from on-disk marker-chain truth: a unit is
//! WRITING while its completion marker is absent, READY with
//! `bbstage.final`, ARCHIVING once `.leos` records an archive pass for
//! it (store.rs appends a record BEFORE the first copy, so a record
//! without a license means in-flight-or-failed, never unknown), and
//! SEALED when the local `.archived` license is in place. The TUI
//! never starts, pauses, or deletes anything: [v]erify and [x]-style
//! purges deliberately stay CLI-only, because those have gates.
//!
//! The archive transfer worker panel (live GiB/s, per-stream gauges)
//! arrives when tui mode drives run_archive in-process; until then the
//! right column shows pipeline totals and the seal ledger.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, UNIX_EPOCH};

use ratatui::crossterm::event::{self, Event, KeyEventKind};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Paragraph, Row, Sparkline, Table};
use ratatui::Frame;

use crate::args::Args;
use crate::claims::{self, CLAIMS_DIR, STALE_SECS};
use crate::core::{
    discover, epoch_now, human_bytes, records_read, walk_unit, LICENSE_NAME, MARKER_DEFAULT,
    STATE_DIR,
};

type R<T> = Result<T, String>;

// Fixed 24-bit palette: immune to site palette remaps (observed: ANSI
// "green" rendered yellow, "cyan" rendered green). Truecolor passes
// through any honest terminal emulator unchanged.
const LIGHT_BLUE: Color = Color::Rgb(95, 175, 255); // gauge fill, WRITING
const CYAN: Color = Color::Rgb(0, 215, 215);        // ARCHIVING, owners
const YELLOW: Color = Color::Rgb(255, 215, 95);     // READY
const GREEN: Color = Color::Rgb(95, 215, 135);      // SEALED, sparkline
const RED: Color = Color::Rgb(255, 95, 95);         // errors, abandoned
const DIM: Color = Color::Rgb(120, 120, 120);       // secondary text

// ----------------------------------------------------------------- model

#[derive(Clone, Copy, PartialEq)]
enum State {
    Writing,
    Ready,
    Archiving,
    Sealed,
}

impl State {
    fn label(self) -> &'static str {
        match self {
            State::Writing => "WRITING (active)",
            State::Ready => "READY (bbstage.final)",
            State::Archiving => "ARCHIVING",
            State::Sealed => "SEALED (.archived)",
        }
    }
    fn color(self) -> Color {
        match self {
            State::Writing => LIGHT_BLUE,
            State::Ready => YELLOW,
            State::Archiving => CYAN,
            State::Sealed => GREEN,
        }
    }
}

struct UnitRow {
    name: String,
    files: usize,
    bytes: u64,
    state: State,
    age: u64, // seconds since last mtime touch
}

struct Snapshot {
    root: String,
    dest: Option<String>,
    token: Option<String>,  // human validity from the Helios JWT
    scratch: Option<String>, // "68% (682.1 / 1000.0 TB)"
    units: Vec<UnitRow>,
    claims: Vec<(String, claims::Claim, u64)>, // (name, payload, beat age s)
    rates: Vec<(String, f64)>, // GiB-accurate bytes/s, derived scan-to-scan
    aggregate: f64,            // sum of the above
    history: VecDeque<f64>,    // aggregate GiB/s per scan, live series
    ledger: VecDeque<String>, // seal-ledger lines, newest first
    scan_ms: u64,
    error: Option<String>,
}

// ----------------------------------------------------------------- scan

/// One full state read of the tree. Runs on a worker thread; the frame
/// loop keeps painting the previous snapshot while this is in flight.
fn scan(root: &Path, marker: &str, dest: Option<String>) -> Snapshot {
    let t0 = Instant::now();
    let mut error = None;

    // live multi-node claims: the queue IS the worker feed
    let claims = claims::live(&root.join(STATE_DIR).join(CLAIMS_DIR), STALE_SECS);
    let claimed: Vec<&str> = claims
        .iter()
        .filter(|(_, _, age)| *age <= STALE_SECS)
        .map(|(name, _, _)| name.strip_suffix(".unit").unwrap_or(name.as_str()))
        .collect();

    let mut units: Vec<UnitRow> = Vec::new();
    for u in discover(root, marker) {
        let (files, bytes) = match walk_unit(&u.path, false, 1) {
            Ok(w) => (w.entries.len(), w.entries.iter().map(|e| e.size).sum()),
            Err(e) => {
                error = Some(format!("walk {}: {e}", u.name));
                (0, 0)
            }
        };
        let archiving =
            !u.licensed && (claimed.contains(&u.name.as_str()) || record_seen(root, &u.name));
        units.push(UnitRow {
            name: u.name,
            files,
            bytes,
            state: if u.licensed {
                State::Sealed
            } else if archiving {
                State::Archiving
            } else {
                State::Ready
            },
            age: epoch_now().saturating_sub(u.mtime),
        });
    }

    // in-progress checkpoints: dirs that look like unit candidates but
    // hold no marker yet; shallow file count only, never descend deep.
    for (_path, name, mtime) in discover_writing(root, marker) {
        units.push(UnitRow {
            name,
            files: 0,
            bytes: 0,
            state: State::Writing,
            age: epoch_now().saturating_sub(mtime),
        });
    }
    units.sort_by_key(|u| std::cmp::Reverse(u.age)); // stalest (sealable) first

    let ledger = records_read(&root.join(STATE_DIR))
        .into_iter()
        .rev()
        .take(200)
        .map(|(epoch, name, target)| {
            let sealed = units.iter().any(|u| u.name == name && u.state == State::Sealed);
            let hh = (epoch % 86400) / 3600;
            let mm = (epoch % 3600) / 60;
            let ss = epoch % 60;
            format!(
                "[{hh:02}:{mm:02}:{ss:02}] {} {name} -> {target}",
                if sealed { "SEALED" } else { "claimed" }
            )
        })
        .collect();

    Snapshot {
        root: root.display().to_string(),
        dest,
        token: token_status(),
        scratch: statvfs_used(root),
        units,
        claims,
        rates: Vec::new(),
        aggregate: 0.0,
        history: VecDeque::new(),
        ledger,
        scan_ms: t0.elapsed().as_millis() as u64,
        error,
    }
}

/// Dirs that hold checkpoint-ish content (a *.bp dir or bbstage.* files)
/// but no completion marker -- i.e. a run in flight. Stops descending at
/// markers, licenses, and state dirs, so nothing inside a unit is seen.
fn discover_writing(root: &Path, marker: &str) -> Vec<(PathBuf, String, u64)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        let mut has_marker = false;
        let mut candidate = false;
        let mut subdirs = Vec::new();
        for ent in rd.flatten() {
            let name = ent.file_name().to_string_lossy().to_string();
            let ft = match ent.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if name == marker {
                has_marker = true;
            } else if name == LICENSE_NAME || name == STATE_DIR || name.starts_with('.') {
                continue;
            } else if ft.is_dir() {
                if name.ends_with(".bp") || name.ends_with(".h5") {
                    candidate = true;
                }
                subdirs.push(ent.path());
            } else if ft.is_file() && name.starts_with("bbstage.") {
                candidate = true; // bbstage.* working files, no final yet
            }
        }
        if has_marker {
            continue; // a complete unit: owned by discover()
        }
        if candidate && dir != root {
            let mtime = std::fs::metadata(&dir)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let name = dir
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            out.push((dir, name, mtime));
            continue; // the unit is this dir; don't descend
        }
        stack.extend(subdirs);
    }
    out
}

/// Helios tokens are JWTs: decode the payload's exp claim, no deps.
fn token_status() -> Option<String> {
    let tok = std::env::var("LEOS_TOKEN")
        .or_else(|_| std::env::var("EOSAUTHZ"))
        .ok()?;
    let parts: Vec<&str> = tok.trim().split('.').collect();
    if parts.len() != 3 {
        return Some("opaque".into()); // static token: validity not readable
    }
    let payload = b64url_decode(parts[1])?;
    let json = String::from_utf8(payload).ok()?;
    let i = json.find("\"exp\"")?;
    let digits: String = json[i + 5..]
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let exp: u64 = digits.parse().ok()?;
    let now = epoch_now();
    Some(if exp <= now {
        "EXPIRED".to_string()
    } else {
        let left = exp - now;
        format!("VALID ({}h {:02}m)", left / 3600, (left % 3600) / 60)
    })
}

fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    let mut bits = 0u32;
    let mut nbits = 0u32;
    let mut out = Vec::new();
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => continue, // '=' padding and any URL noise
        } as u32;
        bits = (bits << 6) | v;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push((bits >> nbits) as u8);
        }
    }
    Some(out)
}

/// used/total of the filesystem under `path` (scratch fill bar).
fn statvfs_used(path: &Path) -> Option<String> {
    use std::ffi::CString;
    let c = CString::new(path.as_os_str().as_encoded_bytes().to_vec()).ok()?;
    let mut sv: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut sv) } != 0 {
        return None;
    }
    let f = sv.f_frsize as u64;
    let total = sv.f_blocks as u64 * f;
    let avail = sv.f_bavail as u64 * f;
    if total == 0 {
        return None;
    }
    let used = total - avail;
    Some(format!(
        "{}% used ({} of {}, {} free)",
        (used * 100) / total,
        human_bytes(used),
        human_bytes(total),
        human_bytes(avail)
    ))
}

// ----------------------------------------------------------------- render

fn draw(f: &mut Frame, s: &Snapshot, paused: bool, next_scan: Duration) {
    let lines = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(f.area());

    // header: identity, target, token, scratch fill
    let mut hdr = vec![
        Span::styled(
            format!(" leos v{} ", env!("CARGO_PKG_VERSION")),
            Style::default().fg(LIGHT_BLUE).add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(" scan {}ms", s.scan_ms)),
    ];
    if let Some(d) = &s.dest {
        hdr.push(Span::raw(format!(" | target: {d}")));
    }
    if let Some(t) = &s.token {
        let bad = t == "EXPIRED";
        hdr.push(Span::raw(" | token: "));
        hdr.push(Span::styled(
            t.clone(),
            Style::default()
                .fg(if bad { RED } else { GREEN })
                .add_modifier(if bad { Modifier::BOLD } else { Modifier::empty() }),
        ));
    }
    if let Some(sc) = &s.scratch {
        hdr.push(Span::raw(format!(" | Lustre scratch: {sc}")));
    }
    f.render_widget(Paragraph::new(Line::from(hdr)), lines[0]);

    // body: units left, pipeline + ledger right
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
        .split(lines[1]);

    let rows: Vec<Row> = s
        .units
        .iter()
        .map(|u| {
            Row::new(vec![
                Cell::from(u.name.clone()),
                Cell::from(if u.state == State::Writing {
                    "-".to_string()
                } else {
                    u.files.to_string()
                }),
                Cell::from(if u.state == State::Writing {
                    "...".to_string()
                } else {
                    human_bytes(u.bytes)
                }),
                Cell::from(u.state.label().to_string()).style(
                    Style::default()
                        .fg(u.state.color())
                        .add_modifier(if u.state == State::Archiving {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
                Cell::from(fmt_age(u.age)),
            ])
        })
        .collect();
    let n_sealed = s.units.iter().filter(|u| u.state == State::Sealed).count();
    let table = Table::new(
        rows,
        [
            Constraint::Min(20),
            Constraint::Length(7),
            Constraint::Length(11),
            Constraint::Length(22),
            Constraint::Length(9),
        ],
    )
    .header(
        Row::new(vec!["UNIT", "FILES", "SIZE", "STATUS", "IDLE"])
            .style(Style::default().add_modifier(Modifier::UNDERLINED)),
    )
    .block(Block::bordered().title(format!(
        " CHECKPOINT UNITS ({}) ",
        s.root
    )));
    f.render_widget(table, cols[0]);

    let right_cons: Vec<Constraint> = {
        let mut c = vec![Constraint::Length(5)];
        if !s.claims.is_empty() {
            c.push(Constraint::Length(s.claims.len().min(8) as u16 * 2 + 2));
        }
        // throughput panel is always present: a fast archive's spike
        // must be visible scrolling away, not vanish with the claims
        c.push(Constraint::Length(9));
        c.push(Constraint::Min(3));
        c
    };
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints(right_cons)
        .split(cols[1]);

    let mut tot = [0u64; 4];
    let mut cnt = [0usize; 4];
    for u in &s.units {
        let i = match u.state {
            State::Writing => 0,
            State::Ready => 1,
            State::Archiving => 2,
            State::Sealed => 3,
        };
        cnt[i] += 1;
        tot[i] += u.bytes;
    }
    let pipe = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(" writing ", Style::default().fg(LIGHT_BLUE)),
            Span::raw(format!("{} units   ", cnt[0])),
            Span::styled(" ready ", Style::default().fg(YELLOW)),
            Span::raw(format!("{} units / {}", cnt[1], human_bytes(tot[1]))),
        ]),
        Line::from(vec![
            Span::styled(" archiving ", Style::default().fg(CYAN)),
            Span::raw(format!("{} units / {}   ", cnt[2], human_bytes(tot[2]))),
            Span::styled(" sealed ", Style::default().fg(GREEN)),
            Span::raw(format!("{} units / {}", cnt[3], human_bytes(tot[3]))),
        ]),
        Line::from(if let Some(e) = &s.error {
            Span::styled(e.clone(), Style::default().fg(RED))
        } else {
            Span::styled(
                " state read from markers only -- this view never mutates data",
                Style::default().fg(DIM),
            )
        }),
    ])
    .block(Block::bordered().title(" PIPELINE "));
    f.render_widget(pipe, right[0]);


    // fleet view: per worker one label line and one Gauge widget,
    // ratatui gauge-example style (unicode fractional blocks, the
    // percentage centered on the bar)
    if !s.claims.is_empty() {
        let shown: Vec<&(String, claims::Claim, u64)> = s.claims.iter().take(8).collect();
        let title = if s.aggregate > 1.0 {
            format!(" MULTI-NODE WORKERS - {:.2} GiB/s aggregate ", s.aggregate / (1u64 << 30) as f64)
        } else {
            " MULTI-NODE WORKERS (.leos/claims, O_EXCL on Lustre) ".to_string()
        };
        let block = Block::bordered().title(title);
        let inner = block.inner(right[1]);
        f.render_widget(block, right[1]);
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints(
                shown
                    .iter()
                    .flat_map(|_| [Constraint::Length(1), Constraint::Length(1)])
                    .collect::<Vec<Constraint>>(),
            )
            .split(inner);
        for (i, (unit, c, age)) in shown.iter().enumerate() {
            let stale = *age > STALE_SECS;
            let short = unit.trim_end_matches(".unit");
            let frac = if stale || c.total == 0 {
                0.0
            } else {
                c.done.min(c.total) as f64 / c.total as f64
            };
            // owner line: rate derived scan-to-scan from the worker's
            // byte ledger, never transmitted
            let rate = s
                .rates
                .iter()
                .find(|(n, _)| n == unit)
                .map(|(_, r)| format!("{:.1} GiB/s", r / (1u64 << 30) as f64))
                .unwrap_or_else(|| "-".to_string());
            let state = if stale {
                format!("{age}s ABANDONED")
            } else if c.total > 0 {
                format!("{:>3}%", (c.done.min(c.total) * 100) / c.total)
            } else {
                " ... ".to_string()
            };
            let head = format!(" {short:<20} {:<16} {state} {rate}", c.owner());
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    head,
                    Style::default().fg(if stale { RED } else { CYAN }),
                ))),
                rows[i * 2],
            );
            // Custom text bar: no Gauge widget, no label overlay, no
            // background style ANYWHERE -- there is no surface left for
            // a palette-remapping terminal to paint a black square on.
            // Solid blocks fill, faint blocks show the remainder, and
            // the percentage is bold plain text at the right edge.
            let bar = rows[i * 2 + 1];
            let pct = if stale {
                "ABANDONED".to_string()
            } else if c.total > 0 {
                format!("{}%", (c.done.min(c.total) * 100) / c.total)
            } else {
                "planning".to_string()
            };
            let col = if stale { RED } else { LIGHT_BLUE };
            let inner = (bar.width as usize)
                .saturating_sub(pct.chars().count() + 2)
                .max(1);
            // work in eighths of a cell (same resolution ratatui's
            // Gauge uses internally): exact math, no float drift
            const PARTS: [&str; 7] = [
                "\u{258F}", "\u{258E}", "\u{258D}", "\u{258C}",
                "\u{258B}", "\u{258A}", "\u{2589}",
            ];
            let eighths = (frac * inner as f64 * 8.0).round() as usize;
            let full = (eighths / 8).min(inner);
            let rem = if full < inner { eighths % 8 } else { 0 };
            let mut fill = "\u{2588}".repeat(full);
            if rem > 0 {
                fill.push_str(PARTS[rem - 1]);
            }
            let empty = "\u{2591}".repeat(inner - fill.chars().count());
            let line = Line::from(vec![
                Span::styled(fill, Style::default().fg(col)),
                Span::styled(empty, Style::default().fg(DIM)),
                Span::styled(
                    format!(" {pct}"),
                    Style::default().fg(col).add_modifier(Modifier::BOLD),
                ),
            ]);
            f.render_widget(Paragraph::new(line), bar);
        }
    }

    // live time series of the fleet aggregate: samples are the same
    // scan-to-scan deltas, accumulated in the driver; Sparkline renders
    // braille-resolution dots over a fixed 240-sample window (~12 min
    // at the default 3 s refresh)
    // the slot depends on whether the workers panel is present
    let chart_i = if s.claims.is_empty() { 1 } else { 2 };
    if right.len() >= 3 {
        let peak = s.history.iter().fold(0.0f64, |a, &v| a.max(v));
        let cur = *s.history.back().unwrap_or(&0.0);
        // Resolution-independent: bars are NORMALIZED to the observed
        // peak (steady rate = level line at ~77% height, 30% headroom)
        // instead of an absolute 0.1 GiB/s grid -- an absolute axis
        // tuned for cluster speeds clips desktop rates to zero (0.02
        // GiB/s rendered an empty panel). Sparkline renders the FIRST
        // width samples, so build an exactly-right-aligned window.
        let w = (right[chart_i].width as usize).saturating_sub(2);
        let mut data: Vec<u64> = Vec::with_capacity(w);
        let shown = s.history.len().min(w);
        data.resize(w - shown, 0);
        let norm = if peak > 0.0 { 100.0 / peak } else { 0.0 };
        data.extend(
            s.history
                .iter()
                .skip(s.history.len() - shown)
                .map(|v| (v * norm).round() as u64),
        );
        // axis: normalized peak 100 + 30% headroom
        let spark = Sparkline::default()
            .max(130)
            .data(data)
            .style(Style::default().fg(GREEN))
            .block(Block::bordered().title(format!(
                " EOS throughput {:.2} | peak {:.2} GiB/s ",
                cur, peak
            )));
        f.render_widget(spark, right[chart_i]);
    }

    let ledger_panel = right.len() - 1;
    let ledger: Vec<Line> = s
        .ledger
        .iter()
        .map(|l| {
            let col = if l.contains("SEALED") {
                GREEN
            } else {
                DIM
            };
            Line::from(Span::styled(l.clone(), Style::default().fg(col)))
        })
        .collect();
    f.render_widget(
        Paragraph::new(ledger).block(
            Block::bordered().title(format!(" SEAL LEDGER (.leos/records, {}) ", n_sealed)),
        ),
        right[ledger_panel],
    );

    // footer
    let foot = if paused {
        " [q] quit   [r] refresh   [p] RESUME auto-refresh (paused)".to_string()
    } else {
        format!(
            " [q] quit   [r] refresh   [p] pause   next scan in {:.0}s   \
             archive/verify/purge stay CLI",
            next_scan.as_secs_f64()
        )
    };
    f.render_widget(
        Paragraph::new(Span::styled(foot, Style::default())),
        lines[2],
    );
}

fn fmt_age(s: u64) -> String {
    if s < 120 {
        format!("{s}s")
    } else if s < 7200 {
        format!("{}m", s / 60)
    } else {
        format!("{}h", s / 3600)
    }
}

/// Was this unit recorded by an archive pass (claim-less local mode)?
fn record_seen(root: &Path, unit: &str) -> bool {
    records_read(&root.join(STATE_DIR))
        .iter()
        .any(|(_, name, _)| name == unit)
}

// ---------------------------------------------------------------- driver

pub fn run(a: &Args) -> R<()> {
    let root = PathBuf::from(a.pos1("path")?);
    if !root.is_dir() {
        return Err(format!("{} is not a directory", root.display()));
    }
    let marker = a.opt("marker").unwrap_or(MARKER_DEFAULT).to_string();
    let dest = a.dest().ok();
    let scan_period: u64 = a
        .usize("refresh", 3)
        .unwrap_or(3)
        .clamp(1, 300) as u64;

    let (tx, rx) = mpsc::channel::<Snapshot>();
    let root_for_scan = root.clone();
    let marker_for_scan = marker.clone();

    let mut snapshot = Snapshot {
        root: root.display().to_string(),
        dest,
        token: token_status(),
        scratch: None,
        units: Vec::new(),
        claims: Vec::new(),
        rates: Vec::new(),
        aggregate: 0.0,
        history: VecDeque::new(),
        ledger: VecDeque::new(),
        scan_ms: 0,
        error: Some("scanning...".into()),
    };

    let mut terminal = ratatui::init();
    let res: R<()> = (|| {
        let mut paused = false;
        let mut inflight = false;
        let mut prev: HashMap<String, (u64, Instant)> = HashMap::new();
        let mut hist: VecDeque<f64> = VecDeque::new();
        let mut prev_sealed: Option<(u64, Instant)> = None;
        let mut last_scan = Instant::now() - Duration::from_secs(scan_period);
        loop {
            if !paused && !inflight && last_scan.elapsed() >= Duration::from_secs(scan_period) {
                let (r, m, tx) = (root_for_scan.clone(), marker_for_scan.clone(), tx.clone());
                let d = snapshot.dest.clone();
                inflight = true;
                last_scan = Instant::now();
                thread::spawn(move || {
                    let _ = tx.send(scan(&r, &m, d));
                });
            }
            while let Ok(mut s) = rx.try_recv() {
                // scan-to-scan deltas of the workers' byte ledgers are
                // their live rates -- derived, never transmitted
                let now = Instant::now();
                let mut rates = Vec::new();
                let mut agg = 0f64;
                for (name, c, _) in &s.claims {
                    if let Some((pdone, t)) = prev.insert(name.clone(), (c.done, now)) {
                        let dt = t.elapsed().as_secs_f64();
                        if c.done > pdone && dt > 0.5 {
                            let r = (c.done - pdone) as f64 / dt;
                            rates.push((name.clone(), r));
                            agg += r;
                        }
                    }
                }
                prev.retain(|n, _| s.claims.iter().any(|(cn, _, _)| cn == n));
                // fallback meter for archives run WITHOUT --claim: the
                // byte ledger is then blank, but sealed bytes still grow
                // scan to scan -- coarse (granularity = one unit's seal
                // event), yet a real signal, not zero. Live claim
                // deltas always win when present.
                let sealed_now: u64 = s
                    .units
                    .iter()
                    .filter(|u| u.state == State::Sealed)
                    .map(|u| u.bytes)
                    .sum();
                if agg == 0.0 {
                    if let Some((b, t)) = prev_sealed {
                        let dt = t.elapsed().as_secs_f64();
                        if sealed_now > b && dt > 0.5 {
                            agg = (sealed_now - b) as f64 / dt;
                        }
                    }
                }
                prev_sealed = Some((sealed_now, Instant::now()));
                s.rates = rates;
                s.aggregate = agg;
                hist.push_back(agg / (1u64 << 30) as f64);
                while hist.len() > 240 {
                    hist.pop_front();
                }
                s.history = hist.clone();
                snapshot = s;
                inflight = false;
            }
            let next = if paused {
                Duration::ZERO
            } else {
                Duration::from_secs(scan_period).saturating_sub(last_scan.elapsed())
            };
            terminal.draw(|f| draw(f, &snapshot, paused, next)).map_err(|e| e.to_string())?;
            if event::poll(Duration::from_millis(200)).map_err(|e| e.to_string())? {
                match event::read().map_err(|e| e.to_string())? {
                    Event::Key(k) if k.kind == KeyEventKind::Press => match k.code {
                        ratatui::crossterm::event::KeyCode::Char('q')
                        | ratatui::crossterm::event::KeyCode::Esc => break,
                        ratatui::crossterm::event::KeyCode::Char('r') => {
                            last_scan = Instant::now() - Duration::from_secs(scan_period)
                        }
                        ratatui::crossterm::event::KeyCode::Char('p') => paused = !paused,
                        _ => {}
                    },
                    Event::Resize(_, _) => {}
                    _ => {}
                }
            }
        }
        Ok(())
    })();
    ratatui::restore();
    res
}
