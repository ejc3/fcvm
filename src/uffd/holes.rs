//! The holes of a snapshot memory image: the pages a MINOR-mode server left unwritten in its
//! backing memfd, and which of them are still holes when a page server is about to map one.

use std::fs::File;

use tracing::warn;

use super::prefetch::CHUNK_BYTES;
use super::warmup::host_page_size;
use super::working_set::GRANULE;

/// [`GRANULE`] as a byte count in the server's address arithmetic.
const GRANULE_BYTES: usize = GRANULE as usize;

/// The granules of a snapshot memory image that a MINOR-mode server left unwritten in its
/// backing memfd, one bit per [`GRANULE`] (4 KiB).
///
/// The server fills it while it populates the memfd, which leaves every all-zero 4 KiB page
/// of the snapshot unwritten. Each marked granule is a candidate: a clone's first touch of
/// one fills it in the memfd without telling the server. [`MemfdHoles`] asks the memfd which
/// candidates are still holes.
///
/// A marked granule is one the builder did not write. On a host whose base page is larger
/// than 4 KiB, a granule beside written data in the same page is backed all the same, so a
/// caller asks about a whole clone page with [`HoleMap::covers`], which is true only when
/// every granule of the page is marked.
///
/// The server builds the map before it binds its socket and never changes it after, so
/// every clone task reads it without a lock.
pub struct HoleMap {
    /// The length of the memory image in granules. A granule past it is never a hole.
    granules: usize,
    /// One bit per granule, allocated at the first [`HoleMap::insert`], so an image with no
    /// holes costs nothing.
    bits: Vec<u64>,
    /// Granules marked.
    marked: usize,
}

impl HoleMap {
    /// An empty map of a memory image of `mem_len` bytes.
    pub fn new(mem_len: usize) -> Self {
        Self {
            granules: mem_len.div_ceil(GRANULE_BYTES),
            bits: Vec::new(),
            marked: 0,
        }
    }

    /// Mark every granule that lies wholly inside `[offset, offset + len)`. A granule the
    /// range only partly covers is left out, and so is anything past the end of the image,
    /// so the map never claims a hole the range does not show.
    pub fn insert(&mut self, offset: usize, len: usize) {
        let Some(end) = offset.checked_add(len) else {
            return;
        };
        let first = offset.div_ceil(GRANULE_BYTES);
        let past_last = (end / GRANULE_BYTES).min(self.granules);
        if first >= past_last {
            return;
        }
        if self.bits.is_empty() {
            self.bits = vec![0; self.granules.div_ceil(64)];
        }
        let mut granule = first;
        while granule < past_last {
            let (word, mask, step) = word_mask(granule, past_last);
            self.marked += (mask & !self.bits[word]).count_ones() as usize;
            self.bits[word] |= mask;
            granule += step;
        }
    }

    /// Whether every granule that `[offset, offset + len)` touches is marked. False for an
    /// empty range and for one that reaches past the end of the image.
    pub fn covers(&self, offset: usize, len: usize) -> bool {
        if self.marked == 0 || len == 0 {
            return false;
        }
        let Some(end) = offset.checked_add(len) else {
            return false;
        };
        let first = offset / GRANULE_BYTES;
        let past_last = end.div_ceil(GRANULE_BYTES);
        if past_last > self.granules {
            return false;
        }
        let mut granule = first;
        while granule < past_last {
            let (word, mask, step) = word_mask(granule, past_last);
            if self.bits[word] & mask != mask {
                return false;
            }
            granule += step;
        }
        true
    }

