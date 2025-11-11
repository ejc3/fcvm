SHELL := /bin/bash

all: build

build:
	cargo build --release
	cd fc-agent && cargo build --release --target aarch64-unknown-linux-musl

clean:
	cargo clean
	cd fc-agent && cargo clean

firecracker:
	@echo "Use scripts/fcvm-init.sh to download Firecracker"

rootfs:
	scripts/create-rootfs-debian.sh ~/.local/share/fcvm/images/rootfs target/release/fc-agent

kernel:
	scripts/build-kernel.sh ~/.local/share/fcvm/images/vmlinux
