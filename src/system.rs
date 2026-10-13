use std::path::Path;
#[cfg(target_os = "linux")]
use std::{collections::BTreeSet, fs};

use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct System {
    pub architecture: &'static str,
    pub logical_cpus: usize,
    pub physical_cores: usize,
    pub available_memory_bytes: u64,
    pub storage: &'static str,
    pub hash_backend: &'static str,
}

impl System {
    pub fn detect(path: &Path) -> Self {
        let logical = std::thread::available_parallelism().map_or(1, usize::from);
        Self {
            architecture: std::env::consts::ARCH,
            logical_cpus: logical,
            physical_cores: physical_cores().unwrap_or(logical).min(logical).max(1),
            available_memory_bytes: available_memory(),
            storage: storage_kind(path),
            hash_backend: crate::hash::backend(),
        }
    }

    pub fn workers(&self, requested: Option<usize>, pieces: u64, piece_length: usize) -> usize {
        let jobs = pieces.div_ceil(16).max(1).min(usize::MAX as u64) as usize;
        if let Some(n) = requested {
            return n.max(1).min(jobs);
        }
        let ceiling = match self.storage {
            "rotational" => 2,
            "solid-state" => 16,
            _ => self.physical_cores.min(16),
        };
        let memory_jobs = (self.available_memory_bytes / (piece_length as u64 * 32).max(1))
            .max(1)
            .min(usize::MAX as u64) as usize;
        self.logical_cpus
            .min(ceiling)
            .min(memory_jobs)
            .min(jobs)
            .max(1)
    }
}

#[cfg(target_os = "linux")]
fn physical_cores() -> Option<usize> {
    let mut cores = BTreeSet::new();
    for entry in fs::read_dir("/sys/devices/system/cpu").ok()?.flatten() {
        let name = entry.file_name();
        let text = name.to_string_lossy();
        if !text
            .strip_prefix("cpu")
            .is_some_and(|s| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit()))
        {
            continue;
        }
        let topology = entry.path().join("topology");
        if let (Ok(package), Ok(core)) = (
            fs::read_to_string(topology.join("physical_package_id")),
            fs::read_to_string(topology.join("core_id")),
        ) {
            cores.insert((package.trim().to_owned(), core.trim().to_owned()));
        }
    }
    (!cores.is_empty()).then_some(cores.len())
}

#[cfg(not(target_os = "linux"))]
fn physical_cores() -> Option<usize> {
    None
}

#[cfg(target_os = "linux")]
fn available_memory() -> u64 {
    fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                line.strip_prefix("MemAvailable:")?
                    .split_whitespace()
                    .next()?
                    .parse::<u64>()
                    .ok()
            })
        })
        .map_or(2 * 1024 * 1024 * 1024, |kb| kb.saturating_mul(1024))
}

#[cfg(target_os = "windows")]
fn available_memory() -> u64 {
    #[repr(C)]
    struct MemoryStatus {
        length: u32,
        load: u32,
        total_phys: u64,
        avail_phys: u64,
        total_page: u64,
        avail_page: u64,
        total_virtual: u64,
        avail_virtual: u64,
        avail_extended: u64,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GlobalMemoryStatusEx(status: *mut MemoryStatus) -> i32;
    }
    let mut status = MemoryStatus {
        length: std::mem::size_of::<MemoryStatus>() as u32,
        load: 0,
        total_phys: 0,
        avail_phys: 0,
        total_page: 0,
        avail_page: 0,
        total_virtual: 0,
        avail_virtual: 0,
        avail_extended: 0,
    };
    if unsafe { GlobalMemoryStatusEx(&mut status) } != 0 {
        status.avail_phys
    } else {
        2 * 1024 * 1024 * 1024
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn available_memory() -> u64 {
    2 * 1024 * 1024 * 1024
}

#[cfg(target_os = "linux")]
fn storage_kind(path: &Path) -> &'static str {
    use std::os::unix::fs::MetadataExt;
    fn rotational(node: &Path, depth: usize) -> Option<bool> {
        if depth > 8 {
            return None;
        }
        let node = node.canonicalize().ok()?;
        for candidate in [
            node.join("queue/rotational"),
            node.parent()?.join("queue/rotational"),
        ] {
            if let Ok(value) = fs::read_to_string(candidate) {
                return Some(value.trim() == "1");
            }
        }
        let mut values = Vec::new();
        for slave in fs::read_dir(node.join("slaves")).ok()?.flatten() {
            if let Some(value) = rotational(&slave.path(), depth + 1) {
                values.push(value);
            }
        }
        (!values.is_empty()).then(|| values.into_iter().any(|v| v))
    }
    let Some(metadata) = fs::metadata(path).ok() else {
        return "unknown";
    };
    let device = metadata.dev();
    let node = std::path::PathBuf::from(format!(
        "/sys/dev/block/{}:{}",
        libc::major(device),
        libc::minor(device)
    ));
    match rotational(&node, 0) {
        Some(true) => "rotational",
        Some(false) => "solid-state",
        None => "unknown",
    }
}

#[cfg(not(target_os = "linux"))]
fn storage_kind(_: &Path) -> &'static str {
    "unknown"
}
