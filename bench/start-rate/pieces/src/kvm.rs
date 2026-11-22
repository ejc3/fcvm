//! A bare arm64 KVM VM, made with the ioctls Firecracker issues for a 1-vCPU microVM: the VM,
//! one memory slot, one initialised vCPU and a GICv3. Dropping it closes the descriptors, and
//! the last close destroys the VM.

use crate::pieces::{ms, Stages};
use anyhow::{bail, Result};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Instant;

const KVMIO: u64 = 0xAE;
const fn io(nr: u64) -> u64 {
    (KVMIO << 8) | nr
}
const fn iow(nr: u64, size: u64) -> u64 {
    (1 << 30) | (size << 16) | (KVMIO << 8) | nr
}
const fn ior(nr: u64, size: u64) -> u64 {
    (2 << 30) | (size << 16) | (KVMIO << 8) | nr
}
const fn iowr(nr: u64, size: u64) -> u64 {
    (3 << 30) | (size << 16) | (KVMIO << 8) | nr
}

#[repr(C)]
struct MemoryRegion {
    slot: u32,
    flags: u32,
    guest_phys_addr: u64,
    memory_size: u64,
    userspace_addr: u64,
}

#[repr(C)]
#[derive(Default)]
struct VcpuInit {
    target: u32,
    features: [u32; 7],
}

#[repr(C)]
struct CreateDevice {
    kind: u32,
    fd: u32,
    flags: u32,
}

#[repr(C)]
struct DeviceAttr {
    flags: u32,
    group: u32,
    attr: u64,
    addr: u64,
}

const KVM_CREATE_VM: u64 = io(0x01);
const KVM_CREATE_VCPU: u64 = io(0x41);
const KVM_SET_USER_MEMORY_REGION: u64 = iow(0x46, std::mem::size_of::<MemoryRegion>() as u64);
const KVM_ARM_VCPU_INIT: u64 = iow(0xae, std::mem::size_of::<VcpuInit>() as u64);
const KVM_ARM_PREFERRED_TARGET: u64 = ior(0xaf, std::mem::size_of::<VcpuInit>() as u64);
const KVM_CREATE_DEVICE: u64 = iowr(0xe0, std::mem::size_of::<CreateDevice>() as u64);
const KVM_SET_DEVICE_ATTR: u64 = iow(0xe1, std::mem::size_of::<DeviceAttr>() as u64);
const KVM_DEV_TYPE_ARM_VGIC_V3: u32 = 7;
const KVM_DEV_ARM_VGIC_GRP_ADDR: u32 = 0;
const KVM_DEV_ARM_VGIC_GRP_NR_IRQS: u32 = 3;
const KVM_DEV_ARM_VGIC_GRP_CTRL: u32 = 4;
const KVM_VGIC_V3_ADDR_TYPE_DIST: u64 = 2;
const KVM_VGIC_V3_ADDR_TYPE_REDIST: u64 = 3;
const KVM_DEV_ARM_VGIC_CTRL_INIT: u64 = 0;
/// Firecracker's arm64 layout: DRAM from 2 GiB, the GIC just under 1 GiB.
const DRAM_START: u64 = 0x8000_0000;
const GIC_DIST: u64 = 0x3fff_0000;
const GIC_REDIST_ONE_VCPU: u64 = GIC_DIST - 0x2_0000;
const MEMORY: u64 = 256 << 20;
const NR_IRQS: u32 = 128;

pub struct BareVm {
    _gic: OwnedFd,
    _vcpu: OwnedFd,
    _vm: OwnedFd,
    _kvm: OwnedFd,
    memory: *mut libc::c_void,
}

// The mapping is only unmapped on drop; nothing else touches it.
unsafe impl Send for BareVm {}

impl Drop for BareVm {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.memory, MEMORY as usize) };
    }
}

fn check(what: &str, ret: libc::c_int) -> Result<libc::c_int> {
    if ret < 0 {
        bail!("{what}: {}", std::io::Error::last_os_error());
    }
    Ok(ret)
}

fn owned(what: &str, ret: libc::c_int) -> Result<OwnedFd> {
    Ok(unsafe { OwnedFd::from_raw_fd(check(what, ret)?) })
}

fn set_attr(gic: &OwnedFd, group: u32, attr: u64, value: *const libc::c_void) -> Result<()> {
    let a = DeviceAttr {
        flags: 0,
        group,
        attr,
        addr: value as u64,
    };
    check("KVM_SET_DEVICE_ATTR", unsafe {
        libc::ioctl(gic.as_raw_fd(), KVM_SET_DEVICE_ATTR as _, &a)
    })?;
    Ok(())
}

