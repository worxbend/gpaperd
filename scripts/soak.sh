#!/usr/bin/env bash
# Soak test: runs the real gpaperd for DURATION seconds in a sandbox while a
# churn loop changes everything around it, then checks what is left.
#
#   scripts/soak.sh [DURATION]        # seconds, default 1200 (20 minutes)
#
# Environment:
#   GPAPERD, GPAPERCTL  binaries to test (default target/debug/*; build first)
#   SOAK_KEEP=1         keep the sandbox directory at the end
#   SOAK_MEM_KB         virtual memory cap of the daemon (default 8 GiB)
#   SOAK_RSS_MAX_KB     final RSS limit (default 262144, 256 MiB)
#
# Isolation: a temp HOME and XDG_* dirs, GSETTINGS_BACKEND=keyfile with a
# compiled test schema, and a private bus from dbus-run-session whose config
# lists no service directory, so no real service is activated on it. The
# user's settings, files and session bus are never touched.
#
# Not covered, because each needs privileges or a real session: clock steps,
# suspend and resume, network changes through nmcli, a live gnome-shell.
set -euo pipefail

MODE=outer
if [[ "${1:-}" == --inner ]]; then MODE=inner; shift; fi
readonly MODE
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly ROOT_DIR
readonly DURATION="${1:-1200}"
readonly GPAPERD="${GPAPERD:-$ROOT_DIR/target/debug/gpaperd}"
readonly GPAPERCTL="${GPAPERCTL:-$ROOT_DIR/target/debug/gpaperctl}"
readonly MEM_KB="${SOAK_MEM_KB:-8388608}"
readonly RSS_MAX_KB="${SOAK_RSS_MAX_KB:-262144}"
readonly SCHEMA_SRC="$ROOT_DIR/crates/daemon/tests/schemas/gnome"
readonly TINY_PNG="$ROOT_DIR/crates/daemon/tests/fixtures/tiny.png"
# 20s, counted as 100 slices of 0.2s. The ready wait states this bound.
# Both SIGTERM waits reuse it: a daemon that ignores the signal must not hang.
readonly DAEMON_WAIT_TRIES=100
readonly DAEMON_WAIT_SLICE=0.2

log() { printf 'soak: %s\n' "$*" >&2; }
die() {
  log "FAIL: $*"
  # CI may not keep the sandbox: show the end of the daemon's log here.
  if [[ -s "${DAEMON_LOG:-}" ]]; then
    log "last lines of $DAEMON_LOG:"
    tail -n 20 -- "$DAEMON_LOG" >&2
  fi
  exit 1
}

# ---------------------------------------------------------------- outer part

outer() {
  # Decimal whole seconds. Each arithmetic use is 10#, so a leading zero is not octal.
  [[ "$DURATION" =~ ^[0-9]+$ ]] || die "DURATION must be whole seconds, got '$DURATION'"
  local tool
  for tool in dbus-run-session glib-compile-schemas jq; do
    command -v "$tool" >/dev/null || die "$tool is not installed"
  done
  [[ -x "$GPAPERD" && -x "$GPAPERCTL" ]] \
    || die "build first: cargo build -p gpaperd-daemon -p gpaperd-ctl"

  local sandbox bus_dir
  sandbox="$(mktemp -d "${TMPDIR:-/tmp}/gpaperd-soak.XXXXXX")"
  # A unix socket path has at most 107 bytes, and TMPDIR may be long: the
  # bus listens in a short directory of its own.
  bus_dir="$(mktemp -d /tmp/gpaperd-bus.XXXXXX)"
  # shellcheck disable=SC2064 # expand the paths now; $? when the trap runs
  trap "cleanup '$sandbox' '$bus_dir' \$?" EXIT

  local name
  for name in home config data state cache runtime schemas; do
    mkdir -m 700 "$sandbox/$name"
  done
  glib-compile-schemas --strict --targetdir "$sandbox/schemas" "$SCHEMA_SRC"
  write_bus_config "$sandbox" "$bus_dir"

  # env -i: nothing of the caller's session leaks in, the bus address least.
  env -i \
    PATH="$PATH" \
    HOME="$sandbox/home" \
    XDG_CONFIG_HOME="$sandbox/config" \
    XDG_DATA_HOME="$sandbox/data" \
    XDG_STATE_HOME="$sandbox/state" \
    XDG_CACHE_HOME="$sandbox/cache" \
    XDG_RUNTIME_DIR="$sandbox/runtime" \
    XDG_DATA_DIRS="/usr/local/share:/usr/share" \
    GSETTINGS_BACKEND=keyfile \
    GSETTINGS_SCHEMA_DIR="$sandbox/schemas" \
    GIO_USE_VFS=local \
    GIO_USE_NETWORK_MONITOR=base \
    NO_COLOR=1 \
    SOAK_SANDBOX="$sandbox" \
    SOAK_BUS_DIR="$bus_dir" \
    SOAK_MEM_KB="$MEM_KB" \
    SOAK_RSS_MAX_KB="$RSS_MAX_KB" \
    GPAPERD="$GPAPERD" \
    GPAPERCTL="$GPAPERCTL" \
    dbus-run-session --config-file="$sandbox/bus.conf" -- \
    "${BASH_SOURCE[0]}" --inner "$DURATION"
}