    /// Whether the clone page at `offset` is marked, and how many bytes from there, up to
    /// `max_len`, get that same answer, in whole pages of `page_size`. An image with no
    /// marks gets `max_len` at once.
    ///
    /// Otherwise an answer covers at most one chunk ([`CHUNK_BYTES`]), the most one populate
    /// call takes, so a long run is not scanned again for every chunk of it. `offset` is page
    /// aligned and `max_len` is not zero.
    pub fn run_at(&self, offset: usize, max_len: usize, page_size: usize) -> (bool, usize) {
        if self.marked == 0 {
            return (false, max_len);
        }
        let max_len = max_len.min(CHUNK_BYTES);
        let pages = max_len.div_ceil(page_size);
        let (inside, run) = if page_size == GRANULE_BYTES {
            // One granule per page, so a run is a run of equal bits, read a word at a time.
            self.granule_run(offset / GRANULE_BYTES, pages)
        } else {
            let inside = self.covers(offset, page_size);
            let mut run = 1usize;
            while run < pages {
                let next = run
                    .checked_mul(page_size)
                    .and_then(|bytes| offset.checked_add(bytes));
                match next {
                    Some(page) if self.covers(page, page_size) == inside => run += 1,
                    _ => break,
                }
            }
            (inside, run)
        };
        (inside, run.saturating_mul(page_size).min(max_len))
    }

    /// Whether granule `first` is marked, and how many granules from it, up to `max`, are
    /// marked the same way. A granule past the end of the image is unmarked, which is what
    /// [`HoleMap::covers`] says about it.
    fn granule_run(&self, first: usize, max: usize) -> (bool, usize) {
        let word = |index: usize| self.bits.get(index).copied().unwrap_or(0);
        let inside = (word(first / 64) >> (first % 64)) & 1 != 0;
        let flip = if inside { u64::MAX } else { 0 };
        let mut run = 0usize;
        let mut granule = first;
        while run < max {
            let from = granule % 64;
            // A set bit is a granule from `granule` on that is marked the other way.
            let differ = (word(granule / 64) ^ flip) >> from;
            let span = 64 - from;
            let same = (differ.trailing_zeros() as usize).min(span);
            run = run.saturating_add(same);
            if same < span {
                break;
            }
            granule = granule.saturating_add(span);
        }
        (inside, run.min(max))
    }
}

/// The candidate holes of a MINOR-mode backing memfd, and a read-only view of the memfd that
/// says which of them are still holes.
///
/// A clone maps the memfd MAP_PRIVATE and registers MINOR only, so its first touch of a hole
/// makes shmem allocate a zero page in the memfd's page cache, and no fault reaches the
/// server. From then on the page is in the memfd: `UFFDIO_CONTINUE` maps it, and every later
/// clone takes a MINOR fault on it. `mincore` over the view says which candidates are in the
/// page cache now. Replay steps over the rest, so a page that leaves the page cache, or is
/// filled after the query, costs one demand fault. A filled page leaves it when the host swaps
/// it out: `mincore` then reports it absent unless it is still in the swap cache, and the
/// guest's MINOR fault on it has `UFFDIO_CONTINUE` swap it back in.
///
/// Not `lseek(SEEK_DATA)` on a private reopen of the memfd, which would answer the same
/// question: `shmem_file_llseek` takes the inode lock exclusively, so every clone's queries
/// would wait on one another, while `mincore` needs only the view and this process's mm read
/// lock.
pub struct MemfdHoles {
    candidates: HoleMap,
    /// A MAP_SHARED, PROT_READ mapping of the memfd that nothing reads through, so it has no
    /// page table entries and `mincore` answers for the memfd's page cache. `None` when there
    /// are no candidates, which is every hugetlb backing, or when the mapping failed.
    view: Option<memmap2::Mmap>,
}

/// What a backing memfd holds from an offset, and for how many bytes.
#[derive(Debug, PartialEq, Eq)]
pub enum MemfdRun {
    /// Data the server wrote, which `UFFDIO_CONTINUE` can map.
    Pages(usize),
    /// Candidate holes a clone has filled, which `UFFDIO_CONTINUE` can map too.
    Filled(usize),
    /// Holes no clone has filled. `UFFDIO_CONTINUE` refuses them.
    Holes(usize),
    /// Candidate holes the memfd could not be asked about. A caller treats them as holes.
    Unknown(usize),
}

