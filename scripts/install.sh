#!/bin/sh
# Installs gpaperd and gpaperctl from a GitHub release.
#
#   curl -fsSLO https://raw.githubusercontent.com/worxbend/gpaperd/main/scripts/install.sh
#   less install.sh && sh install.sh [--version v0.1.1 --no-start]
#
# Everything goes into your home directory; no root is needed:
#   ~/.local/bin/{gpaperd,gpaperctl}           (--prefix changes ~/.local)
#   ~/.local/share/man/man{1,5}/                man pages
#   ~/.config/systemd/user/gpaperd.service      user unit
#   ~/.config/gpaperd/config.toml               starter config, only if absent
#
# Run with --help for the options.
set -eu

repo="worxbend/gpaperd"
version=""
prefix="$HOME/.local"
start="auto"
action="install"
purge=0

usage() {
    cat <<'EOF'
Usage: install.sh [options]

  --version TAG   install this release, e.g. v0.1.1 (default: the newest)
  --prefix DIR    install binaries to DIR/bin, man pages to DIR/share/man
                  (default: ~/.local)
  --start         enable and start the user unit even outside GNOME
  --no-start      install only; do not enable or start the unit
  --uninstall     stop the unit and remove what this script installed,
                  and gpaperd's tile in Settings; config, state and cache stay
  --purge         with --uninstall: also remove config, state, slots, cache
  -h, --help      show this help
EOF
}

say() { printf '%s\n' "gpaperd: $*"; }
warn() { printf '%s\n' "gpaperd: warning: $*" >&2; }
die() {
    printf '%s\n' "gpaperd: error: $*" >&2
    exit 1
}