# Keeps the sandbox, and the daemon's log in it, when the run failed.
cleanup() {
  local sandbox="$1" bus_dir="$2" code="$3"
  rm -rf -- "$bus_dir"
  if [[ "${SOAK_KEEP:-0}" == 1 || "$code" != 0 ]]; then
    log "sandbox kept at $sandbox"
  else
    rm -rf -- "$sandbox"
  fi
}

# A session bus that activates nothing: no <servicedir>, no standard dirs.
write_bus_config() {
  cat >"$1/bus.conf" <<EOF
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:dir=$2</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
EOF
}

# ---------------------------------------------------------------- inner part

inner() {
  local sandbox="${SOAK_SANDBOX:?inner part runs only from the outer part}"
  local bus_dir="${SOAK_BUS_DIR:?inner part runs only from the outer part}"
  # Refuse to run anywhere but in the sandbox the outer part made.
  [[ "$HOME" == "$sandbox/home" && "$GSETTINGS_BACKEND" == keyfile \
    && "$DBUS_SESSION_BUS_ADDRESS" == *"$bus_dir"* ]] \
    || die "not in the sandbox: refusing to run"

  readonly WALLS="$HOME/walls"
  readonly SLOTS="$XDG_DATA_HOME/gpaperd/slots"
  readonly STATE_FILE="$XDG_STATE_HOME/gpaperd/state.json"
  readonly CONFIG_FILE="$XDG_CONFIG_HOME/gpaperd/config.toml"
  readonly KEYFILE="$XDG_CONFIG_HOME/glib-2.0/settings/keyfile"
  readonly DAEMON_LOG="$sandbox/gpaperd.log"
  readonly RSS_LOG="$sandbox/rss.log"
  mkdir -p "$WALLS/light" "$WALLS/dark" "$WALLS/shared/deep" "$(dirname "$CONFIG_FILE")"

  for _ in 1 2 3 4 5 6; do add_image light; add_image dark; done
  add_image shared/deep
  write_config 0
  set_color_scheme default

  # Not on our stdout: a caller's pipe must not wait for the daemon.
  ( ulimit -v "$SOAK_MEM_KB"; exec "$GPAPERD" ) >/dev/null 2>"$DAEMON_LOG" &
  DAEMON_PID=$!
  # Any failure from here on must not leave the daemon running.
  trap stop_daemon EXIT
  wait_ready

  local start now config_writes=0 invalid_writes=0 slot_deletes=0 flips=0
  local adds=0 deletes=0 renames=0 ctl_calls=0 ctl_failures=0 scheme=default
  start=$(date +%s)
  while now=$(date +%s); (( now - start < 10#$DURATION )); do
    alive || die "daemon died after $((now - start)) s; see $DAEMON_LOG"
    sample_rss "$((now - start))"
    case $((RANDOM % 10)) in
      0|1) add_image "$(pick_dir)"; adds=$((adds + 1)) ;;
      2) delete_image "$(pick_dir)" && deletes=$((deletes + 1)) ;;
      3) rename_image "$(pick_dir)" && renames=$((renames + 1)) ;;
      4)
        config_writes=$((config_writes + 1))
        if (( config_writes % 7 == 0 )); then
          printf '[general]\ninterval = "1m"\norder = \n' >"$CONFIG_FILE.new"
          mv -f "$CONFIG_FILE.new" "$CONFIG_FILE"
          invalid_writes=$((invalid_writes + 1))
        else
          write_config "$config_writes"
        fi
        ;;
      5) rm -f "$SLOTS/$(pick_style)"; slot_deletes=$((slot_deletes + 1)) ;;
      6)
        if [[ $scheme == default ]]; then scheme=prefer-dark; else scheme=default; fi
        set_color_scheme "$scheme"; flips=$((flips + 1))
        ;;
      *)
        ctl_calls=$((ctl_calls + 1))
        ctl_random || ctl_failures=$((ctl_failures + 1))
        ;;
    esac
    sleep "$((1 + RANDOM % 4))"
  done

  # Let the last churn settle: a deleted slot is filled again, a broken
  # config replaced by a valid one is applied.
  write_config 0
  sleep 5
  alive || die "daemon died at the end; see $DAEMON_LOG"
  sample_rss "$DURATION"
  local status_json final_rss
  status_json="$("$GPAPERCTL" status --json)" || die "status failed at the end"
  jq -e . >/dev/null <<<"$status_json" || die "status JSON is not valid"
  final_rss="$(rss_kb)"

  kill -TERM "$DAEMON_PID"
  local code
  if ! code="$(wait_daemon)"; then
    kill -KILL "$DAEMON_PID" 2>/dev/null || true
    wait_daemon >/dev/null || true
    trap - EXIT
    die "daemon ignored SIGTERM for 20 s; see $DAEMON_LOG"
  fi
  trap - EXIT
  (( code == 0 )) || die "daemon exited $code on SIGTERM; see $DAEMON_LOG"

  check_slots
  jq -e . "$STATE_FILE" >/dev/null || die "state.json is not valid JSON"
  check_no_temp_files
  check_rss_growth "$final_rss"

  local errors expected_errors
  expected_errors=$(grep -c 'config file is invalid' "$DAEMON_LOG" || true)
  errors=$(grep -E ' (ERROR|WARN) ' "$DAEMON_LOG" | grep -vEc "$EXPECTED_WARNINGS" || true)
  if (( errors > 0 )); then
    # head can SIGPIPE sort; the failure is the die below, not that pipe.
    grep -E ' (ERROR|WARN) ' "$DAEMON_LOG" | grep -vE "$EXPECTED_WARNINGS" \
      | sed -E 's/^[^ ]+ +//' | sort | uniq -c | sort -rn | head -20 || true
    die "$errors unexpected WARN/ERROR lines"
  fi
  # One refusal per invalid write. A swallow or an extra refusal is not a pass.
  (( expected_errors == invalid_writes )) \
    || die "invalid configs refused $expected_errors, wrote $invalid_writes"
  cat <<EOF
