.PHONY: install check
install:
	cargo install --path . --root $(HOME)/.local --force
check:
	cargo fmt --all --check
	cargo clippy --all-targets --all-features -- -D warnings
	cargo test
