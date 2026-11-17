# Troubleshooting

Symptom → cause → fix, ordered by what actually happens in the field. Most
entries come from real campaigns, error text included.

## Auth and access

| Symptom | Cause | Fix |
| :--- | :--- | :--- |
| `eos whoami` refuses, or leos preflight fails within seconds | No/expired token, or wrong scope | Re-mint (`eos token --path ... --permission rwx --tree`); check `echo ${#EOSAUTHZ}` — a truncated paste is the most silent failure |
| `eos cp` upload error shows your **local source path with `&authz=...` appended** | EOS ≥ 5.5 console bug: the `$EOSAUTHZ` env token corrupts local source paths | leos already strips the env token on uploads and carries it as `?authz=` on the destination. Seen with raw `eos cp`? `env -u EOSAUTHZ eos cp src 'root://...dst?authz=TOK'` |
| Reads fail: `3010 [ERROR] Ops ... public access level restriction` with a valid token | Old Enterprise-Linux `xrdcp` on that node ignores `?authz=` | Use a node with the current EOS stack; on GSI Orion verified on `ccdev0002` |
| `xrdfs stat` answers EPERM/3010 under a token on Helios | Helios does not authenticate `xrdfs` with tokens | Not a data problem. Read verification goes through `xrdcp` bytes (`verify --round-trip`, restore) — those work; residual metadata-stat gaps: `--allow-unverified` |
| `eos file stat <path>` prints the `file` usage, exit 22 | The subcommand was removed in EOS ≥ 5.5.5 | `eos file info <path> --size`; leos uses this since `4e507a4` |

## Archive and seal

| Symptom | Cause | Fix |
| :--- | :--- | :--- |
| `FAILED <unit>: eos mv: ... Operation not permitted (errc=1)` on the seal | Helios denies directory rename to capability tokens (both `eos mv` and `eos file rename`) | Use `--seal marker` (or let `--seal auto` probe it out — it performs a real mkdir→rename→rm test). Same safety: the license uploaded last *is* the seal |
| `--dest needs a value, got flag '--dry-run'` | Unset/empty shell variable ate the next token (e.g. `--dest $LEOS_DEST` with `$LEOS_DEST` empty) | Check your exports; leos aborts on purpose rather than archiving to a nonsense destination |
| Unit you archived shows `UNLICENSED` in `status` | The run failed before the license upload (that is the gate working) | Rerun the identical archive command — it resumes into the same `.incomplete` |
| `sealed ... [license refreshed]` on what should be a first archive | A `.archived` license already existed at that destination (earlier successful run, or leftover tree). The tag means exactly this: the store held a license and it was replaced | If unexpected: `eos ls <dest>/<unit>/` and `leos verify` before trusting anything; a stale destination tree is the classic cause |
| `FAILED <unit>: <dest>/<unit> exists without a license -- refusing` | Unlicensed foreign tree or abandoned staging remnant sits at the seal target (rename mode) | Remove/inspect the foreign tree, or rerun with the seal mode the store supports |
| Round-trip verify reports a checksum **mismatch** | Real corruption or mixed-generation content — the worst-case finding, and the reason the check exists | **Do not purge.** Keep the scratch copy, re-run verify to rule out transient reads, then re-archive over the unit (sizes/gates re-verify everything) and round-trip again |
| `purge` refuses a unit that looks licensed | Either the tree changed after licensing (local hashes no longer match — something wrote into a licensed unit!) or the remote license is gone | `eos ls <dest>/<unit>/` to check the remote license; `leos verify <unit>` locally to see which files drifted. There is no override, and this refusal is the last line of defense |
| Janitor removed a staging tree you still wanted | It was `.incomplete`, unlicensed, older than `--older-than-hours` (default 24) | Rerun archive; it restages from scratch. Janitor never touches sealed units |

## Behavior that is correct but might surprise you

* **A `sealed ... MiB/s` average lower than your network.** The number spans
  the hash pass *and* the transfer. Phase rates live in the log timestamps.
* **Nothing happens on rerun.** Every unit is licensed; leos skips them in
  milliseconds. That is the resume design, not a hang.
* **Files you added to a licensed unit are not in the archive.** The license
  records the tree at archive time; `verify` against the license will now
  *fail* for that unit — correctly. Licensed trees are immutable by
  convention: add nothing to them after sealing (see
  [Contract](contract.md#concurrency-rules)).
* **`eos ls -ld a b` reports "no such file"** — two paths become one
  argument; call it once per path.

## First-call-for-help checklist

```bash
echo ${#EOSAUTHZ}                    # token present, plausible length?
eos whoami                           # store accepts you at all?
leos status .                        # per-unit state on the scratch side
leos archive <one unit> --dest $LEOS_DEST --dry-run   # what would happen?
```

If all four look right but a real run fails, the failure line names the
stage (`preflight` / `planning` / `archiving` / `sealed`) — that stage
attribution is usually enough to find your case above.
