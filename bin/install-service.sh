#!/bin/bash
# Register llm-provider-router as a systemd user service.
# Usage: bin/install-service.sh
set -euo pipefail

PROJECT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SERVICE_NAME="llm-provider-router"
SERVICE_FILE="$HOME/.config/systemd/user/${SERVICE_NAME}.service"
UV_BIN="$(command -v uv)"

cat > "$SERVICE_FILE" <<EOF
[Unit]
Description=LLM provider router OpenAI-compatible proxy
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
WorkingDirectory=${PROJECT_DIR}
EnvironmentFile=$HOME/.config/opencode/opencode-secrets.env
EnvironmentFile=$HOME/.config/opencode/opencode-internal.env
EnvironmentFile=$HOME/.config/opencode/opencode-config.env
Environment=LLM_PROVIDER_ROUTER_HOST=127.0.0.1
Environment=LLM_PROVIDER_ROUTER_PORT=8789
Environment=SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
Environment=SSL_CERT_DIR=/etc/ssl/certs
ExecStart=${UV_BIN} run llm-provider-router
Restart=on-failure
RestartSec=3
TimeoutStartSec=60

[Install]
WantedBy=default.target
EOF

echo "Installed: $SERVICE_FILE"

# Ensure uv sync is done so ExecStart works on first start
cd "$PROJECT_DIR"
uv sync --quiet

systemctl --user daemon-reload
systemctl --user enable --now "$SERVICE_NAME"

echo "Service enabled and started."
echo "  Status:  systemctl --user status $SERVICE_NAME"
echo "  Logs:    journalctl --user -u $SERVICE_NAME -f"
echo "  Stop:    systemctl --user stop $SERVICE_NAME"
echo "  Restart: systemctl --user restart $SERVICE_NAME"
