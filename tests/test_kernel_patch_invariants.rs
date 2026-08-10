//! Compile-time guards for architecture-specific kernel patches.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// Apply the ARM64 vsock patch to a minimal source file with the same line and
/// statement layout as Linux 7.1.7, then ask a C compiler to parse it.
///
/// A zero-context hunk once inserted `dsb(sy)` between the two lines of the
/// `vsock = container_of(...)` declaration. `patch --dry-run` accepted that
/// placement, but the kernel build failed because a statement cannot appear in
/// the middle of an initializer. Keeping the split declaration in this fixture
/// makes that exact regression deterministic without downloading a kernel.
#[test]
fn arm64_vsock_rx_barrier_patch_preserves_the_lock_boundary_syntax() {
    let temp = tempfile::tempdir().expect("create isolated kernel-patch fixture");
    let source_dir = temp.path().join("net/vmw_vsock");
    let linux_include = temp.path().join("include/linux");
    let asm_include = temp.path().join("include/asm");
    fs::create_dir_all(&source_dir).expect("create fixture source directory");
    fs::create_dir_all(&linux_include).expect("create fixture Linux include directory");
    fs::create_dir_all(&asm_include).expect("create fixture ARM include directory");

    fs::write(
        linux_include.join("virtio_vsock.h"),
        r#"#include <stddef.h>
struct work_struct { int unused; };
struct delayed_work { int unused; };
struct virtqueue { int unused; };
struct mutex { int unused; };
struct virtio_vsock {
    struct delayed_work rx_work;
    struct virtqueue *vqs[1];
    struct mutex rx_lock;
    int rx_run;
};
#define VSOCK_VQ_RX 0
#define to_delayed_work(work) ((struct delayed_work *)(work))
#define container_of(ptr, type, member) ((type *)0)
#define mutex_lock(lock) ((void)(lock))
"#,
    )
    .expect("write fixture Linux header");
    fs::write(
        asm_include.join("barrier.h"),
        "#define dsb(opt) do { } while (0)\n",
    )
    .expect("write fixture ARM barrier header");

    let mut lines = Vec::new();
    while lines.len() < 18 {
        lines.push(String::new());
    }
    lines.push("#include <linux/virtio_vsock.h>".to_owned()); // Linux 7.1.7 line 19.
    while lines.len() < 630 {
        lines.push(String::new());
    }
    lines.extend([
        "static void virtio_transport_rx_work(struct work_struct *work)".to_owned(),
        "{".to_owned(),
        "\tstruct virtio_vsock *vsock =".to_owned(),
        "\t\tcontainer_of(to_delayed_work(work), struct virtio_vsock, rx_work);".to_owned(),
        "\tstruct virtqueue *vq = vsock->vqs[VSOCK_VQ_RX];".to_owned(),
        "\tsize_t buf_len;".to_owned(),
        "\tvoid *buf;".to_owned(),
        String::new(),
        "\tmutex_lock(&vsock->rx_lock);".to_owned(),
        String::new(),
        "\tif (!vsock->rx_run)".to_owned(),
        "\t\tgoto out;".to_owned(),
        "\t(void)vq;".to_owned(),
        "\t(void)buf_len;".to_owned(),
        "\t(void)buf;".to_owned(),
        "out:".to_owned(),
        "\t;".to_owned(),
        "}".to_owned(),
    ]);
    let source = source_dir.join("virtio_transport.c");
    fs::write(&source, format!("{}\n", lines.join("\n"))).expect("write fixture source");

    let patch_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("kernel/patches-arm64/nv2-vsock-rx-barrier.patch");
    let applied = Command::new("patch")
        .args(["--batch", "--forward", "-p1", "-i"])
        .arg(&patch_path)
        .current_dir(temp.path())
        .output()
        .expect("run patch on the Linux 7.1.7 fixture");
    assert!(
        applied.status.success(),
        "ARM64 vsock patch did not apply to its Linux 7.1.7 fixture:\n{}{}",
        String::from_utf8_lossy(&applied.stdout),
        String::from_utf8_lossy(&applied.stderr)
    );

    let parsed = Command::new("cc")
        .args(["-DCONFIG_ARM64", "-fsyntax-only", "-I"])
        .arg(temp.path().join("include"))
        .arg(&source)
        .output()
        .expect("run the C syntax check");
    assert!(
        parsed.status.success(),
        "ARM64 vsock patch broke C syntax at the RX lock boundary:\n{}{}",
        String::from_utf8_lossy(&parsed.stdout),
        String::from_utf8_lossy(&parsed.stderr)
    );

    let applied_source = fs::read_to_string(source).expect("read patched fixture source");
    let lock = applied_source
        .find("mutex_lock(&vsock->rx_lock);")
        .expect("patched source lost rx_lock acquisition");
    let barrier = applied_source
        .find("dsb(sy);")
        .expect("patched source lost the ARM64 DSB barrier");
    let guard = applied_source
        .find("if (!vsock->rx_run)")
        .expect("patched source lost the rx_run guard");
    assert!(
        lock < barrier && barrier < guard,
        "the DSB must execute after rx_lock acquisition and before the rx_run guard"
    );
}
