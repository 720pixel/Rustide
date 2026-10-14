use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    path::{Path, PathBuf},
    str::FromStr,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail, ensure};
use memmap2::{Mmap, MmapOptions};
use rayon::prelude::*;
use regex::Regex;
use serde::Serialize;
use sha2::Digest;
use walkdir::WalkDir;

use crate::{bencode::Value, hash, merkle, system::System};

pub const MIN_PIECE: usize = 16 * 1024;
pub const AUTO_MAX_PIECE: usize = 16 * 1024 * 1024;
pub const MAX_PIECE: usize = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    V1,
    V2,
    Hybrid,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum IoMode {
    #[default]
    Auto,
    Mmap,
    Buffered,
    Direct,
}

impl FromStr for Protocol {
    type Err = String;
    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "1" | "v1" => Ok(Self::V1),
            "2" | "v2" => Ok(Self::V2),
            "hybrid" => Ok(Self::Hybrid),
            _ => Err("protocol must be v1, v2, or hybrid".into()),
        }
    }
}

#[derive(Clone, Debug)]
pub struct FileEntry {
    pub disk_path: PathBuf,
    pub components: Vec<Vec<u8>>,
    pub length: u64,
    pub padding: bool,
}

#[derive(Clone, Debug)]
pub struct CreateOptions {
    pub target: PathBuf,
    pub output: Option<PathBuf>,
    pub protocol: Protocol,
    pub piece_length: Option<usize>,
    pub threads: Option<usize>,
    pub io: IoMode,
    pub name: Option<String>,
    pub announces: Vec<Vec<String>>,
    pub web_seeds: Vec<String>,
    pub http_seeds: Vec<String>,
    pub dht_nodes: Vec<String>,
    pub private: Option<bool>,
    pub source: Option<String>,
    pub comment: Option<String>,
    pub created_by: Option<String>,
    pub creation_date: Option<i64>,
    pub no_created_by: bool,
    pub no_creation_date: bool,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub include_hidden: bool,
    pub follow_symlinks: bool,
    pub checksums: Vec<String>,
    pub collections: Vec<String>,
    pub similar: Vec<Vec<u8>>,
}

