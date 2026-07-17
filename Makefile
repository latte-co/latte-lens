SHELL := /bin/bash

CARGO ?= cargo
PYTHON ?= python3
TRAEX_BIN ?=
UT_COVERAGE_MIN ?= 93
E2E_COVERAGE_MIN ?= 85
AGENT_COVERAGE_MIN ?= 80
UT_COVERAGE_IGNORE_REGEX ?= (/agent/|/bin/agent_observability_harness\.rs$$|/(app|content_safety|git|main|repo_graph|runtime|tree|ui)\.rs$$)
E2E_COVERAGE_IGNORE_REGEX ?= (/agent/|/bin/agent_observability_harness\.rs$$|/(clipboard|content_safety|diff|git|preview|repo_graph|runtime|search|text_layout|tree)\.rs$$)
AGENT_COVERAGE_IGNORE_REGEX ?= /src/(app|clipboard|content_safety|diff|git|main|preview|repo_graph|runtime|search|text_layout|tree|ui)\.rs$$|/src/bin/
E2E_COVERAGE_TARGET_DIR ?= target/llvm-cov-e2e
BINARY := latte-lens
E2E_ARTIFACT_DIR ?= target/e2e-artifacts

.DEFAULT_GOAL := help

.PHONY: help setup fmt fmt-check check lint test installer-check script-test e2e-self-test e2e-files e2e-git e2e-search e2e agent-ut agent-contract agent-harness-self-test agent-e2e-hook codex-hooks-canary claude-hooks-canary opencode-plugin-canary traex-hooks-canary agent-e2e agent-e2e-tui agent-package-negative agent-ci coverage coverage-unit coverage-e2e coverage-agent coverage-html bench ci build release package package-smoke install clean

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

agent-ut: ## Run Agent module unit tests and compile-fail doctests
	$(CARGO) test --lib --all-features --locked agent::
	$(CARGO) test --doc --all-features --locked

agent-contract: ## Run synthetic Agent contract, state, metadata, and transport suites
	$(CARGO) test --all-features --locked --test agent_observability_contract
	$(CARGO) test --all-features --locked --test agent_state_integration
	$(CARGO) test --all-features --locked --test agent_metadata_integration
	$(CARGO) test --all-features --locked --test agent_transport_contract

agent-harness-self-test: e2e-self-test ## Verify the shared PTY sandbox, recorder, watchdog, and cleanup oracle

agent-e2e-hook: ## Exercise synthetic transport and production Hook CLI contracts
	$(CARGO) test --all-features --locked --test agent_transport_contract
	$(CARGO) test --all-features --locked --test cli_e2e hook_cli_

codex-hooks-canary: ## Validate SessionStart with an installed Codex CLI in an isolated HOME
	$(CARGO) test --all-features --locked --test codex_hooks_compatibility -- --ignored --exact installed_codex_session_start_invokes_the_production_latte_lens_hook --nocapture

claude-hooks-canary: ## Validate SessionStart with an installed Claude CLI in an isolated HOME
	$(CARGO) test --all-features --locked --test claude_hooks_compatibility -- --ignored --exact installed_claude_session_start_invokes_the_production_latte_lens_hook --nocapture

opencode-plugin-canary: ## Validate session.created with an installed OpenCode CLI and isolated plugin
	$(CARGO) test --all-features --locked --test opencode_plugin_compatibility -- --ignored --exact installed_opencode_loads_the_production_latte_lens_plugin --nocapture

traex-hooks-canary: ## Validate SessionStart with an installed TraeX and isolated hook config
	@test -n "$(TRAEX_BIN)" || { echo "TRAEX_BIN must point to the TraeX executable"; exit 2; }
	LATTE_LENS_TRAEX_BIN="$(TRAEX_BIN)" $(CARGO) test --all-features --locked --test traex_hooks_compatibility -- --ignored --exact installed_traex_session_start_invokes_the_production_latte_lens_hook --nocapture

