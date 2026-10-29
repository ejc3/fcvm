# fleet/phone/streamer: the phone streamer. Builds in the CUDA devel image (the phone
# Containerfile's build stage) with the renderer's server, encoder and wire code.
CUDA ?= /usr/local/cuda
NVCH ?= /opt/nv-codec-headers/include
RSRC ?= ../../renderer/src
RTESTS ?= ../../renderer/tests
OUT ?= build

CXXFLAGS := -O2 -std=c++17 -Wall -Wextra -Wno-unused-parameter -I. -I$(RSRC) -I$(CUDA)/include -I$(NVCH)
LDLIBS := -L$(CUDA)/lib64/stubs -lcuda -lssl -lcrypto -ldl -lpthread $(shell pkg-config --libs grpc++ protobuf 2>/dev/null)
HOST_SRC := stamp.cpp nv12.cpp logic.cpp $(RSRC)/json.cpp $(RSRC)/proto.cpp $(RSRC)/admit.cpp $(RSRC)/access.cpp
ALL_SRC := $(HOST_SRC) $(OUT)/screen.pb.cc $(OUT)/screen.grpc.pb.cc screen.cpp source.cpp main.cpp $(RSRC)/subscriber.cpp $(RSRC)/nvenc.cpp $(RSRC)/server.cpp $(RSRC)/jpeg.cpp \
	$(RSRC)/tls.cpp $(RSRC)/gpustats.cpp

.PHONY: all test-host clean
all: $(OUT)/phone-streamer $(OUT)/test-host

# The emulator's controller service (screen.proto), generated with protoc and gRPC's plugin.
# One grouped target (&:): a parallel build runs protoc once for all four outputs.
$(OUT)/screen.pb.cc $(OUT)/screen.pb.h $(OUT)/screen.grpc.pb.cc $(OUT)/screen.grpc.pb.h &: screen.proto
	@mkdir -p $(OUT)
	protoc -I. --cpp_out=$(OUT) --grpc_out=$(OUT) --plugin=protoc-gen-grpc=$$(command -v grpc_cpp_plugin) screen.proto

$(OUT)/phone-streamer: $(ALL_SRC) *.h $(RSRC)/*.h
	@mkdir -p $(OUT)
	$(CXX) $(CXXFLAGS) -I$(OUT) -o $@ $(ALL_SRC) $(LDLIBS)

# Host-only tests need no CUDA or emulator.
$(OUT)/test-host: test_host.cpp $(HOST_SRC) *.h
	@mkdir -p $(OUT)
	$(CXX) -O1 -std=c++17 -Wall -Wextra -Wno-unused-parameter -I. -I$(RSRC) -I$(RTESTS) -o $@ test_host.cpp $(HOST_SRC) -lssl -lcrypto

test-host: $(OUT)/test-host
	$(OUT)/test-host

clean:
	rm -rf $(OUT)
