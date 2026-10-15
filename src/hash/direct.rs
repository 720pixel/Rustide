//! Bounded parallel reads with Linux direct I/O and a portable buffered fallback.

use crate::create::FileEntry;
use anyhow::Result;

/// Hash concatenated v1 payloads in piece order, including synthesized padding.
///
/// `batch_bytes` rounds down to whole pieces, with a minimum of one piece.
/// In the Linux fast path, each worker owns one buffer capped at total length,
/// plus at most 12.5% and less than 12 KiB of alignment slack. Only pieces crossing
/// gaps in that buffer are copied, requiring at most one additional batch payload
/// per worker. At most `min(workers, batches)` workers allocate buffers; digest
/// output, segment records, piece references, and descriptors use additional memory.
/// Linux uses direct reads for file ranges and ordinary reads for small ranges
/// or unsupported filesystems. Each worker holds at most two input descriptors,
/// so large directories do not require a sequential fallback. Other platforms
/// use the sequential reader, which can require one additional reader buffer.
/// Payloads must remain stable while hashing.
pub fn hash_v1_direct(
    files: &[FileEntry],
    piece_length: usize,
    workers: usize,
    batch_bytes: usize,
) -> Result<Vec<[u8; 20]>> {
    hash_v1_direct_with_progress(files, piece_length, workers, batch_bytes, None)
}