impl Default for CreateOptions {
    fn default() -> Self {
        Self {
            target: PathBuf::new(),
            output: None,
            protocol: Protocol::V1,
            piece_length: None,
            threads: None,
            io: IoMode::Auto,
            name: None,
            announces: Vec::new(),
            web_seeds: Vec::new(),
            http_seeds: Vec::new(),
            dht_nodes: Vec::new(),
            private: None,
            source: None,
            comment: None,
            created_by: None,
            creation_date: None,
            no_created_by: false,
            no_creation_date: false,
            include: Vec::new(),
            exclude: Vec::new(),
            include_hidden: false,
            follow_symlinks: false,
            checksums: Vec::new(),
            collections: Vec::new(),
            similar: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct CreateReport {
    pub protocol: Protocol,
    pub files: usize,
    pub payload_bytes: u64,
    pub piece_bytes: usize,
    pub workers: usize,
    pub io_backend: &'static str,
    pub hash_seconds: f64,
    pub total_seconds: f64,
    pub gib_per_second: f64,
    pub system: System,
}

pub struct CreateResult {
    pub value: Value,
    pub report: CreateReport,
}

/// Creation phases and successful payload hashing. Callbacks may run concurrently.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreateProgress {
    Scanning,
    /// Begins a pass; hybrid creation emits v1 followed by v2 with fresh counters.
    Hashing {
        protocol: Protocol,
        files: usize,
        payload_bytes: u64,
        piece_bytes: usize,
        workers: usize,
        io_backend: &'static str,
        hash_backend: &'static str,
    },
    /// Payload bytes completed since the previous event, excluding padding.
    Hashed {
        bytes: u64,
    },
    Checksums,
    Metadata,
}

#[derive(Clone, Copy)]
struct HashProgress<'a> {
    hashed: &'a (dyn Fn(u64) + Sync),
    pass: &'a (dyn Fn(Protocol) + Sync),
}

pub struct HashResult {
    pub v1: Vec<[u8; 20]>,
    pub v2: Vec<merkle::FileHashes>,
}

pub fn automatic_piece_length(total: u64) -> usize {
    let target = total.div_ceil(1000).max(MIN_PIECE as u64);
    target
        .checked_next_power_of_two()
        .unwrap_or(u64::MAX)
        .min(AUTO_MAX_PIECE as u64) as usize
}

pub fn parse_piece_length(text: &str) -> Result<Option<usize>> {
    let text = text.trim().to_ascii_lowercase().replace(' ', "");
    if text == "auto" {
        return Ok(None);
    }
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    ensure!(split > 0, "piece size needs a number");
    let number: u64 = text[..split].parse().context("invalid piece size")?;
    let suffix = &text[split..];
    let bytes = match suffix {
        "" if number < MIN_PIECE as u64 => 1u64
            .checked_shl(number.try_into().context("piece exponent is too large")?)
            .context("piece exponent is too large")?,
        "" | "b" => number,
        "k" | "kb" | "kib" => number.checked_mul(1024).context("piece size overflow")?,
        "m" | "mb" | "mib" => number
            .checked_mul(1024 * 1024)
            .context("piece size overflow")?,
        _ => bail!("use bytes, a power-of-two exponent, KiB, or MiB for piece size"),
    };
    let size: usize = bytes
        .try_into()
        .context("piece size is too large for this system")?;
    validate_piece_length(size)?;
    Ok(Some(size))
}

pub fn validate_piece_length(size: usize) -> Result<()> {
    ensure!(
        (MIN_PIECE..=MAX_PIECE).contains(&size) && size.is_power_of_two(),
        "piece size must be a power of two between 16 KiB and 64 MiB"
    );
    Ok(())
}

pub fn validate_options(options: &CreateOptions) -> Result<()> {
    if let Some(size) = options.piece_length {
        validate_piece_length(size)?;
    }
    ensure!(options.threads != Some(0), "threads must be positive");
    if let Some(name) = &options.name {
        ensure!(
            !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0']),
            "torrent name must be one path component"
        );
    }
    for algorithm in &options.checksums {
        ensure!(
            matches!(
                algorithm.to_ascii_lowercase().replace('-', "").as_str(),
                "sha1" | "sha256" | "sha512" | "md5"
            ),
            "checksum must be sha1, sha256, sha512, or md5"
        );
    }
    for node in &options.dht_nodes {
        parse_node(node)?;
    }
    ensure!(
        options.similar.iter().all(|hash| hash.len() == 20),
        "similar hashes must contain 20 bytes"
    );
    Ok(())
}

fn path_components(path: &Path) -> Result<Vec<Vec<u8>>> {
    path.components()
        .map(|part| {
            let std::path::Component::Normal(value) = part else {
                bail!("invalid relative file path");
            };
            let text = value.to_str().context("file names must be valid UTF-8")?;
            ensure!(
                !text.is_empty() && text != "." && text != "..",
                "invalid file name"
            );
            Ok(text.as_bytes().to_vec())
        })
        .collect()
}

pub fn scan(options: &CreateOptions) -> Result<Vec<FileEntry>> {
    let input = fs::metadata(&options.target)
        .with_context(|| format!("cannot read {}", options.target.display()))?;
    let included: Vec<Regex> = options
        .include
        .iter()
        .map(|s| Regex::new(s))
        .collect::<std::result::Result<_, _>>()
        .context("invalid include expression")?;
    let excluded: Vec<Regex> = options
        .exclude
        .iter()
        .map(|s| Regex::new(s))
        .collect::<std::result::Result<_, _>>()
        .context("invalid exclude expression")?;
    let output = options.output.as_ref().and_then(|p| p.canonicalize().ok());
    let mut files = Vec::new();
    if input.is_file() {
        let components = path_components(Path::new(
            options.target.file_name().context("file has no name")?,
        ))?;
        files.push(FileEntry {
            disk_path: options
                .target
                .canonicalize()
                .context("cannot resolve input file")?,
            components,
            length: input.len(),
            padding: false,
        });
    } else {
        ensure!(input.is_dir(), "input must be a regular file or directory");
        let walker = WalkDir::new(&options.target)
            .follow_links(options.follow_symlinks)
            .into_iter();
        for item in walker.filter_entry(|entry| {
            entry.depth() == 0
                || options.include_hidden
                || !entry.file_type().is_dir()
                || !entry.file_name().to_string_lossy().starts_with('.')
                || !included.is_empty()
        }) {
            let entry = item.context("cannot scan input directory")?;
            if !entry.file_type().is_file() {
                continue;
            }
            if output
                .as_ref()
                .is_some_and(|out| entry.path().canonicalize().ok().as_ref() == Some(out))
            {
                continue;
            }
            let relative = entry
                .path()
                .strip_prefix(&options.target)
                .context("file escaped the input directory")?;
            let components = path_components(relative)?;
            let name = relative.to_string_lossy().replace('\\', "/");
            let explicitly_included = included.iter().any(|r| r.is_match(&name));
            if !included.is_empty() && !explicitly_included {
                continue;
            }
            if !options.include_hidden
                && !explicitly_included
                && components.iter().any(|c| c.starts_with(b"."))
            {
                continue;
            }
            if excluded.iter().any(|r| r.is_match(&name)) {
                continue;
            }
            let metadata = entry.metadata().context("cannot read file metadata")?;
            files.push(FileEntry {
                disk_path: entry
                    .path()
                    .canonicalize()
                    .context("cannot resolve input file")?,
                components,
                length: metadata.len(),
                padding: false,
            });
        }
    }
    files.sort_by(|a, b| a.components.cmp(&b.components));
    ensure!(!files.is_empty(), "input contains no selected files");
    for pair in files.windows(2) {
        ensure!(
            pair[0].components != pair[1].components,
            "duplicate file path"
        );
    }
    for file in &files {
        ensure!(
            file.length <= i64::MAX as u64,
            "file length exceeds the metainfo integer range"
        );
    }
    Ok(files)
}

pub(crate) fn open_input_file(path: &Path) -> std::io::Result<File> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    options.open(path)
}

fn map_file(entry: &FileEntry) -> Result<Mmap> {
    let file = open_input_file(&entry.disk_path)
        .with_context(|| format!("cannot open {}", entry.disk_path.display()))?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() == entry.length,
        "input file type or size changed: {}",
        entry.disk_path.display()
    );
    ensure!(
        entry.length <= isize::MAX as u64,
        "file is too large to map on this system"
    );
    // The file handle stays open until the mapping is established.
    unsafe { MmapOptions::new().map(&file) }
        .with_context(|| format!("cannot map {}", entry.disk_path.display()))
}

