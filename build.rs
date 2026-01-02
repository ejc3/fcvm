fn main() {
    // No compile-time kernel SHA computation.
    // All kernel configuration is read from rootfs-config.toml at runtime.
    // The binary has no hardcoded knowledge of kernel build inputs.
    //
    // Kernel SHA is computed at runtime from the `build_inputs` list in
    // [kernel_profiles.<name>.<arch>] config section.
}
