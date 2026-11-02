# Lastwrite: CLI (TUI, report, daemon) plus the optional desktop app.
# Run from the repository root:  make help

.DEFAULT_GOAL := help
.PHONY: help build gui test lint check ci app dmg sha256 install-app run-app quit-app clean

APP_NAME   := Lastwrite
APP_BUNDLE := target/macos/$(APP_NAME).app
APP_DIR    := /Applications/$(APP_NAME).app
DIST       := target/dist
VERSION    := $(shell awk -F'"' '/^version = /{print $$2; exit}' Cargo.toml)

help: ## Show this help
	@awk 'BEGIN {FS = ":.*## "} /^[a-zA-Z0-9_-]+:.*## / {printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}' $(MAKEFILE_LIST)

build: ## Build the release CLI (TUI, report, daemon)
	cargo build --release --locked
	@echo "built: target/release/lastwrite"

gui: ## Build both release binaries, including the desktop app
	cargo build --release --locked --features gui
	@echo "built: target/release/lastwrite-desktop"

test: ## Run every test, with and without the desktop feature
	cargo test --locked
	cargo test --features gui --locked

lint: ## Lint every target, with and without the desktop feature
	cargo clippy --all-targets --locked -- -D warnings
	cargo clippy --all-targets --features gui --locked -- -D warnings

check: lint test ## What to run before committing

ci: check ## Exactly what .github/workflows/ci.yml runs

app: ## Build Lastwrite.app for macOS (ad-hoc signed)
	contrib/macos-app.sh target/macos
	@echo "built: $(APP_BUNDLE)"

dmg: app ## Build a .dmg and SHA256SUMS (macOS)
	@mkdir -p $(DIST)
	hdiutil create -volname "$(APP_NAME)" -srcfolder "$(APP_BUNDLE)" \
		-ov -format UDZO "$(DIST)/$(APP_NAME)-$(VERSION).dmg"
	$(MAKE) sha256

sha256: ## Write SHA256SUMS next to the built .dmg
	@cd $(DIST) && shasum -a 256 *.dmg > SHA256SUMS && cat SHA256SUMS

install-app: app quit-app ## Rebuild, replace /Applications/Lastwrite.app and launch it
	rm -rf "$(APP_DIR)"
	cp -R "$(APP_BUNDLE)" /Applications/
	open "$(APP_DIR)"

run-app: ## Launch the installed app
	open "$(APP_DIR)"

quit-app: ## Quit a running Lastwrite
	@osascript -e 'tell application "$(APP_NAME)" to quit' >/dev/null 2>&1 || true

clean: ## Remove build output
	cargo clean
