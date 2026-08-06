#!/usr/bin/env bash
# waybar exec script for the whisp-arch custom/whisper module.
# Emits a single line of waybar JSON: the daemon's status file if present,
# otherwise a "daemon not running" fallback. Always exits 0 so waybar keeps
# rendering the module.
set -u

status_file="${XDG_RUNTIME_DIR:-/tmp}/whisp-arch/status.json"

if [ -s "$status_file" ]; then
    # -s: exists and non-empty. The daemon writes it atomically (temp+rename),
    # so a concurrent read always sees a complete line.
    cat "$status_file"
else
    printf '%s\n' '{"state":"down","text":"󰃐","class":"missing","tooltip":"whisp: daemon not running"}'
fi
