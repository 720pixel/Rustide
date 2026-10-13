//! Sequential file reading with reusable buffers and parallel piece hashing.

use std::{
    fs::File,
    io::{self, Read},
    panic::catch_unwind,
    sync::{Arc, Mutex, mpsc},
    thread,
};

use anyhow::{Context, Result, anyhow, ensure};

use crate::create::FileEntry;

struct Job {
    first_piece: usize,
    used: usize,
    buffer: Vec<u8>,
}

struct Completed {
    first_piece: usize,
    hashes: Result<Vec<[u8; 20]>>,
    buffer: Vec<u8>,
}

/// Hash the concatenation of files, including synthesized padding, in piece order.
///
/// A single reader fills whole-piece batches while CPU workers hash earlier
/// batches. `batch_bytes` is rounded down to a multiple of `piece_length`, with
/// a minimum of one piece, then capped at the total payload size. At most
/// `workers + 1` buffers of that effective size are allocated, and fewer are
/// used for small inputs. This bounds payload
/// memory independently of input size. Digest output and temporary per-batch
/// piece references require additional memory proportional to their counts.
///
/// Zero workers or a zero piece length is an error. File lengths must match the
/// supplied entries; short reads and unexpected trailing bytes are errors.
pub fn hash_v1_buffered(
    files: &[FileEntry],
    piece_length: usize,
    workers: usize,
    batch_bytes: usize,
) -> Result<Vec<[u8; 20]>> {
    ensure!(piece_length > 0, "piece length must be greater than zero");
    ensure!(workers > 0, "worker count must be greater than zero");
    let piece_bytes = u64::try_from(piece_length).context("piece length is too large")?;
    let total = files.iter().try_fold(0_u64, |total, entry| {
        total
            .checked_add(entry.length)
            .context("combined file lengths exceed the supported size")
    })?;
    let mut reader = SequentialReader::new(files);
    if total == 0 {
        // Empty entries still need their paths and declared lengths checked.
        ensure!(reader.fill(&mut [0_u8; 1])? == 0, "unexpected input bytes");
        return Ok(Vec::new());
    }
    let effective_batch = (batch_bytes.max(piece_length) / piece_length * piece_length)
        .min(usize::try_from(total).unwrap_or(usize::MAX));
    let piece_count = usize::try_from(total.div_ceil(piece_bytes))
        .context("piece count exceeds the supported size")?;
    let batch_count = usize::try_from(total.div_ceil(effective_batch as u64))
        .context("batch count exceeds the supported size")?;
    let worker_count = workers.min(batch_count);
    let buffer_count = worker_count.saturating_add(1).min(batch_count);
    effective_batch
        .checked_mul(buffer_count)
        .context("requested reader buffer memory exceeds the supported size")?;

    let mut output = Vec::new();
    output
        .try_reserve_exact(piece_count)
        .context("allocate piece digest output")?;
    output.resize(piece_count, [0; 20]);
    let (jobs_tx, jobs_rx) = mpsc::sync_channel::<Job>(worker_count);
    let jobs_rx = Arc::new(Mutex::new(jobs_rx));
    // Only the fixed buffer pool can produce completions, so this queue is
    // bounded by that pool. Workers can always return buffers without blocking
    // the reader while it is sending the next job.
    let (completed_tx, completed_rx) = mpsc::channel::<Completed>();

    thread::scope(|scope| -> Result<Vec<[u8; 20]>> {
        let mut handles = Vec::with_capacity(worker_count);
        for index in 0..worker_count {
            let jobs = Arc::clone(&jobs_rx);
            let completed = completed_tx.clone();
            handles.push(
                thread::Builder::new()
                    .name(format!("rustide-hash-{index}"))
                    .spawn_scoped(scope, move || worker(jobs, completed, piece_length))
                    .context("start piece hashing worker")?,
            );
        }
        drop(completed_tx);
        let result = (|| -> Result<()> {
            let mut allocated = 0;
            let mut pending = 0;
            let mut first_piece = 0;
            loop {
                let mut buffer = if allocated < buffer_count {
                    let mut buffer = Vec::new();
                    buffer
                        .try_reserve_exact(effective_batch)
                        .context("allocate sequential reader buffer")?;
                    buffer.resize(effective_batch, 0);
                    allocated += 1;
                    buffer
                } else {
                    let buffer = collect(&completed_rx, &mut output)?;
                    pending -= 1;
                    buffer
                };
                let used = reader.fill(&mut buffer)?;
                if used == 0 {
                    break;
                }
                let pieces = used.div_ceil(piece_length);
                jobs_tx
                    .send(Job {
                        first_piece,
                        used,
                        buffer,
                    })
                    .map_err(|_| anyhow!("piece hashing workers stopped unexpectedly"))?;
                pending += 1;
                first_piece += pieces;
            }
            while pending > 0 {
                collect(&completed_rx, &mut output)?;
                pending -= 1;
            }
            ensure!(first_piece == piece_count, "input piece count changed");
            Ok(())
        })();
        // Close the queue before joining, including when a read or allocation
        // fails. Workers finish already submitted jobs and then exit.
        drop(jobs_tx);
        for handle in handles {
            handle
                .join()
                .map_err(|_| anyhow!("piece hashing worker panicked"))?;
        }
        result?;
        Ok(output)
    })
}

