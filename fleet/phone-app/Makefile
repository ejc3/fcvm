# Robot Park, the phone app (docs/fleet/ARCHITECTURE.md, "Phones mode"; its control protocol is
# "Phone app" in fleet/PROTOCOL.md). Everything runs in the build container (Containerfile):
#
#   make test                   the scene's unit tests and the shader check
#   make apk AOSP_TOOLS=<dir>   the app (build-apk.sh), with the AOSP build's tools
#                               (<scripts/phone/aosp.sh --root-path>/tools); prints the APK path
#
# Gradle's caches live in GRADLE_HOME (default /mnt/fcvm-btrfs/phone-app/gradle-home). The app
# needs an x86_64 host: the AOSP build's aapt2 and zipalign are x86_64 binaries.
GRADLE_HOME ?= /mnt/fcvm-btrfs/phone-app/gradle-home
IMAGE := localhost/fcvm-phone-app-build:$(shell sha256sum Containerfile | cut -c1-16)
PODMAN ?= podman
RUN = $(PODMAN) run --rm -v "$(CURDIR):/src" -v "$(GRADLE_HOME):/gradle-home" $(IMAGE)

.PHONY: image test apk verification-metadata
image:
	@$(PODMAN) image exists $(IMAGE) || $(PODMAN) build -q -t $(IMAGE) . > /dev/null

test: image
	@mkdir -p "$(GRADLE_HOME)"
	$(RUN) gradle --console=plain -PsceneOnly=true :scene:test

apk: image
	@test -n "$(AOSP_TOOLS)" && test -f "$(AOSP_TOOLS)/android.jar" || { echo "usage: make apk AOSP_TOOLS=<scripts/phone/aosp.sh --root-path>/tools" >&2; exit 2; }
	@mkdir -p "$(GRADLE_HOME)"
	@$(PODMAN) run --rm -v "$(CURDIR):/src" -v "$(GRADLE_HOME):/gradle-home" -v "$(AOSP_TOOLS):/aosp-tools:ro" -e AOSP_TOOLS=/aosp-tools $(IMAGE) \
	  /src/build-apk.sh > /dev/null
	@echo "$(CURDIR)/app/build/apk/robotpark.apk"

# Regenerates gradle/verification-metadata.xml from what both builds resolve; run it with an
# empty GRADLE_HOME, since artifacts already in the cache are not all recorded.
verification-metadata: image
	@test -n "$(AOSP_TOOLS)" || { echo "usage: make verification-metadata AOSP_TOOLS=<dir> GRADLE_HOME=<empty dir>" >&2; exit 2; }
	@mkdir -p "$(GRADLE_HOME)"
	$(RUN) gradle --console=plain --write-verification-metadata sha256 -PsceneOnly=true :scene:test
	$(PODMAN) run --rm -v "$(CURDIR):/src" -v "$(GRADLE_HOME):/gradle-home" -v "$(AOSP_TOOLS):/aosp-tools:ro" $(IMAGE) \
	  gradle --console=plain --write-verification-metadata sha256 -PandroidJar=/aosp-tools/android.jar :app:dexInputs
