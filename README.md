# leos

Archive and restore of [bbstage](https://github.com/denisbertini/ADIOS2)
checkpoints between a POSIX scratch tier (Lustre) and an
[EOS](https://eosweb.web.cern.ch/) object archive — with a completeness
contract that plain copy tools cannot provide.

`eos cp` / `xrdcp` move bytes. They cannot tell you whether a checkpoint
finished, whether the archive copy matches what was on scratch, or whether a
directory is safe to delete when the Lustre quota bites. `leos` adds exactly
that contract, in one static binary with zero runtime dependencies.

```
leos archive <lustre-path> [--dest <eos-path>] [--dry-run]
leos restore <eos-path>    [--dest <lustre-path>] [--dry-run]
leos status  <path>
leos verify  <path>        [--round-trip]
leos purge   <lustre-path> --yes
leos janitor <lustre-root>
```

## The contract

A *checkpoint unit* is a directory holding the bbstage completion marker
(`bbstage.final` by default; `--marker` to change). Nothing else counts as a
checkpoint, and leos never reads a unit the engine has not sealed.

**archive** — per unit, in this order:

1. Walk the tree; record every file's size and an adler-32 checksum of its
   content.
2. Copy to `<dest>/<unit>.incomplete/` (parallel files, per-file streams),
   stat-gating every remote file against the recorded size.
3. Seal: an atomic metadata rename `<unit>.incomplete` → `<unit>`
   (`--seal rename`). On stores without rename support, the manifest itself
   is the seal (`--seal marker`; `auto` probes).
4. Upload the manifest as `<dest>/<unit>/.archived` **last**. A license can
   never exist before the data behind it verified.
5. Mirror the license locally into the unit. Its presence makes the unit
   *licensed* — the only state from which scratch space may be reclaimed.

Every step is idempotent and crash-safe: an interrupted run resumes into the
same `.incomplete`; a `janitor` sweeps abandoned `.incomplete` trees older
than a threshold, using the local record log, and never touches sealed units.

**restore** — the license drives everything. The `.archived` manifest is
fetched first; the payload downloads into `<name>.incomplete/`; every file is
re-hashed against the manifest *before* an atomic move into place. A restored
tree provably matches the tree that was archived, byte for byte. Corrupted
archive content is refused, not half-restored.

**verify** — locally: re-hash the tree against its license. Remotely: stat
every manifest entry through the store (sizes), or `--round-trip` to download
and re-hash everything (full end-to-end check of the archive tier).

**purge** — deletes a local unit only if its license verifies against its own
tree *and* the same license exists on the store. This is the supported way to
reclaim scratch; there is deliberately no `--force`.

## Checksums

The manifest stores adler-32 over file *content*. This is leos's own
integrity record, deliberately independent of XRootD's native adler32
(whose definition includes the file name), so it survives backend changes and
can be re-checked with any zlib implementation:

```
python3 -c "import zlib,sys; print(hex(zlib.adler32(open(sys.argv[1],'rb').read())))" file
```

`--no-checksum` archives with sizes only (faster first pass; restore then
verifies sizes only — not recommended for long-lived archives).

## GSI example

```bash
export LEOS_DEST=root://eos.gsi.de//eos/<project>/<group>/warpx
# watch a scratch tree, archiving checkpoints as they complete:
leos archive /lustre/scratch/<user>/warpx/diags --streams 8 --watch 60
leos status  /lustre/scratch/<user>/warpx/diags
leos verify  root://eos.gsi.de//eos/<project>/<group>/warpx/openpmd_000050.bp5 --round-trip
leos restore root://eos.gsi.de//eos/<project>/<group>/warpx/openpmd_000050.bp5 \
             --dest /lustre/scratch/<user>/restart
leos purge   /lustre/scratch/<user>/warpx/diags --yes   # licensed units only
```

`bbstage` with `Aggregate=on` shapes the Lustre output into few, large files;
that object count carries over to EOS and is what makes the archive
comfortable for the object store.

## Authentication

leos never speaks a protocol; it shells out to the site tools that already
work — `eos cp/mv/mkdir/rm/file stat` for `/eos/...` paths, `xrdcp`/`xrdfs`
for `root://host//path` URLs. Whatever you have authenticated with (kinit,
XrdSciTokens, the EOS console) keeps working; leos inherits it. `--extra-args`
passes site flags (e.g. auth files) through to every copy command.

Backend selection: `--backend auto` (default) picks `xrdcp` for `root://`,
`eoscp` for `/eos/`, and a local-filesystem `copy` backend otherwise (used by
the test suite, useful for tier-to-tier rehearsals).

## Honest limits

- **xrdfs `mv` is version-dependent.** `--seal auto` probes for directory
  rename; where it is unsupported, sealing falls back to `marker` mode (the
  uploaded license *is* the commit record — still safe, one metadata
  operation less atomic). With `eoscp`, `eos mv` on directories is reliable.
- **Remote checksums are not assumed.** Verification is size-based remotely
  plus content adler-32 wherever leos can read the bytes (restore,
  `--round-trip`). If your store can serve native checksums via
  `xrdfs stat`/`eos file stat`, that is a future extension, not a claim today.
- **`size=` parsing of stat output** is tolerant (`size=N`, `size: N`,
  `size N`) but written against documented formats; the first run on a real
  EOS should be a `--dry-run` followed by one small unit and a
  `verify --round-trip`.
- Empty directories are not part of the manifest (bbstage output never has
  any; a checkpoint that needs them is not what this tool targets).
- One archiver per scratch tree (pid lock in `<tree>/.leos/`); run either one
  `--watch` daemon or serial one-shot passes.

## Build and test

```bash
cargo build --release            # single ~600 KB binary, no dependencies
# fully static for cluster compute nodes:
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

`test/e2e.sh` runs the whole contract — dry-run, archive, idempotent rerun,
crash-residue janitor, local/remote/round-trip verify, corruption rejection in
both directions, byte-identical restore, license-gated purge — against a
local directory playing the archive tier. `cargo test` covers the manifest,
checksum, parser and pool internals.

## Status

Internals unit-tested; the full lifecycle passes end-to-end against the
`copy` backend. Behaviour against a real EOS instance (xrdfs stat formats,
`eos mv` directory semantics, authentication edge cases) is written to the
documented interfaces and needs one supervised first run on the target
instance — start with `--dry-run` and `leos status`.

## License

GPL-3.0-or-later. See `LICENSE`.
