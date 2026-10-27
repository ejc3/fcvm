# fleet/renderer: the GPU wall's renderer. Builds inside the CUDA devel image
# (see Containerfile); `make image` builds the container with podman.
CUDA ?= /usr/local/cuda
NVCC ?= $(CUDA)/bin/nvcc
NVCH ?= /opt/nv-codec-headers/include
GENCODE ?= -gencode arch=compute_75,code=sm_75
OUT ?= build
TAG ?= dev
IMAGE ?= localhost/fcvm-renderer:$(TAG)

CXXFLAGS := -O2 -std=c++17 -Wall -Wextra -Wno-unused-parameter -I$(CUDA)/include -I$(NVCH)
NVFLAGS := -O3 -std=c++17 --use_fast_math $(GENCODE) -Xcompiler -Wall
LDLIBS := -L$(CUDA)/lib64/stubs -lcuda -lssl -lcrypto -ldl -lpthread

CORE := json scene proto subscriber admit
HOST := $(CORE) nvenc gpustats pipeline server jpeg tls
CORE_O := $(addprefix $(OUT)/,$(addsuffix .o,$(CORE)))
HOST_O := $(addprefix $(OUT)/,$(addsuffix .o,$(HOST)))

.PHONY: all test-host image clean
all: $(OUT)/fleet-renderer $(OUT)/fleet-renderer-test $(OUT)/test-host

$(OUT)/%.o: src/%.cpp src/*.h
	@mkdir -p $(OUT)
	$(CXX) $(CXXFLAGS) -c $< -o $@

$(OUT)/render.o: src/render.cu src/*.h
	@mkdir -p $(OUT)
	$(NVCC) $(NVFLAGS) -c $< -o $@

$(OUT)/fleet-renderer: $(HOST_O) $(OUT)/render.o $(OUT)/main.o
	$(NVCC) $(GENCODE) -o $@ $^ $(LDLIBS)

$(OUT)/fleet-renderer-test: tests/test_gpu.cpp tests/check.h $(CORE_O) $(OUT)/nvenc.o $(OUT)/render.o
	$(CXX) $(CXXFLAGS) -c tests/test_gpu.cpp -o $(OUT)/test_gpu.o
	$(NVCC) $(GENCODE) -o $@ $(OUT)/test_gpu.o $(CORE_O) $(OUT)/nvenc.o $(OUT)/render.o $(LDLIBS)

# Host-only tests need no CUDA: g++ and the NVENC header.
$(OUT)/test-host: tests/test_host.cpp tests/check.h src/*.cpp src/*.h
	@mkdir -p $(OUT)
	$(CXX) -O1 -std=c++17 -Wall -I$(NVCH) -o $@ tests/test_host.cpp src/json.cpp src/scene.cpp src/admit.cpp src/proto.cpp src/subscriber.cpp src/nvenc.cpp src/jpeg.cpp src/tls.cpp -lssl -lcrypto -ldl -lpthread

# The synthetic viewer (tools/viewerload.cpp); test-host runs its self-test.
$(OUT)/viewerload: tools/viewerload.cpp
	@mkdir -p $(OUT)
	$(CXX) -O2 -std=c++17 -Wall -pthread -o $@ $< -lssl -lcrypto

test-host: $(OUT)/test-host $(OUT)/viewerload
	$(OUT)/test-host
	$(OUT)/viewerload --self-test

image:
	podman build --squash-all -t $(IMAGE) -f Containerfile .

clean:
	rm -rf $(OUT)
