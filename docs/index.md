# leos

**leos** moves [bbstage](https://github.com/denisbertini/ADIOS2) checkpoints
between Lustre scratch and an [EOS](https://eosweb.web.cern.ch/) object
archive, and back — with a completeness contract that decides, with proof,
the three questions copy tools leave to luck: *was the checkpoint complete,
does the archive match byte for byte, is this directory safe to delete?*

`eos cp` and `xrdcp` move bytes well; leos does not reinvent them. It shells
out to your site's own `eos`/`xrdcp` binaries and wraps them in a protocol:
hash manifest → staged upload with per-file size gates → seal → license
uploaded last. The license (`.archived`) is the only thing that ever marks a
unit reclaimable, and it can physically not exist before every byte behind it
verified.

!!! tip "When do you need leos?"

    * **Lustre quota bites** and you need to reclaim diagnostics you *know*
      are safe — not directories you hope are safe.
    * **Campaign data must survive** the scratch purge with a recorded,
      re-checkable proof that the archive equals what was on scratch.
    * **Restarts read from the archive tier** and must never observe a
      half-downloaded checkpoint.

## Why plain copies fall short

| Question | `eos cp` / `xrdcp` | leos |
| :--- | :--- | :--- |
| Was the source a *complete* checkpoint? | not expressible | only units carrying the `bbstage.final` marker are ever read |
| Did every byte arrive intact? | transport-level only | per-file size gate on upload; content adler-32 in a manifest; full re-hash on restore and `verify --round-trip` |
| Is this scratch tree safe to delete? | your memory | `purge` refuses anything whose local license doesn't verify against its tree *and* match the store |
| What happens on a crash at 03:17? | partial tree, no record | `.incomplete` staging tree, resume on rerun, janitor sweeps abandonment |

## Measured performance

GSI Orion → Helios (EOS 5.5.5), one archiver on a cluster node, one
146.51 GiB / 68-file bbstage unit:

| Phase | Rate | Settings |
| :--- | :--- | :--- |
| Hash (scratch read) | **1.38 GiB/s** | `--jobs 8` |
| Copy (scratch → EOS) | **2.25 GiB/s** | `--streams 8` |
| Whole unit, sealed | **171 s** | `--seal marker` |
| Read-back, all bytes re-hashed | `OK (remote verified, round-trip)` | |

!!! note "Read the phases, not the average"

    The `sealed ... = 877 MiB/s` summary divides total bytes by the whole
    two-phase wall time. The transfer itself ran at 2.25 GiB/s (≈8 TiB/hour)
    for that unit; the hash pass over Lustre was the slower phase. See
    [Quickstart](quickstart.md) for how to read the log timestamps.

## Design in three sentences

leos speaks no protocol — every byte moves through the site binaries that
already work under whatever authentication you have (kinit, capability
tokens). The archive is a *protocol*, not a transport: nothing about its
safety depends on store features beyond sequential writes, so it works
identically on a cooperative EOS, a hostile one, or a local directory used
for rehearsal. Everything is resumable by construction: licenses appear only
after full verification, so "rerun the same command" is the complete error
recovery story.

* [Quickstart](quickstart.md) — rehearsal in one minute, first real archive.
* [Contract semantics](contract.md) — what each command guarantees.
* [CLI reference](reference.md) — every flag, the manifest format.
* [GSI Helios site guide](gsi-helios.md) — tokens, EOS ≥ 5.5 quirks.
* [Troubleshooting](troubleshooting.md) — error text → cause → fix.