soak passed: ${DURATION} s
  churn: images added $adds, deleted $deletes, renamed $renames; config writes $config_writes ($invalid_writes invalid); slot deletes $slot_deletes; color-scheme flips $flips; gpaperctl calls $ctl_calls ($ctl_failures failed)
  daemon: rotations $(count 'wallpaper changed'), catalog changes $(count 'image files changed'), reloads $(count 'config file changed: applying it'), invalid configs refused $expected_errors, slot repairs $(count 'slot file is gone'), style-switch refreshes $(count 'slot refreshed')
  rss: final ${final_rss} kB, peak $(sort -k2 -n "$RSS_LOG" | tail -1 | cut -d' ' -f2) kB
  other WARN/ERROR lines: $errors
EOF
}

# Warnings the churn causes on purpose.
readonly EXPECTED_WARNINGS='config file is invalid|slot file is gone'

alive() { kill -0 "$DAEMON_PID" 2>/dev/null; }

stop_daemon() {
  kill -TERM "$DAEMON_PID" 2>/dev/null || return 0
  if ! wait_daemon >/dev/null; then
    kill -KILL "$DAEMON_PID" 2>/dev/null || true
    wait_daemon >/dev/null || true
  fi
}

# State letter from /proc. Empty when the pid is already gone.
# The comm field can contain spaces, so the letter is the token after ") ".
proc_state() {
  local stat
  stat="$(cat "/proc/$DAEMON_PID/stat" 2>/dev/null)" || return 0
  stat="${stat##*) }"
  printf '%s\n' "${stat%% *}"
}