fn worker(
    jobs: Arc<Mutex<mpsc::Receiver<Job>>>,
    completed: mpsc::Sender<Completed>,
    piece_length: usize,
) {
    loop {
        let job = match jobs.lock() {
            Ok(receiver) => match receiver.recv() {
                Ok(job) => job,
                Err(_) => return,
            },
            Err(_) => return,
        };
        let hashes = catch_unwind(|| {
            let pieces: Vec<_> = job.buffer[..job.used].chunks(piece_length).collect();
            super::sha1::hash_many(&pieces)
        })
        .map_err(|_| anyhow!("piece hashing worker panicked"));
        let failed = hashes.is_err();
        if completed
            .send(Completed {
                first_piece: job.first_piece,
                hashes,
                buffer: job.buffer,
            })
            .is_err()
            || failed
        {
            return;
        }
    }
}

fn collect(completed: &mpsc::Receiver<Completed>, output: &mut [[u8; 20]]) -> Result<Vec<u8>> {
    let completed = completed
        .recv()
        .context("piece hashing workers stopped before completing their jobs")?;
    let hashes = completed.hashes?;
    let end = completed
        .first_piece
        .checked_add(hashes.len())
        .context("piece result index overflow")?;
    let destination = output
        .get_mut(completed.first_piece..end)
        .context("piece result exceeds the expected output")?;
    destination.copy_from_slice(&hashes);
    Ok(completed.buffer)
}

struct SequentialReader<'a> {
    entries: &'a [FileEntry],
    index: usize,
    initialized: bool,
    file: Option<File>,
    remaining: u64,
}

impl<'a> SequentialReader<'a> {
    fn new(entries: &'a [FileEntry]) -> Self {
        Self {
            entries,
            index: 0,
            initialized: false,
            file: None,
            remaining: 0,
        }
    }

    fn fill(&mut self, buffer: &mut [u8]) -> Result<usize> {
        let mut written = 0;
        while written < buffer.len() && self.index < self.entries.len() {
            let entry = &self.entries[self.index];
            if !self.initialized {
                if !entry.padding {
                    let file = crate::create::open_input_file(&entry.disk_path)
                        .with_context(|| format!("open {}", entry.disk_path.display()))?;
                    let metadata = file
                        .metadata()
                        .with_context(|| format!("inspect {}", entry.disk_path.display()))?;
                    ensure!(
                        metadata.is_file(),
                        "{} is not a regular file",
                        entry.disk_path.display()
                    );
                    ensure!(
                        metadata.len() == entry.length,
                        "file length changed for {}: expected {}, found {}",
                        entry.disk_path.display(),
                        entry.length,
                        metadata.len()
                    );
                    self.file = Some(file);
                }
                self.remaining = entry.length;
                self.initialized = true;
            }
            if self.remaining == 0 {
                if let Some(file) = self.file.as_mut() {
                    let mut extra = [0_u8; 1];
                    let trailing = loop {
                        match file.read(&mut extra) {
                            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                            result => break result,
                        }
                    }
                    .with_context(|| format!("check end of {}", entry.disk_path.display()))?;
                    ensure!(
                        trailing == 0,
                        "file grew while reading {}",
                        entry.disk_path.display()
                    );
                    let length = file
                        .metadata()
                        .with_context(|| format!("inspect {}", entry.disk_path.display()))?
                        .len();
                    ensure!(
                        length == entry.length,
                        "file length changed while reading {}",
                        entry.disk_path.display()
                    );
                }
                self.file = None;
                self.initialized = false;
                self.index += 1;
                continue;
            }
            let count = self.remaining.min((buffer.len() - written) as u64) as usize;
            let destination = &mut buffer[written..written + count];
            if let Some(file) = self.file.as_mut() {
                file.read_exact(destination)
                    .with_context(|| format!("read {}", entry.disk_path.display()))?;
            } else {
                destination.fill(0);
            }
            self.remaining -= count as u64;
            written += count;
        }
        Ok(written)
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, io::Write, path::Path};

    use super::*;

