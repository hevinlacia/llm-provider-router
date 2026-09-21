#!/usr/bin/env bash
# 供应商 key vault：明文 api-keys.json（本机）↔ SOPS 加密副本 api-keys.sops.json（git）。
# 用法:
#   bin/vault.sh encrypt   # dashboard 改 key 后，重新生成加密副本（git add & commit）
#   bin/vault.sh decrypt   # 新机器恢复：解密副本回明文（需 ~/.config/sops/age/keys.txt）
#   bin/vault.sh status    # 校验加密副本是否存在/可解密（不回显内容）
set -euo pipefail
cd "$(dirname "$0")/.."

AGE_KEY_FILE="${SOPS_AGE_KEY_FILE:-$HOME/.config/sops/age/keys.txt}"
PLAIN="config/api-keys.json"
ENC="config/api-keys.sops.json"

case "${1:-}" in
  encrypt)
    [[ -f "$PLAIN" ]] || { echo "missing $PLAIN"; exit 1; }
    cp "$PLAIN" config/api-keys.sops.json
    SOPS_AGE_KEY_FILE="$AGE_KEY_FILE" sops --encrypt --config config/.sops.yaml --in-place config/api-keys.sops.json
    echo "encrypted config/api-keys.sops.json — git add & commit"
    ;;
  decrypt)
    [[ -f "config/api-keys.sops.json" ]] || { echo "missing config/api-keys.sops.json"; exit 1; }
    SOPS_AGE_KEY_FILE="$AGE_KEY_FILE" sops --decrypt --config config/.sops.yaml config/api-keys.sops.json > "$PLAIN"
    echo "decrypted config/api-keys.json — restart backend or POST /api/config/reload-env to apply"
    ;;
  status)
    [[ -f "$PLAIN" ]] && echo "plaintext: present" || echo "plaintext: MISSING"
    [[ -f "config/api-keys.sops.json" ]] && echo "encrypted copy: present" || echo "encrypted copy: MISSING"
    if [[ -f "config/api-keys.sops.json" ]]; then
      if SOPS_AGE_KEY_FILE="$AGE_KEY_FILE" sops --decrypt --config config/.sops.yaml config/api-keys.sops.json >/dev/null 2>&1; then
        echo "decrypt check: OK"
      else
        echo "decrypt check: FAILED (age key missing or ciphertext corrupt)"
        exit 1
      fi
    fi
    ;;
  *)
    echo "usage: bin/vault.sh {encrypt|decrypt|status}" >&2
    exit 1
    ;;
esac
