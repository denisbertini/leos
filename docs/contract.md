# Contract semantics

leos is small; the guarantees are what you are buying. This page states each
one precisely, including where it stops.

## Checkpoint units and states

A **unit** is a directory containing the completion marker — `bbstage.final`
by default (`--marker=NAME`). The marker is written by the engine when the
checkpoint is fully on disk; leos never reads a unit without it, so a
still-draining checkpoint is invisible to the tool by design.

| State | Marker | Local license | Meaning |
| :--- | :---: | :---: | :--- |
| incomplete / draining | ✗ | – | leos ignores it |
| complete, unarchived | ✓ | ✗ | archivable; purge refuses it |
| **licensed** | ✓ | ✓ | archive verified; purge eligible |

The **license** is the manifest file `.archived`, mirrored from the store
into the unit at seal time. Its presence is the entire licensing story: no
daemon, no database, `ls -a` tells you the state of any tree.

## archive

Per unit, in this order — every step idempotent:

1. **Plan.** Walk the tree (skipping `.leos/`), record every file's size and
   an adler-32 over its content. One read pass over the source.
2. **Stage.** Copy all files into `<dest>/<unit>.incomplete/` with `--jobs`
   files concurrent, each over `--streams` sockets; after each file, stat the
   remote object and gate on the recorded size. Mismatch → the unit fails, no
   license, period.
3. **Seal.** `--seal rename` (where supported): atomic directory rename
   `.incomplete → final`. `--seal marker`: no rename — the store rejected
   directory renames under tokens at some sites (Helios does), so the license
   upload itself is the commit record. `auto` (default) probes by creating
   **and renaming** a probe directory, so the announced mode is the mode the
   seal can actually perform.
4. **License.** Upload the manifest as `<dest>/<unit>/.archived` — strictly
   last, after every gate passed. Then mirror it into the local unit.

**Resume semantics:** rerunning the identical command skips licensed units
instantly and continues into the same `.incomplete` tree (existing remote
files whose size matches the manifest are not re-transferred). This makes
token expiry, Ctrl-C, and node eviction non-events.

**Abandonment:** if a run never resumes, `janitor` sweeps `.incomplete` trees
older than `--older-than-hours` (default 24) using the local record log at
`<tree>/.leos/records.log`, and never touches sealed units.

## restore

The remote license drives everything: fetch `.archived` first (no license →
nothing happens), download the payload into `<name>.incomplete/`, re-hash
every file against the manifest, and only then move the tree atomically into
place. A consumer can never observe a partial restore; corrupted archive
content is refused, not half-restored. The local license mirror is written so
the restored tree is itself licensed (it came from the store's own manifest).

## verify

| Invocation | Check |
| :--- | :--- |
| `leos verify <scratch-path>` | local: re-hash every tree against its license |
| `leos verify <eos-path>` | remote: stat every manifest entry, sizes |
| `--round-trip` | remote: download everything, re-hash all bytes against the manifest — the full content proof of the archive tier |
| `--remote` | force the store-side check on any path (testing) |

`--allow-unverified` downgrades size-stat failures to warnings for stores
where metadata stat is unavailable; the content checks never weaken.

## purge

A unit is deleted only if **both** hold:

1. the local `.archived` parses and its hashes match the tree on disk;
2. the same license exists at `<dest>/<unit>/.archived` on the store.

There is deliberately no `--force`. If you cannot purge, the tool is telling
you something true about that unit.

## Concurrency rules

* **Different paths**: any number of leos runs (archive, verify, restore) in
  parallel — EOS-level read/write concurrency is per file, and leos keeps
  every operation inside one unit's tree or its `.incomplete`.
* **One archiver per scratch tree**: enforced by a pid lock in
  `<tree>/.leos/lock`.
* **Do not re-archive a licensed unit whose content changed.** Rename mode
  is safe (atomic swap); marker mode uploads straight into the unit
  directory and could mix file generations. Archive-once is the intended
  lifecycle — checkpoints are immutable. Re-certification of an unchanged,
  already-licensed unit is fine (gates catch any discrepancy).

## Checksum design

The manifest stores **adler-32 over file content** — leos's own integrity
record, deliberately *not* XRootD's native adler32 (whose definition includes
the file name), so it survives backend changes and is re-checkable with any
zlib:

```bash
python3 -c "import zlib,sys; print(hex(zlib.adler32(open(sys.argv[1],'rb').read())))" file
```

`--no-checksum` records sizes only — faster first pass; restore/verify then
degrade to sizes-only. Acceptable for rehearsal, not recommended for
long-lived archives.

## Honest limits

* **Store-side checksums are not assumed.** Remote verification is
  size-based; content verification happens wherever leos can read the bytes
  (restore, `--round-trip`). A store serving native checksums via metadata
  would be a cheaper extension, not a current claim.
* **Empty directories are not archived** (manifest is a file list; bbstage
  output never has them).
* **Metadata stat behavior varies by EOS build** (see the
  [site guide](gsi-helios.md) for the ≥ 5.5 interface changes leos already
  handles).
