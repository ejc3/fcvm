# fleet/windows/bridge: the Windows bridge. Builds in fleet/windows/Containerfile's build stage,
# against FreeRDP built from source (FREERDP) and with the renderer's server and wire code.
FREERDP ?= /opt/freerdp
RSRC ?= ../../renderer/src
RTESTS ?= ../../renderer/tests
OUT ?= build

CXXFLAGS := -O2 -std=c++17 -Wall -Wextra -Wno-unused-parameter -I. -I$(RSRC)
FREERDP_CFLAGS := -I$(FREERDP)/include/freerdp3 -I$(FREERDP)/include/winpr3
FREERDP_LIBS := -L$(FREERDP)/lib -Wl,-rpath,$(FREERDP)/lib -lfreerdp-client3 -lfreerdp3 -lwinpr3
LDLIBS := -lx264 -lpng -lssl -lcrypto -lpthread
# What the host tests build with: no FreeRDP, no Windows.
HOST_SRC := logic.cpp x264enc.cpp $(RSRC)/json.cpp $(RSRC)/admit.cpp $(RSRC)/h264.cpp $(RSRC)/proto.cpp
ALL_SRC := $(HOST_SRC) rdp.cpp bridge.cpp main.cpp $(RSRC)/subscriber.cpp $(RSRC)/server.cpp $(RSRC)/jpeg.cpp \
	$(RSRC)/tls.cpp $(RSRC)/access.cpp

.PHONY: all test-host clean
all: $(OUT)/windows-bridge $(OUT)/test-host

$(OUT)/windows-bridge: $(ALL_SRC) *.h $(RSRC)/*.h
	@mkdir -p $(OUT)
	$(CXX) $(CXXFLAGS) $(FREERDP_CFLAGS) -o $@ $(ALL_SRC) $(FREERDP_LIBS) $(LDLIBS)

# g++, x264, libpng and OpenSSL; no FreeRDP.
$(OUT)/test-host: test_host.cpp $(HOST_SRC) *.h $(RSRC)/*.h
	@mkdir -p $(OUT)
	$(CXX) -O1 -std=c++17 -Wall -Wextra -Wno-unused-parameter -I. -I$(RSRC) -I$(RTESTS) -o $@ test_host.cpp $(HOST_SRC) $(LDLIBS)

test-host: $(OUT)/test-host
	$(OUT)/test-host

clean:
	rm -rf $(OUT)
