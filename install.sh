#!/bin/bash
# Installs rain-kms as a kiosk on the local display (tty1).
# Build first with `cargo build --release`, then run: sudo ./install.sh
set -euo pipefail
[ "$EUID" -eq 0 ] || { echo "Run with sudo"; exit 1; }
HERE="$(cd "$(dirname "$0")" && pwd)"
[ -x "$HERE/target/release/rain-kms" ] || { echo "Build first: cargo build --release"; exit 1; }

id kiosk >/dev/null 2>&1 || useradd --system --no-create-home --home-dir /nonexistent --shell /usr/bin/nologin --groups video kiosk
install -m 755 "$HERE/target/release/rain-kms" /usr/local/bin/rain-kms
install -m 644 "$HERE/rain-kms.service" /etc/systemd/system/rain-kms.service
systemctl daemon-reload
systemctl disable --now getty@tty1.service 2>/dev/null || true
systemctl enable --now rain-kms.service
sleep 3
systemctl --no-pager --lines=5 status rain-kms.service || true
echo
echo "To remove it: sudo systemctl disable --now rain-kms && sudo systemctl enable --now getty@tty1"
