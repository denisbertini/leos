# Quickstart

## Get the binary

```bash
cargo build --release                                     # target/release/leos
# or a fully static build for cluster nodes without a toolchain:
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl  # runs anywhere
```

Single binary, no runtime dependencies. If `eos`/`xrdcp` work for you
interactively, `leos` inherits that world.

## Rehearse the whole contract — no EOS required

Any POSIX scratch tree with bbstage units (each unit a directory containing
`bbstage.final`; `--marker=NAME` changes the name):

```bash
cd /path/to/diags                       # parent of openpmd_*.bp5 units
leos archive . --dest /tmp/archive-tier --backend copy   # local dir = store
leos status .                           # licensed / UNLICENSED per unit
leos verify  .                          # re-hash trees against licenses
leos purge   . --yes                    # removes only licensed units
```

This exercises the identical code path — staging trees, size gates, seals,
licenses, the purge double-check — against a `copy` backend. Useful before
pointing anything at a real archive, and honest about the semantics.

## First run against a real store

Work one level **above** the units, never inside a unit directory, and never
write logs inside the tree (they would become archive payload):

```bash
cd /lustre/scratch/warpx/diags

# 1. auth gate: prove your credentials on the store before spending minutes
#    on hashing. leos also does a write-probe preflight, but start honest:
eos whoami

# 2. announce only: what would be archived, how big, how many files
leos archive . --dest $LEOS_DEST --dry-run

# 3. one unit, end to end
leos archive openpmd_000000.bp5 --dest $LEOS_DEST --streams 8 --jobs 8
leos verify root://<mgm>//eos/<instance>/<dest>/openpmd_000000.bp5 --round-trip
```

`--round-trip` is the strongest single check in the tool: it downloads every
byte back from the archive and re-hashes it against the manifest. Run it once
per site setup, ideally on the first unit; it is the read path plus the
content proof in one command.

### Reading the log

```
[1794905387] preflight: authenticated write probe at destination...
[1794905388] planning openpmd_000000.bp5: hashing content (one pass over the bytes)...
[1794905494] archiving openpmd_000000.bp5: 68 files, 146.51 GiB -> .../openpmd_000000.bp5
[1794905559] sealed openpmd_000000.bp5 (146.51 GiB in 171s = 877.33 MiB/s)
```

Timestamps are epoch seconds: the `planning → archiving` delta is the hash
pass (pure scratch read), `archiving → sealed` is the real transfer. Judge
your store by the phase deltas, not by the average on the sealed line.

## Everything else

Licensed units are skipped on every later run, so the whole tree is one
unattended command — safe to start, safe to kill, safe to rerun:

```bash
nohup leos archive . --dest $LEOS_DEST --streams 8 --jobs 8 > ~/archive.log 2>&1 &
tail -f ~/archive.log
```

With a capability token (Helios), keep the launch **expiry-safe**: a run that
outlives its token fails the preflight of the next unit and stops; mint a
fresh token, rerun the identical command, everything licensed is skipped.
Details in the [site guide](gsi-helios.md).

## Reclaiming scratch

`purge` deletes a unit only when its local license re-hashes clean against
the tree **and** the same license exists on the store — two independent
checks, no override flag. Before the first purge on a new campaign tree,
prove the full circle once:

```bash
leos restore root://<mgm>//eos/<instance>/<dest>/openpmd_000000.bp5 --dest /tmp/restart-test
diff -r /tmp/restart-test/openpmd_000000.bp5 <scratch>/openpmd_000000.bp5   # or a bpls pass
```

For bbstage output specifically, also let the reader speak (`bpls` on a
restored unit) before trusting `purge` in anger — byte identity is proven by
the tool, semantic readability is one command away, and the purge is the
irreversible step.
