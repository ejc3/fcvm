//! Replaying a recorded working set into a restoring clone.
//!
//! [`plan`] turns a [`PageSet`](super::working_set::PageSet) of snapshot file offsets into the
//! host-address ranges of one particular clone, coalesced into as few pieces as possible;
//! [`populate`] materialises one piece with a single `UFFDIO_COPY`/`UFFDIO_CONTINUE`.
//!
//! Two properties matter and are enforced here rather than by the caller:
//!
//! * **Speculation never blocks demand.** A piece is populated in bounded chunks so the fault
//!   handler can interleave real faults between them; a guest that resumes mid-prefetch waits
//!   for at most one chunk, not for the whole set.
//! * **Speculation never breaks the guest.** Nothing in here can fail a restore. A page that
//!   is already present, a clone that exits mid-prefetch, or a kernel error all stop the
//!   prefetch and leave the VM to fault on demand, which is exactly the behaviour of a
//!   snapshot with no recording at all.

use tracing::debug;
use userfaultfd::Uffd;

use super::working_set::PageSet;

/// Bytes populated per ioctl.
///
/// This is the latency a demand fault can spend queued behind speculation, so it is small
/// enough to be invisible (~100 us of `memcpy` on this host) while still amortising the ioctl
/// over 512 4 KiB pages. It is also exactly one 2 MiB granule, so a hugepage clone never gets
/// a chunk that splits a page.
pub const CHUNK_BYTES: usize = 2 * 1024 * 1024;

/// One guest memory region of a clone, as needed to place snapshot offsets in its address
/// space. A view of the handshake's `GuestRegionUffdMapping`, not the wire type.
#[derive(Debug, Clone, Copy)]
pub struct Region {
    /// Where this region starts in the clone's address space.
    pub base_host_virt_addr: u64,
    /// Where this region starts in the snapshot memory file.
    pub file_offset: u64,
    /// Length of the region, in bytes.
    pub size: usize,
}

/// A contiguous piece of prefetch work: `len` bytes of the snapshot at `file_offset`, to be
/// materialised at `host_addr` in the clone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    pub host_addr: usize,
    pub file_offset: u64,
    pub len: usize,
}

/// Where prefetched bytes come from — the mode-specific half of [`populate`].
pub enum Source<'a> {
    /// A read-only mapping of the snapshot memory file. Pages are copied out of it into the
    /// clone's private anonymous memory (`UFFDIO_COPY`).
    Copy(&'a [u8]),
    /// The clone already maps the shared snapshot memfd `MAP_PRIVATE`; the page only needs a
    /// (read-only) PTE (`UFFDIO_CONTINUE`). No bytes move.
    Minor,
}

/// Why a prefetch stopped short. Neither variant is an error the caller propagates: both mean
/// "these pages stay unpopulated and will fault on demand", which is the no-recording
/// behaviour.
#[derive(Debug, PartialEq, Eq)]
pub enum Stop {
    /// The clone's address space is gone (it exited). Stop prefetching, quietly.
    VmGone,
    /// The kernel refused; the reason is logged.
    Refused,
}

