# Build and install xl.
#   make                      build the release binary
#   make install              install for the current user (~/.local)
#   sudo make install PREFIX=/usr/local
#   make uninstall [PREFIX=...]
#   make deb | appimage | flatpak

PREFIX ?= $(HOME)/.local
DESTDIR ?=
APP_ID := dev.xl.Xl
BIN := target/release/xl
ICONS := target/icons/hicolor

BINDIR := $(DESTDIR)$(PREFIX)/bin
DATADIR := $(DESTDIR)$(PREFIX)/share

.PHONY: all build icons install uninstall deb appimage flatpak clean-packages

all: build

vendor/zed/.git:
	./scripts/fetch-gpui.sh

build: vendor/zed/.git
	cargo build --release -p xl

icons: $(ICONS)/scalable/apps/$(APP_ID).svg

$(ICONS)/scalable/apps/$(APP_ID).svg: packaging/icons/$(APP_ID).svg
	cargo run --release -q -p xl-pack --bin render-icons -- $< $(ICONS)

install: build icons
	install -Dm755 $(BIN) $(BINDIR)/xl
	install -Dm644 packaging/$(APP_ID).desktop $(DATADIR)/applications/$(APP_ID).desktop
	install -Dm644 packaging/$(APP_ID).metainfo.xml $(DATADIR)/metainfo/$(APP_ID).metainfo.xml
	cd $(ICONS) && find . -name '$(APP_ID).*' -exec install -Dm644 {} $(DATADIR)/icons/hicolor/{} \;
	@# Refresh caches when installing into a live system (not when staging into DESTDIR).
	@if [ -z "$(DESTDIR)" ]; then \
		update-desktop-database -q $(DATADIR)/applications 2>/dev/null || true; \
		if [ -f $(DATADIR)/icons/hicolor/icon-theme.cache ]; then gtk-update-icon-cache -q -t $(DATADIR)/icons/hicolor 2>/dev/null || true; fi; \
	fi
	@if [ -z "$(DESTDIR)" ]; then echo "Installed xl to $(PREFIX). Launch it from your app grid or run: xl"; fi

uninstall:
	rm -f $(BINDIR)/xl
	rm -f $(DATADIR)/applications/$(APP_ID).desktop
	rm -f $(DATADIR)/metainfo/$(APP_ID).metainfo.xml
	find $(DATADIR)/icons/hicolor -name '$(APP_ID).*' -delete 2>/dev/null || true
	@if [ -z "$(DESTDIR)" ]; then update-desktop-database -q $(DATADIR)/applications 2>/dev/null || true; fi

deb:
	./packaging/build-deb.sh

appimage:
	./packaging/build-appimage.sh

flatpak:
	./packaging/build-flatpak.sh

clean-packages:
	rm -rf target/icons target/packages target/appdir
