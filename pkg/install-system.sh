#!/usr/bin/env bash
# install-system.sh — copy the wintermute-platform hook artifacts into
# their production paths. Idempotent; safe to re-run.
#
# Run as root (or wrap individual commands in sudo). The script does
# NOT enable greetd or start wintermute.target — that is the caregiver's
# final step, performed by wm-bootstrap on first run.
#
# Layout written:
#   /usr/local/bin/wintermute-session                (700 root:root)
#   /usr/lib/systemd/user/wintermute.target          (644 root:root)
#   /usr/lib/systemd/user/wmd-init.service           (644 root:root)
#   /etc/greetd/config.toml.example                  (644 root:root)
#
# The greetd config is shipped as `.example` so an existing install is
# never overwritten silently. Merge into /etc/greetd/config.toml by hand.

set -euo pipefail

if [ "${EUID:-$(id -u)}" -ne 0 ]; then
    echo "install-system.sh: must be run as root" >&2
    exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

install -Dm755 "$SCRIPT_DIR/bin/wintermute-session" \
    /usr/local/bin/wintermute-session

install -Dm644 "$SCRIPT_DIR/systemd/user/wintermute.target" \
    /usr/lib/systemd/user/wintermute.target

install -Dm644 "$SCRIPT_DIR/systemd/user/wmd-init.service" \
    /usr/lib/systemd/user/wmd-init.service

install -Dm644 "$SCRIPT_DIR/greetd/config.toml.example" \
    /etc/greetd/config.toml.example

echo "install-system.sh: artifacts installed."
echo "  next: copy /etc/greetd/config.toml.example to /etc/greetd/config.toml"
echo "        and run 'systemctl daemon-reload' for both --system and --user."
