#!/usr/bin/env bash
# Checks our ASC MHL output against the official reference implementation:
# schema validation, completeness, and a full re-hash of every file against
# the checksums we recorded. Then the other way round: we append to a history
# the reference tool has written into.
#
# Needs the reference tool and the two schema files:
#   python -m venv mhlref && mhlref/Scripts/pip install ascmhl   (bin/ on macOS)
#   curl -LO https://raw.githubusercontent.com/ascmitc/mhl/master/xsd/ASCMHL.xsd
#   curl -LO https://raw.githubusercontent.com/ascmitc/mhl/master/xsd/ASCMHLDirectory.xsd
#
# Usage: scripts/mhl-interop.sh <dir with ascmhl and ascmhl-debug> <dir with the .xsd files>
set -u
TOOLS="$1"; XSD="$2"
A="$TOOLS/ascmhl"; D="$TOOLS/ascmhl-debug"
HERE="$(cd "$(dirname "$0")/.." && pwd)"
(cd "$HERE" && cargo build -q --release -p backup-cli) || exit 1
BT="$HERE/target/release/bt"
W="$(mktemp -d)"; trap 'rm -rf "$W"' EXIT
# The directory schema imports the main one by URL; point it at the local copy.
sed "s#https://raw.githubusercontent.com/ascmitc/mhl/master/xsd/ASCMHL.xsd#ASCMHL.xsd#" \
  "$XSD/ASCMHLDirectory.xsd" > "$W/ASCMHLDirectory.xsd"
cp "$XSD/ASCMHL.xsd" "$W/"
fail=0
check() { if [ "$2" -eq 0 ]; then echo "  ok   $1"; else echo "  FAIL $1"; fail=1; fi; }

python - "$W/card" <<'PY'
import os, sys
root = sys.argv[1]
files = {
    "PRIVATE/M4ROOT/CLIP/C0001.MP4": 3_000_123,
    "PRIVATE/M4ROOT/CLIP/C0001M01.XML": 900,
    "PRIVATE/M4ROOT/MEDIAPRO.XML": 300,
    "DCIM/100MSDCF/DSC 0001 & co.ARW": 700_001,
    "DCIM/100MSDCF/perché.JPG": 12_345,
    "EMPTY.BIN": 0,
}
for rel, size in files.items():
    path = os.path.join(root, *rel.split("/"))
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "wb") as f:
        f.write(os.urandom(size))
PY

for algo in xxh64 xxh3 xxh128 md5 sha1 c4; do
  echo "== $algo"
  dest="$W/$algo/Trail series"
  "$BT" copy "$W/card" "$dest" --algo "$algo" --mhl >/dev/null 2>&1; check "copy with MHL" $?
  "$D" xsd-schema-check -xsd "$W/ASCMHL.xsd" "$(ls "$dest"/ascmhl/0001_*.mhl)" >/dev/null 2>&1
  check "manifest valid against the ASC MHL schema" $?
  "$D" xsd-schema-check -df -xsd "$W/ASCMHLDirectory.xsd" "$dest/ascmhl/ascmhl_chain.xml" >/dev/null 2>&1
  check "chain valid against the directory schema" $?
  out=$("$A" diff "$dest" 2>&1); ! echo "$out" | grep -qi "error\|new file\|missing"
  check "reference diff: nothing missing, nothing untracked" $?
  out=$("$A" create -v -h "$algo" "$dest" 2>&1); rc=$?; n=$(echo "$out" | grep -c "verified  ")
  [ $rc -eq 0 ] && [ "$n" -eq 6 ] && ! echo "$out" | grep -qi "error\|original hash"
  check "reference re-hashed all 6 files against our checksums ($n verified)" $?
  out=$("$BT" copy "$W/card" "$dest" --algo "$algo" --mhl 2>&1)
  echo "$out" | grep -q "MHL generation 3 .*: 0 new, 6 verified, 0 failed"
  check "we append generation 3 on top of the reference's" $?
  "$A" create -h "$algo" "$dest" >/dev/null 2>&1; check "reference accepts the history again" $?
  out=$("$BT" verify "$dest" --record 2>&1); rc=$?
  [ $rc -eq 0 ] && echo "$out" | grep -q "6 of 6 files intact" && echo "$out" | grep -q "recorded as generation 5"
  check "our verify finds all 6 intact and records generation 5" $?
  "$D" xsd-schema-check -xsd "$W/ASCMHL.xsd" "$(ls "$dest"/ascmhl/0005_*.mhl)" >/dev/null 2>&1
  check "verification manifest valid against the schema" $?
  "$A" create -h "$algo" "$dest" >/dev/null 2>&1; check "reference accepts the history with our verification in it" $?
done

echo "== corruption is caught by the reference from our checksums"
dest="$W/xxh64/Trail series"
python -c "
import sys
p = sys.argv[1]
with open(p, 'r+b') as f:
    f.seek(1000); b = f.read(1); f.seek(1000); f.write(bytes([b[0] ^ 1]))
" "$dest/PRIVATE/M4ROOT/CLIP/C0001.MP4"
out=$("$BT" verify "$dest" 2>&1); rc=$?
[ $rc -ne 0 ] && echo "$out" | grep -q "5 of 6 files intact, 1 changed" && echo "$out" | grep -q "CHANGED PRIVATE/M4ROOT/CLIP/C0001.MP4"
check "our verify names the flipped byte's file" $?
out=$("$A" create -v -h xxh64 "$dest" 2>&1); rc=$?
[ $rc -ne 0 ] && echo "$out" | grep -q "hash mismatch"; check "flipped byte reported as hash mismatch" $?

echo "== a history the reference started, continued by us with another checksum type"
dest="$W/mixed/Trail series"; mkdir -p "$dest"; cp -r "$W/card/." "$dest/"
"$A" create -h md5 "$dest" >/dev/null 2>&1; check "reference creates generation 1 (md5, with directory hashes)" $?
out=$("$BT" verify "$dest" 2>&1); rc=$?
[ $rc -eq 0 ] && echo "$out" | grep -q "6 of 6 files intact, 0 changed, 0 missing, 0 not in the history"
check "our verify reads a history written only by the reference" $?
out=$("$BT" copy "$W/card" "$dest" --algo xxh64 --mhl 2>&1)
echo "$out" | grep -q "MHL generation 2 .*: 0 new, 6 verified, 0 failed"; check "we add xxh64, tied to the md5 on record" $?
"$D" xsd-schema-check -xsd "$W/ASCMHL.xsd" "$(ls "$dest"/ascmhl/0002_*.mhl)" >/dev/null 2>&1
check "two-checksum manifest valid against the schema" $?
out=$("$A" create -v -h xxh64 "$dest" 2>&1); rc=$?; n=$(echo "$out" | grep -c "verified  ")
[ $rc -eq 0 ] && [ "$n" -eq 6 ]; check "reference verifies our xxh64 entries ($n verified)" $?

exit $fail
