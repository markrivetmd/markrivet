#!/usr/bin/env bash
# Package a release build of markrivet, after checking it is the binary it
# claims to be. The release workflow runs this on each build; running it locally
# gives the same checks and the same archive.
#
#   bash tools/release-archive.sh TARGET VERSION COMMIT OUTDIR
#
# TARGET is x86_64-unknown-linux-gnu or aarch64-unknown-linux-gnu, and the
# binary is target/release/markrivet, built natively for it. The script refuses,
# exiting non-zero with the reason, unless:
#
#   - the binary is an ELF executable for TARGET's machine;
#   - no GLIBC_ version it needs (every entry of .gnu.version_r, whatever its
#     flags, since the loader checks them all) is above the floor, 2.34;
#   - `markrivet --version` prints exactly "markrivet VERSION COMMIT": not
#     "-dirty", not "unknown", not another commit.
#
# It then writes OUTDIR/markrivet-TARGET.tar.gz holding markrivet-TARGET/ with
# markrivet, LICENSE and README.md. Owners, modes, order and the mtime (the
# commit's) are fixed and gzip stores no name or time, so the archive's bytes
# depend only on the binary and the commit. The archive is read back and must
# hold exactly those four entries, the binary byte-identical and executable.
set -euo pipefail
export LC_ALL=C
umask 022

GLIBC_FLOOR=2.34

die() { echo "release-archive: $*" >&2; exit 1; }

[ $# -eq 4 ] || die "usage: release-archive.sh TARGET VERSION COMMIT OUTDIR"
target=$1 version=$2 commit=$3 outdir=$4

case $target in
x86_64-unknown-linux-gnu) machine='x86-64' ;;
aarch64-unknown-linux-gnu) machine='ARM aarch64' ;;
*) die "unsupported target '$target' (x86_64-unknown-linux-gnu or aarch64-unknown-linux-gnu)" ;;
esac
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "VERSION '$version' is not X.Y.Z"
[[ $commit =~ ^[0-9a-f]{40}$ ]] || die "COMMIT '$commit' is not a full 40-hex commit id"
[ -n "$outdir" ] || die "OUTDIR is empty"

for tool in file readelf tar gzip git cmp; do
  command -v "$tool" >/dev/null || die "needs $tool"
done

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
bin=$root/target/release/markrivet
{ [ -f "$bin" ] && [ -x "$bin" ]; } || die "no executable $bin: build it with cargo build --locked --release -p markrivet-cli --bin markrivet"

# Machine: an archive named for one target must carry that target's binary.
kind=$(file -b "$bin") || die "file failed on $bin"
case $kind in
"ELF 64-bit LSB "*", $machine, "*) ;;
*) die "$bin is not an ELF executable for $target ($machine): $kind" ;;
esac

# glibc floor: the highest GLIBC_ version the binary needs.
needs=$(readelf -W --version-info "$bin") || die "readelf failed on $bin"
glibc=$(printf '%s\n' "$needs" | awk '
  /^Version needs section/ { inr = 1; next }
  /^Version (symbols|definition) section/ { inr = 0; next }
  inr { for (i = 1; i < NF; i++) if ($i == "Name:" && $(i + 1) ~ /^GLIBC_/) print $(i + 1) }
') || die "cannot read the version needs of $bin"
[ -n "$glibc" ] || die "$bin lists no GLIBC_ version needs: not a dynamically linked glibc binary"
bad=$(printf '%s\n' "$glibc" | grep -vE '^GLIBC_[0-9]+(\.[0-9]+)+$' || true)
[ -z "$bad" ] || die "$bin needs a glibc version that is not numeric: $(printf '%s' "$bad" | paste -sd' ')"
highest=$(printf '%s\n' "$glibc" | sed 's/^GLIBC_//' | sort -uV | tail -n1)
top=$(printf '%s\n%s\n' "$GLIBC_FLOOR" "$highest" | sort -V | tail -n1)
[ "$top" = "$GLIBC_FLOOR" ] ||
  die "$bin needs GLIBC_$highest, above the floor GLIBC_$GLIBC_FLOOR: build it against an older glibc (the release workflow builds in ubuntu:22.04, glibc 2.35)"

# Identity: the binary names the release commit, built from a clean tree.
want="markrivet $version $commit"
got=$("$bin" --version) || die "$bin --version failed"
if [ "$got" != "$want" ]; then
  case $got in
  *-dirty) die "identity mismatch: $bin was built from a tree with changes (-dirty): '$got', expected '$want'" ;;
  *unknown) die "identity mismatch: $bin names no commit (unknown, built outside a git checkout or one git refused): '$got', expected '$want'" ;;
  *) die "identity mismatch: $bin prints '$got', expected '$want'" ;;
  esac
fi

mtime=$(git -C "$root" log -1 --format=%ct "$commit^{commit}") || die "cannot read the commit time of $commit"
[[ $mtime =~ ^[0-9]+$ ]] || die "commit time '$mtime' is not a number"

name=markrivet-$target
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
mkdir "$stage/$name"
chmod 0755 "$stage/$name"
cp "$bin" "$stage/$name/markrivet"
cp "$root/LICENSE" "$root/README.md" "$stage/$name/"
chmod 0755 "$stage/$name/markrivet"
chmod 0644 "$stage/$name/LICENSE" "$stage/$name/README.md"

mkdir -p "$outdir"
archive=$outdir/$name.tar.gz
tar --create --file=- --directory="$stage" --format=gnu --sort=name \
  --owner=0 --group=0 --numeric-owner --mtime="@$mtime" "$name" |
  gzip -n -9 >"$stage/archive.tar.gz"
mv "$stage/archive.tar.gz" "$archive"

listing=$(tar -tzf "$archive") || die "cannot list $archive"
expected=$(printf '%s\n' "$name/" "$name/LICENSE" "$name/README.md" "$name/markrivet")
[ "$listing" = "$expected" ] || die "$archive holds $(printf '%s' "$listing" | paste -sd' '), expected $(printf '%s' "$expected" | paste -sd' ')"
mode=$(tar -tvzf "$archive" "$name/markrivet" | awk '{ print $1 }') || die "cannot read the binary's mode in $archive"
[ "$mode" = "-rwxr-xr-x" ] || die "the binary in $archive has mode $mode, not -rwxr-xr-x"
tar -xOzf "$archive" "$name/markrivet" | cmp -s - "$bin" || die "the binary in $archive differs from $bin"

echo "release-archive: $archive ($want, needs GLIBC_$highest at most)"
