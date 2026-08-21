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

frontend-dev:
    npm --prefix frontend run dev