/// One `mincore` answer about a backing memfd: which host pages of `[start, start + len)` were
/// in its page cache when the call ran. Replay keeps one for the whole replay of a clone and
/// passes it to every [`MemfdHoles::run_at`], which answers from it while replay is inside that
/// range. A stretch of candidates that alternates between filled and not, page by page, then
/// costs one call per chunk. Asking afresh at every page would cost one call per page, each
/// over the rest of the stretch.
pub struct ResidentWindow {
    start: usize,
    /// The bytes the answer covers. Zero when there is no answer.
    len: usize,
    /// One `mincore` entry per host page of the range. A host page is never smaller than
    /// 4 KiB, so a chunk needs at most this many.
    entries: [u8; CHUNK_BYTES / 4096],
    /// `mincore` calls made through this window.
    #[cfg(test)]
    queries: usize,
}

impl Default for ResidentWindow {
    fn default() -> Self {
        Self {
            start: 0,
            len: 0,
            entries: [0; CHUNK_BYTES / 4096],
            #[cfg(test)]
            queries: 0,
        }
    }
}

impl MemfdHoles {
    /// The `candidates` a server left unwritten in `memfd`, with a view of `memfd` to ask
    /// about them. A view that cannot be mapped is logged, and every candidate then reads
    /// [`MemfdRun::Unknown`].
    pub fn new(candidates: HoleMap, memfd: &File) -> Self {
        let view = if candidates.marked == 0 {
            None
        } else {
            // SAFETY: a read-only shared mapping of a memfd that is sealed against writes.
            // Nothing reads through it; it exists for `mincore`.
            match unsafe { memmap2::MmapOptions::new().map(memfd) } {
                Ok(view) => Some(view),
                Err(error) => {
                    warn!(
                        target: "uffd",
                        %error,
                        "could not map the backing memfd to ask which holes clones have filled; \
                         replay steps over every candidate hole"
                    );
                    None
                }
            }
        };
        Self { candidates, view }
    }

    /// What the memfd holds at the clone page at `offset`, and how many bytes from there, up
    /// to `max_len` and in whole pages of `page_size`, hold the same. Replay asks this before
    /// each chunk: it steps over holes and maps pages. Once there are candidates an answer
    /// covers at most one chunk ([`CHUNK_BYTES`]). `offset` is page aligned and `max_len` is
    /// not zero. `window` holds the last `mincore` answer and is reused while `offset` lies
    /// inside it, so an answer can be as old as the chunk it was asked for.
    pub fn run_at(
        &self,
        window: &mut ResidentWindow,
        offset: usize,
        max_len: usize,
        page_size: usize,
    ) -> MemfdRun {
        let (candidate, len) = self.candidates.run_at(offset, max_len, page_size);
        if !candidate {
            return MemfdRun::Pages(len);
        }
        let resident = self
            .view
            .as_ref()
            .and_then(|view| resident_run(view, window, offset, len, page_size));
        match resident {
            Some((true, run)) => MemfdRun::Filled(run),
            Some((false, run)) => MemfdRun::Holes(run),
            None => MemfdRun::Unknown(len),
        }
    }
}

