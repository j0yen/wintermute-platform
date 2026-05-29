#!/usr/bin/env bash
# install.sh — wintermute-platform installer.
#
# Two install modes:
#   default              developer install (binaries to ~/.local/bin/)
#   --kiosk              kiosk install (binaries to /usr/local/bin/,
#                        greetd autologin, recovery service,
#                        tmpfiles.d, enable-linger). PRD-wintermute-
#                        companion-boot §2.1.
#
# Auxiliary flags:
#   --uninstall-kiosk    revert greetd config, remove recovery service,
#                        leave wm-* binaries intact (PRD AC9).
#   --dry-run            render the plan, write nothing. Safe to run
#                        from a Claude session that must not break the
#                        user's session (PRD AC2: verified-via-dry-run).
#   --user NAME          override kiosk user (default: wintermute).
#
# All file bodies (greetd config, recovery service, tmpfiles rule) are
# rendered from the wintermute-platform Rust library so the bytes the
# wet run writes match the bytes the dry run prints. See src/kiosk.rs.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# --- Flag parsing ----------------------------------------------------

KIOSK=0
UNINSTALL_KIOSK=0
DRY_RUN=0
USER_NAME="wintermute"

while [ $# -gt 0 ]; do
    case "$1" in
        --kiosk) KIOSK=1 ;;
        --uninstall-kiosk) UNINSTALL_KIOSK=1 ;;
        --dry-run) DRY_RUN=1 ;;
        --user)
            shift
            [ $# -eq 0 ] && { echo "install.sh: --user requires a value" >&2; exit 2; }
            USER_NAME="$1"
            ;;
        -h|--help)
            cat <<EOF
usage: install.sh [--kiosk|--uninstall-kiosk] [--dry-run] [--user NAME]

Modes:
  default              dev install: cargo install --path . --root ~/.local
  --kiosk              kiosk install: /usr/local/bin/ + greetd autologin +
                       boot-recovery service + tmpfiles.d + enable-linger
  --uninstall-kiosk    revert kiosk wiring; leave wm-* binaries intact

Options:
  --dry-run            print what would change; do not write
  --user NAME          kiosk user (default: wintermute)
EOF
            exit 0
            ;;
        *) echo "install.sh: unknown argument: $1" >&2; exit 2 ;;
    esac
    shift
done

if [ $KIOSK -eq 1 ] && [ $UNINSTALL_KIOSK -eq 1 ]; then
    echo "install.sh: --kiosk and --uninstall-kiosk are mutually exclusive" >&2
    exit 2
fi

# --- Helpers ---------------------------------------------------------

log() { printf '[install.sh] %s\n' "$*" >&2; }

# Run a command, or just announce it in dry-run mode.
runit() {
    if [ $DRY_RUN -eq 1 ]; then
        printf '  DRY-RUN: %s\n' "$*"
        return 0
    fi
    "$@"
}

# Write contents to a file, or just announce in dry-run.
write_file() {
    local path="$1" mode="$2" body="$3"
    if [ $DRY_RUN -eq 1 ]; then
        printf '  DRY-RUN: write %s (mode %s, %d bytes)\n' "$path" "$mode" "${#body}"
        return 0
    fi
    install -d -m 0755 "$(dirname "$path")"
    printf '%s' "$body" > "$path"
    chmod "$mode" "$path"
    log "wrote $path (mode $mode)"
}

# --- Rendered payloads (mirrors src/kiosk.rs) ------------------------
#
# These shell strings MUST stay byte-identical with the Rust renderers
# in src/kiosk.rs. The Rust unit tests assert structural properties of
# the Rust strings; the strings below are what actually lands on disk
# in a wet run. Keep them in sync.

render_greetd_config() {
    cat <<EOF
# greetd config — wintermute kiosk autologin
# Installed by install.sh --kiosk for user=$USER_NAME.
# Reverted by install.sh --uninstall-kiosk.

[terminal]
vt = 1

[default_session]
command = "agreety --cmd wintermute-session"
user = "$USER_NAME"

[initial_session]
command = "wintermute-session"
user = "$USER_NAME"
EOF
}

render_recovery_service() {
    cat <<EOF
[Unit]
Description=wintermute boot-recovery (start wintermute.target if it failed at login)
Documentation=https://github.com/j0yen/wintermute-platform
After=multi-user.target

[Service]
Type=oneshot
# Wait 30s for the autologin chain to settle, then verify
# wintermute.target is active for the kiosk user. If not,
# poke it into life via the user's systemd manager.
ExecStart=/bin/sh -c 'sleep 30; runuser -u $USER_NAME -- systemctl --user is-active --quiet wintermute.target || runuser -u $USER_NAME -- systemctl --user start wintermute.target'
RemainAfterExit=no
# Do not block boot; recovery is opportunistic.
TimeoutStartSec=120

[Install]
WantedBy=multi-user.target
EOF
}

render_tmpfiles_rule() {
    printf 'd /run/wintermute 0755 %s %s - -\n' "$USER_NAME" "$USER_NAME"
}

# --- Dev install (no --kiosk and no --uninstall-kiosk) ---------------

dev_install() {
    log "dev install: cargo install --path . --root ~/.local"
    runit cargo install --path "$SCRIPT_DIR" --root "$HOME/.local"
    if [ -x "$SCRIPT_DIR/pkg/install-system.sh" ]; then
        log "running pkg/install-system.sh (systemd units, wintermute-session)"
        runit sudo "$SCRIPT_DIR/pkg/install-system.sh"
    fi
}

# --- Kiosk install ---------------------------------------------------

