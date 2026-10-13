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
        let memory_jobs = (self.available_memory_bytes / (piece_length as u64 * 32).max(1))
            .max(1)
            .min(usize::MAX as u64) as usize;
        if let Some(n) = requested {
            return n.max(1).min(jobs).min(memory_jobs);
        }
        let ceiling = match self.storage {
            "rotational" => 2,
            "solid-state" => self.physical_cores.min(32),
            _ => self.physical_cores.min(16),
        };
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
    let available = fs::read_to_string("/proc/meminfo")
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
        .map_or(2 * 1024 * 1024 * 1024, |kb| kb.saturating_mul(1024));
    let root = Path::new("/sys/fs/cgroup");
    let mut budget = available;
    if let Ok(groups) = fs::read_to_string("/proc/self/cgroup") {
        for group in groups.lines() {
            let Some((hierarchy, controllers, path)) =
                group.split_once(':').and_then(|(first, rest)| {
                    rest.split_once(':')
                        .map(|(controllers, path)| (first, controllers, path))
                })
            else {
                continue;
            };
            let (base, limit, usage) = if hierarchy == "0" && controllers.is_empty() {
                (root.to_path_buf(), "memory.max", "memory.current")
            } else if controllers
                .split(',')
                .any(|controller| controller == "memory")
            {
                (
                    root.join("memory"),
                    "memory.limit_in_bytes",
                    "memory.usage_in_bytes",
                )
            } else {
                continue;
            };
            if path.split('/').any(|component| component == "..") {
                continue;
            }
            let mut directory = base.join(path.trim_start_matches('/'));
            loop {
                if let Some(remaining) = memory_remaining(&directory, limit, usage) {
                    budget = budget.min(remaining);
                }
                if directory == base || !directory.pop() || !directory.starts_with(&base) {
                    break;
                }
            }
        }
    }
    budget
}

#[cfg(target_os = "linux")]
fn memory_remaining(directory: &Path, limit: &str, usage: &str) -> Option<u64> {
    let limit = fs::read_to_string(directory.join(limit))
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?;
    let usage = fs::read_to_string(directory.join(usage))
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?;
    Some(limit.saturating_sub(usage))
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

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn explicit_workers_still_respect_available_memory() {
        let system = System {
            architecture: "x86_64",
            logical_cpus: 128,
            physical_cores: 64,
            available_memory_bytes: 256 * 1024 * 1024,
            storage: "solid-state",
            hash_backend: "portable",
        };
        assert_eq!(system.workers(Some(128), 4000, 16 * 1024 * 1024), 1);
        assert_eq!(system.workers(None, 4000, 16 * 1024 * 1024), 1);
    }

    #[test]
    fn memory_budget_handles_unlimited_and_exhausted_cgroups() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("memory.max"), "1048576\n").unwrap();
        fs::write(directory.path().join("memory.current"), "65536\n").unwrap();
        assert_eq!(
            memory_remaining(directory.path(), "memory.max", "memory.current"),
            Some(983040)
        );
        fs::write(directory.path().join("memory.max"), "max\n").unwrap();
        assert_eq!(
            memory_remaining(directory.path(), "memory.max", "memory.current"),
            None
        );
        fs::write(directory.path().join("memory.max"), "1\n").unwrap();
        assert_eq!(
            memory_remaining(directory.path(), "memory.max", "memory.current"),
            Some(0)
        );
    }
}