/// Place a recorded working set in one clone's address space.
///
/// Runs are clipped to the regions the clone actually mapped, aligned out to its page size
/// (prefetching a few neighbouring pages is free; a partial page is not addressable), clipped
/// to the memory file, and finally merged so alignment cannot produce overlapping copies.
pub fn plan(set: &PageSet, regions: &[Region], page_size: usize, mem_len: u64) -> Vec<Segment> {
    let mut segments: Vec<Segment> = Vec::new();

    for run in set.runs() {
        let run_end = run.offset.saturating_add(run.len).min(mem_len);
        if run_end <= run.offset {
            continue;
        }
        for region in regions {
            let region_end = region.file_offset.saturating_add(region.size as u64);
            let start = run.offset.max(region.file_offset);
            let end = run_end.min(region_end).min(mem_len);
            if end <= start {
                continue;
            }

            // Align within the region: the region base is page-aligned, so aligning the
            // offset *relative to it* keeps host addresses aligned too.
            let rel_start = (start - region.file_offset) as usize & !(page_size - 1);
            let rel_end = ((end - region.file_offset) as usize)
                .next_multiple_of(page_size)
                .min(region.size);
            if rel_end <= rel_start {
                continue;
            }
            // A page that runs past the end of the memory file cannot be copied whole; leave
            // it to the fault path, which zero-fills the missing tail.
            let file_offset = region.file_offset + rel_start as u64;
            let len = rel_end - rel_start;
            if file_offset + len as u64 > mem_len {
                continue;
            }

            let segment = Segment {
                host_addr: (region.base_host_virt_addr + rel_start as u64) as usize,
                file_offset,
                len,
            };

            // Alignment can make this segment overlap or abut the previous one (two runs in
            // the same page, or two adjacent pages). Merge instead of copying twice.
            //
            // The gap is computed with `checked_sub` rather than compared: guest memory
            // regions are separate mmaps, so a region holding LATER file offsets can sit at a
            // LOWER host address, and a plain subtraction would underflow on such a snapshot
            // (panicking in debug, silently wrapping in release).
            let merged = segments.last_mut().is_some_and(|prev| {
                match segment.host_addr.checked_sub(prev.host_addr) {
                    Some(gap)
                        if gap <= prev.len
                            && segment.file_offset == prev.file_offset + gap as u64 =>
                    {
                        prev.len = prev.len.max(gap + segment.len);
                        true
                    }
                    _ => false,
                }
            });
            if !merged {
                segments.push(segment);
            }
        }
    }

    segments
}

/// Materialise up to [`CHUNK_BYTES`] of `segment`, starting `done` bytes into it, in ONE
/// ioctl.
///
/// Returns how many bytes are now resident from that point, or the outcome that stopped it.
/// The caller drives this one chunk at a time so it can serve demand faults in between.
/// Partial progress is normal and is reported so the caller can continue from there:
///
/// * `EAGAIN` after progress — the kernel stopped early (`mmap_changing`, or it hit a page
///   that is already present); the copied prefix still counts.
/// * `EEXIST` with no progress — the page is already resident (a demand fault beat us to it,
///   or a previous chunk covered it). Skip exactly one page and carry on.
/// * `ESRCH` — the clone's mm is gone.
pub fn populate_chunk(
    uffd: &Uffd,
    source: &Source<'_>,
    segment: &Segment,
    done: usize,
    page_size: usize,
    vm_id: &str,
) -> Result<usize, Stop> {
    debug_assert!(done < segment.len);
    let host_addr = segment.host_addr + done;
    let file_offset = segment.file_offset + done as u64;
    let len = CHUNK_BYTES.min(segment.len - done);
    let dst = host_addr as *mut std::ffi::c_void;
    let result = match source {
        Source::Copy(mmap) => {
            // Checked, not indexed. `plan` bounds segments by the `mem_len` it was given,
            // which is a DIFFERENT value from this mapping's length (the caller passes
            // `ctx.mem_size` and `&mmap[..]` separately). They are equal today - both come
            // from the same `metadata().len()` - but nothing enforces it here, and this
            // module promises at the top of the file that it can never fail a restore. A
            // slice index would make a broken invariant a panic inside the fault-handler
            // task, which hangs the guest; refusing degrades to demand paging instead.
            let Some(chunk) = mmap.get(file_offset as usize..file_offset as usize + len) else {
                debug!(
                    target: "uffd",
                    vm_id = %vm_id,
                    file_offset,
                    len,
                    mapped = mmap.len(),
                    "prefetch chunk runs past the snapshot mapping"
                );
                return Err(Stop::Refused);
            };
            // SAFETY: `chunk` is a `len`-byte slice of the snapshot mapping, and `dst` is a
            // page-aligned range inside a region the clone registered with this uffd.
            unsafe { uffd.copy(chunk.as_ptr() as *const std::ffi::c_void, dst, len, true) }
        }
        Source::Minor => uffd
            .r#continue(dst, len, true)
            .map(|mapped| mapped as usize),
    };

    match result {
        Ok(copied) if copied > 0 => Ok(copied),
        // The kernel never reports zero-byte success; treat it as no progress rather than
        // spinning on the same address forever.
        Ok(_) => Ok(page_size),
        Err(userfaultfd::Error::PartiallyCopied(copied)) if copied > 0 => Ok(copied),
        Err(e) => match errno_of(&e) {
            Some(libc::EEXIST) => Ok(page_size),
            Some(libc::ESRCH) => Err(Stop::VmGone),
            // Zero-progress EAGAIN: an event-generating operation is in flight
            // (`mmap_changing`). Nothing is waiting on a speculative page, so give up on
            // this segment rather than spinning; the pages fault on demand.
            Some(libc::EAGAIN) => {
                debug!(
                    target: "uffd",
                    vm_id = %vm_id,
                    addr = format!("0x{host_addr:x}"),
                    "prefetch chunk deferred (mmap_changing)"
                );
                Err(Stop::Refused)
            }
            _ => {
                debug!(
                    target: "uffd",
                    vm_id = %vm_id,
                    addr = format!("0x{host_addr:x}"),
                    error = ?e,
                    "prefetch chunk refused by kernel"
                );
                Err(Stop::Refused)
            }
        },
    }
}

