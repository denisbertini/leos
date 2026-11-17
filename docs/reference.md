# CLI reference

```
leos archive <path>  [--dest <target>] [transfer options] [--dry-run]
leos restore <target> [--dest <scratch-path>] [transfer options] [--dry-run]
leos status  <path>  [--dest <target>]
leos verify  <path>  [--dest <target>] [--remote] [--round-trip]
leos purge   <path>  --yes
leos janitor <path>  [--older-than-hours N] [--dry-run]
```

`<path>` is either a single unit (a directory containing the marker) or a
tree whose direct children are scanned for units. `--dest` may come from
`$LEOS_DEST`. Options are `--key=value` or `--key value`; a valued option
that receives another flag as its value (the classic unset-shell-variable
failure) aborts immediately instead of silently consuming it.

## Commands

| Command | Purpose |
| :--- | :--- |
| `archive` | Plan (hash) → staged copy with size gates → seal → license last. Skips licensed units; resumes into `.incomplete`. |
| `restore` | Fetch remote license, download to temp tree, re-hash all, atomic move into place. |
| `status` | Per-unit state: licensed / UNLICENSED / pending, sizes, dest. |
| `verify` | Local re-hash vs license; remote size gate vs manifest; `--round-trip` = full content proof. |
| `purge` | Delete licensed scratch units (local license verifies against tree **and** exists on store). Requires `--yes`. |
| `janitor` | Sweep abandoned `.incomplete` archives using `<tree>/.leos/records.log`. |

## Options

<div class="opt-names" markdown="1">

| Option | Default | Applies to | Meaning |
| :--- | :--- | :--- | :--- |
| `--dest=TARGET` | `$LEOS_DEST` | archive, restore, status, verify | Archive-side location: `/eos/...` path or `root://host//eos/...` URL |
| `--streams=N` | 4 archive / 8 restore | archive, restore | Per-file xrootd/eos transfer streams |
| `--jobs=N` | 8 | archive, restore | Concurrent file transfers (also the hashing pool) |
| `--backend=auto\|eoscp\|xrdcp\|copy` | `auto` | all store ops | `auto`: `root://` → xrdcp, `/eos/` → eoscp, else local copy |
| `--marker=NAME` | `bbstage.final` | all scratch ops | Completion-marker filename |
| `--seal=auto\|rename\|marker` | `auto` | archive | Seal strategy; `auto` probes mkdir **and rename** |
| `--token=TOK` | `$LEOS_TOKEN`, then `$EOSAUTHZ` | store ops | Capability token (Helios) |
| `--extra-args='...'` | — | archive, restore | Appended verbatim to every copy command |
| `--watch=SECS` | off | archive | Keep re-scanning the tree |
| `--max-gib=N` | none | archive | Byte budget per pass |
| `--older-than-hours=N` | 24 | janitor | Age threshold for `.incomplete` sweeps |
| `--dry-run` | off | archive, restore, janitor | Announce work, change nothing |
| `--no-checksum` | off | archive | Record sizes only (see tradeoff in [Contract](contract.md)) |
| `--round-trip` | off | verify | Download + re-hash everything from the store |
| `--remote` | off | verify | Force the store-side check for any path |
| `--allow-unverified` | off | archive, restore | Tolerate stores whose metadata stat fails |
| `--stop-on-error` | off | archive | Abort the pass at the first failed unit (default: continue, report count) |
| `--yes` | — | purge | Mandatory confirmation; there is no `--force` |

</div>

## Environment

| Variable | Effect |
| :--- | :--- |
| `LEOS_DEST` | Default `--dest` |
| `LEOS_TOKEN` | Capability token; wins over `EOSAUTHZ` (token precedence: `--token` > `LEOS_TOKEN` > `EOSAUTHZ`) |
| `EOSAUTHZ` | The native EOS authz variable — inherited by every `eos`/`xrdcp` child; leos additionally appends `?authz=` to copy URLs (see the [site guide](gsi-helios.md)) |
| `EOS_MGM_URL` | Passed through untouched; needed by `eos` console commands |

## Output

Progress lines are `[<epoch-seconds>] <message>` on stderr; final per-unit
verdicts on stdout: `OK <unit>` / `UNLICENSED <unit>` / `FAILED <unit>:
<reason>`. The `sealed` line carries payload, wall time, and average rate —
use the phase timestamps for the real transfer rate
([Quickstart](quickstart.md#reading-the-log)). Exit status is non-zero if any
unit failed.

## On-disk state

=== "On the scratch tree"

    ```
    <tree>/.leos/lock          pid lock, one archiver per tree
    <tree>/.leos/records.log   ts|unit|target lines for janitor & audit
    <unit>/.archived           local license mirror
    ```

=== "On the store"

    ```
    <dest>/<unit>.incomplete/  staging tree (payload, no license)
    <dest>/<unit>/.archived    license — present ⟺ sealed & verified
    <dest>/.leos-writeprobe    ephemeral preflight probe
    ```

## Manifest format

One header line, then one tab-separated line per file
(`relpath<TAB>size<TAB>adler32-hex8`):

```
# leos manifest version=1 unit=openpmd_000000.bp5 sealed=1794905559 tool="leos 0.1.0" files=68 bytes=157316845977
bbstage.final	6	00000001
data.0	2474760192	3f9a1c22
...
```

Paths are relative, traversal (`..`) is refused at parse time, and each
checksum is verifiable with any zlib implementation
([Contract](contract.md#checksum-design)).