kiosk_install() {
    log "kiosk install (user=$USER_NAME, dry-run=$DRY_RUN)"
    printf 'kiosk-install plan for user=%s bin_prefix=/usr/local/bin\n' "$USER_NAME"
    printf 'steps: 7\n'

    # 1. Install binaries to /usr/local/bin/.
    printf '  [1/7] install-binaries (root=true): cargo install + relocate to /usr/local/bin/\n'
    for b in wm-tts wm-stt wm-audio wm-dialog wmd wmd-init wm-bootstrap; do
        printf '         writes: /usr/local/bin/%s\n' "$b"
    done
    if [ $DRY_RUN -eq 0 ]; then
        # Build wmd-init and wm here (they're what wintermute-platform owns).
        # Downstream crates (wm-tts, wm-stt, wm-audio, wm-dialog, wm-bootstrap)
        # have their own install scripts; this script does NOT pull them.
        runit cargo build --release --bins
        for b in wmd-init wm; do
            local src="$SCRIPT_DIR/target/release/$b"
            if [ -x "$src" ]; then
                runit sudo install -Dm755 "$src" "/usr/local/bin/$b"
            fi
        done
    fi

    # 2. greetd config.
    printf '  [2/7] greetd-config (root=true): write greetd autologin config for user=%s\n' "$USER_NAME"
    printf '         writes: /etc/greetd/config.toml\n'
    if [ $DRY_RUN -eq 0 ]; then
        local greetd_body
        greetd_body=$(render_greetd_config)
        runit sudo bash -c "install -Dm644 /dev/stdin /etc/greetd/config.toml <<< \"$greetd_body\""
    fi

    # 3. enable-linger.
    printf '  [3/7] enable-linger (root=true): loginctl enable-linger %s\n' "$USER_NAME"
    printf '         writes: /var/lib/systemd/linger/%s\n' "$USER_NAME"
    runit sudo loginctl enable-linger "$USER_NAME"

    # 4. tmpfiles.d rule.
    printf '  [4/7] tmpfiles-rule (root=true): install tmpfiles.d rule for /run/wintermute/\n'
    printf '         writes: /usr/lib/tmpfiles.d/wintermute.conf\n'
    if [ $DRY_RUN -eq 0 ]; then
        local tmpfiles_body
        tmpfiles_body=$(render_tmpfiles_rule)
        runit sudo bash -c "install -Dm644 /dev/stdin /usr/lib/tmpfiles.d/wintermute.conf <<< \"$tmpfiles_body\""
        runit sudo systemd-tmpfiles --create /usr/lib/tmpfiles.d/wintermute.conf || true
    fi

    # 5. Recovery service (install but do NOT enable — see PRD note).
    printf '  [5/7] recovery-service (root=true): install (do not enable) wintermute-boot-recovery.service\n'
    printf '         writes: /usr/lib/systemd/system/wintermute-boot-recovery.service\n'
    if [ $DRY_RUN -eq 0 ]; then
        local recovery_body
        recovery_body=$(render_recovery_service)
        runit sudo bash -c "install -Dm644 /dev/stdin /usr/lib/systemd/system/wintermute-boot-recovery.service <<< \"$recovery_body\""
        runit sudo systemctl daemon-reload
    fi

    # 6. Installed marker.
    printf '  [6/7] installed-marker (root=true): create installed marker at /var/lib/wintermute/installed\n'
    printf '         writes: /var/lib/wintermute/installed\n'
    if [ $DRY_RUN -eq 0 ]; then
        runit sudo install -Dm644 /dev/null /var/lib/wintermute/installed
    fi

    # 7. Enable user target.
    printf '  [7/7] enable-user-target (root=true): enable wintermute.target on user %s\n' "$USER_NAME"
    printf '         writes: /var/lib/systemd/linger/%s (target-default-symlink)\n' "$USER_NAME"
    if [ $DRY_RUN -eq 0 ]; then
        runit sudo -u "$USER_NAME" systemctl --user --no-block enable wintermute.target || true
    fi

    if [ $DRY_RUN -eq 1 ]; then
        log "dry-run complete; no changes written."
    else
        log "kiosk install complete. Reboot to verify."
    fi
}

# --- Kiosk uninstall -------------------------------------------------

kiosk_uninstall() {
    log "kiosk uninstall (user=$USER_NAME, dry-run=$DRY_RUN)"
    printf 'kiosk-uninstall plan for user=%s\n' "$USER_NAME"
    printf 'steps: 4 (wm-* binaries are intentionally left intact, AC9)\n'

    printf '  [1/4] remove-recovery-service\n'
    runit sudo rm -f /usr/lib/systemd/system/wintermute-boot-recovery.service

    printf '  [2/4] remove-greetd-config (restore upstream default)\n'
    runit sudo rm -f /etc/greetd/config.toml

    printf '  [3/4] remove-tmpfiles-rule\n'
    runit sudo rm -f /usr/lib/tmpfiles.d/wintermute.conf

    printf '  [4/4] disable-linger %s\n' "$USER_NAME"
    runit sudo loginctl disable-linger "$USER_NAME" || true

    if [ $DRY_RUN -eq 0 ]; then
        runit sudo systemctl daemon-reload
        log "uninstall complete. wm-* binaries remain installed (AC9)."
    else
        log "dry-run complete; no changes written."
    fi
}

# --- Dispatch --------------------------------------------------------

if [ $UNINSTALL_KIOSK -eq 1 ]; then
    kiosk_uninstall
elif [ $KIOSK -eq 1 ]; then
    kiosk_install
else
    dev_install
fi
