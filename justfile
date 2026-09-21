fmt:
    cargo fmt --all

lint:
    cargo clippy --all-targets --all-features -- -D warnings

test:
    cargo test --all-targets --all-features

build:
    cargo build --all-targets --all-features

check: fmt lint test build

frontend-install:
    npm --prefix frontend install

frontend-build:
    npm --prefix frontend run build

# —— 供应商 key vault（SOPS age 加密）——
# 明文 config/api-keys.json 仅本机（gitignored）；加密副本 api-keys.sops.json 提交 git。
# dashboard 改 key 后跑 just vault-encrypt 同步加密副本。
# 跨机恢复：just vault-decrypt （需 ~/.config/sops/age/keys.txt 私钥）。
vault-encrypt:
    cp config/api-keys.json config/api-keys.sops.json
    SOPS_AGE_KEY_FILE={{age_key_file}} sops --encrypt --config config/.sops.yaml --in-place config/api-keys.sops.json
    @echo "encrypted config/api-keys.sops.json — git add & commit"

vault-decrypt:
    SOPS_AGE_KEY_FILE={{age_key_file}} sops --decrypt --config config/.sops.yaml config/api-keys.sops.json > config/api-keys.json
    @echo "decrypted config/api-keys.json（重启后端或调 /api/config/reload-env 后生效）"

age_key_file := "~/.config/sops/age/keys.txt"

frontend-dev:
    npm --prefix frontend run dev