/// Whether the page at `offset` in the file `view` maps is in the page cache, and how many
/// bytes from there, up to `len` and in whole pages of `page_size`, get the same answer.
/// The answer comes from `window` when `offset` lies inside it, and otherwise from a new
/// `mincore` call over `[offset, offset + len)`, which `window` then holds. `None` when
/// `mincore` cannot be asked about that range.
///
/// `mincore` reports every page of a file mapping as resident when the caller neither owns
/// the file nor could open it for writing. The server created the memfd, so it owns it.
fn resident_run(
    view: &[u8],
    window: &mut ResidentWindow,
    offset: usize,
    len: usize,
    page_size: usize,
) -> Option<(bool, usize)> {
    let host_page = usize::try_from(host_page_size()).ok()?;
    let len = len.min(CHUNK_BYTES);
    if len == 0
        || page_size < host_page
        || !page_size.is_multiple_of(host_page)
        || !offset.is_multiple_of(page_size)
        || offset.checked_add(len)? > view.len()
    {
        return None;
    }
    if offset < window.start || offset - window.start >= window.len {
        window.len = 0;
        if len.div_ceil(host_page) > window.entries.len() {
            return None;
        }
        // SAFETY: `[offset, offset + len)` lies inside `view`, a live mapping, and
        // `window.entries` holds one byte for each host page of it, which is all mincore
        // writes.
        let rc = unsafe {
            libc::mincore(
                view.as_ptr().add(offset).cast_mut().cast(),
                len,
                window.entries.as_mut_ptr(),
            )
        };
        #[cfg(test)]
        {
            window.queries += 1;
        }
        if rc != 0 {
            return None;
        }
        window.start = offset;
        window.len = len;
    }
    // The answer reaches no further than the window. `offset` and the window's start are both
    // multiples of the host page, so `offset` starts one of the window's entries.
    let len = len.min(window.start + window.len - offset);
    let skip = (offset - window.start) / host_page;
    let count = len.div_ceil(host_page);
    let entries = &window.entries[skip..skip + count];
    let per_page = page_size / host_page;
    let resident = |page: usize| {
        entries[page * per_page..((page + 1) * per_page).min(count)]
            .iter()
            .all(|entry| entry & 1 != 0)
    };
    let pages = len.div_ceil(page_size);
    let first = resident(0);
    let mut run = 1usize;
    while run < pages && resident(run) == first {
        run += 1;
    }
    Some((first, (run * page_size).min(len)))
}

