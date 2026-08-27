#!/usr/bin/env bash
# Build, install the binary, and enable the user service.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build --release
sudo install -m755 target/release/tea /usr/local/bin/tea

install -Dm644 dist/tea.service ~/.config/systemd/user/tea.service
systemctl --user daemon-reload
systemctl --user enable --now tea.service

echo
echo "installed. logs:   journalctl --user -u tea -f"
echo "            stop:   systemctl --user stop tea"
