# GSI Helios site guide

Everything on this page was verified against GSI Orion + Helios in
production, the hard way. Helios is EOS 5.5.5 with capability-token access;
several console behaviors differ from older EOS and from the documented
ideal. `leos` already implements the workarounds — this page explains what
it is doing and what to check when auth misbehaves.

## Topology

| Piece | Where | Notes |
| :--- | :--- | :--- |
| Scratch | `/lustre/rz/<user>/...` on Orion login/compute nodes | Lustre client |
| Archive | Helios EOS, `root://helios.hpc.gsi.de` | `export EOS_MGM_URL=root://helios.hpc.gsi.de` |
| Token minting | a Kerberized host with the EOS console (e.g. `lxbk0960.gsi.de`) | **no Lustre mount** — mint there, work on a cluster node |
| EOS client on cluster | rolling 5.5.5 install — not every node has `eos` yet | verified working: `ccdev0002` |

An admin must create your top directory (`/eos/helios/Users/<user>/`) once
before you can mint tokens for it.

## Authentication: three flows, three token forms

Helios accepts the capability token in **three different ways depending on
the operation** — the single most common source of confusion:

| Operation | Token form | Why |
| :--- | :--- | :--- |
| Console metadata — `whoami`, `mkdir`, `mv`, `rm`, `file info` | `$EOSAUTHZ` environment | native console behavior |
| Console upload — `eos cp <local> <eos-path>` | `?authz=` on the **destination** only | EOS ≥ 5.5 console *corrupts the local source path* by appending `&authz=` to it when the env token is set — leos strips the env token for uploads and carries it on the destination URL |
| Reads — `xrdcp` | `?authz=` on the **source** URL, `root://` form only | `eos cp` cannot read with a token (upstream XRootD limitation: authz applies to destinations only); a bare `/eos/...` path handed to `xrdcp` is a *local* path |

leos implements exactly this mapping: archive/metadata flow through the
`eoscp` console backend, restore and `verify --round-trip` address units as
`root://helios.hpc.gsi.de//eos/...` so `xrdcp` carries the token on reads.
Token precedence: `--token` > `$LEOS_TOKEN` > `$EOSAUTHZ`.

New tokens from EOS 5.5.5 are prefixed `zteos64:`; older 5.4.7 minted a raw
serialized base64 form. Both are opaque to leos — pass them through whole.

## Working session pattern

```bash
# 1. on the Kerberized mint host:
kinit <user>
export EOS_MGM_URL=root://helios.hpc.gsi.de
eos token --path /eos/helios/Users/<user>/ --permission rwx --tree \
          --expires $(( $(date +%s) + 60*60*8 ))
# copy the printed token

# 2. on the cluster node (fresh shells lose exports — re-export every time):
export EOS_MGM_URL=root://helios.hpc.gsi.de
export EOSAUTHZ='zteos64:...'
export LEOS_DEST=/eos/helios/Users/<user>/warpx/diags
echo token_len=${#EOSAUTHZ}        # sanity: paste survived?
eos whoami                          # auth gate: expect your uid, not a refusal

# 3. archive from one level ABOVE the units, log OUTSIDE the tree:
cd /lustre/.../diags
nohup ~/bin/leos archive . --dest $LEOS_DEST --streams 8 --jobs 8 \
      --seal marker > ~/archive.log 2>&1 &
tail -f ~/archive.log
```

!!! warning "Two operational habits"

    * Never start an archiver from *inside* a unit directory, and never let
      a log file land inside the tree — it becomes archive payload.
    * Tokens expire (~8 h). A run that outlives its token fails the next
      unit's preflight within ~1 s (this is deliberate: leos probes before
      spending minutes hashing). Mint a fresh token, rerun the identical
      command; licensed units are skipped.

## Helios-specific behaviors leos handles (and you should know)

* **Directory rename is denied to tokens** (`eos mv` and `eos file rename`
  both answer `EPERM`). Rename-sealing is impossible, so sealing uses
  **marker mode**: the license uploaded last *is* the commit record — same
  safety, one less metadata operation. `--seal auto` probes an actual
  mkdir→rename→rm cycle and announces
  `seal mode: marker-only (rename unsupported)`; passing `--seal marker`
  explicitly is equivalent.
* **`eos file stat` no longer exists** in EOS ≥ 5.5.5 (it prints the `file`
  usage, exit 22). Its replacement, `eos file info <path> --size`, is what
  leos uses for remote size gates, with an `eos ls` fallback for existence.
* **`xrdfs` does not authenticate with the token** on Helios
  (EPERM/3010), so remote metadata stat is unavailable through that path.
  Content verification is unaffected — restore and `--round-trip` read the
  bytes themselves. `--allow-unverified` exists for the residual case of a
  store where size-stat always fails.
* **Old Enterprise-Linux `xrdcp` on some nodes ignores `?authz=`** and fails
  reads with `3010 [ERROR] Ops ... public access level restriction` even with
  a valid token. Run leos on a node with the current EOS stack instead of
  fighting it.
* `eos ls -ld` treats two path arguments as **one** path — one path per call.

## Performance as measured

One archiver, `--streams 8 --jobs 8`, 146.51 GiB unit (68 × ~2.3 GiB files):

| Phase | Rate |
| :--- | :--- |
| Hash pass (Lustre read) | 1.38 GiB/s |
| Upload (Lustre → Helios) | 2.25 GiB/s (≈8 TiB/h) |
| Round-trip read-back | same order; proven `OK` on every byte |

The `sealed ... MiB/s` summary averages both phases — read the log
timestamps for the truth. If you need more: raise `--streams/--jobs` per
unit and watch the *copy-phase* delta only; if it does not move, you are at
a site/path ceiling, not a leos ceiling.

!!! note "Know your quota before a TiB-scale sweep"

    `eos quota /eos/helios/Users/<user>/` — better to discover the ceiling
    in planning than at 60 % of a copy.

## Token hygiene

Tokens are path-scoped and short-lived by design, but they appear in `ps` on
shared nodes (they are command arguments of child processes). Keep expiries
tight, prefer dedicated nodes for unattended archiving, and treat a pasted
token like a password for its lifetime.
