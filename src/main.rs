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
//!
//! Zero dependencies by design. The EOS wire is subprocesses (`eos cp` /
//! `xrdcp` / `xrdfs`), so site authentication stays with the tools that
//! already work. Integrity = a manifest of per-file sizes and content
//! adler32, uploaded as `.archived` LAST: the license, and the deletion
//! license. Build: `cargo build --release`, or static with the musl target.

mod adler32;
mod args;
mod backend;
mod core;
mod store;

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
  --token=TOK   or $LEOS_TOKEN, or plain $EOSAUTHZ. The token is passed to
                eos/xrdcp children as $EOSAUTHZ and appended as ?authz= on
                copy URLs. Note: `eos cp` cannot READ with a token (upstream
                XRootD bug: authz applies to destinations only), so restore
                and verify --round-trip must address units as
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
