# fleet/lambda: the Lambda tile's container image, the renderer image plus the fcvm binary as
# its Lambda runtime. `make build` at the top first, on an x86_64 host: the lane runs on g4dn.
TAG ?= dev
IMAGE ?= localhost/fcvm-lambda-tile:$(TAG)
RENDERER ?= localhost/fcvm-renderer:fleet
FCVM ?= $(shell readlink -f ../../target/release/fcvm)
ENGINE ?= podman

.PHONY: image
image:
	@test -x "$(FCVM)" || { echo "no fcvm binary at $(FCVM): run make build first"; exit 1; }
	@test "$$(uname -m)" = x86_64 || { echo "the lane runs on x86_64 (g4dn): build this image on an x86_64 host"; exit 1; }
	@set -eu; \
	context=$$(mktemp -d); \
	trap 'rm -rf "$$context"' EXIT; \
	cp "$(FCVM)" "$$context/fcvm"; \
	cp Containerfile "$$context/Containerfile"; \
	$(ENGINE) build --build-arg "RENDERER=$(RENDERER)" --label "org.opencontainers.image.revision=$(TAG)" \
	    -t "$(IMAGE)" -f "$$context/Containerfile" "$$context"
