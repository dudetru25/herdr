#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(dirname -- "$script_dir")
source_bin=${HERDR_RELOAD_SOURCE:-"$repo_root/target/release/herdr"}
destination=${HERDR_RELOAD_DESTINATION:-"${HOME:?HOME must be set}/.local/bin/herdr"}

case "${1:-}" in
    "")
        mode=reload
        ;;
    --check)
        mode=check
        ;;
    *)
        echo "usage: $0 [--check]" >&2
        exit 2
        ;;
esac

canonical_executable() {
    executable=$1
    executable_dir=$(CDPATH= cd -- "$(dirname -- "$executable")" 2>/dev/null && pwd -P) ||
        return 1
    printf '%s/%s\n' "$executable_dir" "$(basename -- "$executable")"
}

same_executable_path() {
    left=$1
    right=$2
    left_canonical=$(canonical_executable "$left" 2>/dev/null || printf '%s\n' "$left")
    right_canonical=$(canonical_executable "$right" 2>/dev/null || printf '%s\n' "$right")
    [ "$left_canonical" = "$right_canonical" ]
}

check_bare_resolution() {
    bare_bin=$(command -v herdr 2>/dev/null || true)
    if [ -z "$bare_bin" ]; then
        echo "error: bare herdr is not available on PATH" >&2
        echo "reload-local destination: $destination" >&2
        return 1
    fi

    bare_canonical=$(canonical_executable "$bare_bin") || {
        echo "error: cannot resolve bare herdr path: $bare_bin" >&2
        return 1
    }
    destination_canonical=$(canonical_executable "$destination") || {
        echo "error: reload-local destination directory does not exist: $(dirname -- "$destination")" >&2
        return 1
    }

    if [ "$bare_canonical" != "$destination_canonical" ]; then
        echo "error: bare herdr resolves to $bare_bin ($bare_canonical)" >&2
        echo "reload-local destination is $destination ($destination_canonical)" >&2
        echo "make the destination the first Herdr executable on PATH, or set HERDR_RELOAD_DESTINATION to the PATH-selected binary" >&2
        return 1
    fi

    printf 'bare: %s\n' "$bare_bin"
    printf 'destination: %s\n' "$destination"
}

check_reload_resolution() {
    if [ -x "$destination" ]; then
        check_bare_resolution
        return
    fi

    saved_ifs=$IFS
    IFS=:
    for path_dir in ${PATH:-}; do
        if [ -z "$path_dir" ]; then
            path_dir=.
        fi
        path_candidate="$path_dir/herdr"
        if same_executable_path "$path_candidate" "$destination"; then
            IFS=$saved_ifs
            printf 'bare after install: %s\n' "$destination"
            printf 'destination: %s\n' "$destination"
            return
        fi
        if [ -x "$path_candidate" ]; then
            IFS=$saved_ifs
            echo "error: installing $destination would leave bare herdr resolving to $path_candidate" >&2
            echo "make the destination directory precede other Herdr executables on PATH, or set HERDR_RELOAD_DESTINATION to the PATH-selected binary" >&2
            return 1
        fi
    done
    IFS=$saved_ifs

    echo "error: reload-local destination directory is not on PATH: $(dirname -- "$destination")" >&2
    return 1
}

if [ "$mode" = check ]; then
    check_bare_resolution
    status=$("$destination" status --json)
    printf 'status: %s\n' "$status"
    case "$status" in
        *'"compatible":true'*)
            exit 0
            ;;
        *)
            echo "error: bare client is not confirmed compatible with the running server" >&2
            exit 1
            ;;
    esac
fi

check_reload_resolution

if [ "${HERDR_RELOAD_SKIP_BUILD:-0}" != "1" ]; then
    (
        cd "$repo_root"
        cargo build --release --locked
    )
fi

if [ ! -x "$source_bin" ]; then
    echo "error: release binary is missing or not executable: $source_bin" >&2
    exit 1
fi

destination_dir=$(dirname -- "$destination")
mkdir -p "$destination_dir"
next_bin=$(mktemp "$destination_dir/.herdr-next.XXXXXX")
cleanup() {
    rm -f -- "$next_bin"
}
trap cleanup EXIT HUP INT TERM

install -m 0755 "$source_bin" "$next_bin"
if [ "$(uname -s)" = "Darwin" ]; then
    codesign --verify --deep --strict "$next_bin"
fi
"$next_bin" --version >/dev/null

# Replacing the path with a fresh inode is required on macOS. Copying over a
# running executable can leave taskgated rejecting later launches even when a
# subsequent codesign verification succeeds.
mv -f -- "$next_bin" "$destination"
trap - EXIT HUP INT TERM

"$destination" --version

if [ "${HERDR_RELOAD_SKIP_HANDOFF:-0}" = "1" ]; then
    echo "Installed $destination (live handoff skipped)."
    exit 0
fi

"$destination" server live-handoff --import-exe "$destination"
