#!/usr/bin/env bash
# Builds the macOS (Apple silicon) agent's release binaries on a Mac and copies them back, for
#   farm-release release-agent --platform macos-arm64 --prebuilt <DEST> ...
# (PROTOCOL 11.2; agent/README.md "macOS release"). Run from the desktop checkout (Git Bash on Windows works).
#
#   tools/mac-agent-build.sh [DEST]          default DEST: agent/dist/macos-arm64-prebuilt
#
# It pushes HEAD (committed work only) to a git remote that is a bare repository on the Mac, checks that commit out
# in a build checkout there, runs `cargo build --release -p farm-agent -p zerofarm-launcher` (niced, without any
# ZEROFARM_TRUSTED_* override), checks the binaries (arm64, versions, trusted keys, code signature) and copies
# farm-agent and ZeroFarm into DEST. Nothing is signed or released here: that is farm-release's job, on the desktop
# that holds the release key.
#
# Settings (environment; keep machine names out of the repository):
#   ZEROFARM_MAC            ssh target of the Mac (required)
#   ZEROFARM_MAC_REMOTE     git remote of the bare repository on the Mac (default: mac)
#   ZEROFARM_MAC_CHECKOUT   build checkout on the Mac, relative to its home (default: zero-farm-wt/mac-release)
#   ZEROFARM_MAC_TARGET     CARGO_TARGET_DIR on the Mac, relative to its home (default: cargo-target/zero-farm/mac-release)
#   ZEROFARM_MAC_JOBS       CARGO_BUILD_JOBS on the Mac (default: 6)
set -euo pipefail

mac=${ZEROFARM_MAC:?set ZEROFARM_MAC to the ssh target of the build Mac}
remote=${ZEROFARM_MAC_REMOTE:-mac}
checkout=${ZEROFARM_MAC_CHECKOUT:-zero-farm-wt/mac-release}
target=${ZEROFARM_MAC_TARGET:-cargo-target/zero-farm/mac-release}
jobs=${ZEROFARM_MAC_JOBS:-6}
top=$(git rev-parse --show-toplevel)
dest=${1:-$top/agent/dist/macos-arm64-prebuilt}

head=$(git rev-parse HEAD)
if [ -n "$(git status --porcelain -- agent)" ]; then
    echo "note: agent/ has uncommitted changes; the Mac builds the commit $head without them" >&2
fi
mirror=$(git remote get-url "$remote")
mirror_path=${mirror#*:}
echo "pushing $head to $remote"
git push -q -f "$remote" "$head:refs/heads/mac-release"

ssh "$mac" bash -s -- "$mirror_path" "$checkout" "$target" "$jobs" "$head" <<'REMOTE'
set -euo pipefail
mirror=$1 checkout=$2 target=$3 jobs=$4 head=$5
export PATH="$HOME/.cargo/bin:$PATH" CARGO_TARGET_DIR="$HOME/$target" CARGO_BUILD_JOBS="$jobs"
unset ZEROFARM_TRUSTED_KEYS ZEROFARM_TRUSTED_DATA_KEYS ZEROFARM_SETUP_PAYLOAD
cd "$HOME"
if [ ! -d "$checkout/.git" ]; then
    git clone -q "$mirror" "$checkout"
fi
cd "$checkout"
git fetch -q origin mac-release
git checkout -q -f "$head"
cd agent
nice -n 15 cargo build --release -p farm-agent -p zerofarm-launcher
out="$CARGO_TARGET_DIR/release"
for b in farm-agent ZeroFarm; do
    file "$out/$b" | grep -q "arm64" || { echo "$b is not an arm64 executable" >&2; exit 1; }
    codesign -v "$out/$b" || { echo "$b has no valid (ad-hoc) code signature" >&2; exit 1; }
done
echo "farm-agent $("$out/farm-agent" version), ZeroFarm $("$out/ZeroFarm" version) (commit $head)"
"$out/farm-agent" keys
REMOTE

mkdir -p "$dest"
home=$(ssh "$mac" 'printf %s "$HOME"')
for b in farm-agent ZeroFarm; do
    scp -q "$mac:$home/$target/release/$b" "$dest/$b"
done
echo "$head" > "$dest/COMMIT"
echo "copied farm-agent and ZeroFarm into $dest"
echo "next: farm-release release-agent --platform macos-arm64 --prebuilt \"$dest\" --url https://farm.greggg230.com --channel canary --min-agent 0.3.0 --github greggg230/zero-farm"