# Reaps DAEMON_PID and prints its exit code. Returns 1 if it is still
# running after the ready wait's 20s. A zombie still answers kill -0,
# so this reads the proc state and only then waits.
wait_daemon() {
  local tries state code
  for tries in $(seq 0 "$DAEMON_WAIT_TRIES"); do
    state="$(proc_state)"
    if [[ -z "$state" || "$state" == Z ]]; then
      code=0
      wait "$DAEMON_PID" 2>/dev/null || code=$?
      printf '%s\n' "$code"
      return 0
    fi
    if (( tries == DAEMON_WAIT_TRIES )); then
      return 1
    fi
    sleep "$DAEMON_WAIT_SLICE"
  done
}

count() { grep -c -- "$1" "$DAEMON_LOG" || true; }

wait_ready() {
  local tries
  for tries in $(seq 1 "$DAEMON_WAIT_TRIES"); do
    alive || die "daemon exited at start; see $DAEMON_LOG"
    if "$GPAPERCTL" status >/dev/null 2>&1; then
      log "daemon $DAEMON_PID ready after $tries tries; log at $DAEMON_LOG"
      return
    fi
    sleep "$DAEMON_WAIT_SLICE"
  done
  die "daemon not ready after 20 s"
}

pick_style() { if (( RANDOM % 2 )); then echo light; else echo dark; fi; }
pick_dir() { case $((RANDOM % 5)) in 0|1) echo light ;; 2|3) echo dark ;; *) echo shared/deep ;; esac; }

# A PNG ends with IEND: bytes after it make a new file of the same picture.
add_image() {
  local dir="$WALLS/$1" name
  name="img-$(date +%s%N)-$RANDOM.png"
  { cat "$TINY_PNG"; printf '%s' "$name"; } >"$dir/.$name.part"
  mv "$dir/.$name.part" "$dir/$name"
}

