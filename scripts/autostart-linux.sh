#!/usr/bin/env sh
set -eu

UNIT_DIR="${HOME}/.config/systemd/user"
UNIT="${UNIT_DIR}/local-focus.service"

mkdir -p "$UNIT_DIR"
cat > "$UNIT" <<EOF
[Unit]
Description=Local Focus activity tracker

[Service]
ExecStart=${HOME}/.cargo/bin/local-focus serve
Restart=always
RestartSec=5

[Install]
WantedBy=default.target
EOF

systemctl --user daemon-reload
systemctl --user enable --now local-focus.service
echo "Local Focus will start at login. Dashboard: http://127.0.0.1:4799"
