# fleet/coord: the coordinator's container image. `make build` at the top first; the
# image is built for the architecture of this host (Fargate runs ARM64).
TAG ?= dev
IMAGE ?= localhost/fcvm-fleet-coord:$(TAG)
FCVM ?= $(shell readlink -f ../../target/release/fcvm)

.PHONY: image
image:
	@test -x "$(FCVM)" || { echo "no fcvm binary at $(FCVM): run make build first"; exit 1; }
	rm -rf build && mkdir -p build && cp "$(FCVM)" build/fcvm && cp Containerfile build/
	podman build -t $(IMAGE) -f build/Containerfile build
	rm -rf build
