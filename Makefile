# Codebridge build tooling.
# The vendored libghostty-vt build requires Zig 0.15.2 (see build.rs).

CB_INSTALL_PATH := $(shell command -v cb 2>/dev/null)

.PHONY: help build release web fmt clippy test check install run daemon clean

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}'

build: ## Debug build
	cargo build

release: web ## Release build (rebuilds the embedded PWA first)
	cargo build --release

web: ## Build the phone PWA embedded into the binary
	cd web && npm ci && npm run build

fmt: ## Check formatting
	cargo fmt --all -- --check

clippy: ## Lint with warnings denied
	cargo clippy --all-targets --all-features -- -D warnings

test: ## Run the test suite
	cargo test --all-targets

check: fmt clippy test ## Run the full CI gate (fmt + clippy + test)

install: release ## Build release and install over $(which cb) (rm-before-cp; macOS inode-safe)
	@if [ -z "$(CB_INSTALL_PATH)" ]; then \
		echo "error: 'cb' not found on PATH; nothing to overwrite"; exit 1; \
	fi
	rm -f "$(CB_INSTALL_PATH)"
	cp target/release/cb "$(CB_INSTALL_PATH)"
	@echo "installed $$($(CB_INSTALL_PATH) --version) -> $(CB_INSTALL_PATH)"

run: build ## Build and launch the TUI client
	./target/debug/cb

daemon: build ## Build and start the broker/conductor daemon
	./target/debug/cb daemon

clean: ## Remove build artifacts
	cargo clean
