SHELL := /bin/bash

APP_NAME := Phosphor
BIN_NAME := phosphor
DIST_DIR := dist

# Version from cargo metadata (needs jq)
VERSION := $(shell cargo metadata --no-deps --format-version 1 | \
	jq -r '.packages[] | select(.name=="phosphor") | .version')

UNAME_S := $(shell uname -s)

MAC_OUT := $(DIST_DIR)/$(APP_NAME)-$(VERSION)-macOS.pkg
WIN_OUT := $(DIST_DIR)/$(APP_NAME)-$(VERSION)-windows-x86_64.zip
LIN_OUT := $(DIST_DIR)/$(APP_NAME)-$(VERSION)-linux-amd64.deb

.PHONY: help clean dist linux_deb windows_zip macos_pkg

help:
	@echo "Phosphor packaging"
	@echo ""
	@echo "Targets (run on the matching OS):"
	@echo "  make linux_deb     - Linux only: build .deb via cargo deb"
	@echo "  make windows_zip   - Windows only: build portable zip (exe + docs)"
	@echo "  make macos_pkg     - macOS only: rename/copy existing .pkg into dist/"
	@echo "  make dist          - build the one that matches this OS"
	@echo "  make clean         - remove dist/"
	@echo ""
	@echo "Detected:"
	@echo "  OS      = $(UNAME_S)"
	@echo "  Version = $(VERSION)"

clean:
	rm -rf $(DIST_DIR)

# Build whatever matches the current OS
dist:
	@mkdir -p $(DIST_DIR)
	@if [[ "$(UNAME_S)" == "Linux" ]]; then \
	  $(MAKE) linux_deb; \
	elif [[ "$(UNAME_S)" == "Darwin" ]]; then \
	  $(MAKE) macos_pkg; \
	else \
	  echo "Assuming Windows (uname=$(UNAME_S)). Run 'make windows_zip' in a MSYS/MinGW shell, or use the windows_zip target manually."; \
	  exit 1; \
	fi

# -----------------------
# Linux (cargo-deb)
# -----------------------
linux_deb:
	@if [[ "$(UNAME_S)" != "Linux" ]]; then \
	  echo "ERROR: linux_deb must be run on Linux (uname=$(UNAME_S))"; exit 1; \
	fi
	@mkdir -p $(DIST_DIR)
	cargo deb
	@DEB_PATH=$$(ls -1 target/debian/*.deb | head -n 1); \
	  if [[ -z "$$DEB_PATH" ]]; then echo "ERROR: no .deb produced in target/debian"; exit 1; fi; \
	  cp "$$DEB_PATH" "$(LIN_OUT)"; \
	  echo "Built: $(LIN_OUT)"

# -----------------------
# Windows (portable ZIP)
# -----------------------
windows_zip:
	@mkdir -p $(DIST_DIR)
	@# This target is intended to run on Windows in a shell that supports bash/zip,
	@# e.g. Git Bash. If you prefer PowerShell-only, tell me and I'll rewrite it.
	cargo build --release
	rm -rf "$(DIST_DIR)/_winpkg"
	mkdir -p "$(DIST_DIR)/_winpkg"
	cp "target/release/$(BIN_NAME).exe" "$(DIST_DIR)/_winpkg/"
	cp README.md LICENSE "$(DIST_DIR)/_winpkg/" 2>/dev/null || true
	(cd "$(DIST_DIR)/_winpkg" && zip -9 -r "../$(notdir $(WIN_OUT))" .)
	rm -rf "$(DIST_DIR)/_winpkg"
	@echo "Built: $(WIN_OUT)"

# -----------------------
# macOS (.pkg naming only)
# -----------------------
macos_pkg:
	@if [[ "$(UNAME_S)" != "Darwin" ]]; then \
	  echo "ERROR: macos_pkg must be run on macOS (uname=$(UNAME_S))"; exit 1; \
	fi
	@mkdir -p $(DIST_DIR)
	@# Assumption: you already created a pkg somewhere (example: dist/Phosphor-<ver>.pkg)
	@# Set PKG_IN=... when calling make, e.g.:
	@#   make macos_pkg PKG_IN=dist/Phosphor-$(VERSION).pkg
	@if [[ -z "$${PKG_IN:-}" ]]; then \
	  echo "ERROR: set PKG_IN to the path of the built .pkg"; \
	  echo "Example: make macos_pkg PKG_IN=dist/$(APP_NAME)-$(VERSION).pkg"; \
	  exit 1; \
	fi
	cp "$$PKG_IN" "$(MAC_OUT)"
	@echo "Built: $(MAC_OUT)"