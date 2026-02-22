SHELL := /bin/bash

APP_NAME := Phosphor
BIN_NAME := phosphor
DIST_DIR := dist

UNAME_S := $(shell uname -s 2>/dev/null || echo Windows)

# Version extraction without jq:
# - On Linux/macOS: sed from Cargo.toml
# - On Windows: PowerShell reads Cargo.toml and extracts version = "x.y.z"
ifeq ($(UNAME_S),Windows)
VERSION := $(shell powershell -NoProfile -Command \
  "$$t=Get-Content Cargo.toml -Raw; $$m=[regex]::Match($$t,'(?m)^version\\s*=\\s*\"([^\"]+)\"'); if($$m.Success){$$m.Groups[1].Value}else{''}" )
else
VERSION := $(shell sed -n 's/^version[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' Cargo.toml | head -n 1)
endif

MAC_OUT := $(DIST_DIR)/$(APP_NAME)-$(VERSION)-macOS.pkg
WIN_OUT := $(DIST_DIR)/$(APP_NAME)-$(VERSION)-windows-x86_64.zip
LIN_OUT := $(DIST_DIR)/$(APP_NAME)-$(VERSION)-linux-amd64.deb

.PHONY: help clean dist linux_deb windows_zip macos_pkg

help:
	@echo "Targets:"
	@echo "  make linux_deb     - Linux only: build .deb via cargo deb"
	@echo "  make windows_zip   - Windows only: build portable zip (exe + docs)"
	@echo "  make macos_pkg     - macOS only: rename/copy existing .pkg into dist/"
	@echo "  make dist          - build the one that matches this OS"
	@echo "  make clean         - remove dist/"
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
	elif [[ "$(UNAME_S)" == "Windows" ]]; then \
	  $(MAKE) windows_zip; \
	else \
	  echo "Unknown OS: $(UNAME_S)"; exit 1; \
	fi

# -----------------------
# Linux: cargo deb
# -----------------------
linux_deb:
	@if [[ "$(UNAME_S)" != "Linux" ]]; then echo "ERROR: run linux_deb on Linux"; exit 1; fi
	@mkdir -p $(DIST_DIR)
	cargo deb
	@DEB_PATH=$$(ls -1 target/debian/*.deb | head -n 1); \
	  if [[ -z "$$DEB_PATH" ]]; then echo "ERROR: no .deb produced in target/debian"; exit 1; fi; \
	  cp "$$DEB_PATH" "$(LIN_OUT)"; \
	  echo "Built: $(LIN_OUT)"

# -----------------------
# Windows: portable zip
# -----------------------
windows_zip:
	@if [[ "$(UNAME_S)" != "Windows" ]]; then echo "ERROR: run windows_zip on Windows"; exit 1; fi
	@mkdir -p $(DIST_DIR)
	cargo build --release
	# Use PowerShell to zip (no zip.exe dependency)
	powershell -NoProfile -Command "\
	  $$ErrorActionPreference='Stop'; \
	  $$dist='$(DIST_DIR)'; \
	  $$ver='$(VERSION)'; \
	  $$out='$(WIN_OUT)'; \
	  $$tmp=Join-Path $$dist '_winpkg'; \
	  if(Test-Path $$tmp){Remove-Item -Recurse -Force $$tmp}; \
	  New-Item -ItemType Directory -Force -Path $$tmp | Out-Null; \
	  Copy-Item 'target\\release\\$(BIN_NAME).exe' $$tmp; \
	  if(Test-Path 'README.md'){Copy-Item 'README.md' $$tmp}; \
	  if(Test-Path 'LICENSE'){Copy-Item 'LICENSE' $$tmp}; \
	  if(Test-Path $$out){Remove-Item $$out -Force}; \
	  Compress-Archive -Path (Join-Path $$tmp '*') -DestinationPath $$out; \
	  Remove-Item -Recurse -Force $$tmp; \
	  Write-Host ('Built: ' + $$out) \
	"

# -----------------------
# macOS: rename/copy pkg
# -----------------------
macos_pkg:
	@if [[ "$(UNAME_S)" != "Darwin" ]]; then echo "ERROR: run macos_pkg on macOS"; exit 1; fi
	@mkdir -p $(DIST_DIR)
	@if [[ -z "$${PKG_IN:-}" ]]; then \
	  echo "ERROR: set PKG_IN to the path of the built .pkg"; \
	  echo "Example: make macos_pkg PKG_IN=dist/$(APP_NAME)-$(VERSION).pkg"; \
	  exit 1; \
	fi
	cp "$$PKG_IN" "$(MAC_OUT)"
	@echo "Built: $(MAC_OUT)"