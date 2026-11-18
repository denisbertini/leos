# leos

Archive and restore of [bbstage](https://github.com/denisbertini/ADIOS2)
checkpoints between a POSIX scratch tier (Lustre) and an
[EOS](https://eosweb.web.cern.ch/) object archive, with the completeness
contract plain copy tools cannot offer. One static binary; shells out to your
site's own tools (`eos`, `xrdcp`) instead of reinventing protocols.

`eos cp` / `xrdcp` move bytes. They cannot tell you whether a checkpoint was
complete, whether the archive copy matches scratch byte for byte, or whether
a directory is safe to delete when the Lustre quota bites. `leos` adds
exactly those three answers.

```
leos archive <scratch-path>  [--dest <eos-path>] [--dry-run] [--claim]
leos restore <eos-path>      [--dest <scratch-path>]
leos status  <path>
leos verify  <path>          [--round-trip]
leos purge   <scratch-path>  --yes
leos janitor <scratch-root>
leos tui     <scratch-path>  [--refresh=SECS]
```

## Fleet mode and the dashboard

`--claim` turns any number of data-mover nodes into one drain without
MPI and without a coordinator: every node races `O_CREAT|O_EXCL` claim
files in `<path>/.leos/claims` (atomic on the Lustre MDT), archives the
units it wins, and heartbeats the claim while working. A crashed or
evicted holder's claim goes stale after 120 s and is stolen by any
survivor; nodes join and leave freely (`--claim --watch 60`). Sealing
stays the identical marker chain per unit — coordination never touches
the integrity contract.

`leos tui` is the read-only window onto all of it: WRITING/READY/
ARCHIVING/SEALED per unit from the marker chain alone, the live claim
list as the multi-node worker panel (`owner:beat` per unit), the seal
ledger from `.leos/records.log`, token validity decoded from the Helios
JWT, and scratch fill from `statvfs`. It mutates nothing — archive,
verify and purge deliberately stay CLI, because those have gates.

## The contract, compactly

| Command | What it guarantees |
|---|---|
| **archive** | Only sealed units (containing `bbstage.final`) are read. Payload lands in `<dest>/<unit>.incomplete/` with every file size-gated against a per-file hash manifest, is sealed (atomic rename; where the store forbids rename, the license itself is the seal), and the license `.archived` is uploaded **last**. A license cannot exist before the bytes behind it verified. |
| **restore** | License-driven: download to a temp tree, re-hash everything against the manifest, atomic move into place. A half-restored tree is never observable. |
| **verify** | Local: re-hash the tree against its license. Remote: stat every manifest file through the store; `--round-trip` downloads and re-hashes every byte on the archive side. |
| **purge** | Deletes a scratch unit only if its license verifies against its tree **and** the same license exists on the store. The only supported reclaim path; no `--force` exists. |
| **janitor** | Sweeps abandoned `.incomplete` trees (>24 h, tracked in `.leos/records.log`), never touches sealed units. |

Every step is resumable: rerun the identical command. Token expiry, Ctrl-C,
node eviction — all recoverable, because nothing is licensed until it fully
verified.

## Measured, not extrapolated

GSI Orion → Helios (EOS 5.5.5), one archiver on a cluster node, one 146.51 GiB /
68-file bbstage unit:

| Phase | Rate | Settings |
|---|---|---|
| Hash (scratch read) | 1.38 GiB/s | `--jobs 8` |
| Copy (scratch → EOS) | 2.25 GiB/s | `--streams 8` |
| Whole unit sealed | 171 s | `--seal marker` |
| Read-back, all bytes re-hashed | `OK (remote verified, round-trip)` | |

## Quickstart

```bash
# rehearsal, no EOS needed — a local directory plays the archive tier:
leos archive ./diags --dest /tmp/archive-tier --backend copy
leos verify  ./diags && leos status ./diags

# real store: auth gate, dry-run, one unit, round-trip proof, then the sweep
eos whoami
leos archive ./diags --dest $LEOS_DEST --dry-run
leos archive ./diags/openpmd_000000.bp5 --dest $LEOS_DEST --streams 8 --jobs 8
leos verify  root://<mgm>//eos/.../openpmd_000000.bp5 --round-trip
```

Site authentication (Helios capability tokens, EOS ≥ 5.5 quirks) is covered
in [docs/gsi-helios.md](docs/gsi-helios.md).

## Build and test

```bash
cargo build --release                                      # ~740 KB, zero deps
cargo build --release --target x86_64-unknown-linux-musl   # fully static
cargo test && ./test/e2e.sh
```

`test/e2e.sh` exercises the entire contract against a local directory
standing in for the store: dry-run, archive, idempotent rerun, crash-residue
janitor, local/remote/round-trip verify, corruption rejection in both
directions, byte-identical restore, license-gated purge.

## Documentation

Full contract semantics, the CLI reference, the GSI/Helios site guide, and a
troubleshooting table live in [`docs/`](docs/) (MkDocs-Material; published by
`.github/workflows/docs.yml`).

## License

GPL-3.0-or-later. See `LICENSE`.
