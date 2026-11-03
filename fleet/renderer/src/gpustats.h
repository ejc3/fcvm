// GPU name, temperature, power, utilisation, memory and clocks through NVML, loaded at
// run time from libnvidia-ml.so.1 (provided by CDI). Sampled once a second.
#pragma once
#include <atomic>
#include <mutex>
#include <string>
#include <thread>

struct GpuSample {
  std::string name;
  double temp_c = -1, power_w = -1, util_pct = -1, mem_used_mb = -1, mem_total_mb = -1;
  // graphics clocks, MHz: now, the application clock (what a busy GPU runs at) and the board's maximum
  double clock_mhz = -1, app_clock_mhz = -1, max_clock_mhz = -1;
  bool ok = false;
  std::string error;
};

class GpuStats {
 public:
  // pci_bus_id as CUDA reports it ("0000:00:05.0"); selects the NVML device.
  void start(const std::string& pci_bus_id);
  void stop();
  GpuSample latest();

 private:
  void run();
  std::string bus_;
  std::thread th_;
  std::atomic<bool> stop_{false};
  std::mutex mu_;
  GpuSample cur_;
};