    #[cfg(unix)]
    #[test]
    fn fifo_input_is_rejected_without_waiting_for_a_writer() {
        use std::os::unix::ffi::OsStrExt;
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("pipe");
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // The CString stays alive for the syscall.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let entry = FileEntry {
            disk_path: path,
            components: vec![b"pipe".to_vec()],
            length: 0,
            padding: false,
        };
        let (send, receive) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = send.send(hash_v1_buffered(&[entry], 16384, 1, 16384).is_err());
        });
        assert!(
            receive
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap()
        );
    }

    fn file(directory: &Path, name: &str, bytes: &[u8]) -> FileEntry {
        let disk_path = directory.join(name);
        fs::write(&disk_path, bytes).unwrap();
        FileEntry {
            disk_path,
            components: vec![name.as_bytes().to_vec()],
            length: bytes.len() as u64,
            padding: false,
        }
    }

    fn expected(bytes: &[u8], piece: usize) -> Vec<[u8; 20]> {
        bytes.chunks(piece).map(super::super::sha1::hash).collect()
    }

    #[test]
    fn known_concatenation_and_empty_entries() {
        let directory = tempfile::tempdir().unwrap();
        let files = [
            file(directory.path(), "empty", b""),
            file(directory.path(), "first", b"a"),
            file(directory.path(), "second", b"bc"),
        ];
        let hashes = hash_v1_buffered(&files, 3, 2, 3).unwrap();
        assert_eq!(
            hashes,
            vec![[
                0xa9, 0x99, 0x3e, 0x36, 0x47, 0x06, 0x81, 0x6a, 0xba, 0x3e, 0x25, 0x71, 0x78, 0x50,
                0xc2, 0x6c, 0x9c, 0xd0, 0xd8, 0x9d,
            ]]
        );
        assert!(hash_v1_buffered(&[], 3, 2, 0).unwrap().is_empty());
        assert!(hash_v1_buffered(&files[..1], 3, 2, 0).unwrap().is_empty());
    }

    #[test]
    fn tiny_input_caps_an_oversized_batch_allocation() {
        let directory = tempfile::tempdir().unwrap();
        let entries = [file(directory.path(), "small", b"abc")];
        assert_eq!(
            hash_v1_buffered(&entries, 64 * 1024 * 1024, 16, usize::MAX).unwrap(),
            expected(b"abc", 64 * 1024 * 1024)
        );
    }

    #[test]
    fn cross_file_padding_batch_boundaries_and_order() {
        let directory = tempfile::tempdir().unwrap();
        let first: Vec<_> = (0..17_913).map(|i| (i * 37) as u8).collect();
        let second: Vec<_> = (0..41_093).map(|i| (i * 19 + 3) as u8).collect();
        let padding = FileEntry {
            disk_path: directory.path().join("no-backing-file"),
            components: vec![b"padding".to_vec()],
            length: 2_071,
            padding: true,
        };
        let files = [
            file(directory.path(), "first", &first),
            padding,
            file(directory.path(), "empty", b""),
            file(directory.path(), "second", &second),
        ];
        let mut bytes = first;
        bytes.resize(bytes.len() + 2_071, 0);
        bytes.extend(second);
        for piece in [1, 7, 1024, 4096] {
            for workers in [1, 3] {
                for batch in [0, piece * 3 + 1, piece * 16] {
                    assert_eq!(
                        hash_v1_buffered(&files, piece, workers, batch).unwrap(),
                        expected(&bytes, piece),
                        "piece {piece}, workers {workers}, batch {batch}"
                    );
                }
            }
        }
    }

    #[test]
    fn rejects_lengths_missing_files_and_invalid_configuration() {
        let directory = tempfile::tempdir().unwrap();
        let entry = file(directory.path(), "data", b"abcdef");
        for length in [0, 5, 7] {
            let mut incorrect = entry.clone();
            incorrect.length = length;
            assert!(hash_v1_buffered(&[incorrect], 2, 2, 4).is_err());
        }
        let mut missing = entry.clone();
        missing.disk_path = directory.path().join("missing");
        missing.length = 0;
        assert!(hash_v1_buffered(&[entry, missing], 2, 2, 2).is_err());
        assert!(hash_v1_buffered(&[], 0, 1, 1).is_err());
        assert!(hash_v1_buffered(&[], 1, 0, 1).is_err());
    }

    #[test]
    fn detects_truncation_and_growth_during_reading() {
        let directory = tempfile::tempdir().unwrap();
        let entries = [file(directory.path(), "truncated", b"abcdef")];
        let mut reader = SequentialReader::new(&entries);
        assert_eq!(reader.fill(&mut [0; 2]).unwrap(), 2);
        fs::write(&entries[0].disk_path, b"a").unwrap();
        assert!(reader.fill(&mut [0; 8]).is_err());

        let entries = [file(directory.path(), "grown", b"abcdef")];
        let mut reader = SequentialReader::new(&entries);
        assert_eq!(reader.fill(&mut [0; 2]).unwrap(), 2);
        fs::OpenOptions::new()
            .append(true)
            .open(&entries[0].disk_path)
            .unwrap()
            .write_all(b"g")
            .unwrap();
        assert!(reader.fill(&mut [0; 8]).is_err());
    }
}