fn total_length(files: &[FileEntry]) -> Result<u64> {
    files.iter().try_fold(0u64, |total, file| {
        total
            .checked_add(file.length)
            .context("total file size overflow")
    })
}

pub(crate) fn payload_bytes_in_range(
    files: &[FileEntry],
    ends: &[u64],
    mut begin: u64,
    end: u64,
) -> u64 {
    let mut bytes = 0;
    while begin < end {
        let index = ends.partition_point(|&stop| stop <= begin);
        let stop = ends[index].min(end);
        if !files[index].padding {
            bytes += stop - begin;
        }
        begin = stop;
    }
    bytes
}

pub fn hash_files(
    files: &[FileEntry],
    piece_length: usize,
    protocol: Protocol,
    workers: usize,
) -> Result<HashResult> {
    hash_files_with_io(
        files,
        piece_length,
        protocol,
        workers,
        IoMode::Auto,
        piece_length.saturating_mul(16),
    )
}

pub fn hash_files_with_io(
    files: &[FileEntry],
    piece_length: usize,
    protocol: Protocol,
    workers: usize,
    io: IoMode,
    batch_bytes: usize,
) -> Result<HashResult> {
    hash_files_with_io_inner(
        files,
        piece_length,
        protocol,
        workers,
        io,
        batch_bytes,
        None,
    )
}

fn hash_files_with_io_inner(
    files: &[FileEntry],
    piece_length: usize,
    protocol: Protocol,
    workers: usize,
    io: IoMode,
    batch_bytes: usize,
    progress: Option<HashProgress<'_>>,
) -> Result<HashResult> {
    validate_piece_length(piece_length)?;
    let io = if io == IoMode::Auto {
        files
            .iter()
            .find(|file| !file.padding)
            .map_or(IoMode::Mmap, |file| {
                automatic_io(&System::detect(&file.disk_path), protocol)
            })
    } else {
        io
    };
    if let Some(progress) = progress {
        (progress.pass)(if protocol == Protocol::V2 {
            Protocol::V2
        } else {
            Protocol::V1
        });
    }
    let buffered_v1 = match io {
        IoMode::Direct if protocol != Protocol::V2 => {
            Some(hash::direct::hash_v1_direct_with_progress(
                files,
                piece_length,
                workers.max(1),
                batch_bytes,
                progress.map(|progress| progress.hashed),
            )?)
        }
        IoMode::Buffered if protocol != Protocol::V2 => {
            Some(hash::reader::hash_v1_buffered_with_progress(
                files,
                piece_length,
                workers.max(1),
                batch_bytes,
                progress.map(|progress| progress.hashed),
            )?)
        }
        _ => None,
    };
    if protocol == Protocol::V1
        && let Some(v1) = buffered_v1
    {
        return Ok(HashResult { v1, v2: Vec::new() });
    }
    let total = total_length(files)?;
    let count: usize = total
        .div_ceil(piece_length as u64)
        .try_into()
        .context("too many pieces for this system")?;
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(workers.max(1))
        .build()?;
    let batch_size = 16;
    let shared_maps = if files.len() <= 256 {
        Some(
            files
                .iter()
                .map(|file| {
                    if file.padding || file.length == 0 {
                        Ok(None)
                    } else {
                        map_file(file).map(Some)
                    }
                })
                .collect::<Result<Vec<_>>>()?,
        )
    } else {
        None
    };
    let mut ends = Vec::with_capacity(files.len());
    let mut offset = 0u64;
    for file in files {
        offset += file.length;
        ends.push(offset);
    }
    pool.install(|| {
        let v1 = if let Some(v1) = buffered_v1 {
            v1
        } else if protocol != Protocol::V2 {
            let mut hashes = vec![[0u8; 20]; count];
            hashes.par_chunks_mut(batch_size).enumerate().try_for_each(
                |(batch, output)| -> Result<()> {
                    let starts: Vec<u64> = (0..output.len())
                        .map(|i| ((batch * batch_size + i) as u64) * piece_length as u64)
                        .collect();
                    let mut local = BTreeMap::new();
                    if shared_maps.is_none() {
                        let mut needed = BTreeSet::new();
                        for &start in &starts {
                            let stop = start.saturating_add(piece_length as u64).min(total);
                            let mut position = start;
                            while position < stop {
                                let index = ends.partition_point(|&end| end <= position);
                                if !files[index].padding {
                                    needed.insert(index);
                                }
                                position = ends[index].min(stop);
                            }
                        }
                        for index in needed {
                            local.insert(index, map_file(&files[index])?);
                        }
                    }
                    #[cfg(target_os = "linux")]
                    {
                        // Populate contiguous pages before the lanes read separate pieces.
                        let start = starts[0];
                        let stop = start
                            .saturating_add((output.len() * piece_length) as u64)
                            .min(total);
                        let mut position = start;
                        while position < stop {
                            let index = ends.partition_point(|&end| end <= position);
                            let previous = if index == 0 { 0 } else { ends[index - 1] };
                            let take = (ends[index].min(stop) - position) as usize;
                            if !files[index].padding {
                                let mapping = match &shared_maps {
                                    Some(maps) => maps[index].as_ref().unwrap(),
                                    None => &local[&index],
                                };
                                let begin = (position - previous) as usize;
                                if mapping
                                    .advise_range(memmap2::Advice::PopulateRead, begin, take)
                                    .is_err()
                                {
                                    let _ = mapping.advise_range(
                                        memmap2::Advice::WillNeed,
                                        begin,
                                        take,
                                    );
                                }
                            }
                            position += take as u64;
                        }
                    }
                    let mapping = |index: usize| -> &[u8] {
                        match &shared_maps {
                            Some(maps) => maps[index].as_ref().unwrap(),
                            None => &local[&index],
                        }
                    };
                    let mut owned: Vec<Option<Vec<u8>>> = Vec::with_capacity(output.len());
                    for &start in &starts {
                        let stop = start.saturating_add(piece_length as u64).min(total);
                        let first = ends.partition_point(|&end| end <= start);
                        if !files[first].padding && stop <= ends[first] {
                            owned.push(None);
                            continue;
                        }
                        let mut buffer = Vec::with_capacity((stop - start) as usize);
                        let mut position = start;
                        while position < stop {
                            let index = ends.partition_point(|&end| end <= position);
                            let previous = if index == 0 { 0 } else { ends[index - 1] };
                            let take = (ends[index].min(stop) - position) as usize;
                            if files[index].padding {
                                buffer.resize(buffer.len() + take, 0);
                            } else {
                                let begin = (position - previous) as usize;
                                buffer.extend_from_slice(&mapping(index)[begin..begin + take]);
                            }
                            position += take as u64;
                        }
                        owned.push(Some(buffer));
                    }
                    let pieces: Vec<&[u8]> = starts
                        .iter()
                        .enumerate()
                        .map(|(i, &start)| {
                            if let Some(buffer) = &owned[i] {
                                return buffer.as_slice();
                            }
                            let index = ends.partition_point(|&end| end <= start);
                            let previous = if index == 0 { 0 } else { ends[index - 1] };
                            let begin = (start - previous) as usize;
                            let length = (total - start).min(piece_length as u64) as usize;
                            &mapping(index)[begin..begin + length]
                        })
                        .collect();
                    hash::sha1::hash_many_into(&pieces, output);
                    if let Some(progress) = progress {
                        let start = starts[0];
                        let stop = start
                            .saturating_add((output.len() * piece_length) as u64)
                            .min(total);
                        let bytes = payload_bytes_in_range(files, &ends, start, stop);
                        if bytes > 0 {
                            (progress.hashed)(bytes);
                        }
                    }
                    Ok(())
                },
            )?;
            hashes
        } else {
            Vec::new()
        };
        let v2 = if protocol != Protocol::V1 {
            if protocol == Protocol::Hybrid
                && let Some(progress) = progress
            {
                (progress.pass)(Protocol::V2);
            }
            files
                .par_iter()
                .enumerate()
                .map(|(index, file)| -> Result<merkle::FileHashes> {
                    if file.padding || file.length == 0 {
                        return Ok(merkle::FileHashes {
                            root: None,
                            piece_layer: Vec::new(),
                        });
                    }
                    match &shared_maps {
                        Some(maps) => Ok(merkle::hash_file_with_progress(
                            maps[index].as_ref().unwrap(),
                            piece_length,
                            progress.map(|progress| progress.hashed),
                        )),
                        None => Ok(merkle::hash_file_with_progress(
                            &map_file(file)?,
                            piece_length,
                            progress.map(|progress| progress.hashed),
                        )),
                    }
                })
                .collect::<Result<Vec<_>>>()?
        } else {
            Vec::new()
        };
        Ok(HashResult { v1, v2 })
    })
}