/// The raw errno behind a `userfaultfd` error, whichever variant it arrived in.
///
/// The crate pins its own `nix`, so the `Errno` values cannot be matched directly against
/// fcvm's — compare the raw integer instead.
fn errno_of(e: &userfaultfd::Error) -> Option<i32> {
    match e {
        userfaultfd::Error::CopyFailed(errno) | userfaultfd::Error::ZeropageFailed(errno) => {
            Some(*errno as i32)
        }
        userfaultfd::Error::SystemError(errno) => Some(*errno as i32),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::uffd::working_set::GRANULE;

    const G: u64 = GRANULE;
    const PAGE: usize = 4096;

    fn set_of(mem_len: u64, granules: &[u64]) -> PageSet {
        let mut set = PageSet::empty(mem_len);
        for g in granules {
            set.insert_range(g * G, G);
        }
        set
    }

    /// One region at a non-zero host address: file offsets must land at the right host
    /// addresses, and contiguous granules must become ONE segment.
    #[test]
    fn plan_maps_file_offsets_into_the_clone_and_coalesces() {
        let mem_len = 64 * G;
        let regions = [Region {
            base_host_virt_addr: 0x7f00_0000_0000,
            file_offset: 0,
            size: (64 * G) as usize,
        }];
        let set = set_of(mem_len, &[0, 1, 2, 9]);

        assert_eq!(
            plan(&set, &regions, PAGE, mem_len),
            vec![
                Segment {
                    host_addr: 0x7f00_0000_0000,
                    file_offset: 0,
                    len: 3 * PAGE
                },
                Segment {
                    host_addr: 0x7f00_0000_0000 + 9 * PAGE,
                    file_offset: 9 * G,
                    len: PAGE
                },
            ]
        );
    }

    /// Firecracker splits guest memory into several regions with their own host bases; a run
    /// that spans the boundary must be split at it, not copied across it.
    #[test]
    fn plan_splits_runs_at_region_boundaries() {
        let mem_len = 32 * G;
        let regions = [
            Region {
                base_host_virt_addr: 0x1000_0000,
                file_offset: 0,
                size: (16 * G) as usize,
            },
            Region {
                base_host_virt_addr: 0x9000_0000, // deliberately NOT contiguous with region 0
                file_offset: 16 * G,
                size: (16 * G) as usize,
            },
        ];
        // One run straddling the boundary: granules 15,16,17.
        let set = set_of(mem_len, &[15, 16, 17]);

        assert_eq!(
            plan(&set, &regions, PAGE, mem_len),
            vec![
                Segment {
                    host_addr: 0x1000_0000 + 15 * PAGE,
                    file_offset: 15 * G,
                    len: PAGE
                },
                Segment {
                    host_addr: 0x9000_0000,
                    file_offset: 16 * G,
                    len: 2 * PAGE
                },
            ]
        );
    }

    /// Guest memory regions are independent mmaps: the region holding LATER file offsets can
    /// sit at a LOWER host address. Planning must handle that without underflowing the
    /// merge arithmetic, and must place every page at its own region's base.
    #[test]
    fn plan_handles_regions_whose_host_addresses_descend() {
        let mem_len = 32 * G;
        let regions = [
            Region {
                base_host_virt_addr: 0x9000_0000, // low file offset, HIGH host address
                file_offset: 0,
                size: (16 * G) as usize,
            },
            Region {
                base_host_virt_addr: 0x1000_0000, // high file offset, LOW host address
                file_offset: 16 * G,
                size: (16 * G) as usize,
            },
        ];
        let set = set_of(mem_len, &[15, 16, 17]);

        assert_eq!(
            plan(&set, &regions, PAGE, mem_len),
            vec![
                Segment {
                    host_addr: 0x9000_0000 + 15 * PAGE,
                    file_offset: 15 * G,
                    len: PAGE
                },
                Segment {
                    host_addr: 0x1000_0000,
                    file_offset: 16 * G,
                    len: 2 * PAGE
                },
            ]
        );
    }

    /// A 4 KiB-granular recording replayed into a 2 MiB-backed clone: every segment must be
    /// hugepage-aligned and hugepage-sized, and two granules in the same huge page must
    /// produce ONE segment rather than two overlapping ones.
    #[test]
    fn plan_aligns_up_to_the_clone_page_size_without_overlapping() {
        const HUGE: usize = 2 * 1024 * 1024;
        let mem_len = 8 * HUGE as u64;
        let regions = [Region {
            base_host_virt_addr: 0x4000_0000,
            file_offset: 0,
            size: 8 * HUGE,
        }];
        // Granules 1 and 300 are both inside the FIRST 2 MiB page (512 granules); 700 is in
        // the second.
        let set = set_of(mem_len, &[1, 300, 700]);

        let segments = plan(&set, &regions, HUGE, mem_len);
        assert_eq!(
            segments,
            vec![Segment {
                host_addr: 0x4000_0000,
                file_offset: 0,
                len: 2 * HUGE
            }],
            "three scattered 4 KiB granules in two adjacent huge pages = one 4 MiB copy"
        );
        for s in &segments {
            assert_eq!(s.host_addr % HUGE, 0, "segment must be hugepage-aligned");
            assert_eq!(
                s.len % HUGE,
                0,
                "segment must be a whole number of huge pages"
            );
        }
    }

    /// Pages outside what the clone mapped, and pages past the end of the memory file, are
    /// dropped rather than turned into an out-of-bounds copy.
    #[test]
    fn plan_drops_pages_outside_the_regions_and_the_file() {
        let mem_len = 8 * G;
        let regions = [Region {
            base_host_virt_addr: 0x2000_0000,
            file_offset: 2 * G,
            size: (4 * G) as usize, // covers file [2G, 6G)
        }];
        // 0 and 1 are before the region, 6 and 7 after it.
        let set = set_of(mem_len, &[0, 1, 3, 6, 7]);

        assert_eq!(
            plan(&set, &regions, PAGE, mem_len),
            vec![Segment {
                host_addr: 0x2000_0000 + PAGE,
                file_offset: 3 * G,
                len: PAGE
            }]
        );

        // A region that claims more than the file holds: the tail page is not copyable.
        let short_file = 3 * G + 100;
        let set = set_of(mem_len, &[3]);
        assert!(
            plan(&set, &regions, PAGE, short_file).is_empty(),
            "a page that runs past the end of the memory file must be left to the fault path"
        );
    }

    #[test]
    fn plan_of_an_empty_set_is_empty() {
        let regions = [Region {
            base_host_virt_addr: 0x1000,
            file_offset: 0,
            size: (8 * G) as usize,
        }];
        assert!(plan(&PageSet::empty(8 * G), &regions, PAGE, 8 * G).is_empty());
    }

    // =========================================================================
    // Against a real userfaultfd. These stand in for a clone's guest memory:
    // anonymous MAP_PRIVATE pages registered MISSING, exactly what Firecracker
    // hands the server in COPY mode.
    // =========================================================================

    use std::os::unix::io::AsRawFd;

    /// Anonymous private memory, like a clone's guest RAM before restore.
    fn guest_memory(len: usize) -> usize {
        // SAFETY: a fresh anonymous mapping; the address is returned to the caller.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(p, libc::MAP_FAILED, "mmap guest memory");
        p as usize
    }

    fn register_missing(base: usize, len: usize) -> Uffd {
        let uffd = userfaultfd::UffdBuilder::new()
            .close_on_exec(true)
            .non_blocking(true)
            .user_mode_only(true)
            .create()
            .expect("creating userfaultfd (via /dev/userfaultfd)");
        uffd.register(base as *mut std::ffi::c_void, len)
            .expect("registering guest memory");
        uffd
    }

    /// Block until the uffd has an event, or give up. Event-driven, never a sleep:
    /// `poll` returns the instant the kernel queues the fault.
    fn wait_for_event(uffd: &Uffd, timeout_ms: i32) -> bool {
        let mut pfd = libc::pollfd {
            fd: uffd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one initialised pollfd, owned fd.
        unsafe { libc::poll(&mut pfd, 1, timeout_ms) > 0 }
    }

    fn read_u8(addr: usize) -> u8 {
        // SAFETY: `addr` is inside a live mapping.
        unsafe { std::ptr::read_volatile(addr as *const u8) }
    }

    /// Populate a whole plan, asserting nothing refuses it.
    fn run_plan(uffd: &Uffd, source: &Source<'_>, segments: &[Segment], page_size: usize) {
        for segment in segments {
            let mut done = 0usize;
            while done < segment.len {
                match populate_chunk(uffd, source, segment, done, page_size, "test") {
                    Ok(progress) => done += progress,
                    Err(stop) => panic!("prefetch stopped: {stop:?}"),
                }
            }
        }
    }

    /// A page in the recorded set is resident with the right contents and produces NO fault
    /// when the guest reads it; a page outside the set still faults normally. This is the
    /// whole feature in one test.
    #[test]
    fn prefetched_pages_carry_snapshot_content_and_never_fault() {
        const PAGES: usize = 8;
        let len = PAGES * PAGE;
        // "Snapshot": page i is filled with byte i + 1.
        let snapshot: Vec<u8> = (0..PAGES)
            .flat_map(|i| std::iter::repeat_n(i as u8 + 1, PAGE))
            .collect();

        let base = guest_memory(len);
        let uffd = register_missing(base, len);
        let regions = [Region {
            base_host_virt_addr: base as u64,
            file_offset: 0,
            size: len,
        }];

        let recorded = set_of(len as u64, &[0, 1, 2, 5]);
        let segments = plan(&recorded, &regions, PAGE, len as u64);
        assert_eq!(segments.len(), 2, "0-2 coalesce, 5 stands alone");
        run_plan(&uffd, &Source::Copy(&snapshot), &segments, PAGE);

        for page in [0usize, 1, 2, 5] {
            assert_eq!(
                read_u8(base + page * PAGE),
                page as u8 + 1,
                "prefetched page {page} must hold snapshot content"
            );
        }
        assert!(
            !wait_for_event(&uffd, 0),
            "reading a prefetched page must not generate a fault - that is the point"
        );

        // A page that was NOT recorded still faults, proving the registration is live and
        // that prefetch degrades to demand paging for anything it missed.
        let reader = std::thread::spawn(move || read_u8(base + 3 * PAGE));
        assert!(
            wait_for_event(&uffd, 5000),
            "an unrecorded page must still fault"
        );
        let event = uffd.read_event().unwrap().expect("queued fault");
        let fault_addr = match event {
            userfaultfd::Event::Pagefault { addr, .. } => addr as usize & !(PAGE - 1),
            other => panic!("expected a page fault, got {other:?}"),
        };
        assert_eq!(fault_addr, base + 3 * PAGE);
        // SAFETY: resolving the pending fault at a registered address.
        unsafe {
            uffd.copy(
                snapshot[3 * PAGE..].as_ptr() as *const std::ffi::c_void,
                fault_addr as *mut std::ffi::c_void,
                PAGE,
                true,
            )
        }
        .unwrap();
        assert_eq!(reader.join().unwrap(), 4);

        // SAFETY: unmapping our own mapping.
        unsafe { libc::munmap(base as *mut std::ffi::c_void, len) };
    }

    /// A corrupt record must cost nothing but the prefetch: the clone restores on demand,
    /// with the RIGHT BYTES, and never hangs.
    ///
    /// Ported from the closed #742 branch, which had it as an end-to-end serve test. Valid
    /// magic is the dangerous shape — it is exactly what a length or checksum check can wave
    /// through — and a bitmap that silently replayed the WRONG pages into a guest would look
    /// like success from the outside. So every case here pins all three outcomes: nothing
    /// replayed, every page faulted on demand, and the bytes the guest reads are correct.
    #[test]
    fn a_corrupt_working_set_falls_back_to_on_demand_faulting() {
        use crate::uffd::working_set::WorkingSetStore;

        const PAGES: usize = 6;
        let len = PAGES * PAGE;
        // "Snapshot": page i is filled with byte i + 1.
        let snapshot: Vec<u8> = (0..PAGES)
            .flat_map(|i| std::iter::repeat_n(i as u8 + 1, PAGE))
            .collect();

        // Each case writes a working-set file that has a VALID `FCVMWSET` magic and is
        // unusable after it. Building the bytes by hand (rather than via the encoder) means
        // the test cannot be fooled by a change to the encoder itself.
        type Corrupt = Box<dyn Fn(&std::path::Path, u64)>;
        let cases: Vec<(&str, Corrupt)> = vec![
            // The #742 payload: valid magic, garbage after it, shorter than a header.
            (
                "valid magic, garbage body",
                Box::new(|ws: &std::path::Path, _len: u64| {
                    std::fs::write(ws, b"FCVMWSET this is not a working set").unwrap();
                }),
            ),
            // Structurally perfect for ANOTHER image: right magic, version, granule, image
            // size, granule count and bitmap length, claiming every page. A length or
            // checksum check waves this through; only the identity key rejects it. If it
            // were replayed, a foreign bitmap would populate this guest.
            (
                "well-formed but foreign",
                Box::new(|ws: &std::path::Path, len: u64| {
                    let granules = len.div_ceil(GRANULE);
                    let mut buf = Vec::new();
                    buf.extend_from_slice(b"FCVMWSET");
                    buf.extend_from_slice(&1u64.to_le_bytes()); // version
                    buf.extend_from_slice(&GRANULE.to_le_bytes());
                    buf.extend_from_slice(&len.to_le_bytes());
                    buf.extend_from_slice(&granules.to_le_bytes());
                    buf.extend_from_slice(&[0xAAu8; 32]); // a key that is not this image's
                    for _ in 0..granules.div_ceil(64) {
                        buf.extend_from_slice(&u64::MAX.to_le_bytes()); // every page "recorded"
                    }
                    std::fs::write(ws, &buf).unwrap();
                }),
            ),
            // A genuine record for THIS image, one byte short. Produced through the real API
            // so the header and key are authentic and only the bitmap is truncated.
            (
                "authentic header, truncated bitmap",
                Box::new(|ws: &std::path::Path, len: u64| {
                    let image = ws.with_file_name("memory.bin");
                    let store = WorkingSetStore::open(
                        &image,
                        len,
                        &image.with_file_name("config.json"),
                        &image.with_file_name("snapshot.lock"),
                    )
                    .unwrap();
                    let mut observed = store.recorder();
                    observed.insert_range(0, len);
                    assert!(store.merge_and_persist(&observed).unwrap().persisted);
                    let mut good = std::fs::read(ws).unwrap();
                    good.truncate(good.len() - 1);
                    std::fs::write(ws, &good).unwrap();
                }),
            ),
        ];

        for (label, write_corrupt) in cases {
            let dir = tempfile::tempdir().unwrap();
            let image = dir.path().join("memory.bin");
            let generation_config = dir.path().join("config.json");
            std::fs::write(&generation_config, b"generation-1").unwrap();
            std::fs::write(&image, &snapshot).unwrap();
            write_corrupt(&WorkingSetStore::path_for(&image), len as u64);

            // 1. Nothing is replayed: the record is refused, so there is no plan at all.
            let generation_lock = dir.path().join("snapshot.lock");
            let store =
                WorkingSetStore::open(&image, len as u64, &generation_config, &generation_lock)
                    .unwrap();
            let recorded = store.to_prefetch();
            assert!(
                recorded.is_empty(),
                "[{label}] a corrupt record must never be replayed"
            );

            let base = guest_memory(len);
            let uffd = register_missing(base, len);
            let regions = [Region {
                base_host_virt_addr: base as u64,
                file_offset: 0,
                size: len,
            }];
            let segments = plan(&recorded, &regions, PAGE, len as u64);
            assert!(
                segments.is_empty(),
                "[{label}] nothing to prefetch means no segments"
            );
            run_plan(&uffd, &Source::Copy(&snapshot), &segments, PAGE);

            // 2. Every page is served on demand, and 3. carries the right bytes.
            let mut faults = 0u64;
            for page in 0..PAGES {
                let reader = std::thread::spawn(move || read_u8(base + page * PAGE));
                assert!(
                    wait_for_event(&uffd, 5000),
                    "[{label}] page {page} must fault - the fallback is demand paging"
                );
                let event = uffd.read_event().unwrap().expect("queued fault");
                let fault_addr = match event {
                    userfaultfd::Event::Pagefault { addr, .. } => addr as usize & !(PAGE - 1),
                    other => panic!("[{label}] expected a page fault, got {other:?}"),
                };
                assert_eq!(fault_addr, base + page * PAGE, "[{label}] wrong fault addr");
                faults += 1;
                // SAFETY: resolving the pending fault at a registered address.
                unsafe {
                    uffd.copy(
                        snapshot[page * PAGE..].as_ptr() as *const std::ffi::c_void,
                        fault_addr as *mut std::ffi::c_void,
                        PAGE,
                        true,
                    )
                }
                .unwrap();
                assert_eq!(
                    reader.join().unwrap(),
                    page as u8 + 1,
                    "[{label}] page {page} must hold its snapshot byte"
                );
            }
            assert_eq!(
                faults, PAGES as u64,
                "[{label}] every page must have been served on demand"
            );

            // The corrupt file is REPAIRED rather than left to poison every later restore:
            // the image is unchanged, so publication passes the identity gate and the next
            // server replays a correct record.
            let mut observed = store.recorder();
            observed.insert_range(0, 2 * GRANULE);
            let outcome = store.merge_and_persist(&observed).unwrap();
            assert!(
                outcome.persisted && !outcome.superseded,
                "[{label}] recording over a corrupt record must repair it"
            );
            let repaired =
                WorkingSetStore::open(&image, len as u64, &generation_config, &generation_lock)
                    .unwrap()
                    .to_prefetch();
            assert_eq!(repaired.len(), 2, "[{label}] the repaired record must load");

            // SAFETY: unmapping our own mapping.
            unsafe { libc::munmap(base as *mut std::ffi::c_void, len) };
        }
    }

    /// Prefetching a page the guest already faulted (EEXIST) is a no-op, not a failure: the
    /// two paths race by design.
    #[test]
    fn prefetch_tolerates_an_already_resident_page() {
        let len = 4 * PAGE;
        let snapshot = vec![0xABu8; len];
        let base = guest_memory(len);
        let uffd = register_missing(base, len);
        let segment = Segment {
            host_addr: base,
            file_offset: 0,
            len,
        };

        // Populate once...
        run_plan(&uffd, &Source::Copy(&snapshot), &[segment], PAGE);
        // ...and again over the same range. Every page is present now, so each chunk reports
        // EEXIST with no progress and the loop must still terminate having covered it.
        let mut done = 0usize;
        let mut iterations = 0;
        while done < segment.len {
            done += populate_chunk(
                &uffd,
                &Source::Copy(&snapshot),
                &segment,
                done,
                PAGE,
                "test",
            )
            .expect("EEXIST must not abort a prefetch");
            iterations += 1;
            assert!(iterations <= len / PAGE, "must not spin on a present page");
        }
        assert_eq!(read_u8(base), 0xAB);

        // SAFETY: unmapping our own mapping.
        unsafe { libc::munmap(base as *mut std::ffi::c_void, len) };
    }

    /// ISOLATION. Prefetch touches pages the guest never asked for, so it must be proven that
    /// a prefetched page is private: two clones prefetched from ONE snapshot mapping, one
    /// writes, and the other clone AND the snapshot file on disk must be untouched.
    ///
    /// This is the mechanism-level version of `test_snapshot_clone_isolation_*`; it fails in
    /// milliseconds if someone maps the snapshot writable or shared.
    #[test]
    fn prefetched_pages_are_private_to_each_clone() {
        use std::io::Write;

        const PAGES: usize = 4;
        let len = PAGES * PAGE;

        // A real snapshot file, mapped read-only exactly the way the server maps it.
        let path = std::env::temp_dir().join(format!(
            "fcvm-prefetch-iso-{}-{:?}.mem",
            std::process::id(),
            std::thread::current().id()
        ));
        let mut f = std::fs::File::create(&path).unwrap();
        for page in 0..PAGES {
            f.write_all(&vec![0x10u8 + page as u8; PAGE]).unwrap();
        }
        f.sync_all().unwrap();
        drop(f);
        let original = std::fs::read(&path).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        // SAFETY: read-only mapping of a file nothing else writes.
        let snapshot = unsafe { memmap2::MmapOptions::new().len(len).map(&file).unwrap() };

        // Two clones, each prefetching the whole recorded set.
        let mut recorded = PageSet::empty(len as u64);
        recorded.insert_range(0, len as u64);
        let mut clones = Vec::new();
        for _ in 0..2 {
            let base = guest_memory(len);
            let uffd = register_missing(base, len);
            let regions = [Region {
                base_host_virt_addr: base as u64,
                file_offset: 0,
                size: len,
            }];
            let segments = plan(&recorded, &regions, PAGE, len as u64);
            run_plan(&uffd, &Source::Copy(&snapshot), &segments, PAGE);
            clones.push((base, uffd));
        }

        let (a, _uffd_a) = &clones[0];
        let (b, _uffd_b) = &clones[1];
        assert_eq!(read_u8(*a), 0x10);
        assert_eq!(read_u8(*b), 0x10);

        // Clone A writes to a prefetched page.
        // SAFETY: writing inside clone A's own mapping.
        unsafe { std::ptr::write_volatile(*a as *mut u8, 0xEE) };

        assert_eq!(read_u8(*a), 0xEE, "clone A must see its own write");
        assert_eq!(
            read_u8(*b),
            0x10,
            "MEMORY LEAKED BETWEEN CLONES: clone B saw clone A's write to a prefetched page"
        );
        assert_eq!(
            snapshot[0], 0x10,
            "SNAPSHOT CORRUPTED: clone A's write reached the shared snapshot mapping"
        );
        drop(snapshot);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            original,
            "SNAPSHOT CORRUPTED: the memory file changed after a clone wrote to a prefetched page"
        );

        for (base, _) in &clones {
            // SAFETY: unmapping our own mapping.
            unsafe { libc::munmap(*base as *mut std::ffi::c_void, len) };
        }
        std::fs::remove_file(&path).unwrap();
    }
}
