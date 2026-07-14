SHELL := /bin/bash

CARGO ?= cargo
PYTHON ?= python3
COVERAGE_MIN ?= 80
BINARY := latte-lens
E2E_ARTIFACT_DIR ?= target/e2e-artifacts

.DEFAULT_GOAL := help

.PHONY: help setup fmt fmt-check check lint test installer-check script-test e2e-self-test e2e-files e2e-git e2e-search e2e coverage coverage-html bench ci build release package package-smoke install clean

help: ## Show available commands
	@awk 'BEGIN {FS = ":.*## "; printf "Latte Lens engineering commands:\n\n"} /^[a-zA-Z0-9_-]+:.*## / {printf "  %-15s %s\n", $$1, $$2}' $(MAKEFILE_LIST)

setup: ## Install Rust components and the local coverage command
	rustup component add rustfmt clippy llvm-tools-preview
	cargo install cargo-llvm-cov --locked

fmt: ## Format Rust sources
	$(CARGO) fmt --all

fmt-check: ## Check Rust formatting without changing files
	$(CARGO) fmt --all --check

check: ## Type-check every target
	$(CARGO) check --all-targets --all-features --locked

lint: ## Run Clippy with warnings denied
	$(CARGO) clippy --all-targets --all-features --locked -- -D warnings

test: ## Run unit and integration tests
	$(CARGO) test --all-targets --all-features --locked

installer-check: ## Check the POSIX installer syntax
	sh -n install.sh

script-test: installer-check ## Run installer and release automation tests
	$(PYTHON) -m unittest scripts/test_install.py scripts/test_install_windows.py scripts/test_generate_release_notes.py scripts/test_verify_release_package.py

e2e-self-test: ## Verify the PTY parser, sandbox, evidence, and cleanup harness
	$(PYTHON) scripts/e2e_tui.py --self-test

e2e-files: e2e-self-test ## Exercise Files navigation and refresh through the production TUI
	$(CARGO) build --locked
	$(PYTHON) scripts/e2e_tui.py target/debug/$(BINARY) --scenario files --artifact-dir $(E2E_ARTIFACT_DIR)

e2e-git: e2e-self-test ## Exercise Git Changes and diff journeys through the production TUI
	$(CARGO) build --locked
	$(PYTHON) scripts/e2e_tui.py target/debug/$(BINARY) --scenario git-changes --artifact-dir $(E2E_ARTIFACT_DIR)

e2e-search: e2e-self-test ## Exercise file/text search and Preview find through the production TUI
	$(CARGO) build --locked
	$(PYTHON) scripts/e2e_tui.py target/debug/$(BINARY) --scenario search-preview --artifact-dir $(E2E_ARTIFACT_DIR)

e2e: e2e-self-test ## Build and run every production TUI scenario
	$(CARGO) build --locked
	$(PYTHON) scripts/e2e_tui.py target/debug/$(BINARY) --scenario all --artifact-dir $(E2E_ARTIFACT_DIR)

coverage: ## Enforce the line coverage threshold (default: 80%)
	@command -v cargo-llvm-cov >/dev/null 2>&1 || { echo "cargo-llvm-cov is missing; run 'make setup'"; exit 1; }
	$(CARGO) llvm-cov --workspace --all-targets --all-features --locked --fail-under-lines $(COVERAGE_MIN)

coverage-html: ## Generate an HTML coverage report
	@command -v cargo-llvm-cov >/dev/null 2>&1 || { echo "cargo-llvm-cov is missing; run 'make setup'"; exit 1; }
	$(CARGO) llvm-cov --workspace --all-targets --all-features --locked --html
	@echo "Coverage report: target/llvm-cov/html/index.html"

bench: ## Run performance benchmarks
	$(CARGO) bench --locked

ci: fmt-check check lint test script-test e2e ## Run the same quality gate used by CI

build: ## Build a debug binary
	$(CARGO) build --locked

release: ## Build an optimized binary
	$(CARGO) build --release --locked

package: ## Build a release archive and SHA-256 checksum
	scripts/build-release.sh

package-smoke: package ## Build and verify the release archive contents and checksum
	$(PYTHON) scripts/verify-release-package.py "dist/*.tar.gz" --binary $(BINARY)

install: ## Install latte-lens from this checkout
	$(CARGO) install --path . --locked --force

clean: ## Remove generated build and package artifacts
	$(CARGO) clean
	rm -rf dist
