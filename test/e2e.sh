#!/bin/bash
# leos end-to-end test with the local 'copy' backend playing the EOS tier.
# Exercises: dry-run, archive, idempotent rerun, crash-residue janitor,
# status, verify (local + remote + round-trip), restore, purge licensing.
set -euo pipefail

LEOS=$(cd "$(dirname "${LEOS:-./target/release/leos}")" && pwd)/$(basename "${LEOS:-./target/release/leos}")
T=$(mktemp -d /tmp/leos-e2e.XXXX)
SCRATCH=$T/scratch
EOS=$T/eos
RESTART=$T/restart
mkdir -p "$SCRATCH" "$EOS" "$RESTART"
cd "$T"
mkunit() { # name nfiles size_MiB
  local d="$SCRATCH/$1"; mkdir -p "$d/sub"
  for i in $(seq "$2"); do head -c $((3*1024*1024)) /dev/urandom > "$d/sub/f$i.dat"; done
  head -c 4096 /dev/urandom > "$d/md"
  touch "$d/bbstage.final"
}
fail() { echo "E2E FAIL: $1"; exit 1; }

mkunit chk000005.bp5 2
mkunit chk000010.bp5 2

echo "== 1. status finds two pending units"
$LEOS status "$SCRATCH" | grep -q "2 pending" || fail status
$LEOS status "$SCRATCH/../$(basename $SCRATCH)" >/dev/null

echo "== 2. dry-run archives nothing"
out=$($LEOS archive "$SCRATCH" --dest "$EOS" --dry-run 2>&1)
case $out in *DRY*) ;; *) fail dry ;; esac
[ -z "$(ls -A $EOS)" ] || fail "dry wrote data"

echo "== 3. archive both units (one dir at a time: scan+skip licensed)"
$LEOS archive "$SCRATCH" --dest "$EOS" --seal rename 2>err.log
grep -q "sealed chk000005.bp5" err.log || fail "unit1 not sealed"
grep -q "sealed chk000010.bp5" err.log || fail "unit2 not sealed"
[ -f "$EOS/chk000005.bp5/.archived" ] || fail "no remote license"
[ -f "$SCRATCH/chk000005.bp5/.archived" ] || fail "no local license"
[ ! -e "$EOS/chk000005.bp5.incomplete" ] || fail "incomplete left behind"

echo "== 4. rerun is a no-op (idempotent)"
out=$($LEOS archive "$SCRATCH" --dest "$EOS" 2>&1)
case $out in *"nothing pending"*) ;; *) fail rerun ;; esac

echo "== 5. janitor sweeps stale .incomplete, keeps sealed units"
mkdir -p "$EOS/crash0099.bp5.incomplete"; echo x > "$EOS/crash0099.bp5.incomplete/partial"
mkdir -p "$SCRATCH/.leos"
echo "100|crash0099.bp5|$EOS/crash0099.bp5.incomplete" >> "$SCRATCH/.leos/records.log"
echo "99999999999|chk000005.bp5|$EOS/chk000005.bp5.incomplete" >> "$SCRATCH/.leos/records.log"
$LEOS janitor "$SCRATCH" --dest "$EOS"
[ ! -e "$EOS/crash0099.bp5.incomplete" ] || fail "janitor left stale tree"
[ -d "$EOS/chk000005.bp5" ] || fail "janitor ate a sealed unit"

echo "== 6. verify local + remote + round-trip"
out=$($LEOS verify "$SCRATCH/chk000005.bp5" 2>&1)
case $out in *OK*) ;; *) fail verify-local ;; esac
out=$($LEOS verify "$EOS/chk000010.bp5" --remote 2>&1)
case $out in *"remote verified"*) ;; *) fail verify-remote ;; esac
out=$($LEOS verify "$EOS/chk000010.bp5" --remote --round-trip 2>&1)
case $out in *round-trip*) ;; *) fail roundtrip ;; esac

echo "== 7. corruption is detected (both directions)"
echo garbage >> "$EOS/chk000010.bp5/sub/f1.dat"
$LEOS verify "$EOS/chk000010.bp5" --remote >/dev/null 2>&1 && fail "corruption missed (size)"
$LEOS restore "$EOS/chk000010.bp5" --remote --dest "$RESTART" >/dev/null 2>&1 \
  && fail "restore accepted corrupt unit"

echo "== 8. restore of a healthy unit is byte-identical"
$LEOS restore "$EOS/chk000005.bp5" --remote --dest "$RESTART" 2>&1 >/dev/null
diff -r --exclude=.archived "$SCRATCH/chk000005.bp5" "$RESTART/chk000005.bp5" || fail "restore bytes differ"

echo "== 9. purge respects the license chain"
out=$($LEOS purge "$SCRATCH" --dest "$EOS" 2>&1)
case $out in *PURGE-READY*) ;; *) fail purge-plan ;; esac
# a unit without remote license must be refused:
rm "$EOS/chk000005.bp5/.archived"
$LEOS purge "$SCRATCH" --dest "$EOS" --yes >/dev/null 2>&1 && fail "purge without remote license" || true
[ -d "$SCRATCH/chk000005.bp5" ] || fail "purge deleted despite refusal"
touch "$EOS/chk000005.bp5/.archived"  # license back (size not checked against tree, presence is)
cp "$SCRATCH/chk000005.bp5/.archived" "$EOS/chk000005.bp5/.archived"
out=$($LEOS purge "$SCRATCH" --dest "$EOS" --yes 2>&1)
case $out in *purged*) ;; *) fail purge-exec ;; esac
[ ! -d "$SCRATCH/chk000005.bp5" ] || fail "purged unit still present"

echo "== 10. single-unit <path> addressing works"
mkunit chk000015.bp5 1
$LEOS archive "$SCRATCH/chk000015.bp5" --dest "$EOS" 2>&1 >/dev/null
[ -f "$EOS/chk000015.bp5/.archived" ] || fail "single-unit archive"
grep -qF ".leos" "$EOS/chk000015.bp5/.archived" && fail "tool state leaked into manifest"
[ ! -e "$EOS/chk000015.bp5/.leos" ] || fail "tool state copied into archive"

echo
echo "E2E PASS  ($T left for inspection)"
