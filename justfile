# rssr — RSS and Atom feeds from the shell. `just --list` for recipes.

set shell := ["bash", "-cu"]

# Personal recipes (feed lists, scratch databases) live here and are not tracked.
import? 'local.just'

default: build

# Debug build
build:
    cargo build --workspace

# Optimised binary at target/release/rssr
release:
    cargo build --release
    @ls -l target/release/rssr | awk '{printf "binary: %.2f MB\n", $5/1048576}'

test:
    cargo test --workspace

# Formatting and lints, as CI would run them
lint:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings

fmt:
    cargo fmt --all

# Put the release binary on PATH at ~/.local/bin/rssr
install: release
    mkdir -p "$HOME/.local/bin"
    install -m 755 target/release/rssr "$HOME/.local/bin/rssr"
    @echo "installed $("$HOME/.local/bin/rssr" --version)"

uninstall:
    rm -f "$HOME/.local/bin/rssr"

clean:
    cargo clean
