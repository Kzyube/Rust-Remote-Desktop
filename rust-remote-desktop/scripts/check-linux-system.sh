#!/bin/sh
# Read-only checks for the portable Linux test bundle.
set -eu

bundle_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
architecture=$(uname -m)
session_type=${XDG_SESSION_TYPE:-unknown}

printf 'Architecture: %s\n' "$architecture"
if [ "$architecture" != x86_64 ]; then
    printf '%s\n' 'This bundle requires x86_64 Linux. Build from source for another architecture.' >&2
    exit 1
fi

printf 'Desktop session: %s\n' "$session_type"
case "$session_type" in
    wayland)
        printf '%s\n' 'Wayland: try the viewer. Sharing this Wayland desktop is not implemented.'
        ;;
    x11)
        printf '%s\n' 'X11: viewer and host adapters are implemented; the host also requires XTEST.'
        ;;
    *)
        printf '%s\n' 'Run the app inside a graphical desktop. Check XDG_SESSION_TYPE on that desktop.'
        ;;
esac

if command -v getconf >/dev/null 2>&1; then
    getconf GNU_LIBC_VERSION || true
fi

if command -v ldd >/dev/null 2>&1; then
    printf '\n%s\n' 'Startup-linked libraries:'
    ldd "$bundle_dir/rust-remote-desktop"
fi

printf '\n%s\n' 'Graphics drivers and runtime-loaded GUI libraries are checked only when the window opens.'
printf '%s\n' 'No settings were changed. To launch: ./rust-remote-desktop'
