# Robot Park, the phone app (docs/fleet/ARCHITECTURE.md, "Phones mode"; its control protocol is
# "Phone app" in fleet/PROTOCOL.md). Everything runs in the build container (Containerfile):
#
#   make test                   the scene's unit tests and the shader check
#   make apk AOSP_TOOLS=<dir>   the app (build-apk.sh), with the AOSP build's tools
#                               (<scripts/phone/aosp.sh --root-path>/tools); prints the APK path
#
# Gradle's caches live in GRADLE_HOME (default /mnt/fcvm-btrfs/phone-app/gradle-home), and its
# distribution in GRADLE_DIST (make gradle-dist). The app
# needs an x86_64 host: the AOSP build's aapt2 and zipalign are x86_64 binaries.
GRADLE_HOME ?= /mnt/fcvm-btrfs/phone-app/gradle-home
# The Gradle distribution the image installs, at the Containerfile's version and checksum. It is
# downloaded once into GRADLE_DIST and copied into the build context, so a cached GRADLE_DIST
# (CI keeps one) builds the image without a download.
GRADLE_DIST ?= /mnt/fcvm-btrfs/phone-app/gradle-dist
GRADLE_VERSION := $(shell sed -n 's/^ARG GRADLE_VERSION=//p' Containerfile)
GRADLE_SHA256 := $(shell sed -n 's/^ARG GRADLE_SHA256=//p' Containerfile)
GRADLE_ZIP := gradle-$(GRADLE_VERSION)-bin.zip
IMAGE := localhost/fcvm-phone-app-build:$(shell sha256sum Containerfile | cut -c1-16)
PODMAN ?= podman
RUN = $(PODMAN) run --rm -v "$(CURDIR):/src" -v "$(GRADLE_HOME):/gradle-home" $(IMAGE)

.PHONY: image gradle-dist test apk verification-metadata
image:
	@$(PODMAN) image exists $(IMAGE) || { \
	  $(MAKE) --no-print-directory gradle-dist && \
	  mkdir -p .gradle-dist && cp "$(GRADLE_DIST)/$(GRADLE_ZIP)" .gradle-dist/ && \
	  $(PODMAN) build -q -t $(IMAGE) . > /dev/null; }

# A unique temporary name and a rename, so concurrent fetches into one GRADLE_DIST cannot
# publish a partial file; the checksum is checked before the rename and again in the image.
gradle-dist:
	@test -n "$(GRADLE_VERSION)" && test -n "$(GRADLE_SHA256)" || { echo "Containerfile names no GRADLE_VERSION or GRADLE_SHA256" >&2; exit 2; }
	@if echo "$(GRADLE_SHA256)  $(GRADLE_DIST)/$(GRADLE_ZIP)" | sha256sum -c --status - 2>/dev/null; then \
	  echo "gradle distribution: using $(GRADLE_DIST)/$(GRADLE_ZIP), no download"; \
	else \
	  echo "gradle distribution: downloading $(GRADLE_ZIP)"; \
	  mkdir -p "$(GRADLE_DIST)" && \
	  tmp=$$(mktemp "$(GRADLE_DIST)/.$(GRADLE_ZIP).XXXXXX") && \
	  { curl -fsSL --retry 5 --retry-all-errors -o "$$tmp" "https://services.gradle.org/distributions/$(GRADLE_ZIP)" && \
	    echo "$(GRADLE_SHA256)  $$tmp" | sha256sum -c --quiet - && \
	    chmod 644 "$$tmp" && mv "$$tmp" "$(GRADLE_DIST)/$(GRADLE_ZIP)"; } || { rm -f "$$tmp"; exit 1; }; \
	fi

test: image
	@mkdir -p "$(GRADLE_HOME)"
	$(RUN) gradle --console=plain -PsceneOnly=true :scene:test

apk: image
	@test -n "$(AOSP_TOOLS)" && test -f "$(AOSP_TOOLS)/android.jar" || { echo "usage: make apk AOSP_TOOLS=<scripts/phone/aosp.sh --root-path>/tools" >&2; exit 2; }
	@mkdir -p "$(GRADLE_HOME)"
	@out=$$($(PODMAN) run --rm -v "$(CURDIR):/src" -v "$(GRADLE_HOME):/gradle-home" -v "$(AOSP_TOOLS):/aosp-tools:ro" -e AOSP_TOOLS=/aosp-tools $(IMAGE) \
	  /src/build-apk.sh) && echo "$(CURDIR)/$${out#/src/}"

# Regenerates gradle/verification-metadata.xml from what both builds resolve; run it with an
# empty GRADLE_HOME, since artifacts already in the cache are not all recorded.
verification-metadata: image
	@test -n "$(AOSP_TOOLS)" || { echo "usage: make verification-metadata AOSP_TOOLS=<dir> GRADLE_HOME=<empty dir>" >&2; exit 2; }
	@mkdir -p "$(GRADLE_HOME)"
	$(RUN) gradle --console=plain --write-verification-metadata sha256 -PsceneOnly=true :scene:test
	$(PODMAN) run --rm -v "$(CURDIR):/src" -v "$(GRADLE_HOME):/gradle-home" -v "$(AOSP_TOOLS):/aosp-tools:ro" $(IMAGE) \
	  gradle --console=plain --write-verification-metadata sha256 -PandroidJar=/aosp-tools/android.jar :app:dexInputs