impl BareVm {
    pub fn create(stages: &mut Stages) -> Result<Self> {
        let t = Instant::now();
        let kvm = owned("open /dev/kvm", unsafe {
            libc::open(c"/dev/kvm".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC)
        })?;
        let vm = owned("KVM_CREATE_VM", unsafe {
            libc::ioctl(kvm.as_raw_fd(), KVM_CREATE_VM as _, 0)
        })?;
        stages.push(("create-vm", ms(t)));

        let t = Instant::now();
        let memory = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                MEMORY as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        if memory == libc::MAP_FAILED {
            bail!("mmap of guest memory: {}", std::io::Error::last_os_error());
        }
        let region = MemoryRegion {
            slot: 0,
            flags: 0,
            guest_phys_addr: DRAM_START,
            memory_size: MEMORY,
            userspace_addr: memory as u64,
        };
        let r = unsafe { libc::ioctl(vm.as_raw_fd(), KVM_SET_USER_MEMORY_REGION as _, &region) };
        if r < 0 {
            let error = std::io::Error::last_os_error();
            unsafe { libc::munmap(memory, MEMORY as usize) };
            bail!("KVM_SET_USER_MEMORY_REGION: {error}");
        }
        stages.push(("memslot", ms(t)));
        // From here the mapping is owned by the value under construction.
        let unmap = scopeguard(memory);

        let t = Instant::now();
        let vcpu = owned("KVM_CREATE_VCPU", unsafe {
            libc::ioctl(vm.as_raw_fd(), KVM_CREATE_VCPU as _, 0)
        })?;
        let mut init = VcpuInit::default();
        check("KVM_ARM_PREFERRED_TARGET", unsafe {
            libc::ioctl(vm.as_raw_fd(), KVM_ARM_PREFERRED_TARGET as _, &mut init)
        })?;
        // PSCI 0.2, as Firecracker asks for.
        init.features[0] |= 1 << 2;
        check("KVM_ARM_VCPU_INIT", unsafe {
            libc::ioctl(vcpu.as_raw_fd(), KVM_ARM_VCPU_INIT as _, &init)
        })?;
        stages.push(("vcpu", ms(t)));

        let t = Instant::now();
        let mut dev = CreateDevice {
            kind: KVM_DEV_TYPE_ARM_VGIC_V3,
            fd: 0,
            flags: 0,
        };
        check("KVM_CREATE_DEVICE vgic-v3", unsafe {
            libc::ioctl(vm.as_raw_fd(), KVM_CREATE_DEVICE as _, &mut dev)
        })?;
        let gic = unsafe { OwnedFd::from_raw_fd(dev.fd as i32) };
        let dist = GIC_DIST;
        let redist = GIC_REDIST_ONE_VCPU;
        set_attr(
            &gic,
            KVM_DEV_ARM_VGIC_GRP_ADDR,
            KVM_VGIC_V3_ADDR_TYPE_DIST,
            &dist as *const u64 as _,
        )?;
        set_attr(
            &gic,
            KVM_DEV_ARM_VGIC_GRP_ADDR,
            KVM_VGIC_V3_ADDR_TYPE_REDIST,
            &redist as *const u64 as _,
        )?;
        let nr = NR_IRQS;
        set_attr(
            &gic,
            KVM_DEV_ARM_VGIC_GRP_NR_IRQS,
            0,
            &nr as *const u32 as _,
        )?;
        set_attr(
            &gic,
            KVM_DEV_ARM_VGIC_GRP_CTRL,
            KVM_DEV_ARM_VGIC_CTRL_INIT,
            std::ptr::null(),
        )?;
        stages.push(("gic", ms(t)));

        Ok(BareVm {
            _gic: gic,
            _vcpu: vcpu,
            _vm: vm,
            _kvm: kvm,
            memory: unmap.release(),
        })
    }
}

/// Unmaps guest memory if construction fails after the memory slot.
struct Unmap(Option<*mut libc::c_void>);
fn scopeguard(memory: *mut libc::c_void) -> Unmap {
    Unmap(Some(memory))
}
impl Unmap {
    fn release(mut self) -> *mut libc::c_void {
        self.0.take().unwrap()
    }
}
impl Drop for Unmap {
    fn drop(&mut self) {
        if let Some(memory) = self.0 {
            unsafe { libc::munmap(memory, MEMORY as usize) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_numbers_match_the_kernel_headers() {
        // From <linux/kvm.h> on arm64.
        assert_eq!(KVM_CREATE_VM, 0xae01);
        assert_eq!(KVM_CREATE_VCPU, 0xae41);
        assert_eq!(KVM_SET_USER_MEMORY_REGION, 0x4020_ae46);
        assert_eq!(KVM_ARM_VCPU_INIT, 0x4020_aeae);
        assert_eq!(KVM_ARM_PREFERRED_TARGET, 0x8020_aeaf);
        assert_eq!(KVM_CREATE_DEVICE, 0xc00c_aee0);
        assert_eq!(KVM_SET_DEVICE_ATTR, 0x4018_aee1);
    }
}