agent-e2e: ## Run all-platform headless Agent runtime/App scenarios
	$(CARGO) test --all-features --locked --test agent_runtime_e2e

agent-e2e-tui: ## Run the synthetic Agent vertical slice through a real POSIX PTY
	$(CARGO) build --locked --features agent-observability-harness --bin latte-lens-agent-harness
	$(PYTHON) scripts/agent_e2e_tui.py target/debug/latte-lens-agent-harness --artifact-dir $(E2E_ARTIFACT_DIR)/agent

agent-package-negative: ## Prove the default build does not expose the synthetic Agent harness
	@tmp=$$(mktemp -d); trap 'rm -rf "$$tmp"' EXIT; \
		CARGO_TARGET_DIR="$$tmp" $(CARGO) build --locked; \
		test -x "$$tmp/debug/$(BINARY)"; \
		test ! -e "$$tmp/debug/latte-lens-agent-harness"; \
		"$$tmp/debug/$(BINARY)" --help | grep -q "Repository or directory to inspect"; \
		"$$tmp/debug/$(BINARY)" --help | grep -q "hook"; \
		! "$$tmp/debug/$(BINARY)" --help | grep -qi "synthetic\|harness"

agent-ci: agent-ut agent-contract agent-harness-self-test agent-e2e-hook agent-e2e agent-e2e-tui agent-package-negative ## Run all synthetic Agent observability gates

coverage: coverage-unit coverage-e2e coverage-agent ## Enforce UT, production PTY E2E, and Agent Core coverage floors

coverage-unit: ## Enforce 93% line coverage for the direct unit-test responsibility surface
	@command -v cargo-llvm-cov >/dev/null 2>&1 || { echo "cargo-llvm-cov is missing; run 'make setup'"; exit 1; }
	$(CARGO) llvm-cov clean --workspace
	$(CARGO) llvm-cov --workspace --all-features --lib --bins --locked \
		--ignore-filename-regex '$(UT_COVERAGE_IGNORE_REGEX)' \
		--fail-under-lines $(UT_COVERAGE_MIN)

coverage-e2e: e2e-self-test ## Enforce 85% line coverage for the production PTY interaction surface
	@command -v cargo-llvm-cov >/dev/null 2>&1 || { echo "cargo-llvm-cov is missing; run 'make setup'"; exit 1; }
	CARGO='$(CARGO)' PYTHON='$(PYTHON)' BINARY='$(BINARY)' \
		E2E_COVERAGE_MIN='$(E2E_COVERAGE_MIN)' \
		E2E_COVERAGE_IGNORE_REGEX='$(E2E_COVERAGE_IGNORE_REGEX)' \
		E2E_COVERAGE_TARGET_DIR='$(E2E_COVERAGE_TARGET_DIR)' \
		scripts/coverage-e2e.sh

coverage-agent: ## Enforce 80% line coverage across the complete synthetic Agent Core
	@command -v cargo-llvm-cov >/dev/null 2>&1 || { echo "cargo-llvm-cov is missing; run 'make setup'"; exit 1; }
	$(CARGO) llvm-cov clean --workspace
	$(CARGO) llvm-cov --workspace --all-targets --all-features --locked \
		--ignore-filename-regex '$(AGENT_COVERAGE_IGNORE_REGEX)' \
		--fail-under-lines $(AGENT_COVERAGE_MIN)

coverage-html: ## Generate an HTML coverage report
	@command -v cargo-llvm-cov >/dev/null 2>&1 || { echo "cargo-llvm-cov is missing; run 'make setup'"; exit 1; }
	$(CARGO) llvm-cov --workspace --all-targets --all-features --locked --html
	@echo "Coverage report: target/llvm-cov/html/index.html"

bench: ## Run performance benchmarks
	$(CARGO) bench --locked

ci: fmt-check check lint test script-test e2e agent-e2e-tui agent-package-negative ## Run the same quality gate used by CI

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
