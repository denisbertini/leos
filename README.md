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

## Authentication

leos never speaks a protocol; it shells out to the site tools that already
work — `eos cp/mv/mkdir/rm/file info` for `/eos/...` paths, `xrdcp`/`xrdfs`
for `root://host//path` URLs. Whatever you have authenticated with (kinit,
capability tokens) keeps working; leos inherits it. For Helios-style
capability tokens leos passes `$EOSAUTHZ` to every child and appends
`?authz=` to copy URLs. `--extra-args` passes site flags through to every
copy command.

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
- **Token support follows the site-demonstrated flows, not the theoretical
  ones.** With a token: eos-console *writes and metadata* (`cp` to, `mkdir`,
  `mv`, `rm`, `whoami` via `$EOSAUTHZ`) and `xrdcp` copies with `?authz=` on
  either end are the operations Helios's own documentation demonstrates;
  leos therefore routes archive through `/eos/...` paths (console backend)
  and reads through `root://` URLs (xrdcp backend). Whether `xrdfs stat`
  honors `$EOSAUTHZ` is XRootD-version-dependent — if remote `status
  --remote` complains about permissions while archive works fine, that is
  this gap, not a data problem.
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
`copy` backend. Behaviour against a real EOS instance is written to the
documented (and, for Helios, admin-demonstrated) interfaces but still needs
one supervised first run. The checklist:

1. `kinit` → mint token → `EOSAUTHZ=$TOKEN eos whoami` shows your tree.
2. Write probe: `leos archive <one small unit> --dest /eos/... --dry-run`,
   then without `--dry-run`; confirm `eos file stat` reports expected sizes.
3. `leos verify root://<mgm>//eos/.../<unit> --round-trip` — this is the
   read path and the strongest single check of the whole chain.
4. `leos restore ... --dest /tmp` and compare against scratch with `diff -r`.
5. Only then point `purge` at a scratch tree.

## License

GPL-3.0-or-later. See `LICENSE`.
