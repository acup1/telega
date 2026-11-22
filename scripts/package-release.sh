#!/bin/sh
# Build a native release archive. Paths passed by the caller can be relative.
set -eu

usage() {
    echo "Usage: $0 [output-directory [telega|tgp-viewer]]" >&2
    exit 2
}
[ "$#" -le 2 ] || usage
selection=${2:-all}
case "$selection" in
    all|telega|tgp-viewer) ;;
    *) usage ;;
esac
repo_dir=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
target_dir=${CARGO_TARGET_DIR:-"$repo_dir/target"}
out=${1:-"$repo_dir/target/dist"}
mkdir -p "$out" "$target_dir"
out=$(CDPATH='' cd -- "$out" && pwd)
target_dir=$(CDPATH='' cd -- "$target_dir" && pwd)
cd "$repo_dir"

target=${CARGO_BUILD_TARGET:-$(rustc -vV | sed -n 's/^host: //p')}
case "$target" in
    aarch64-apple-darwin) system=aarch64-darwin ;;
    x86_64-unknown-linux-gnu) system=x86_64-linux ;;
    aarch64-unknown-linux-gnu) system=aarch64-linux ;;
    *) echo "Unsupported release target: $target" >&2; exit 1 ;;
esac

stage=$(mktemp -d "$out/.telega-package.XXXXXX")
trap 'rm -rf "$stage"' EXIT
mkdir -p "$stage/bin" "$stage/home"
cp LICENSE "$stage/LICENSE"

package() (
    program=$1
    shift
    if [ "$program" = tgp-viewer ]; then
        unset TG_API_ID TG_API_HASH
    fi
    cargo build --locked --release --target "$target" --target-dir "$target_dir" -p "$program" --bin "$program" "$@"
    cp "$target_dir/$target/release/$program" "$stage/bin/$program"
    archive="$program-$system.tar.gz"
    if [ "$system" = aarch64-darwin ]; then
        otool -L "$stage/bin/$program" >"$stage/libraries"
        # Nix's Darwin SDK can link Apple's libiconv under its store path.
        iconv=$(awk '$1 ~ /^\/nix\/store\/[^\/]+-libiconv-[^\/]+\/lib\/libiconv\.2\.dylib$/ {print $1}' "$stage/libraries")
        if [ -n "$iconv" ]; then
            install_name_tool -change "$iconv" /usr/lib/libiconv.2.dylib "$stage/bin/$program"
            codesign -f -s - "$stage/bin/$program"
            otool -L "$stage/bin/$program" >"$stage/libraries"
        fi
        if ! awk 'NR > 1 && $1 !~ /^\/(usr\/lib|System\/Library)\// { exit 1 }' "$stage/libraries"; then
            echo "The release binary depends on libraries outside macOS." >&2
            exit 1
        fi
    fi
    (
        cd "$stage/home"
        env -i PATH="$PATH" HOME="$stage/home" "$stage/bin/$program" --help >/dev/null
    )
    COPYFILE_DISABLE=1 tar -czf "$stage/$archive" -C "$stage" "bin/$program" LICENSE
    (
        cd "$stage"
        if command -v sha256sum >/dev/null 2>&1; then
            sha256sum "$archive"
        else
            shasum -a 256 "$archive"
        fi
    ) >"$stage/$archive.sha256"
    mv "$stage/$archive" "$stage/$archive.sha256" "$out/"
    printf 'Built %s/%s\n' "$out" "$archive"
)

if [ "$selection" != telega ]; then
    package tgp-viewer
fi
if [ "$selection" != tgp-viewer ]; then
    package telega --features bundled-app
fi
