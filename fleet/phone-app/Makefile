# Robot Park, the phone app (docs/fleet/ARCHITECTURE.md, "Phones mode"; its control protocol is
# "Phone app" in fleet/PROTOCOL.md). Everything runs in the build container (Containerfile):
#
#   make test               the scene's unit tests and the shader check (no Android SDK needed)
#   make apk SDK=<dir>      the app, against an SDK from scripts/phone/sdk.sh; prints the APK path
#
# Gradle's caches live in GRADLE_HOME (default /mnt/fcvm-btrfs/phone-app/gradle-home). The app
# needs an x86_64 host: the Android build tools (aapt2) are x86_64 binaries.
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
	@test -n "$(SDK)" && test -d "$(SDK)/platforms/android-35" || { echo "usage: make apk SDK=<dir from scripts/phone/sdk.sh --sdk-path>" >&2; exit 2; }
	@mkdir -p "$(GRADLE_HOME)"
	$(PODMAN) run --rm -v "$(CURDIR):/src" -v "$(GRADLE_HOME):/gradle-home" -v "$(SDK):/sdk:ro" -e ANDROID_HOME=/sdk $(IMAGE) \
	  gradle --console=plain :app:assembleRelease
	@echo "$(CURDIR)/app/build/outputs/apk/release/app-release.apk"

# Regenerates gradle/verification-metadata.xml from what both builds resolve; run it with an
# empty GRADLE_HOME, since artifacts already in the cache (aapt2's) are not all recorded.
verification-metadata: image
	@test -n "$(SDK)" || { echo "usage: make verification-metadata SDK=<dir> GRADLE_HOME=<empty dir>" >&2; exit 2; }
	@mkdir -p "$(GRADLE_HOME)"
	$(RUN) gradle --console=plain --write-verification-metadata sha256 -PsceneOnly=true :scene:test
	$(PODMAN) run --rm -v "$(CURDIR):/src" -v "$(GRADLE_HOME):/gradle-home" -v "$(SDK):/sdk:ro" -e ANDROID_HOME=/sdk $(IMAGE) \
	  gradle --console=plain --write-verification-metadata sha256 :app:assembleRelease
