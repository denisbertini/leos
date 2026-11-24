//! leos: marker-chained checkpoint archiving between a POSIX scratch tier
//! (Lustre) and an EOS archive, for checkpoints produced by the bbstage
//! ADIOS2 engine.
//!
//!   leos archive <lustre-path> [--dest <eos-path>] [--dry-run]
//!   leos restore <eos-path>    [--dest <lustre-path>] [--dry-run]
//!   leos status  <path>
//!   leos verify  <path>            # checksum / round-trip check
//!   leos purge   <lustre-path> --yes
//!   leos janitor <lustre-root>
//!   leos tui     <lustre-path> [--refresh=SECS]   # read-only dashboard
//!
//! Dependencies: none for the data path -- the EOS wire is subprocesses
//! (`eos cp` / `xrdcp` / `xrdfs`), so site authentication stays with the
//! tools that already work. The single exception is the `tui` module's
//! ratatui/libc, which change nothing about the binary's portability:
//! pure Rust, static musl build unaffected.
//! Integrity = a manifest of per-file sizes and content adler32,
//! uploaded as `.archived` LAST: the license, and the deletion
//! license. Build: `cargo build --release`, or static with the musl target.

mod adler32;
mod args;
mod backend;
mod claims;
mod core;
mod store;
mod tui;

use args::Args;

const HELP: &str = "\
leos 0.1 - archive bbstage checkpoints from Lustre scratch to EOS and back

usage:
  leos archive <lustre-path> [--dest <eos-path>] [--dry-run]
  leos restore <eos-path>    [--dest <lustre-path>] [--dry-run]
  leos status  <lustre-path> [--dest <eos-path>]
  leos verify  <path>        [--dest <eos-path>] [--round-trip] [--remote]
  leos purge   <lustre-path> --yes
  leos janitor <lustre-root> [--older-than-hours N] [--dry-run]
  leos tui     <lustre-path> [--dest <eos-path>] [--refresh=SECS]

<path> may be a single checkpoint (a directory containing the completion
marker, default bbstage.final) or a tree to scan for them. --dest may also
come from $LEOS_DEST.

how it works
  archive  copies each complete checkpoint to <dest>/<unit>.incomplete/,
           verifies remote sizes against a per-file manifest (sizes +
           content adler32), seals it with an atomic metadata rename, and
           uploads the manifest as .archived LAST -- then mirrors it locally
           as the deletion license. Crash-safe and idempotent: reruns resume
           into the same .incomplete; janitor sweeps abandoned ones.
  restore  requires the remote .archived license, downloads into a temp
           tree, re-hashes everything against the manifest, and moves it
           into place atomically: a restored tree provably matches what was
           archived, byte for byte.
  verify   local unit: re-hash the tree against its license. remote unit:
           stat every manifest file through the store; --round-trip
           downloads, re-hashes, and deletes (full end-to-end check).
           --remote forces the store-side check for any path (testing).
  purge    deletes local trees that hold a license verifying against the
           tree AND whose license exists on the store. This is the ONLY
           supported way to reclaim scratch.
  tui      read-only live dashboard: WRITING/READY/ARCHIVING/SEALED per
           unit from the marker chain, live multi-node claims, seal
           ledger, token validity, scratch fill. q quit, r refresh,
           p pause. Mutates nothing.

multi-node archive (no MPI, no coordinator):
  run `leos archive --claim` on any number of data-mover nodes over the
  same <lustre-path>: each node claims whole units via O_CREAT|O_EXCL
  files in <path>/.leos/claims (atomic on the Lustre MDT), archives what
  it won, heartbeats while working. A crashed holder's claim is stolen
  after 120 s by any survivor; nodes join and leave freely (add --watch
  to keep draining). Sealing stays the unchanged marker-chain path.

transfer options (archive/restore):
  --streams=N   per-file xrootd/eos streams    (default 4 archive, 8 restore)
  --jobs=N      concurrent file transfers      (default 8)
  --backend=auto|eoscp|xrdcp|copy              (auto: root:// -> xrdcp,
                                                /eos/ -> eoscp, else local copy)
  --extra-args='--foo --bar'                   appended to copy commands
  --marker=NAME                                (default bbstage.final)
  --watch=SECS                                 archive: keep scanning
  --max-gib=N                                  archive: byte budget per pass
  --seal=auto|rename|marker                    sealing strategy (auto probes)
  --no-checksum                                sizes only (archive)
  --allow-unverified                           accept stores where stat fails

capability tokens (Helios EOS):
  --token=TOK   or $LEOS_TOKEN, or plain $EOSAUTHZ. The token is appended as
                ?authz= to copy URLs AND to xrdfs metadata paths (stat/ls/
                mkdir/mv/rm) — verified on Helios 2026-11-24. Note: `eos cp`
                cannot READ with a token (upstream XRootD bug: authz applies
                to destinations only), so restore and verify --round-trip
                must address units as
                root://<mgm-host>//eos/... (auto-selects the xrdcp backend).
                Tokens expire -- leos failures are resumable: re-issue the
                token, rerun; .incomplete trees are picked up or swept.

examples (GSI Helios):
  export EOS_MGM_URL=root://helios.hpc.gsi.de
  export EOSAUTHZ=$(eos token --path /eos/helios/Users/$USER/ \
      --permission rwx --tree --expires $(( $(date +%s) + 60*60*8 )))
  export LEOS_DEST=/eos/helios/Users/$USER/warpx          # writes: eos console
  leos archive /lustre/scratch/warpx/diags --streams 8
  leos status  /lustre/scratch/warpx/diags
  # reads go through xrdcp with ?authz= (eos cp cannot read with tokens):
  leos verify  root://helios.hpc.gsi.de//eos/helios/Users/$USER/warpx/openpmd_000050.bp5 --round-trip
  leos restore root://helios.hpc.gsi.de//eos/helios/Users/$USER/warpx/openpmd_000050.bp5 \
               --dest /lustre/restart
  leos purge   /lustre/scratch/warpx/diags --yes
";

fn main() {
    let a = Args::parse();
    let cmd = match a.sub.as_deref() {
        None | Some("--help") | Some("-h") => {
            eprint!("{HELP}");
            std::process::exit(if a.sub.is_some() { 0 } else { 2 });
        }
        Some(c) => c.to_string(),
    };
    if a.flag("help") {
        eprint!("{HELP}");
        return;
    }
    let r = match cmd.as_str() {
        "archive" => store::run_archive(&a),
        "restore" => store::run_restore(&a),
        "status" => store::run_status(&a),
        "verify" => store::run_verify(&a),
        "purge" => store::run_purge(&a),
        "janitor" => store::run_janitor(&a),
        "tui" => tui::run(&a),
        other => {
            eprintln!("leos: unknown command '{other}' (see --help)");
            std::process::exit(2);
        }
    };
    if let Err(e) = r {
        eprintln!("leos {cmd}: error: {e}");
        std::process::exit(1);
    }
}
