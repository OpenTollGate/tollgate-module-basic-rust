# SPDX-License-Identifier: MIT
#
# OpenWrt feed Makefile for tollgate-wrt (Rust edition).
#
# This Makefile follows the prebuilt-binary pattern from the Go predecessor:
# CI cross-compiles the Rust binary for each target arch, stages it into
# the package directory, and the OpenWrt SDK only packages the artifact.
#
# Service name is `tollgate-wrt` for interop with the existing init.d slot.
# Binary installs to /usr/bin/tollgate-module-basic-rust.

PKG_MAKEFILE_DIR:=$(dir $(abspath $(lastword $(MAKEFILE_LIST))))

include $(TOPDIR)/rules.mk

PKG_NAME:=tollgate-wrt
PKG_VERSION:=$(if $(strip $(PACKAGE_VERSION)),$(PACKAGE_VERSION),0.1.0)
PKG_FLAGS:=overwrite

# The Rust binary is built natively by CI and staged into $(PKG_MAKEFILE_DIR)
# as tollgate-module-basic-rust. The SDK only packages it.
PREBUILT_BIN:=$(PKG_MAKEFILE_DIR)tollgate-module-basic-rust

PKG_BUILD_DIR:=$(BUILD_DIR)/$(PKG_NAME)-$(PKG_VERSION)

PKG_MAINTAINER:=TollGate <tollgate@tollgate.me>
PKG_LICENSE:=MIT
PKG_LICENSE_FILES:=LICENSE-MIT

PKG_BUILD_PARALLEL:=1
PKG_USE_MIPS16:=0

include $(INCLUDE_DIR)/package.mk

define Package/$(PKG_NAME)
	SECTION:=net
	CATEGORY:=Network
	TITLE:=TollGate Basic Module (Rust)
	DEPENDS:=+nodogsplash +jq
	PROVIDES:=nodogsplash-files
endef

define Package/$(PKG_NAME)/description
	TollGate Basic Module for OpenWrt — Rust edition (CDK wallet).
	Drop-in replacement for the Go binary, using CDK instead of gonuts.
endef

# ── preinst: stamp install_time, create /etc/tollgate ─────────────────
define Package/$(PKG_NAME)/preinst
#!/bin/sh

mkdir -p /etc/tollgate

if [ -f /etc/tollgate/install.json ]; then
	CURRENT_TIMESTAMP=$$(date +%s)
	jq ".install_time = $$CURRENT_TIMESTAMP" /etc/tollgate/install.json > /tmp/install.json.tmp && \
		mv /tmp/install.json.tmp /etc/tollgate/install.json || \
		echo "Warning: failed to update install_time" >&2
else
	CURRENT_TIMESTAMP=$$(date +%s)
	echo "{\"install_time\": $$CURRENT_TIMESTAMP}" > /etc/tollgate/install.json
fi

exit 0
endef

# ── postinst: portal symlink, enable + start service ──────────────────
define Package/$(PKG_NAME)/postinst
#!/bin/sh

# Create captive-portal symlink: /etc/nodogsplash/htdocs -> tollgate portal
if [ -L /etc/nodogsplash/htdocs ]; then
	echo "captive-portal symlink already exists"
else
	if [ -d /etc/nodogsplash/htdocs ]; then
		mv /etc/nodogsplash/htdocs /etc/nodogsplash/htdocs.backup
	fi
	rm -rf /etc/nodogsplash/htdocs
	ln -sf /etc/tollgate/tollgate-captive-portal-site /etc/nodogsplash/htdocs
	echo "created /etc/nodogsplash/htdocs -> /etc/tollgate/tollgate-captive-portal-site"
fi

# Enable and (re)start the service
if [ -x /etc/init.d/tollgate-wrt ]; then
	/etc/init.d/tollgate-wrt enable 2>/dev/null || true
	/etc/init.d/tollgate-wrt stop 2>/dev/null || true
	/etc/init.d/tollgate-wrt start 2>/dev/null || true
fi

exit 0
endef

# ── Build steps: copy prebuilt binary, optional UPX ───────────────────
define Build/Prepare
	mkdir -p $(PKG_BUILD_DIR)
	[ -f "$(PREBUILT_BIN)" ] || { echo "ERROR: prebuilt binary not found at $(PREBUILT_BIN). CI should stage it before invoking the SDK." >&2; exit 1; }
	$(CP) $(PREBUILT_BIN) $(PKG_BUILD_DIR)/tollgate-module-basic-rust
endef

define Build/Configure
endef

define Build/Compile
	@if [ "$(USE_UPX)" = "1" ]; then \
		if which upx >/dev/null 2>&1; then \
			ls -lh $(PKG_BUILD_DIR)/tollgate-module-basic-rust; \
			upx $(UPX_FLAGS) $(PKG_BUILD_DIR)/tollgate-module-basic-rust; \
			ls -lh $(PKG_BUILD_DIR)/tollgate-module-basic-rust; \
		else \
			echo "ERROR: USE_UPX=1 but upx not on PATH" >&2; \
			exit 1; \
		fi; \
	fi
endef

# ── Install: binary, init script, config dir, portal site, keep list ──
define Package/$(PKG_NAME)/install
	$(INSTALL_DIR) $(1)/usr/bin
	$(INSTALL_BIN) $(PKG_BUILD_DIR)/tollgate-module-basic-rust $(1)/usr/bin/tollgate-module-basic-rust

	$(INSTALL_DIR) $(1)/etc/init.d
	$(INSTALL_BIN) $(PKG_MAKEFILE_DIR)files/etc/init.d/tollgate-wrt $(1)/etc/init.d/

	$(INSTALL_DIR) $(1)/etc/tollgate
	$(INSTALL_DIR) $(1)/etc/tollgate/ecash

	$(INSTALL_DIR) $(1)/etc/tollgate/tollgate-captive-portal-site
	$(CP) $(PKG_MAKEFILE_DIR)files/tollgate-captive-portal-site/* $(1)/etc/tollgate/tollgate-captive-portal-site/

	$(INSTALL_DIR) $(1)/lib/upgrade/keep.d
	$(INSTALL_DATA) $(PKG_MAKEFILE_DIR)files/lib/upgrade/keep.d/tollgate $(1)/lib/upgrade/keep.d/
endef

FILES_$(PKG_NAME) += \
	/usr/bin/tollgate-module-basic-rust \
	/etc/init.d/tollgate-wrt \
	/etc/tollgate/tollgate-captive-portal-site/* \
	/lib/upgrade/keep.d/tollgate

$(eval $(call BuildPackage,$(PKG_NAME)))