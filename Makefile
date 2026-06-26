# Makefile for kakehashi

# Variables
CARGO = cargo
TARGET_DIR = target
RELEASE_DIR = $(TARGET_DIR)/release
BINARY_NAME = kakehashi
RELEASE_BINARY = $(RELEASE_DIR)/$(BINARY_NAME)

# Default target
.PHONY: all
all: build

# Build the project in release mode
.PHONY: build
build:
	$(CARGO) build --release

build-debug:
	$(CARGO) build

# Build the project in debug mode
.PHONY: debug
debug:
	$(CARGO) build

# Clean build artifacts
.PHONY: clean
clean:
	$(CARGO) clean

# Run unit tests only (excludes integration and E2E tests)
.PHONY: test
test:
	$(CARGO) test --lib

# Run integration and E2E tests (excludes unit tests)
# - Integration tests: tests/test_*.rs (no feature required)
# - E2E tests: tests/e2e_*.rs (requires e2e feature)
# Default to the parallel runner: `cargo test` runs integration binaries
# sequentially, leaving a many-core machine idle while each binary waits on a
# spawned server. The script runs the binaries in a bounded parallel pool (~3x
# faster here) with identical coverage. Use test_e2e_sequential to debug
# timing-sensitive tests without cross-binary load.
.PHONY: test_e2e
test_e2e:
	CARGO="$(CARGO)" scripts/test_e2e_parallel.sh

.PHONY: test_e2e_sequential
test_e2e_sequential:
	$(CARGO) test --features e2e --test 'test_*' --test 'e2e_*'

# Run all tests (unit + integration + E2E)
.PHONY: test_all
test_all:
	$(CARGO) test --features e2e

# Check code formatting and linting
.PHONY: check
check:
	if git grep --quiet -E '#\[allow\(dead_code\)\]' -- src; then echo 'Do not leave dead_code. Codes must be wired' >&2 && exit 1; fi
	$(CARGO) check
	$(CARGO) clippy -- -D warnings
	$(CARGO) fmt --check

# Format code
.PHONY: fmt
format:
	$(CARGO) fmt

# Run linter (clippy)
.PHONY: lint
lint:
	$(CARGO) clippy --all-targets --all-features -- -D warnings

# Install the binary to ~/.cargo/bin
.PHONY: install
install: build
	$(CARGO) install --path .

# Determine if running in Nix environment (skip deps if so)
ifdef TREE_SITTER_GRAMMARS
  NVIM_DEPS =
else
  NVIM_DEPS = deps
endif

# Run all test files
test_nvim: $(NVIM_DEPS) build-debug
	nvim -es --headless --noplugin -u ./scripts/minimal_init.lua -c "lua MiniTest.run()"

# Run test from file at `$FILE` environment variable
test_nvim_file: $(NVIM_DEPS) build-debug
	nvim --headless --noplugin -u ./scripts/minimal_init.lua -c "lua MiniTest.run_file('$(FILE)')"

# Download 'mini.nvim' to use its 'mini.test' testing module
deps: deps/nvim deps/tree-sitter

deps/nvim: build-debug deps/nvim/mini.nvim deps/nvim/nvim-treesitter deps/nvim/catppuccin

deps/nvim/mini.nvim:
	git clone --filter=blob:none https://github.com/nvim-mini/mini.nvim $@

deps/nvim/nvim-treesitter:
	git clone --filter=blob:none https://github.com/nvim-treesitter/nvim-treesitter --branch main $@

deps/nvim/catppuccin:
	git clone --filter=blob:none https://github.com/catppuccin/nvim $@

target/debug/kakehashi: build-debug

deps/tree-sitter/.installed: target/debug/kakehashi
	@mkdir -p deps/tree-sitter
	for lang in lua rust markdown markdown_inline yaml html; do \
		./target/debug/kakehashi language install $$lang --data-dir deps/tree-sitter --force; \
	done
	@touch $@

deps/tree-sitter: deps/tree-sitter/.installed

deps/vim/prabirshrestha/vim-lsp:
	git clone --filter=blob:none https://github.com/prabirshrestha/vim-lsp $@

deps/vim: build-debug deps/vim/prabirshrestha/vim-lsp

# Show help
.PHONY: help
help:
	@echo "Available targets:"
	@echo "  build         - Build the project in release mode (default)"
	@echo "  debug         - Build the project in debug mode"
	@echo "  clean         - Clean build artifacts"
	@echo "  test          - Run unit tests only"
	@echo "  test_e2e      - Run integration and E2E tests"
	@echo "  test_all      - Run all tests (unit + integration + E2E)"
	@echo "  test_nvim     - Run all Neovim test files"
	@echo "  test_nvim_file - Run test from file at \$$FILE environment variable"
	@echo "  check         - Run code checks (clippy, fmt)"
	@echo "  format        - Format code with rustfmt"
	@echo "  lint          - Run clippy linter"
	@echo "  install       - Install binary to ~/.cargo/bin"
	@echo "  deps          - Download dependencies for Neovim testing"
	@echo "  verify        - Check if the binary exists"
	@echo "  help          - Show this help message"

# Check if the binary exists
.PHONY: verify
verify:
	@if [ -f "$(RELEASE_BINARY)" ]; then \
		echo "✅ Binary exists at $(RELEASE_BINARY)"; \
		ls -la "$(RELEASE_BINARY)"; \
	else \
		echo "❌ Binary not found at $(RELEASE_BINARY)"; \
		echo "Run 'make build' to create it."; \
	fi
