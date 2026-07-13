#!/bin/bash
# Register llm-provider-router as a blue/green hot-deploy systemd user service.
# Usage: bin/install-service.sh
set -euo pipefail

PROJECT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
USER_UNIT_DIR="$HOME/.config/systemd/user"

mkdir -p "$USER_UNIT_DIR"
cp "$PROJECT_DIR/systemd/llm-provider-router.service" "$USER_UNIT_DIR/llm-provider-router.service"
cp "$PROJECT_DIR/systemd/llm-provider-router-backend@.service" "$USER_UNIT_DIR/llm-provider-router-backend@.service"

echo "Installed:"
echo "  $USER_UNIT_DIR/llm-provider-router.service"
echo "  $USER_UNIT_DIR/llm-provider-router-backend@.service"

cd "$PROJECT_DIR"
uv sync --quiet

systemctl --user daemon-reload
"$PROJECT_DIR/bin/hot-deploy-router.py" bootstrap --slot blue --stop-other

echo "Service enabled and started."
echo "  Proxy:   systemctl --user status llm-provider-router"
echo "  Backend: systemctl --user status llm-provider-router-backend@blue"
echo "  Deploy:  $PROJECT_DIR/bin/hot-deploy-router.py deploy"
echo "  Status:  $PROJECT_DIR/bin/hot-deploy-router.py status"
