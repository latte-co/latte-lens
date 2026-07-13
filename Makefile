SHELL := /bin/bash

CARGO ?= cargo
PYTHON ?= python3
COVERAGE_MIN ?= 80
BINARY := latte-lens

.DEFAULT_GOAL := help

.PHONY: help setup fmt fmt-check check lint test script-test e2e coverage coverage-html bench ci build release package package-smoke install clean

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
	$(CARGO) check --all-targets --locked

lint: ## Run Clippy with warnings denied
	$(CARGO) clippy --all-targets --locked -- -D warnings

test: ## Run unit and integration tests
	$(CARGO) test --all-targets --locked

script-test: ## Run release automation tests
	$(PYTHON) -m unittest scripts/test_generate_release_notes.py scripts/test_verify_release_package.py

e2e: ## Build and exercise the real TUI through a pseudo-terminal
	$(CARGO) build --locked
	$(PYTHON) scripts/e2e_tui.py target/debug/$(BINARY)

coverage: ## Enforce the line coverage threshold (default: 80%)
	@command -v cargo-llvm-cov >/dev/null 2>&1 || { echo "cargo-llvm-cov is missing; run 'make setup'"; exit 1; }
	$(CARGO) llvm-cov --workspace --all-targets --locked --fail-under-lines $(COVERAGE_MIN)

coverage-html: ## Generate an HTML coverage report
	@command -v cargo-llvm-cov >/dev/null 2>&1 || { echo "cargo-llvm-cov is missing; run 'make setup'"; exit 1; }
	$(CARGO) llvm-cov --workspace --all-targets --locked --html
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