fn integer(value: u64) -> Result<Value> {
    Ok(Value::Int(
        value.try_into().context("integer exceeds metainfo range")?,
    ))
}
fn bytes(value: impl AsRef<[u8]>) -> Value {
    Value::Bytes(value.as_ref().to_vec())
}
fn strings(values: &[String]) -> Value {
    Value::List(values.iter().map(bytes).collect())
}

fn file_tree_insert(
    tree: &mut BTreeMap<Vec<u8>, Value>,
    path: &[Vec<u8>],
    leaf: Value,
) -> Result<()> {
    let (component, rest) = path.split_first().context("empty file path")?;
    let node = tree
        .entry(component.clone())
        .or_insert_with(|| Value::Dict(BTreeMap::new()));
    let children = node
        .as_dict_mut()
        .context("file path conflicts with a directory")?;
    if rest.is_empty() {
        ensure!(children.is_empty(), "duplicate file path");
        children.insert(Vec::new(), leaf);
    } else {
        ensure!(
            !children.contains_key(b"".as_slice()),
            "file path conflicts with a directory"
        );
        file_tree_insert(children, rest, leaf)?;
    }
    Ok(())
}

pub fn parse_node(text: &str) -> Result<Value> {
    let (host, port) = text
        .rsplit_once(':')
        .context("DHT node must be host:port")?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    ensure!(!host.is_empty(), "DHT host is empty");
    let port: u16 = port.parse().context("invalid DHT port")?;
    ensure!(port > 0, "DHT port must be positive");
    Ok(Value::List(vec![bytes(host), Value::Int(port.into())]))
}

pub fn create(options: &CreateOptions) -> Result<CreateResult> {
    create_inner(options, None)
}