pub(crate) fn hash_v1_direct_with_progress(
    files: &[FileEntry],
    piece_length: usize,
    workers: usize,
    batch_bytes: usize,
    progress: Option<&(dyn Fn(u64) + Sync)>,
) -> Result<Vec<[u8; 20]>> {
    #[cfg(target_os = "linux")]
    {
        let budget = linux::file_budget();
        if budget == 0 {
            return super::reader::hash_v1_buffered_with_progress(
                files,
                piece_length,
                workers,
                batch_bytes,
                progress,
            );
        }
        linux::hash_with_progress(
            files,
            piece_length,
            workers.min(budget),
            batch_bytes,
            true,
            progress,
        )
    }
    #[cfg(not(target_os = "linux"))]
    {
        super::reader::hash_v1_buffered_with_progress(
            files,
            piece_length,
            workers,
            batch_bytes,
            progress,
        )
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::{
        alloc::{Layout, alloc_zeroed, dealloc},
        fs::{File, OpenOptions},
        io,
        ptr::NonNull,
        sync::{
            Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
    };

    use crate::{create::FileEntry, hash::sha1};
    use anyhow::{Context, Result, ensure};
    use rayon::prelude::*;

    const ALIGN: usize = 4096;
    const DIRECT_MIN: usize = 64 * 1024;

    #[derive(Clone, Copy)]
    struct Segment {
        logical_start: usize,
        physical_start: usize,
        length: usize,
    }

    fn push_segment(segments: &mut Vec<Segment>, segment: Segment) {
        if let Some(previous) = segments.last_mut()
            && previous.logical_start + previous.length == segment.logical_start
            && previous.physical_start + previous.length == segment.physical_start
        {
            previous.length += segment.length;
        } else {
            segments.push(segment);
        }
    }

    fn buffer_length(effective_batch: usize, direct_files: usize) -> Result<usize> {
        let slack = (effective_batch / DIRECT_MIN)
            .min(direct_files)
            .checked_add(1)
            .and_then(|fragments| fragments.checked_mul(2 * ALIGN))
            .context("alignment slack overflow")?;
        let rounded = effective_batch
            .checked_add(slack)
            .and_then(|bytes| bytes.checked_add(ALIGN - 1))
            .context("buffer length overflow")?
            / ALIGN
            * ALIGN;
        Ok(rounded)
    }

    fn hash_segments(
        buffer: &[u8],
        segments: &[Segment],
        used: usize,
        piece_length: usize,
        scratch: &mut Vec<Vec<u8>>,
    ) -> Vec<[u8; 20]> {
        if segments.len() == 1 {
            let start = segments[0].physical_start;
            let pieces: Vec<_> = buffer[start..start + used].chunks(piece_length).collect();
            return sha1::hash_many(&pieces);
        }

        let mut owned = Vec::<Option<usize>>::with_capacity(used.div_ceil(piece_length));
        let mut locations = Vec::with_capacity(owned.capacity());
        let mut index = 0;
        let mut copied = 0;
        for begin in (0..used).step_by(piece_length) {
            let length = piece_length.min(used - begin);
            let end = begin + length;
            while segments[index].logical_start + segments[index].length <= begin {
                index += 1;
            }
            let segment = segments[index];
            let physical = segment.physical_start + begin - segment.logical_start;
            locations.push((physical, length));
            if end <= segment.logical_start + segment.length {
                owned.push(None);
                continue;
            }

            if copied == scratch.len() {
                scratch.push(Vec::with_capacity(length));
            }
            let piece = &mut scratch[copied];
            piece.clear();
            let mut position = begin;
            let mut part = index;
            while position < end {
                let segment = segments[part];
                let offset = position - segment.logical_start;
                let count = (segment.length - offset).min(end - position);
                let start = segment.physical_start + offset;
                piece.extend_from_slice(&buffer[start..start + count]);
                position += count;
                part += 1;
            }
            owned.push(Some(copied));
            copied += 1;
        }
        // Finish allocating boundary pieces before borrowing any owned payload.
        let pieces: Vec<_> = owned
            .iter()
            .zip(locations)
            .map(|(piece, (start, length))| {
                piece
                    .map(|index| scratch[index].as_slice())
                    .unwrap_or_else(|| &buffer[start..start + length])
            })
            .collect();
        sha1::hash_many(&pieces)
    }

    fn descriptor_budget(soft_limit: libc::rlim_t) -> usize {
        // Each input may hold two descriptors. Reserve room for the caller,
        // standard streams, runtime internals, and final output creation.
        (soft_limit.saturating_sub(32) / 2).min(256) as usize
    }

    pub(super) fn file_budget() -> usize {
        let mut limit = std::mem::MaybeUninit::<libc::rlimit>::uninit();
        // SAFETY: getrlimit writes a complete rlimit on success; its result is
        // inspected before reading the initialized value.
        if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, limit.as_mut_ptr()) } != 0 {
            return 0;
        }
        // SAFETY: the successful call above initialized the complete structure.
        descriptor_budget(unsafe { limit.assume_init() }.rlim_cur)
    }

    struct AlignedBuffer {
        pointer: NonNull<u8>,
        layout: Layout,
    }

    impl AlignedBuffer {
        fn new(length: usize) -> Result<Self> {
            ensure!(length > 0, "aligned allocation must not be empty");
            let layout = Layout::from_size_align(length, ALIGN)?;
            // SAFETY: Layout is valid and nonzero. Null is handled as an error.
            let pointer = NonNull::new(unsafe { alloc_zeroed(layout) })
                .context("allocate aligned read buffer")?;
            Ok(Self { pointer, layout })
        }

        fn bytes(&mut self) -> &mut [u8] {
            // SAFETY: the allocation is live, initialized, uniquely borrowed, and
            // its original length is recorded in layout.
            unsafe { std::slice::from_raw_parts_mut(self.pointer.as_ptr(), self.layout.size()) }
        }
    }

    impl Drop for AlignedBuffer {
        fn drop(&mut self) {
            // SAFETY: this is the original allocation and layout, freed exactly once.
            unsafe { dealloc(self.pointer.as_ptr(), self.layout) };
        }
    }

    struct Input {
        regular: Option<File>,
        direct: Option<File>,
        direct_disabled: AtomicBool,
        length: u64,
    }

    impl Input {
        fn validate(&self) -> Result<()> {
            if let Some(file) = &self.regular {
                ensure!(
                    file.metadata()?.len() == self.length,
                    "input length changed while hashing"
                );
            }
            Ok(())
        }
    }

    #[derive(Default)]
    struct InputCache {
        current: Option<(usize, Input)>,
    }

    impl InputCache {
        fn get(&mut self, index: usize, files: &[FileEntry], allow_direct: bool) -> Result<&Input> {
            if self
                .current
                .as_ref()
                .is_none_or(|(cached, _)| *cached != index)
            {
                if let Some((_, previous)) = self.current.take() {
                    previous.validate()?;
                }
                self.current = Some((index, open_input(&files[index], allow_direct)?));
            }
            Ok(&self.current.as_ref().unwrap().1)
        }

        fn validate(&self) -> Result<()> {
            if let Some((_, input)) = &self.current {
                input.validate()?;
            }
            Ok(())
        }
    }

    fn open_input(entry: &FileEntry, allow_direct: bool) -> Result<Input> {
        if entry.padding {
            return Ok(Input {
                regular: None,
                direct: None,
                direct_disabled: AtomicBool::new(true),
                length: entry.length,
            });
        }
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let regular = options
            .open(&entry.disk_path)
            .with_context(|| format!("open {}", entry.disk_path.display()))?;
        let metadata = regular.metadata()?;
        ensure!(
            metadata.is_file() && metadata.len() == entry.length,
            "input type or length changed for {}",
            entry.disk_path.display()
        );
        #[cfg(target_os = "linux")]
        let direct = if allow_direct && entry.length > 0 {
            use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
            options.custom_flags(libc::O_DIRECT | libc::O_NOFOLLOW | libc::O_NONBLOCK);
            match options.open(&entry.disk_path) {
                Ok(file) => {
                    let direct_meta = file.metadata()?;
                    ensure!(
                        direct_meta.ino() == metadata.ino()
                            && direct_meta.dev() == metadata.dev()
                            && direct_meta.len() == entry.length,
                        "input changed while opening direct descriptor"
                    );
                    Some(file)
                }
                Err(error) if unsupported(&error) => None,
                Err(error) => return Err(error).context("open direct descriptor"),
            }
        } else {
            None
        };
        #[cfg(not(target_os = "linux"))]
        let direct = None;
        Ok(Input {
            regular: Some(regular),
            direct,
            direct_disabled: AtomicBool::new(false),
            length: entry.length,
        })
    }

    fn unsupported(error: &io::Error) -> bool {
        matches!(
            error.raw_os_error(),
            Some(libc::EINVAL | libc::EOPNOTSUPP | libc::ENOSYS)
        )
    }

    fn read_at(file: &File, output: &mut [u8], offset: u64) -> io::Result<usize> {
        loop {
            #[cfg(unix)]
            let result = {
                use std::os::unix::fs::FileExt;
                file.read_at(output, offset)
            };
            #[cfg(windows)]
            let result = {
                use std::os::windows::fs::FileExt;
                file.seek_read(output, offset)
            };
            match result {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                other => return other,
            }
        }
    }

    fn read_exact_at(file: &File, mut output: &mut [u8], mut offset: u64) -> Result<()> {
        while !output.is_empty() {
            let read = read_at(file, output, offset)?;
            ensure!(read != 0, "unexpected EOF at file offset {offset}");
            offset = offset
                .checked_add(read as u64)
                .context("file offset overflow")?;
            output = &mut output[read..];
        }
        Ok(())
    }

    #[cfg(test)]
    fn hash(
        files: &[FileEntry],
        piece_length: usize,
        workers: usize,
        batch_bytes: usize,
        allow_direct: bool,
    ) -> Result<Vec<[u8; 20]>> {
        hash_with_progress(
            files,
            piece_length,
            workers,
            batch_bytes,
            allow_direct,
            None,
        )
    }

    pub(super) fn hash_with_progress(
        files: &[FileEntry],
        piece_length: usize,
        workers: usize,
        batch_bytes: usize,
        allow_direct: bool,
        progress: Option<&(dyn Fn(u64) + Sync)>,
    ) -> Result<Vec<[u8; 20]>> {
        ensure!(
            piece_length > 0 && workers > 0,
            "piece length and workers must be positive"
        );
        let batch_pieces = batch_bytes.max(piece_length) / piece_length;
        let batch_bytes = batch_pieces * piece_length;
        let mut total = 0_u64;
        let mut ends = Vec::with_capacity(files.len());
        let mut direct_files = 0;
        for entry in files {
            total = total
                .checked_add(entry.length)
                .context("total length overflow")?;
            ends.push(total);
            // Validate empty files as well, without retaining directory-sized
            // descriptor tables. Workers reopen only their current input.
            open_input(entry, false)?;
            if allow_direct && !entry.padding && entry.length >= DIRECT_MIN as u64 {
                direct_files += 1;
            }
        }
        if total == 0 {
            return Ok(Vec::new());
        }
        let batches =
            usize::try_from(total.div_ceil(batch_bytes as u64)).context("batch count overflow")?;
        let count =
            usize::try_from(total.div_ceil(piece_length as u64)).context("piece count overflow")?;
        let buffer_bytes = buffer_length(
            batch_bytes.min(usize::try_from(total).unwrap_or(usize::MAX)),
            direct_files,
        )?;
        let workers = workers.min(batches);
        workers
            .checked_mul(buffer_bytes)
            .context("worker memory overflow")?;
        let mut output = Vec::new();
        output
            .try_reserve_exact(count)
            .context("allocate digest output")?;
        output.resize(count, [0_u8; 20]);
        let shared = Mutex::new(output);
        let next_batch = AtomicUsize::new(0);
        let failed = AtomicBool::new(false);
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(workers)
            .build()?;
        pool.install(|| {
            // There are exactly `workers` loop bodies, each owning one reusable
            // buffer, independent of Rayon splitting or stealing decisions.
            (0..workers)
                .into_par_iter()
                .try_for_each(|_| -> Result<()> {
                    let result = (|| {
                        let mut buffer = AlignedBuffer::new(buffer_bytes)?;
                        let mut inputs = InputCache::default();
                        let mut segments = Vec::new();
                        let mut scratch = Vec::new();
                        loop {
                            if failed.load(Ordering::Relaxed) {
                                break;
                            }
                            let batch = next_batch.fetch_add(1, Ordering::Relaxed);
                            if batch >= batches {
                                break;
                            }
                            let begin = (batch as u64)
                                .checked_mul(batch_bytes as u64)
                                .context("batch offset overflow")?;
                            let used = (total - begin).min(batch_bytes as u64) as usize;
                            let stop = begin + used as u64;
                            let bytes = buffer.bytes();
                            segments.clear();
                            let mut position = begin;
                            let mut physical_cursor = 0_usize;
                            while position < stop {
                                let index = ends.partition_point(|&end| end <= position);
                                let previous = if index == 0 { 0 } else { ends[index - 1] };
                                let length = (ends[index].min(stop) - position) as usize;
                                let input = inputs.get(index, files, allow_direct)?;
                                let mut physical_start = physical_cursor;
                                if let Some(regular) = &input.regular {
                                    let offset = position - previous;
                                    let mut direct_done = false;
                                    if length >= DIRECT_MIN
                                        && !input.direct_disabled.load(Ordering::Relaxed)
                                        && let Some(direct) = &input.direct
                                    {
                                        let aligned_start = physical_cursor.div_ceil(ALIGN) * ALIGN;
                                        let lead = offset as usize % ALIGN;
                                        let requested = (lead + length).div_ceil(ALIGN) * ALIGN;
                                        let aligned_offset = offset - lead as u64;
                                        match read_at(
                                            direct,
                                            &mut bytes[aligned_start..aligned_start + requested],
                                            aligned_offset,
                                        ) {
                                            Ok(read) if read >= lead + length => {
                                                physical_start = aligned_start + lead;
                                                direct_done = true;
                                            }
                                            Ok(_) => {}
                                            Err(error) if unsupported(&error) => {
                                                input
                                                    .direct_disabled
                                                    .store(true, Ordering::Relaxed);
                                            }
                                            Err(error) => return Err(error).context("direct read"),
                                        }
                                    }
                                    if !direct_done {
                                        read_exact_at(
                                            regular,
                                            &mut bytes[physical_start..physical_start + length],
                                            offset,
                                        )?;
                                    }
                                } else {
                                    bytes[physical_start..physical_start + length].fill(0);
                                }
                                push_segment(
                                    &mut segments,
                                    Segment {
                                        logical_start: (position - begin) as usize,
                                        physical_start,
                                        length,
                                    },
                                );
                                physical_cursor = physical_start + length;
                                position += length as u64;
                            }
                            let hashes =
                                hash_segments(bytes, &segments, used, piece_length, &mut scratch);
                            let first = batch * batch_pieces;
                            let mut result = shared
                                .lock()
                                .map_err(|_| anyhow::anyhow!("result lock poisoned"))?;
                            result[first..first + hashes.len()].copy_from_slice(&hashes);
                            drop(result);
                            if let Some(progress) = progress {
                                let bytes = crate::create::payload_bytes_in_range(
                                    files, &ends, begin, stop,
                                );
                                if bytes > 0 {
                                    progress(bytes);
                                }
                            }
                        }
                        inputs.validate()?;
                        Ok(())
                    })();
                    if result.is_err() {
                        failed.store(true, Ordering::Relaxed);
                    }
                    result
                })
        })?;
        for entry in files {
            open_input(entry, false)?;
        }
        shared
            .into_inner()
            .map_err(|_| anyhow::anyhow!("result lock poisoned"))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use ::sha1::Digest;
        use std::fs;

        #[test]
        fn descriptor_budget_reserves_handles_and_keeps_the_upper_bound() {
            assert_eq!(descriptor_budget(0), 0);
            assert_eq!(descriptor_budget(32), 0);
            assert_eq!(descriptor_budget(33), 0);
            assert_eq!(descriptor_budget(34), 1);
            assert_eq!(descriptor_budget(64), 16);
            assert_eq!(descriptor_budget(1024), 256);
            assert_eq!(descriptor_budget(libc::RLIM_INFINITY), 256);
        }

        fn entry(root: &std::path::Path, name: &str, bytes: &[u8]) -> FileEntry {
            let path = root.join(name);
            fs::write(&path, bytes).unwrap();
            FileEntry {
                disk_path: path,
                components: vec![name.as_bytes().to_vec()],
                length: bytes.len() as u64,
                padding: false,
            }
        }

        #[test]
        fn aligned_buffer_rejects_zero_and_initializes_all_bytes() {
            assert!(AlignedBuffer::new(0).is_err());
            let mut buffer = AlignedBuffer::new(8192).unwrap();
            assert_eq!(buffer.bytes().as_ptr() as usize % ALIGN, 0);
            assert!(buffer.bytes().iter().all(|&byte| byte == 0));
        }

        #[test]
        fn scattered_segments_preserve_pieces_crossing_multiple_gaps() {
            let mut buffer = vec![0xa5; 3 * ALIGN];
            let mut stream = Vec::new();
            let mut segments = Vec::new();
            for (physical_start, length) in [(0, 17), (ALIGN, 53), (2 * ALIGN, 101)] {
                let logical_start = stream.len();
                let bytes: Vec<_> = (0..length)
                    .map(|offset| ((logical_start + offset) * 31) as u8)
                    .collect();
                buffer[physical_start..physical_start + length].copy_from_slice(&bytes);
                stream.extend_from_slice(&bytes);
                push_segment(
                    &mut segments,
                    Segment {
                        logical_start,
                        physical_start,
                        length,
                    },
                );
            }
            for byte in 0..13 {
                buffer[2 * ALIGN + 101 + byte] = byte as u8;
                push_segment(
                    &mut segments,
                    Segment {
                        logical_start: stream.len(),
                        physical_start: 2 * ALIGN + 101 + byte,
                        length: 1,
                    },
                );
                stream.push(byte as u8);
            }
            assert_eq!(segments.len(), 3);
            assert_eq!(segments[2].length, 114);
            let mut scratch = Vec::new();
            for piece in [1, 7, 55, 64, 128, 256] {
                let expected: Vec<[u8; 20]> = stream
                    .chunks(piece)
                    .map(|bytes| ::sha1::Sha1::digest(bytes).into())
                    .collect();
                assert_eq!(
                    hash_segments(&buffer, &segments, stream.len(), piece, &mut scratch),
                    expected
                );
                let short = &stream[..stream.len() - 3];
                let expected: Vec<[u8; 20]> = short
                    .chunks(piece)
                    .map(|bytes| ::sha1::Sha1::digest(bytes).into())
                    .collect();
                assert_eq!(
                    hash_segments(&buffer, &segments, short.len(), piece, &mut scratch),
                    expected
                );
            }
        }

        #[test]
        fn alignment_slack_is_bounded_and_rejects_overflow() {
            for length in [
                1,
                ALIGN - 1,
                DIRECT_MIN - 1,
                DIRECT_MIN,
                DIRECT_MIN + 1,
                17 * DIRECT_MIN + 4095,
            ] {
                let allocated = buffer_length(length, usize::MAX).unwrap();
                assert_eq!(allocated % ALIGN, 0);
                assert!(allocated >= length + 2 * ALIGN);
                assert!(allocated - length < length / 8 + 3 * ALIGN);
            }
            assert_eq!(
                buffer_length(16 * DIRECT_MIN, 0).unwrap(),
                16 * DIRECT_MIN + 2 * ALIGN
            );
            assert_eq!(
                buffer_length(16 * DIRECT_MIN, 1).unwrap(),
                16 * DIRECT_MIN + 4 * ALIGN
            );
            assert!(buffer_length(usize::MAX, usize::MAX).is_err());
        }

        #[test]
        fn direct_fragment_thresholds_preserve_discontiguous_piece_data() {
            let temp = tempfile::tempdir().unwrap();
            let mut entries = Vec::new();
            let mut stream = Vec::new();
            for index in 0..9 {
                let length = [DIRECT_MIN - 1, DIRECT_MIN, DIRECT_MIN + 1][index % 3];
                let data: Vec<_> = (0..length)
                    .map(|offset| ((offset * 17 + index * 29) % 251) as u8)
                    .collect();
                entries.push(entry(temp.path(), &index.to_string(), &data));
                stream.extend_from_slice(&data);
                let padding = index + 1;
                entries.push(FileEntry {
                    disk_path: temp.path().join("unbacked"),
                    components: vec![b"pad".to_vec()],
                    length: padding as u64,
                    padding: true,
                });
                stream.resize(stream.len() + padding, 0);
            }
            for piece in [16384, DIRECT_MIN, 4 * DIRECT_MIN] {
                let expected: Vec<[u8; 20]> = stream
                    .chunks(piece)
                    .map(|bytes| ::sha1::Sha1::digest(bytes).into())
                    .collect();
                for direct in [false, true] {
                    for batch in [3 * DIRECT_MIN + 7, 20 * DIRECT_MIN] {
                        assert_eq!(
                            hash(&entries, piece, 3, batch, direct).unwrap(),
                            expected,
                            "piece={piece}, batch={batch}, direct={direct}"
                        );
                    }
                }
            }
        }

        #[test]
        fn direct_and_fallback_preserve_cross_file_padding_and_order() {
            let temp = tempfile::tempdir().unwrap();
            let data: Vec<u8> = (0..2 * 1024 * 1024 + 37).map(|n| (n * 17) as u8).collect();
            let mut entries = vec![
                entry(temp.path(), "empty", b""),
                entry(temp.path(), "prefix", b"misaligned"),
                entry(temp.path(), "data", &data),
            ];
            entries.push(FileEntry {
                disk_path: temp.path().join("unbacked"),
                components: vec![b"pad".to_vec()],
                length: 8191,
                padding: true,
            });
            entries.push(entry(temp.path(), "end", b"last"));
            let mut bytes = b"misaligned".to_vec();
            bytes.extend_from_slice(&data);
            bytes.resize(bytes.len() + 8191, 0);
            bytes.extend_from_slice(b"last");
            for piece in [16384, 1024 * 1024, 16 * 1024 * 1024] {
                let expected: Vec<_> = bytes.chunks(piece).map(sha1::hash).collect();
                for direct in [true, false] {
                    for batch in [piece, piece * 16 + 3] {
                        assert_eq!(hash(&entries, piece, 3, batch, direct).unwrap(), expected);
                    }
                }
            }
        }

        #[test]
        fn short_final_reads_and_tiny_payload_allocation_match_reference() {
            let temp = tempfile::tempdir().unwrap();
            for length in [0, 1, 4095, 4096, 4097, 16 * 16384 + 19] {
                let bytes: Vec<u8> = (0..length).map(|n| n as u8).collect();
                let input = entry(temp.path(), "data", &bytes);
                let expected: Vec<_> = bytes.chunks(16384).map(sha1::hash).collect();
                assert_eq!(
                    hash(&[input], 16384, 2, 16 * 16384, true).unwrap(),
                    expected
                );
            }
            let input = entry(temp.path(), "tiny", b"abc");
            assert_eq!(
                hash(&[input], 64 * 1024 * 1024, 16, usize::MAX, true).unwrap(),
                vec![sha1::hash(b"abc")]
            );
        }

        #[test]
        fn rejects_missing_changed_nonregular_and_invalid_inputs() {
            use std::{
                ffi::CString,
                os::unix::{ffi::OsStrExt, fs::symlink},
            };
            let temp = tempfile::tempdir().unwrap();
            let input = entry(temp.path(), "data", b"abcdef");
            let mut changed = input.clone();
            changed.length = 7;
            assert!(hash(&[changed], 16384, 1, 16384, true).is_err());
            let held = open_input(&input, true).unwrap();
            fs::write(&input.disk_path, b"a").unwrap();
            assert!(read_exact_at(held.regular.as_ref().unwrap(), &mut [0; 6], 0).is_err());
            let mut missing = input.clone();
            missing.disk_path = temp.path().join("missing");
            assert!(hash(&[missing], 16384, 1, 16384, true).is_err());
            let mut linked = input.clone();
            linked.disk_path = temp.path().join("link");
            symlink(&input.disk_path, &linked.disk_path).unwrap();
            assert!(hash(&[linked], 16384, 1, 16384, true).is_err());
            let mut fifo = input;
            fifo.disk_path = temp.path().join("fifo");
            let path = CString::new(fifo.disk_path.as_os_str().as_bytes()).unwrap();
            // SAFETY: path is a live C string inside the test's temporary directory.
            assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
            assert!(hash(&[fifo], 16384, 1, 16384, true).is_err());
            assert!(hash(&[], 0, 1, 0, true).is_err());
            assert!(hash(&[], 16384, 0, 0, true).is_err());
            assert!(hash(&[], 16384, 1, 0, true).unwrap().is_empty());
            for error in [libc::EINVAL, libc::EOPNOTSUPP, libc::ENOSYS] {
                assert!(unsupported(&io::Error::from_raw_os_error(error)));
            }
            assert!(!unsupported(&io::Error::from_raw_os_error(libc::EIO)));
        }
    }
}
