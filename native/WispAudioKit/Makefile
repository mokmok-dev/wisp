APP_NAME := Wispctl
BUNDLE_DIR := build/$(APP_NAME).app
BIN_NAME := wispctl
BIN_RELEASE := .build/arm64-apple-macosx/release/$(BIN_NAME)
BIN_DEBUG := .build/arm64-apple-macosx/debug/$(BIN_NAME)
INFO_PLIST := Resources/Info.plist
ENTITLEMENTS := Resources/wispctl.entitlements

.PHONY: build bundle run clean test-japanese

build:
	swift build

build-release:
	swift build -c release

bundle: build
	rm -rf $(BUNDLE_DIR)
	mkdir -p $(BUNDLE_DIR)/Contents/MacOS
	cp $(BIN_DEBUG) $(BUNDLE_DIR)/Contents/MacOS/$(BIN_NAME)
	cp $(INFO_PLIST) $(BUNDLE_DIR)/Contents/Info.plist
	codesign --force --sign - \
		--entitlements $(ENTITLEMENTS) \
		$(BUNDLE_DIR)/Contents/MacOS/$(BIN_NAME)
	@echo "Bundle ready: $(BUNDLE_DIR)"

bundle-release: build-release
	rm -rf $(BUNDLE_DIR)
	mkdir -p $(BUNDLE_DIR)/Contents/MacOS
	cp $(BIN_RELEASE) $(BUNDLE_DIR)/Contents/MacOS/$(BIN_NAME)
	cp $(INFO_PLIST) $(BUNDLE_DIR)/Contents/Info.plist
	codesign --force --sign - \
		--entitlements $(ENTITLEMENTS) \
		$(BUNDLE_DIR)/Contents/MacOS/$(BIN_NAME)
	@echo "Release bundle ready: $(BUNDLE_DIR)"

run: bundle
	$(BUNDLE_DIR)/Contents/MacOS/$(BIN_NAME)

clean:
	rm -rf .build build