/// Create metainfo with phase updates and payload progress from completed pieces.
/// The callback should be lightweight and may run on hashing worker threads.
pub fn create_with_progress(
    options: &CreateOptions,
    progress: &(dyn Fn(CreateProgress) + Sync),
) -> Result<CreateResult> {
    create_inner(options, Some(progress))
}

fn create_inner(
    options: &CreateOptions,
    progress: Option<&(dyn Fn(CreateProgress) + Sync)>,
) -> Result<CreateResult> {
    let started = Instant::now();
    validate_options(options)?;
    if let Some(progress) = progress {
        progress(CreateProgress::Scanning);
    }
    let original = scan(options)?;
    let snapshots: Vec<_> = original
        .iter()
        .map(|f| fs::metadata(&f.disk_path).and_then(|m| m.modified()))
        .collect::<std::io::Result<_>>()?;
    let payload_bytes = total_length(&original)?;
    let piece_length = options
        .piece_length
        .unwrap_or_else(|| automatic_piece_length(payload_bytes));
    validate_piece_length(piece_length)?;
    let mut files = Vec::new();
    let mut offset = 0u64;
    for (index, file) in original.iter().enumerate() {
        files.push(file.clone());
        offset = offset
            .checked_add(file.length)
            .context("file layout overflow")?;
        if options.protocol == Protocol::Hybrid && index + 1 < original.len() {
            let padding =
                (piece_length as u64 - offset % piece_length as u64) % piece_length as u64;
            if padding > 0 {
                files.push(FileEntry {
                    disk_path: PathBuf::new(),
                    components: vec![b".pad".to_vec(), format!("{offset}-{padding}").into_bytes()],
                    length: padding,
                    padding: true,
                });
                offset += padding;
            }
        }
    }
    let system = System::detect(&options.target);
    let io = match options.io {
        IoMode::Auto => automatic_io(&system, options.protocol),
        mode => mode,
    };
    let workers = system
        .workers(
            options.threads,
            offset.div_ceil(piece_length as u64),
            piece_length,
        )
        .min(if io == IoMode::Direct && options.threads.is_none() {
            8
        } else {
            usize::MAX
        });
    let hashing = Instant::now();
    let batch_bytes = reader_batch_size(&system, workers, piece_length);
    let hashed = |bytes| {
        if let Some(progress) = progress {
            progress(CreateProgress::Hashed { bytes });
        }
    };
    let pass = |protocol| {
        if let Some(progress) = progress {
            progress(CreateProgress::Hashing {
                protocol,
                files: original.len(),
                payload_bytes,
                piece_bytes: piece_length,
                workers,
                io_backend: io_backend(io, protocol),
                hash_backend: if protocol == Protocol::V2 {
                    "sha256"
                } else {
                    hash::backend()
                },
            });
        }
    };
    let hashes = hash_files_with_io_inner(
        &files,
        piece_length,
        options.protocol,
        workers,
        io,
        batch_bytes,
        progress.map(|_| HashProgress {
            hashed: &hashed,
            pass: &pass,
        }),
    )?;
    let hash_seconds = hashing.elapsed().as_secs_f64();
    let single = options.target.is_file();
    let name = options.name.clone().unwrap_or_else(|| {
        options
            .target
            .file_name()
            .map_or_else(|| "data".into(), |s| s.to_string_lossy().into_owned())
    });
    ensure!(
        !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\']),
        "torrent name must be one path component"
    );
    let mut info = BTreeMap::new();
    info.insert(b"name".to_vec(), bytes(&name));
    info.insert(b"piece length".to_vec(), integer(piece_length as u64)?);
    if let Some(private) = options.private {
        info.insert(b"private".to_vec(), Value::Int(private.into()));
    }
    if let Some(source) = &options.source {
        info.insert(b"source".to_vec(), bytes(source));
    }
    if !options.collections.is_empty() {
        info.insert(b"collections".to_vec(), strings(&options.collections));
    }
    if !options.similar.is_empty() {
        info.insert(
            b"similar".to_vec(),
            Value::List(options.similar.iter().map(bytes).collect()),
        );
    }
    let mut checksum_values = BTreeMap::<usize, BTreeMap<Vec<u8>, Value>>::new();
    if !options.checksums.is_empty() {
        if let Some(progress) = progress {
            progress(CreateProgress::Checksums);
        }
        for (index, file) in files.iter().enumerate().filter(|(_, f)| !f.padding) {
            let mapping = if file.length > 0 {
                Some(map_file(file)?)
            } else {
                None
            };
            let data = mapping.as_deref().unwrap_or(&[]);
            let mut values = BTreeMap::new();
            for algorithm in &options.checksums {
                let normalized = algorithm.to_ascii_lowercase().replace('-', "");
                let (key, value) = match normalized.as_str() {
                    "sha1" => ("sha1", bytes(hash::sha1::hash(data))),
                    "sha256" => ("sha256", bytes(sha2::Sha256::digest(data))),
                    "sha512" => ("sha512", bytes(sha2::Sha512::digest(data))),
                    "md5" => ("md5sum", bytes(hex::encode(md5::Md5::digest(data)))),
                    _ => bail!("checksum must be sha1, sha256, sha512, or md5"),
                };
                values.insert(key.as_bytes().to_vec(), value);
            }
            checksum_values.insert(index, values);
        }
    }
    if let Some(progress) = progress {
        progress(CreateProgress::Metadata);
    }
    if options.protocol != Protocol::V2 {
        info.insert(
            b"pieces".to_vec(),
            bytes(hashes.v1.into_iter().flatten().collect::<Vec<_>>()),
        );
        if single {
            info.insert(b"length".to_vec(), integer(payload_bytes)?);
            if let Some(values) = checksum_values.get(&0) {
                info.extend(values.clone());
            }
        } else {
            let list = files
                .iter()
                .enumerate()
                .map(|(index, file)| -> Result<Value> {
                    let mut entry = checksum_values.get(&index).cloned().unwrap_or_default();
                    entry.insert(b"length".to_vec(), integer(file.length)?);
                    entry.insert(
                        b"path".to_vec(),
                        Value::List(file.components.iter().map(bytes).collect()),
                    );
                    if file.padding {
                        entry.insert(b"attr".to_vec(), bytes("p"));
                    }
                    Ok(Value::Dict(entry))
                })
                .collect::<Result<Vec<_>>>()?;
            info.insert(b"files".to_vec(), Value::List(list));
        }
    }
    let mut metainfo = BTreeMap::new();
    if options.protocol != Protocol::V1 {
        let mut tree = BTreeMap::new();
        let mut layers = BTreeMap::new();
        for (index, file) in files.iter().enumerate().filter(|(_, f)| !f.padding) {
            let mut leaf = checksum_values.get(&index).cloned().unwrap_or_default();
            leaf.insert(b"length".to_vec(), integer(file.length)?);
            if let Some(root) = hashes.v2[index].root {
                leaf.insert(b"pieces root".to_vec(), bytes(root));
                if !hashes.v2[index].piece_layer.is_empty() {
                    layers.insert(
                        root.to_vec(),
                        bytes(
                            hashes.v2[index]
                                .piece_layer
                                .iter()
                                .flatten()
                                .copied()
                                .collect::<Vec<_>>(),
                        ),
                    );
                }
            }
            let path = if single {
                vec![name.as_bytes().to_vec()]
            } else {
                file.components.clone()
            };
            file_tree_insert(&mut tree, &path, Value::Dict(leaf))?;
        }
        info.insert(b"meta version".to_vec(), Value::Int(2));
        info.insert(b"file tree".to_vec(), Value::Dict(tree));
        metainfo.insert(b"piece layers".to_vec(), Value::Dict(layers));
    }
    metainfo.insert(b"info".to_vec(), Value::Dict(info));
    metainfo.insert(b"encoding".to_vec(), bytes("UTF-8"));
    if !options.no_created_by {
        metainfo.insert(
            b"created by".to_vec(),
            bytes(
                options
                    .created_by
                    .as_deref()
                    .unwrap_or(concat!("Rustide ", env!("CARGO_PKG_VERSION"))),
            ),
        );
    }
    if !options.no_creation_date {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .min(i64::MAX as u64) as i64;
        metainfo.insert(
            b"creation date".to_vec(),
            Value::Int(options.creation_date.unwrap_or(now)),
        );
    }
    if let Some(comment) = &options.comment {
        metainfo.insert(b"comment".to_vec(), bytes(comment));
    }
    let announces: Vec<_> = options
        .announces
        .iter()
        .filter(|tier| !tier.is_empty())
        .collect();
    if let Some(first) = announces.first().and_then(|tier| tier.first()) {
        metainfo.insert(b"announce".to_vec(), bytes(first));
        metainfo.insert(
            b"announce-list".to_vec(),
            Value::List(announces.into_iter().map(|tier| strings(tier)).collect()),
        );
    }
    if !options.web_seeds.is_empty() {
        metainfo.insert(b"url-list".to_vec(), strings(&options.web_seeds));
    }
    if !options.http_seeds.is_empty() {
        metainfo.insert(b"httpseeds".to_vec(), strings(&options.http_seeds));
    }
    if !options.dht_nodes.is_empty() {
        metainfo.insert(
            b"nodes".to_vec(),
            Value::List(
                options
                    .dht_nodes
                    .iter()
                    .map(|node| parse_node(node))
                    .collect::<Result<_>>()?,
            ),
        );
    }
    for (file, modified) in original.iter().zip(snapshots) {
        let current = fs::metadata(&file.disk_path)?;
        ensure!(
            current.len() == file.length && current.modified()? == modified,
            "input changed while hashing: {}",
            file.disk_path.display()
        );
    }
    Ok(CreateResult {
        value: Value::Dict(metainfo),
        report: CreateReport {
            protocol: options.protocol,
            files: original.len(),
            payload_bytes,
            piece_bytes: piece_length,
            workers,
            io_backend: io_backend(io, options.protocol),
            hash_seconds,
            total_seconds: started.elapsed().as_secs_f64(),
            gib_per_second: if hash_seconds > 0.0 {
                payload_bytes as f64 / 1073741824.0 / hash_seconds
            } else {
                0.0
            },
            system,
        },
    })
}

