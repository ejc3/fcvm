#include "gpustats.h"

#include <dlfcn.h>

#include <chrono>
#include <cstring>

namespace {
typedef int nvmlReturn_t;
typedef void* nvmlDevice_t;
struct nvmlUtilization_t { unsigned int gpu, memory; };
struct nvmlMemory_t { unsigned long long total, free, used; };
struct Nvml {
  nvmlReturn_t (*init)();
  nvmlReturn_t (*by_bus)(const char*, nvmlDevice_t*);
  nvmlReturn_t (*by_index)(unsigned int, nvmlDevice_t*);
  nvmlReturn_t (*name)(nvmlDevice_t, char*, unsigned int);
  nvmlReturn_t (*temp)(nvmlDevice_t, int, unsigned int*);
  nvmlReturn_t (*power)(nvmlDevice_t, unsigned int*);
  nvmlReturn_t (*util)(nvmlDevice_t, nvmlUtilization_t*);
  nvmlReturn_t (*mem)(nvmlDevice_t, nvmlMemory_t*);
};
}  // namespace

void GpuStats::start(const std::string& pci_bus_id) {
  bus_ = pci_bus_id;
  th_ = std::thread([this] { run(); });
}

void GpuStats::stop() {
  stop_ = true;
  if (th_.joinable()) th_.join();
}

GpuSample GpuStats::latest() {
  std::lock_guard<std::mutex> l(mu_);
  return cur_;
}

void GpuStats::run() {
  auto fail = [this](const std::string& e) {
    std::lock_guard<std::mutex> l(mu_);
    cur_.ok = false;
    cur_.error = e;
  };
  void* lib = dlopen("libnvidia-ml.so.1", RTLD_NOW);
  if (!lib) { fail(std::string("dlopen libnvidia-ml.so.1: ") + dlerror()); return; }
  Nvml n;
  n.init = (nvmlReturn_t(*)())dlsym(lib, "nvmlInit_v2");
  n.by_bus = (nvmlReturn_t(*)(const char*, nvmlDevice_t*))dlsym(lib, "nvmlDeviceGetHandleByPciBusId_v2");
  n.by_index = (nvmlReturn_t(*)(unsigned int, nvmlDevice_t*))dlsym(lib, "nvmlDeviceGetHandleByIndex_v2");
  n.name = (nvmlReturn_t(*)(nvmlDevice_t, char*, unsigned int))dlsym(lib, "nvmlDeviceGetName");
  n.temp = (nvmlReturn_t(*)(nvmlDevice_t, int, unsigned int*))dlsym(lib, "nvmlDeviceGetTemperature");
  n.power = (nvmlReturn_t(*)(nvmlDevice_t, unsigned int*))dlsym(lib, "nvmlDeviceGetPowerUsage");
  n.util = (nvmlReturn_t(*)(nvmlDevice_t, nvmlUtilization_t*))dlsym(lib, "nvmlDeviceGetUtilizationRates");
  n.mem = (nvmlReturn_t(*)(nvmlDevice_t, nvmlMemory_t*))dlsym(lib, "nvmlDeviceGetMemoryInfo");
  if (!n.init || !n.by_index || !n.name || !n.temp || !n.power || !n.util || !n.mem) { fail("NVML lacks a needed symbol"); return; }
  if (n.init() != 0) { fail("nvmlInit failed"); return; }
  nvmlDevice_t dev = nullptr;
  if (!(n.by_bus && !bus_.empty() && n.by_bus(bus_.c_str(), &dev) == 0) && n.by_index(0, &dev) != 0) {
    fail("no NVML device for " + bus_);
    return;
  }
  char name[96] = {0};
  n.name(dev, name, sizeof name);
  while (!stop_) {
    GpuSample s;
    s.name = name;
    unsigned int v = 0;
    if (n.temp(dev, 0 /*NVML_TEMPERATURE_GPU*/, &v) == 0) s.temp_c = v;
    if (n.power(dev, &v) == 0) s.power_w = v / 1000.0;
    nvmlUtilization_t u{};
    if (n.util(dev, &u) == 0) s.util_pct = u.gpu;
    nvmlMemory_t m{};
    if (n.mem(dev, &m) == 0) { s.mem_used_mb = m.used / 1048576.0; s.mem_total_mb = m.total / 1048576.0; }
    s.ok = true;
    {
      std::lock_guard<std::mutex> l(mu_);
      cur_ = s;
    }
    for (int i = 0; i < 10 && !stop_; i++) std::this_thread::sleep_for(std::chrono::milliseconds(100));
  }
}
