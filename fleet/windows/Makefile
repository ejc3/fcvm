# fleet/windows: the Windows bridge's container image (Containerfile) and its host tests. The
# image is named after its sources and the renderer code it builds with, so checkouts with other
# sources never replace each other's image. The sandbox image itself is build-image.sh's.
ENGINE ?= podman
ID := $(shell cd .. && find windows/Containerfile windows/bridge renderer/src renderer/tests/check.h -type f ! -path 'windows/bridge/build/*' -print0 | LC_ALL=C sort -z | xargs -0 sha256sum | sha256sum | cut -c1-16)
TAG ?= $(ID)
IMAGE ?= localhost/fcvm-windows-bridge:$(TAG)
# Extra podman build flags (a proxy on the host's loopback needs --network=host).
BUILD_FLAGS ?=

.PHONY: bridge-image bridge-image-id test-host test-host-asan
bridge-image-id:
	@echo $(ID)

bridge-image:
	$(ENGINE) build -q $(BUILD_FLAGS) --build-context renderer=../renderer -t $(IMAGE) -f Containerfile .

test-host:
	$(MAKE) -C bridge test-host

test-host-asan:
	$(MAKE) -C bridge test-host-asan