# Keeps at least one image in each directory.
random_image() {
  local images=("$WALLS/$1"/*.png)
  (( ${#images[@]} > 1 )) || return 1
  printf '%s\n' "${images[RANDOM % ${#images[@]}]}"
}

delete_image() { local image; image="$(random_image "$1")" || return 1; rm -f "$image"; }

rename_image() {
  local image
  image="$(random_image "$1")" || return 1
  mv "$image" "$(dirname "$image")/renamed-$(date +%s%N).png"
}

write_config() {
  local n="$1" order=random hidden=false link=true
  (( n % 2 )) && order=alphabetical
  (( n % 3 == 1 )) && hidden=true
  (( n % 5 == 2 )) && link=false
  cat >"$CONFIG_FILE.new" <<EOF
# soak config, write $n
[general]
interval = "1m"
order = "$order"
rotate_hidden = $hidden
link = $link
on_external_change = "reassert-now"

[light]
sources = ["~/walls/light", { dir = "~/walls/shared", recursive = true }]

[dark]
sources = ["~/walls/dark", { dir = "~/walls/shared", recursive = true }]
EOF
  mv -f "$CONFIG_FILE.new" "$CONFIG_FILE"
}

# Rewrites color-scheme in the keyfile backend in one rename, the way a
# settings tool commits, and keeps every other key as it is.
set_color_scheme() {
  mkdir -p "$(dirname "$KEYFILE")"
  touch "$KEYFILE"
  awk -v value="'$1'" '
    /^\[/ {
      in_group = ($0 == "[org/gnome/desktop/interface]")
      print
      if (in_group) { print "color-scheme=" value; seen = 1 }
      next
    }
    in_group && /^color-scheme=/ { next }
    { print }
    END { if (!seen) { print ""; print "[org/gnome/desktop/interface]"; print "color-scheme=" value } }
  ' "$KEYFILE" >"$KEYFILE.soak"
  mv -f "$KEYFILE.soak" "$KEYFILE"
}

ctl_random() {
  local style
  style="$(pick_style)"
  case $((RANDOM % 6)) in
    0) "$GPAPERCTL" next --style "$style" >/dev/null ;;
    1) ctl_previous "$style" ;;
    2) "$GPAPERCTL" pause --style "$style" >/dev/null \
         && sleep 1 && "$GPAPERCTL" resume --style "$style" >/dev/null ;;
    3) "$GPAPERCTL" reload >/dev/null ;;
    4) "$GPAPERCTL" status --json | jq -e . >/dev/null ;;
    *) "$GPAPERCTL" status >/dev/null ;;
  esac
}

# With too little history the daemon answers "no earlier image": no fault.
ctl_previous() {
  local reply
  reply="$("$GPAPERCTL" previous --style "$1" 2>&1)" && return
  [[ "$reply" == *"no earlier"* ]] || { log "previous: $reply"; return 1; }
}

rss_kb() { awk '/^VmRSS:/ { print $2 }' "/proc/$DAEMON_PID/status"; }

sample_rss() { printf '%s %s\n' "$1" "$(rss_kb)" >>"$RSS_LOG"; }

# RSS is bounded: under the limit, and the peak of the last quarter of the
# run is at most 1.5 times the peak of the first quarter plus 16 MiB.
check_rss_growth() {
  (( $1 <= RSS_MAX_KB )) || die "final RSS $1 kB is over $RSS_MAX_KB kB"
  local quarter=$((10#$DURATION / 4)) first last
  first=$(awk -v q="$quarter" '$1 <= q && $2 > m { m = $2 } END { print m + 0 }' "$RSS_LOG")
  last=$(awk -v q="$((10#$DURATION - quarter))" '$1 >= q && $2 > m { m = $2 } END { print m + 0 }' "$RSS_LOG")
  (( last <= first * 3 / 2 + 16384 )) \
    || die "RSS grows: first-quarter peak $first kB, last-quarter peak $last kB"
}

check_slots() {
  local style slot uri light_uri dark_uri
  for style in light dark; do
    slot="$SLOTS/$style"
    [[ -s "$slot" ]] || die "slot $style is missing or empty"
    [[ "$(head -c 8 "$slot" | od -An -tx1 | tr -d ' \n')" == 89504e470d0a1a0a ]] \
      || die "slot $style is not a PNG"
  done
  light_uri="$(file_uri "$SLOTS/light")" || die "slot light path is not absolute"
  dark_uri="$(file_uri "$SLOTS/dark")" || die "slot dark path is not absolute"
  for uri in "picture-uri='$light_uri'" "picture-uri-dark='$dark_uri'"; do
    grep -qF -- "$uri" "$KEYFILE" || die "keyfile lacks $uri"
  done
}

# Same bytes as the daemon's file_uri (url::Url::from_file_path, which uses
# Path::components). Empty and `.` segments are dropped; `..` stays. Each
# kept segment is percent-encoded. Special-path bytes are controls, DEL,
# non-ASCII, and space " # % < > ? \ ` { }.
file_uri() {
  FILE_URI_PATH="$1" LC_ALL=C awk '
    BEGIN {
      path = ENVIRON["FILE_URI_PATH"]
      if (substr(path, 1, 1) != "/") exit 1
      for (i = 1; i < 256; i++) ord[sprintf("%c", i)] = i
      uri = "file://"
      n = split(path, parts, "/")
      for (i = 2; i <= n; i++) {
        if (parts[i] == "" || parts[i] == ".") continue
        uri = uri "/" encode(parts[i])
      }
      if (uri == "file://") uri = "file:///"
      print uri
    }
    function encode(seg,    i, c, b, out, hex) {
      out = ""
      hex = "0123456789ABCDEF"
      for (i = 1; i <= length(seg); i++) {
        c = substr(seg, i, 1)
        b = ord[c]
        if (b < 32 || b >= 127 || b == 32 || b == 34 || b == 35 || b == 37 ||
            b == 60 || b == 62 || b == 63 || b == 92 || b == 96 || b == 123 ||
            b == 125) {
          out = out "%" substr(hex, int(b / 16) + 1, 1) substr(hex, (b % 16) + 1, 1)
        } else out = out c
      }
      return out
    }
  '
}

check_no_temp_files() {
  local left
  left="$(find "$XDG_DATA_HOME" "$XDG_STATE_HOME" "$XDG_CACHE_HOME" \
    \( -name '*.tmp' -o -name '*.tmp-*' \) -print)"
  [[ -z "$left" ]] || die "temp files left: $left"
}

if [[ "$MODE" == inner ]]; then inner; else outer; fi
