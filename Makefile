SHELL := /bin/bash

APP_NAME := Phosphor
BIN_NAME := phosphor
DIST_DIR := dist

# Extract version from Cargo.toml
VERSION := $(shell sed -n 's/^version[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' Cargo.toml | head -n 1)

UNAME_S := $(shell uname -s)

MAC_OUT := $(DIST_DIR)/$(APP_NAME)-$(VERSION)-macOS.pkg
LIN_OUT := $(DIST_DIR)/$(APP_NAME)-$(VERSION)-linux-amd64.deb

.PHONY: help clean dist linux_deb macos_pkg

help:
	@echo "Targets:"
	@echo "  make linux_deb   - build .deb via cargo deb"
	@echo "  make macos_pkg   - rename/copy macOS pkg"
	@echo "  make dist        - build for current OS"
	@echo "  make clean"
	@echo ""
	@echo "Detected OS=$(UNAME_S) VERSION=$(VERSION)"

clean:
	rm -rf $(DIST_DIR)

dist:
	@mkdir -p $(DIST_DIR)
	@if [[ "$(UNAME_S)" == "Linux" ]]; then \
	  $(MAKE) linux_deb; \
	elif [[ "$(UNAME_S)" == "Darwin" ]]; then \
	  $(MAKE) macos_pkg; \
	else \
	  echo "Unsupported OS for this Makefile"; exit 1; \
	fi

# -----------------------
# Linux
# -----------------------
linux_deb:
	@if [[ "$(UNAME_S)" != "Linux" ]]; then echo "Run this on Linux"; exit 1; fi
	@mkdir -p $(DIST_DIR)
	cargo deb
	@DEB_PATH=$$(ls -1 target/debian/*.deb | head -n 1); \
	cp "$$DEB_PATH" "$(LIN_OUT)"; \
	echo "Built: $(LIN_OUT)"

# -----------------------
# macOS
# -----------------------
macos_pkg:
	@if [[ "$(UNAME_S)" != "Darwin" ]]; then echo "Run this on macOS"; exit 1; fi
	@mkdir -p $(DIST_DIR)
	@if [[ -z "$${PKG_IN:-}" ]]; then \
	  echo "Usage:"; \
	  echo "make macos_pkg PKG_IN=path/to/pkg"; \
	  exit 1; \
	fi
	cp "$$PKG_IN" "$(MAC_OUT)"
	@echo "Built: $(MAC_OUT)"