fn io_backend(io: IoMode, protocol: Protocol) -> &'static str {
    if io == IoMode::Direct && protocol != Protocol::V2 && cfg!(target_os = "linux") {
        "direct"
    } else if matches!(io, IoMode::Buffered | IoMode::Direct) && protocol != Protocol::V2 {
        "buffered"
    } else {
        "mmap"
    }
}

pub(crate) fn automatic_io(system: &System, protocol: Protocol) -> IoMode {
    if system.storage == "rotational" {
        IoMode::Buffered
    } else if cfg!(target_os = "linux")
        && system.storage == "solid-state"
        && protocol == Protocol::V1
    {
        IoMode::Direct
    } else {
        IoMode::Mmap
    }
}

pub(crate) fn reader_batch_size(system: &System, workers: usize, piece_length: usize) -> usize {
    piece_length
        .saturating_mul(16)
        .min(
            (system.available_memory_bytes / 4 / (workers as u64 + 1)).min(usize::MAX as u64)
                as usize,
        )
        .max(piece_length)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn reference_tree(data: &[u8], leaves: usize) -> merkle::Hash {
        let mut hashes: Vec<merkle::Hash> = data
            .chunks(merkle::BLOCK_LENGTH)
            .map(|block| sha2::Sha256::digest(block).into())
            .collect();
        hashes.resize(leaves, [0; 32]);
        while hashes.len() > 1 {
            hashes = hashes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| {
                    let bytes: Vec<_> = pair.iter().flatten().copied().collect();
                    sha2::Sha256::digest(bytes).into()
                })
                .collect();
        }
        hashes[0]
    }

    fn reference_file(data: &[u8], piece: usize) -> merkle::FileHashes {
        merkle::FileHashes {
            root: if data.is_empty() {
                None
            } else {
                Some(reference_tree(
                    data,
                    data.len()
                        .div_ceil(merkle::BLOCK_LENGTH)
                        .next_power_of_two(),
                ))
            },
            piece_layer: if data.len() > piece {
                data.chunks(piece)
                    .map(|bytes| reference_tree(bytes, piece / merkle::BLOCK_LENGTH))
                    .collect()
            } else {
                Vec::new()
            },
        }
    }

    #[test]
    fn hashing_progress_counts_payload_and_preserves_reference_hashes() {
        let directory = tempfile::tempdir().unwrap();
        let mut files = Vec::new();
        let mut stream = Vec::new();
        let mut expected_v2 = Vec::new();
        let mut payload = 0;
        for (index, length) in [0, MIN_PIECE - 1, 19 * MIN_PIECE + 17, 1]
            .into_iter()
            .enumerate()
        {
            let bytes: Vec<_> = (0..length)
                .map(|n| ((n * 37 + index) % 251) as u8)
                .collect();
            let disk_path = directory.path().join(index.to_string());
            fs::write(&disk_path, &bytes).unwrap();
            files.push(FileEntry {
                disk_path,
                components: vec![index.to_string().into_bytes()],
                length: length as u64,
                padding: false,
            });
            payload += length as u64;
            stream.extend_from_slice(&bytes);
            expected_v2.push(reference_file(&bytes, MIN_PIECE));
            if index == 1 {
                let padding = 17 * MIN_PIECE + 5;
                files.push(FileEntry {
                    disk_path: PathBuf::new(),
                    components: vec![b"padding".to_vec()],
                    length: padding as u64,
                    padding: true,
                });
                stream.resize(stream.len() + padding, 0);
                expected_v2.push(reference_file(&[], MIN_PIECE));
            }
        }
        let expected_v1: Vec<[u8; 20]> = stream
            .chunks(MIN_PIECE)
            .map(|bytes| sha1::Sha1::digest(bytes).into())
            .collect();
        for io in [IoMode::Mmap, IoMode::Buffered, IoMode::Direct] {
            for protocol in [Protocol::V1, Protocol::V2, Protocol::Hybrid] {
                let passes = Mutex::new(Vec::<(Protocol, u64, usize)>::new());
                let pass = |protocol| passes.lock().unwrap().push((protocol, 0, 0));
                let hashed = |bytes| {
                    assert!(bytes > 0);
                    let mut passes = passes.lock().unwrap();
                    let active = passes.last_mut().expect("hashing phase before bytes");
                    active.1 += bytes;
                    active.2 += 1;
                    assert!(active.1 <= payload);
                    if active.0 == Protocol::V2 {
                        assert!(bytes <= MIN_PIECE as u64);
                    }
                };
                let result = hash_files_with_io_inner(
                    &files,
                    MIN_PIECE,
                    protocol,
                    3,
                    io,
                    3 * MIN_PIECE,
                    Some(HashProgress {
                        hashed: &hashed,
                        pass: &pass,
                    }),
                )
                .unwrap();
                assert_eq!(
                    result.v1,
                    if protocol == Protocol::V2 {
                        Vec::new()
                    } else {
                        expected_v1.clone()
                    }
                );
                assert_eq!(
                    result.v2,
                    if protocol == Protocol::V1 {
                        Vec::new()
                    } else {
                        expected_v2.clone()
                    }
                );
                let passes = passes.into_inner().unwrap();
                let protocols: Vec<_> = passes.iter().map(|pass| pass.0).collect();
                assert_eq!(
                    protocols,
                    match protocol {
                        Protocol::V1 => vec![Protocol::V1],
                        Protocol::V2 => vec![Protocol::V2],
                        Protocol::Hybrid => vec![Protocol::V1, Protocol::V2],
                    }
                );
                for (_, bytes, events) in passes {
                    assert_eq!(bytes, payload);
                    assert!(events > 1, "progress must update during the pass");
                }
            }
        }
    }

    #[test]
    fn creation_progress_covers_empty_single_directory_and_checksum_phases() {
        let temporary = tempfile::tempdir().unwrap();
        let empty = temporary.path().join("empty");
        let single = temporary.path().join("single");
        let directory = temporary.path().join("directory");
        fs::write(&empty, []).unwrap();
        fs::write(&single, b"abc").unwrap();
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join("a"), b"abc").unwrap();
        fs::write(directory.join("b"), vec![7; MIN_PIECE + 19]).unwrap();
        fs::write(directory.join("empty"), []).unwrap();
        for (target, payload) in [(empty, 0), (single, 3), (directory, MIN_PIECE as u64 + 22)] {
            for protocol in [Protocol::V1, Protocol::V2, Protocol::Hybrid] {
                let options = CreateOptions {
                    target: target.clone(),
                    protocol,
                    piece_length: Some(MIN_PIECE),
                    threads: Some(2),
                    io: IoMode::Buffered,
                    checksums: vec!["sha256".into()],
                    no_creation_date: true,
                    ..Default::default()
                };
                let events = Mutex::new(Vec::new());
                let result = create_with_progress(&options, &|event| {
                    events.lock().unwrap().push(event);
                })
                .unwrap();
                assert_eq!(result.value, create(&options).unwrap().value);
                let events = events.into_inner().unwrap();
                assert_eq!(events.first(), Some(&CreateProgress::Scanning));
                assert_eq!(events.last(), Some(&CreateProgress::Metadata));
                assert_eq!(
                    events
                        .iter()
                        .filter(|event| **event == CreateProgress::Checksums)
                        .count(),
                    1
                );
                let mut totals = Vec::<u64>::new();
                for event in events {
                    match event {
                        CreateProgress::Hashing {
                            payload_bytes,
                            io_backend,
                            protocol,
                            ..
                        } => {
                            assert_eq!(payload_bytes, payload);
                            assert_eq!(
                                io_backend,
                                if protocol == Protocol::V2 {
                                    "mmap"
                                } else {
                                    "buffered"
                                }
                            );
                            totals.push(0);
                        }
                        CreateProgress::Hashed { bytes } => *totals.last_mut().unwrap() += bytes,
                        _ => {}
                    }
                }
                assert_eq!(
                    totals,
                    vec![payload; if protocol == Protocol::Hybrid { 2 } else { 1 }]
                );
                let document = crate::metadata::from_value(result.value).unwrap();
                assert_eq!(
                    crate::metadata::verify(&document, &options.target, Some(2)).unwrap()["valid"],
                    true
                );
            }
        }
    }

    #[test]
    fn piece_policy_matches_integer_boundaries() {
        assert_eq!(automatic_piece_length(0), 16384);
        assert_eq!(automatic_piece_length(16_384_000), 16384);
        assert_eq!(automatic_piece_length(16_384_001), 32768);
        assert_eq!(automatic_piece_length(1_048_576_000), 1048576);
        assert_eq!(automatic_piece_length(1_048_576_001), 2097152);
        assert_eq!(automatic_piece_length(u64::MAX), AUTO_MAX_PIECE);
        for size in ["32MiB", "64MiB", "25", "26"] {
            assert!(parse_piece_length(size).unwrap().unwrap() > AUTO_MAX_PIECE);
        }
        for size in ["0", "15K", "3M", "128M", "67"] {
            assert!(parse_piece_length(size).is_err());
        }
    }

    #[test]
    fn cross_file_hashing_matches_concatenation() {
        let dir = tempfile::tempdir().unwrap();
        let lengths = [0, 16383, 1, 32777, 3, 0, 65535];
        let mut stream = Vec::new();
        let mut files = Vec::new();
        for (index, length) in lengths.into_iter().enumerate() {
            let data: Vec<_> = (0..length)
                .map(|i| ((i * 19 + index * 11) % 251) as u8)
                .collect();
            let path = dir.path().join(format!("{index}.bin"));
            fs::write(&path, &data).unwrap();
            stream.extend_from_slice(&data);
            files.push(FileEntry {
                disk_path: path,
                components: vec![format!("{index}.bin").into_bytes()],
                length: length as u64,
                padding: false,
            });
        }
        let output = hash_files(&files, 16384, Protocol::V1, 4).unwrap();
        let expected: Vec<_> = stream.chunks(16384).map(hash::sha1::hash).collect();
        assert_eq!(output.v1, expected);
    }

    #[test]
    fn generated_hybrid_keeps_small_file_roots_correct() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a"), vec![7; 16385]).unwrap();
        fs::write(dir.path().join("b"), vec![9; 31]).unwrap();
        fs::write(dir.path().join("empty"), []).unwrap();
        let result = create(&CreateOptions {
            target: dir.path().to_owned(),
            protocol: Protocol::Hybrid,
            piece_length: Some(32768),
            threads: Some(2),
            ..Default::default()
        })
        .unwrap();
        let info = result.value.get(b"info").unwrap();
        let root = info
            .get(b"file tree")
            .unwrap()
            .get(b"b")
            .unwrap()
            .get(b"")
            .unwrap()
            .get(b"pieces root")
            .unwrap()
            .as_bytes()
            .unwrap();
        assert_eq!(root, sha2::Sha256::digest([9; 31]).as_slice());
    }
}
