.PHONY: help build test clippy pi pi-gnu pi32 linux-x86 macos clean

BIN := muagent
PKG := muagent
PROFILE ?= min
PROFILE_DIR := $(if $(filter release,$(PROFILE)),release,$(if $(filter min,$(PROFILE)),min,debug))

help:
	@echo "μAgent build targets"
	@echo ""
	@echo "  make build           # host dev build"
	@echo "  make test            # package tests"
	@echo "  make clippy          # clippy --all-targets -D warnings"
	@echo ""
	@echo "Cross-compile (requires cargo-zigbuild: \`cargo install cargo-zigbuild\` + \`brew install zig\`):"
	@echo "  make pi              # Pi 4/5 / Rock 5 — aarch64-unknown-linux-musl  (static, portable)"
	@echo "  make pi-gnu          # Pi 4/5 — aarch64-unknown-linux-gnu             (smaller, dynamic)"
	@echo "  make pi32            # Pi Zero / Pi 3 32-bit — armv7-unknown-linux-musleabihf"
	@echo "  make linux-x86       # x86_64-unknown-linux-musl                      (server Linux)"
	@echo ""
	@echo "Override profile:  make pi PROFILE=release   (default: min = LTO + strip + opt-z)"

build:
	cargo build

test:
	cargo test

clippy:
	cargo clippy --all-targets -- -D warnings

pi: TARGET := aarch64-unknown-linux-musl
pi-gnu: TARGET := aarch64-unknown-linux-gnu
pi32: TARGET := armv7-unknown-linux-musleabihf
linux-x86: TARGET := x86_64-unknown-linux-musl

pi pi-gnu pi32 linux-x86:
	@command -v cargo-zigbuild >/dev/null || { echo "cargo-zigbuild not found. Install: cargo install cargo-zigbuild && brew install zig"; exit 1; }
	rustup target add $(TARGET)
	cargo zigbuild --profile=$(PROFILE) --target=$(TARGET) -p $(PKG)
	@echo ""
	@echo "Binary: target/$(TARGET)/$(PROFILE_DIR)/$(BIN)"
	@ls -lh target/$(TARGET)/$(PROFILE_DIR)/$(BIN) 2>/dev/null | awk '{print "Size:   " $$5}'
	@file target/$(TARGET)/$(PROFILE_DIR)/$(BIN) 2>/dev/null || true

clean:
	cargo clean
