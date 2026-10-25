# fleet/coord: the coordinator's container image. `make build` at the top first; the
# image is built for the architecture of this host (Fargate runs ARM64).
TAG ?= dev
IMAGE ?= localhost/fcvm-fleet-coord:$(TAG)
FCVM ?= $(shell readlink -f ../../target/release/fcvm)
ENGINE ?= podman

.PHONY: image
image:
	@test -x "$(FCVM)" || { echo "no fcvm binary at $(FCVM): run make build first"; exit 1; }
	@set -eu; \
	context=$$(mktemp -d); \
	trap 'rm -rf "$$context"' EXIT; \
	cp "$(FCVM)" "$$context/fcvm"; \
	cp Containerfile "$$context/Containerfile"; \
	$(ENGINE) build --label "org.opencontainers.image.revision=$(TAG)" -t "$(IMAGE)" -f "$$context/Containerfile" "$$context"
