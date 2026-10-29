# fleet/phone: the phone container image. Its sources and the renderer code the streamer
# builds with (fleet/renderer/src) name it, so checkouts with other sources never replace
# each other's image.
ENGINE ?= podman
ID := $(shell cd .. && find phone renderer/src renderer/tests/check.h -type f ! -path 'phone/streamer/build/*' -print0 | LC_ALL=C sort -z | xargs -0 sha256sum | sha256sum | cut -c1-16)
TAG ?= $(ID)
IMAGE ?= localhost/fcvm-phone:$(TAG)

.PHONY: image image-id test-host
image-id:
	@echo $(ID)

image:
	$(ENGINE) build -q --build-context renderer=../renderer -t $(IMAGE) .

test-host:
	$(MAKE) -C streamer test-host