/// The word holding `granule`, the bits of that word from `granule` up to `past_last` (or to
/// the end of the word), and how many granules those bits are.
fn word_mask(granule: usize, past_last: usize) -> (usize, u64, usize) {
    let from = granule % 64;
    let upto = (past_last - granule + from).min(64);
    let mask = (u64::MAX >> (64 - (upto - from))) << from;
    (granule / 64, mask, upto - from)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: usize = 4096;

    /// The granules in `0..granules` the map says are holes, one at a time.
    fn holes(map: &HoleMap, granules: usize) -> Vec<usize> {
        (0..granules)
            .filter(|granule| map.covers(granule * PAGE, PAGE))
            .collect()
    }

    /// `insert` marks the granules a range covers whole and nothing else: not a granule it
    /// covers only in part, and nothing past the end of the image.
    #[test]
    fn a_hole_map_marks_exactly_the_granules_a_range_covers_whole() {
        // 200 granules, so the bitmap spans four words and a range crosses a word boundary.
        let mut map = HoleMap::new(200 * PAGE);
        assert_eq!(holes(&map, 200), Vec::<usize>::new());

        map.insert(PAGE, 2 * PAGE);
        // Starts 100 bytes into granule 5 and ends 100 bytes into granule 7: only 6 is whole.
        map.insert(5 * PAGE + 100, 2 * PAGE);
        // Granules 60 to 69, across the boundary between the first and second word.
        map.insert(60 * PAGE, 10 * PAGE);
        // Inserting a marked granule again counts it once.
        map.insert(2 * PAGE, PAGE);
        // Granule 199 is the last one; the range runs past the image, which adds nothing.
        map.insert(199 * PAGE, 4 * PAGE);
        // A range shorter than one granule marks nothing.
        map.insert(100 * PAGE, PAGE - 1);

        let mut want = vec![1, 2, 6];
        want.extend(60..70);
        want.push(199);
        assert_eq!(holes(&map, 200), want);
        assert_eq!(map.marked, want.len());
        assert!(
            !map.covers(199 * PAGE, 2 * PAGE),
            "a range that reaches past the image is not a hole"
        );
        assert!(!map.covers(PAGE, 0), "an empty range is not a hole");
    }

    /// A clone page bigger than a granule is a hole only when every granule in it is one. A
    /// map that answered for any one granule would have replay skip guest data.
    #[test]
    fn a_large_page_is_a_hole_only_when_every_granule_is() {
        const LARGE: usize = 16 * 1024;
        const HUGE: usize = 2 * 1024 * 1024;
        let mut map = HoleMap::new(2 * HUGE);

        // Every granule of the first huge page except one in its middle.
        map.insert(0, 300 * PAGE);
        map.insert(301 * PAGE, HUGE - 301 * PAGE);
        assert!(!map.covers(0, HUGE));
        assert_eq!(map.run_at(0, 2 * HUGE, HUGE), (false, HUGE));
        // The 16 KiB page holding granule 300 has one data granule; its neighbours have none.
        assert!(!map.covers(300 * PAGE / LARGE * LARGE, LARGE));
        assert!(map.covers(0, LARGE));
        assert_eq!(map.run_at(0, 4 * LARGE, LARGE), (true, 4 * LARGE));
        assert_eq!(
            map.run_at(288 * PAGE, 4 * LARGE, LARGE),
            (true, 3 * LARGE),
            "three pages of holes, then the page with the data granule"
        );
        assert_eq!(
            map.run_at(300 * PAGE, 4 * LARGE, LARGE),
            (false, LARGE),
            "the page with the data granule, then pages of holes"
        );

        map.insert(300 * PAGE, PAGE);
        assert!(map.covers(0, HUGE), "every granule is a hole now");
        assert_eq!(map.run_at(0, 2 * HUGE, HUGE), (true, HUGE));
        assert!(!map.covers(HUGE, HUGE), "the second huge page has no holes");
    }

    /// With one granule per page `run_at` reads the bitmap a word at a time. Its answer must
    /// be the page-by-page one: whether the first page is marked, and how many pages from
    /// there `covers` answers the same, up to `max_len`, past the end of the image included.
    #[test]
    fn a_hole_map_run_is_the_page_by_page_answer() {
        const GRANULES: usize = 300;
        let mut map = HoleMap::new(GRANULES * PAGE);
        // Runs that start and end inside words, cross word boundaries, end exactly at one
        // (190 and 191), and reach the last granule of the image.
        for (first, len) in [
            (0, 1),
            (3, 2),
            (60, 10),
            (127, 2),
            (130, 50),
            (190, 2),
            (250, 1),
            (255, 45),
        ] {
            map.insert(first * PAGE, len * PAGE);
        }
        for first in 0..GRANULES {
            for max in [
                1,
                2,
                63,
                64,
                65,
                200,
                GRANULES - first,
                GRANULES - first + 5,
            ] {
                let inside = map.covers(first * PAGE, PAGE);
                let mut want = 1;
                while want < max && map.covers((first + want) * PAGE, PAGE) == inside {
                    want += 1;
                }
                assert_eq!(
                    map.run_at(first * PAGE, max * PAGE, PAGE),
                    (inside, want * PAGE),
                    "granule {first}, up to {max} pages"
                );
            }
        }
    }

    /// `run_at` answers for the page at an offset and says how far that answer holds, in
    /// whole pages, no further than `max_len` and one chunk.
    #[test]
    fn a_hole_map_reports_runs_of_holes_and_data() {
        let empty = HoleMap::new(4 * CHUNK_BYTES);
        assert_eq!(
            empty.run_at(0, 3 * CHUNK_BYTES, PAGE),
            (false, 3 * CHUNK_BYTES),
            "an image with no holes is answered at once, past one chunk"
        );

        let mut map = HoleMap::new(4 * CHUNK_BYTES);
        map.insert(2 * PAGE, 3 * PAGE);
        map.insert(7 * PAGE, PAGE);
        assert_eq!(map.run_at(0, 10 * PAGE, PAGE), (false, 2 * PAGE));
        assert_eq!(map.run_at(2 * PAGE, 8 * PAGE, PAGE), (true, 3 * PAGE));
        assert_eq!(
            map.run_at(3 * PAGE, PAGE, PAGE),
            (true, PAGE),
            "capped at max_len"
        );
        assert_eq!(map.run_at(5 * PAGE, 5 * PAGE, PAGE), (false, 2 * PAGE));
        assert_eq!(map.run_at(7 * PAGE, 3 * PAGE, PAGE), (true, PAGE));
        assert_eq!(map.run_at(8 * PAGE, 2 * PAGE, PAGE), (false, 2 * PAGE));

        // A run longer than a chunk is answered one chunk at a time.
        map.insert(CHUNK_BYTES, 2 * CHUNK_BYTES);
        assert_eq!(
            map.run_at(CHUNK_BYTES, 3 * CHUNK_BYTES, PAGE),
            (true, CHUNK_BYTES)
        );
        assert_eq!(
            map.run_at(8 * PAGE, 3 * CHUNK_BYTES, PAGE),
            (false, CHUNK_BYTES - 8 * PAGE),
            "data from granule 8 up to the run of holes that starts at the second chunk"
        );
    }

    /// Replay passes one [`ResidentWindow`] to every [`MemfdHoles::run_at`], so a stretch of
    /// candidates that alternates between filled and not costs one `mincore` call per chunk.
    /// Its answers must be the ones a fresh window at every call gives, page for page.
    #[test]
    fn a_reused_resident_window_answers_like_a_fresh_one() {
        use std::os::fd::FromRawFd;
        use std::os::unix::fs::FileExt;

        let page = usize::try_from(host_page_size()).expect("a host page size");
        let per_chunk = CHUNK_BYTES / page;
        // Four data pages, then candidates over two chunks and a bit, every other one filled.
        let data = 4;
        let candidates = 2 * per_chunk + 7;
        let len = (data + candidates) * page;
        // SAFETY: memfd_create with a valid name; a descriptor it returns is new and ours.
        let fd = unsafe { libc::memfd_create(c"fcvm-holes-test".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0, "memfd_create: {}", std::io::Error::last_os_error());
        // SAFETY: `fd` was just returned and nothing else owns it.
        let memfd = unsafe { File::from_raw_fd(fd) };
        memfd.set_len(len as u64).expect("sizing the memfd");
        let mut want = Vec::new();
        for index in 0..data {
            memfd
                .write_at(&[0xAB], (index * page) as u64)
                .expect("writing a data page");
            want.push('P');
        }
        for index in 0..candidates {
            if index % 2 == 0 {
                // Writing one byte makes shmem allocate the page, as a clone's touch does.
                memfd
                    .write_at(&[0], ((data + index) * page) as u64)
                    .expect("filling a hole");
                want.push('F');
            } else {
                want.push('H');
            }
        }
        let mut map = HoleMap::new(len);
        map.insert(data * page, candidates * page);
        let holes = MemfdHoles::new(map, &memfd);

        // Every page's answer, walking the image the way replay does, and the mincore calls.
        let walk = |reuse: bool| {
            let mut kept = ResidentWindow::default();
            let mut queries = 0;
            let mut labels = Vec::new();
            let mut at = 0;
            while at < len {
                let mut fresh = ResidentWindow::default();
                let window = if reuse { &mut kept } else { &mut fresh };
                let (label, run) = match holes.run_at(window, at, len - at, page) {
                    MemfdRun::Pages(run) => ('P', run),
                    MemfdRun::Filled(run) => ('F', run),
                    MemfdRun::Holes(run) => ('H', run),
                    MemfdRun::Unknown(run) => ('?', run),
                };
                queries += fresh.queries;
                labels.extend(std::iter::repeat_n(label, run / page));
                at += run;
            }
            (labels, queries + kept.queries)
        };
        let (fresh_labels, fresh_queries) = walk(false);
        let (kept_labels, kept_queries) = walk(true);
        assert_eq!(fresh_labels, want, "a fresh window at every call");
        assert_eq!(
            kept_labels, fresh_labels,
            "a window reused across calls answers the same"
        );
        assert_eq!(
            fresh_queries, candidates,
            "a fresh window costs one call per page of an alternating stretch"
        );
        assert_eq!(
            kept_queries,
            candidates.div_ceil(per_chunk),
            "a reused window costs one call per chunk"
        );
    }
}
