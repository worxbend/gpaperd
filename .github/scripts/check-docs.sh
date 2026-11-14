#!/usr/bin/env bash
# Checks that the docs keep up with the code:
#   - every key in dist/config.example.toml is in README.md and gpaperd.toml(5);
#   - every subcommand of gpaperd and gpaperctl is in its man page;
#   - the man pages render without groff warnings.
#
# Usage: .github/scripts/check-docs.sh [BIN_DIR]
# BIN_DIR holds built gpaperd and gpaperctl (default: target/debug). The
# binaries only print --help; nothing else runs.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
bin_dir=${1:-"$root/target/debug"}
failures=0

fail() {
    printf 'check-docs: %s\n' "$*" >&2
    failures=$((failures + 1))
}

# Keys of the example config: every `name =` outside comments.
config_keys() {
    sed 's/#.*//' "$root/dist/config.example.toml" |
        grep -oE '\b[a-z_]+[[:space:]]*=' |
        tr -d ' =' |
        sort -u
}

# Subcommands from a clap --help: the indented names under "Commands:".
subcommands() {
    "$1" --help |
        awk '/^Commands:/ { listing = 1; next } /^$/ { listing = 0 } listing { print $1 }' |
        grep -vx help
}

while read -r key; do
    grep -qF -- "\`$key\`" "$root/README.md" || fail "README.md does not document \`$key\`"
    grep -qE -- "(^|[^a-z_])${key}([^a-z_]|$)" "$root/dist/man/gpaperd.toml.5" ||
        fail "gpaperd.toml(5) does not document $key"
done < <(config_keys)

for pair in gpaperd:gpaperd.1 gpaperctl:gpaperctl.1; do
    binary="$bin_dir/${pair%%:*}"
    page="$root/dist/man/${pair#*:}"
    [[ -x $binary ]] || { fail "$binary is not built"; continue; }
    while read -r command; do
        # roff escapes the hyphen: check-config is check\-config.
        grep -qF -- ".B ${command//-/\\-}" "$page" || fail "${page##*/} does not document $command"
    done < <(subcommands "$binary")
done

for page in "$root"/dist/man/*.[0-9]; do
    warnings=$(LC_ALL=C.UTF-8 groff -man -ww -z -Tutf8 "$page" 2>&1)
    [[ -z $warnings ]] || fail "${page##*/}: $warnings"
done

if ((failures > 0)); then
    exit 1
fi
echo "check-docs: ok"
