//! Bounded parallel reads with Linux direct I/O and a portable buffered fallback.

use crate::create::FileEntry;
use anyhow::Result;

/// Hash concatenated v1 payloads in piece order, including synthesized padding.
///
/// `batch_bytes` rounds down to whole pieces, with a minimum of one piece.
/// In the Linux fast path, each worker owns one buffer capped at total length, plus
/// less than 8 KiB of alignment slack. At most `min(workers, batches)` buffers
/// are allocated; digest output and file descriptors require additional memory.
/// Linux uses direct reads for batches contained in one file and ordinary reads
/// for cross-file batches or unsupported filesystems. Inputs with more than 256
/// real files, and other platforms, use the sequential buffered reader to bound
/// open descriptors; that fallback can require one additional reader buffer.
/// Payloads must remain stable while hashing.
pub fn hash_v1_direct(
    files: &[FileEntry],
    piece_length: usize,
    workers: usize,
    batch_bytes: usize,
) -> Result<Vec<[u8; 20]>> {
    if files.iter().filter(|entry| !entry.padding).count() > 256 {
        return super::reader::hash_v1_buffered(files, piece_length, workers, batch_bytes);
    }
    #[cfg(target_os = "linux")]
    {
        linux::hash(files, piece_length, workers, batch_bytes, true)
    }
    #[cfg(not(target_os = "linux"))]
    {
        super::reader::hash_v1_buffered(files, piece_length, workers, batch_bytes)
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

    pub(super) fn hash(
        files: &[FileEntry],
        piece_length: usize,
        workers: usize,
        batch_bytes: usize,
        allow_direct: bool,
    ) -> Result<Vec<[u8; 20]>> {
        ensure!(
            piece_length > 0 && workers > 0,
            "piece length and workers must be positive"
        );
        let batch_pieces = batch_bytes.max(piece_length) / piece_length;
        let batch_bytes = batch_pieces * piece_length;
        let mut total = 0_u64;
        let mut ends = Vec::with_capacity(files.len());
        let inputs = files
            .iter()
            .map(|entry| {
                total = total
                    .checked_add(entry.length)
                    .context("total length overflow")?;
                ends.push(total);
                open_input(entry, allow_direct)
            })
            .collect::<Result<Vec<_>>>()?;
        if total == 0 {
            return Ok(Vec::new());
        }
        let batches =
            usize::try_from(total.div_ceil(batch_bytes as u64)).context("batch count overflow")?;
        let count =
            usize::try_from(total.div_ceil(piece_length as u64)).context("piece count overflow")?;
        let buffer_bytes = batch_bytes
            .min(usize::try_from(total).unwrap_or(usize::MAX))
            .checked_add(2 * ALIGN - 1)
            .context("buffer length overflow")?
            / ALIGN
            * ALIGN;
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
                            let index = ends.partition_point(|&end| end <= begin);
                            let previous = if index == 0 { 0 } else { ends[index - 1] };
                            let bytes = buffer.bytes();
                            let mut prefix = 0;
                            let mut done = false;
                            let input = &inputs[index];
                            if stop <= ends[index]
                                && input.regular.is_some()
                                && !input.direct_disabled.load(Ordering::Relaxed)
                                && let Some(direct) = &input.direct
                            {
                                let offset = begin - previous;
                                let lead = offset as usize % ALIGN;
                                let requested = (lead + used).div_ceil(ALIGN) * ALIGN;
                                let aligned_offset = offset - lead as u64;
                                match read_at(direct, &mut bytes[..requested], aligned_offset) {
                                    Ok(read) if read >= lead + used => {
                                        prefix = lead;
                                        done = true;
                                    }
                                    Ok(_) => {
                                        // A legal short transfer need not be EOF. Retry
                                        // the required range through the regular descriptor.
                                    }
                                    Err(error) if unsupported(&error) => {
                                        input.direct_disabled.store(true, Ordering::Relaxed);
                                    }
                                    Err(error) => return Err(error).context("direct read"),
                                }
                            }
                            if !done {
                                let mut position = begin;
                                let mut destination = 0;
                                while position < stop {
                                    let index = ends.partition_point(|&end| end <= position);
                                    let previous = if index == 0 { 0 } else { ends[index - 1] };
                                    let length = (ends[index].min(stop) - position) as usize;
                                    let target = &mut bytes[destination..destination + length];
                                    if let Some(regular) = &inputs[index].regular {
                                        read_exact_at(regular, target, position - previous)?;
                                    } else {
                                        target.fill(0);
                                    }
                                    position += length as u64;
                                    destination += length;
                                }
                            }
                            let pieces: Vec<_> =
                                bytes[prefix..prefix + used].chunks(piece_length).collect();
                            let hashes = sha1::hash_many(&pieces);
                            let first = batch * batch_pieces;
                            let mut result = shared
                                .lock()
                                .map_err(|_| anyhow::anyhow!("result lock poisoned"))?;
                            result[first..first + hashes.len()].copy_from_slice(&hashes);
                        }
                        Ok(())
                    })();
                    if result.is_err() {
                        failed.store(true, Ordering::Relaxed);
                    }
                    result
                })
        })?;
        for input in &inputs {
            if let Some(file) = &input.regular {
                ensure!(
                    file.metadata()?.len() == input.length,
                    "input length changed while hashing"
                );
            }
        }
        shared
            .into_inner()
            .map_err(|_| anyhow::anyhow!("result lock poisoned"))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::fs;

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