# An XDG base is kept only when it is absolute. Anything else is the
# home fallback, matching Paths::resolve.
absolute_base() {
    case "$1" in
    /*) printf '%s\n' "$1" ;;
    *) printf '%s\n' "$2" ;;
    esac
}

# Purge must not rm -rf a relative path the daemon does not use.
remove_abs_trees() {
    for dir in "$@"; do
        case "$dir" in
        /*) ;;
        *) die "refusing to remove a relative path: $dir" ;;
        esac
    done
    rm -rf -- "$@"
}

line_ends_with() {
    text=$1
    suffix=$2
    text_len=${#text}
    suffix_len=${#suffix}
    [ "$text_len" -ge "$suffix_len" ] || return 1
    drop=$((text_len - suffix_len))
    while [ "$drop" -gt 0 ]; do
        text=${text#?}
        drop=$((drop - 1))
    done
    [ "$text" = "$suffix" ]
}

# The archive name is a fixed string. A miss does not run sha256sum.
checksum_archive() {
    archive=$1
    sums=$2
    found=
    while IFS= read -r line || [ -n "$line" ]; do
        if line_ends_with "$line" " $archive"; then
            [ -z "$found" ] || die "SHA256SUMS names $archive more than once"
            found=$line
        fi
    done < "$sums"
    [ -n "$found" ] || die "checksum mismatch for $archive"
    printf '%s\n' "$found" > "$work/checksum-line"
    (cd "$work" && sha256sum -c --status checksum-line) ||
        die "checksum mismatch for $archive"
}

# One ExecStart word. systemd splits on spaces, expands % and $, and
# treats \ as a C escape. Do not pass the path through sed.
systemd_exec_word() {
    path=$1
    escaped=
    while [ -n "$path" ]; do
        rest=${path#?}
        char=${path%"$rest"}
        path=$rest
        # The single-quoted backslashes are printf escapes, not quote escapes.
        # shellcheck disable=SC1003
        case "$char" in
        \\) escaped=$(printf '%s\\\\' "$escaped") ;;
        '"') escaped=$(printf '%s\\"' "$escaped") ;;
        %) escaped=$(printf '%s%%%%' "$escaped") ;;
        '$') escaped=$(printf '%s$$' "$escaped") ;;
        *) escaped=$(printf '%s%s' "$escaped" "$char") ;;
        esac
    done
    printf '"%s"' "$escaped"
}

write_user_unit() {
    src_unit=$1
    dest=$2
    exec_path=$3
    case "$exec_path" in
    *'
'*) die "install prefix contains a newline" ;;
    esac
    unit_new=$unit_dir/.gpaperd.service.new
    exec_word=$(systemd_exec_word "$exec_path")
    while IFS= read -r line || [ -n "$line" ]; do
        case "$line" in
        ExecStart=*) printf 'ExecStart=%s\n' "$exec_word" ;;
        *) printf '%s\n' "$line" ;;
        esac
    done < "$src_unit" > "$unit_new"
    chmod 644 "$unit_new"
    mv -f "$unit_new" "$dest"
}

install_starter_config() {
    config_dir=$1
    example=$2
    mkdir -p "$config_dir"
    chmod 700 "$config_dir"
    cp "$example" "$config_dir/config.example.toml"
    config_file=$config_dir/config.toml
    if [ -e "$config_file" ]; then
        chmod go-rwx "$config_file"
        say "kept your config: $config_file"
        return
    fi
    (
        umask 077
        set -C
        cat > "$config_file" <<'EOF'
# gpaperd config. Every key: man 5 gpaperd.toml, or config.example.toml here.
# Edits apply while gpaperd runs; `gpaperd check-config` validates this file.

[general]
interval = "15m"        # 1m ..= 7d
order = "random"        # or "alphabetical"

[light]
sources = ["~/Pictures/Wallpapers/light"]

[dark]
sources = ["~/Pictures/Wallpapers/dark"]
EOF
    )
    chmod 600 "$config_file"
    mkdir -p "$HOME/Pictures/Wallpapers/light" "$HOME/Pictures/Wallpapers/dark"
    say "wrote a starter config: $config_file"
    say "put images into ~/Pictures/Wallpapers/light and ~/Pictures/Wallpapers/dark"
}

while [ $# -gt 0 ]; do
    case "$1" in
    --version)
        [ $# -ge 2 ] || die "--version needs a tag"
        version=$2
        shift 2
        ;;
    --prefix)
        [ $# -ge 2 ] || die "--prefix needs a directory"
        prefix=$2
        shift 2
        ;;
    --start) start="yes"; shift ;;
    --no-start) start="no"; shift ;;
    --uninstall) action="uninstall"; shift ;;
    --purge) purge=1; shift ;;
    -h | --help) usage; exit 0 ;;
    *) die "unknown option: $1 (see --help)" ;;
    esac
done
case "$prefix" in
/*) ;;
*) die "--prefix must be an absolute path: $prefix" ;;
esac

[ "$(id -u)" -ne 0 ] || die "run this as your own user, not as root: gpaperd is a per-user service"

config_home=$(absolute_base "${XDG_CONFIG_HOME:-}" "$HOME/.config")
data_home=$(absolute_base "${XDG_DATA_HOME:-}" "$HOME/.local/share")
state_home=$(absolute_base "${XDG_STATE_HOME:-}" "$HOME/.local/state")
cache_home=$(absolute_base "${XDG_CACHE_HOME:-}" "$HOME/.cache")
bin_dir="$prefix/bin"
man_dir="$prefix/share/man"
unit_dir="$config_home/systemd/user"
unit="$unit_dir/gpaperd.service"

has() { command -v "$1" >/dev/null 2>&1; }
have_systemd() { has systemctl && systemctl --user show-environment >/dev/null 2>&1; }

in_gnome() {
    case ":${XDG_CURRENT_DESKTOP:-}:" in
    *:GNOME:*) return 0 ;;
    *) return 1 ;;
    esac
}

uninstall() {
    # Purge deletes the slots GNOME points at and the saved original keys.
    if [ -x "$bin_dir/gpaperctl" ] && "$bin_dir/gpaperctl" status >/dev/null 2>&1; then
        say "putting your original wallpaper back (gpaperctl restore)"
        if ! "$bin_dir/gpaperctl" restore; then
            [ "$purge" -eq 0 ] ||
                die "gpaperctl restore failed: fix that, or stop gpaperd (systemctl --user stop gpaperd) to purge without restoring"
            warn "gpaperctl restore failed"
        fi
    elif [ "$purge" -eq 1 ]; then
        warn "gpaperd is not running: wallpaper not restored; if GNOME shows no wallpaper after this, pick one in Settings"
    fi
    if have_systemd; then
        systemctl --user disable --now gpaperd.service >/dev/null 2>&1 || true
    fi
    # The daemon writes its Settings tile; without the daemon it is stale.
    rm -f "$unit" "$bin_dir/gpaperd" "$bin_dir/gpaperctl" \
        "$man_dir/man1/gpaperd.1" "$man_dir/man1/gpaperctl.1" "$man_dir/man5/gpaperd.toml.5" \
        "$data_home/gnome-background-properties/gpaperd.xml"
    if have_systemd; then systemctl --user daemon-reload || true; fi
    if [ "$purge" -eq 1 ]; then
        remove_abs_trees "$config_home/gpaperd" "$data_home/gpaperd" \
            "$state_home/gpaperd" "$cache_home/gpaperd"
        say "removed gpaperd, its config, state, slots and cache"
    else
        say "removed gpaperd; config ($config_home/gpaperd) and state stay (use --purge to remove them)"
    fi
}

if [ "$action" = "uninstall" ]; then
    uninstall
    exit 0
fi

# --- download ---------------------------------------------------------------

case "$(uname -m)" in
x86_64 | amd64) arch="x86_64" ;;
aarch64 | arm64) arch="aarch64" ;;
*) die "no release build for $(uname -m); build from source: cargo install --git https://github.com/$repo --locked gpaperd-daemon gpaperd-ctl" ;;
esac
[ "$(uname -s)" = "Linux" ] || die "gpaperd runs on Linux only"

# Same seconds as gpaperd_ctl::CALL_TIMEOUT. This file states no other bound.
call_timeout=35
if has curl; then
    # Bounds a stall, not the transfer, like wget --timeout.
    fetch() { curl --proto '=https' --tlsv1.2 --connect-timeout "$call_timeout" --speed-limit 1 --speed-time "$call_timeout" -fsSL "$1" -o "$2"; }
    fetch_text() { curl --proto '=https' --tlsv1.2 --connect-timeout "$call_timeout" --speed-limit 1 --speed-time "$call_timeout" -fsSL "$1"; }
elif has wget; then
    fetch() { wget -q --https-only --timeout="$call_timeout" -O "$2" "$1"; }
    fetch_text() { wget -q --https-only --timeout="$call_timeout" -O - "$1"; }
else
    die "curl or wget is needed"
fi
for tool in tar sha256sum; do
    has "$tool" || die "$tool is needed"
done

if [ -z "$version" ]; then
    # The newest release, pre-releases included (gpaperd is still 0.x).
    version=$(fetch_text "https://api.github.com/repos/$repo/releases?per_page=1" |
        sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1)
    [ -n "$version" ] || die "could not find the newest release of $repo"
fi
case "$version" in v*) ;; *) version="v$version" ;; esac

name="gpaperd-$version-$arch-linux-gnu"
base="https://github.com/$repo/releases/download/$version"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM

say "downloading $name"
fetch "$base/$name.tar.gz" "$work/$name.tar.gz" ||
    die "cannot download $name.tar.gz from release $version (see https://github.com/$repo/releases)"
fetch "$base/SHA256SUMS" "$work/SHA256SUMS" || die "release $version has no SHA256SUMS"
checksum_archive "$name.tar.gz" "$work/SHA256SUMS"
tar -C "$work" -xzf "$work/$name.tar.gz"
src="$work/$name"

# --- install ----------------------------------------------------------------

was_running=0
if have_systemd && systemctl --user is-active --quiet gpaperd.service; then
    was_running=1
fi

mkdir -p "$bin_dir" "$man_dir/man1" "$man_dir/man5" "$unit_dir"
# A fresh name then rename, so a running gpaperd keeps its old file.
for bin in gpaperd gpaperctl; do
    cp "$src/$bin" "$bin_dir/.$bin.new"
    chmod 755 "$bin_dir/.$bin.new"
    mv -f "$bin_dir/.$bin.new" "$bin_dir/$bin"
done
cp "$src/dist/man/gpaperd.1" "$src/dist/man/gpaperctl.1" "$man_dir/man1/"
cp "$src/dist/man/gpaperd.toml.5" "$man_dir/man5/"
write_user_unit "$src/dist/gpaperd.service" "$unit" "$bin_dir/gpaperd"
say "installed $("$bin_dir/gpaperd" --version) to $bin_dir"

install_starter_config "$config_home/gpaperd" "$src/dist/config.example.toml"

# --- checks -----------------------------------------------------------------

missing=$(ldd "$bin_dir/gpaperd" 2>/dev/null | sed -n 's/^[[:space:]]*\([^ ]*\) => not found.*/\1/p' | tr '\n' ' ')
[ -z "$missing" ] || warn "missing shared libraries: $missing(install GLib, libseccomp and fontconfig from your distribution)"
has bwrap || warn "bwrap (bubblewrap) is not installed: images cannot be decoded and every image will be refused; install it, then run systemctl --user restart gpaperd"
loaders=0
for dir in /usr/lib/glycin-loaders /usr/lib64/glycin-loaders /usr/libexec/glycin-loaders /usr/lib/*/glycin-loaders; do
    [ -d "$dir" ] && loaders=1
done
if [ "$loaders" -eq 0 ]; then
    warn "no glycin loaders found (package glycin-loaders): images cannot be decoded and every image will be refused"
fi
case ":$PATH:" in
*":$bin_dir:"*) ;;
*) warn "$bin_dir is not on your PATH; add it to use gpaperctl" ;;
esac
"$bin_dir/gpaperd" --config "$config_home/gpaperd/config.toml" check-config >/dev/null ||
    warn "your config has errors: run gpaperd check-config"

# --- start ------------------------------------------------------------------

if ! have_systemd; then
    warn "no systemd user manager here: start gpaperd yourself inside your GNOME session"
    exit 0
fi
systemctl --user daemon-reload
if [ "$start" = "auto" ]; then
    if in_gnome; then start="yes"; else start="no"; fi
    [ "$start" = "yes" ] || say "not a GNOME session (XDG_CURRENT_DESKTOP=${XDG_CURRENT_DESKTOP:-unset}): the unit is installed but not started"
fi
if [ "$start" = "yes" ]; then
    systemctl --user enable gpaperd.service >/dev/null 2>&1 ||
        die "systemctl --user enable gpaperd.service failed"
    if [ "$was_running" -eq 1 ]; then
        systemctl --user restart gpaperd.service
        say "restarted gpaperd"
    else
        systemctl --user start gpaperd.service
        say "started gpaperd; it starts with every GNOME login from now on"
    fi
    say "check it: gpaperctl status   logs: journalctl --user -u gpaperd -f"
else
    say "to start it: systemctl --user enable --now gpaperd.service"
fi
