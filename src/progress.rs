use std::{
    io::{self, IsTerminal, Write},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use rustide::create::{CreateProgress, Protocol};

const LIVE_INTERVAL: Duration = Duration::from_millis(150);
const PLAIN_INTERVAL: Duration = Duration::from_secs(2);

pub struct Progress {
    shared: Option<Arc<Shared>>,
    worker: Option<JoinHandle<()>>,
}

struct Shared {
    bytes: AtomicU64,
    display: Mutex<Display>,
    wake: Condvar,
    live: bool,
}

struct Display {
    phase: Phase,
    started: Instant,
    rendered: Instant,
    width: usize,
    stopped: bool,
}

enum Phase {
    Idle,
    Status(&'static str),
    Hashing { protocol: Protocol, total: u64 },
}

impl Progress {
    pub fn new(enabled: bool, simple: bool) -> Self {
        if !enabled {
            return Self {
                shared: None,
                worker: None,
            };
        }
        let live = !simple
            && io::stderr().is_terminal()
            && std::env::var_os("TERM").is_none_or(|term| term != "dumb");
        let now = Instant::now();
        let shared = Arc::new(Shared {
            bytes: AtomicU64::new(0),
            display: Mutex::new(Display {
                phase: Phase::Idle,
                started: now,
                rendered: now,
                width: 0,
                stopped: false,
            }),
            wake: Condvar::new(),
            live,
        });
        let worker_shared = shared.clone();
        let worker = thread::spawn(move || {
            let interval = if live { LIVE_INTERVAL } else { PLAIN_INTERVAL };
            let mut display = worker_shared.display.lock().unwrap();
            while !display.stopped {
                let (next, _) = worker_shared.wake.wait_timeout(display, interval).unwrap();
                display = next;
                if !display.stopped && display.rendered.elapsed() >= interval {
                    worker_shared.render(&mut display);
                }
            }
        });
        Self {
            shared: Some(shared),
            worker: Some(worker),
        }
    }

    pub fn update(&self, event: CreateProgress) {
        let Some(shared) = &self.shared else { return };
        if let CreateProgress::Hashed { bytes } = event {
            shared.bytes.fetch_add(bytes, Ordering::Relaxed);
            return;
        }
        let mut display = shared.display.lock().unwrap();
        shared.finish_phase(&mut display);
        display.started = Instant::now();
        display.phase = match event {
            CreateProgress::Scanning => Phase::Status("Scanning..."),
            CreateProgress::Hashing {
                protocol,
                files,
                payload_bytes,
                piece_bytes,
                workers,
                io_backend,
                hash_backend,
            } => {
                shared.bytes.store(0, Ordering::Relaxed);
                let _ = writeln!(
                    io::stderr().lock(),
                    "{} | {} files | {} | {} pieces | {} workers | {}/{}",
                    protocol_name(protocol),
                    files,
                    byte_size(payload_bytes as f64),
                    byte_size(piece_bytes as f64),
                    workers,
                    hash_backend,
                    io_backend,
                );
                Phase::Hashing {
                    protocol,
                    total: payload_bytes,
                }
            }
            CreateProgress::Checksums => Phase::Status("Checksums..."),
            CreateProgress::Metadata => Phase::Status("Metadata..."),
            CreateProgress::Hashed { .. } => unreachable!(),
        };
        shared.render(&mut display);
    }

    pub fn writing(&self) {
        let Some(shared) = &self.shared else { return };
        let mut display = shared.display.lock().unwrap();
        shared.finish_phase(&mut display);
        display.phase = Phase::Status("Writing...");
        display.started = Instant::now();
        shared.render(&mut display);
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        if let Some(shared) = &self.shared {
            let mut display = shared.display.lock().unwrap();
            display.stopped = true;
            // Errors also leave the cursor on a clean line, without claiming completion.
            shared.clear(&mut display);
            shared.wake.notify_one();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Shared {
    fn render(&self, display: &mut Display) {
        let elapsed = display.started.elapsed().as_secs_f64();
        let line = match display.phase {
            Phase::Idle => return,
            Phase::Status(label) => {
                if elapsed >= 1.0 {
                    format!("{label} {elapsed:.1}s")
                } else {
                    label.into()
                }
            }
            Phase::Hashing { protocol, total } => {
                hash_line(protocol, self.bytes.load(Ordering::Relaxed), total, elapsed)
            }
        };
        let mut stderr = io::stderr().lock();
        if self.live {
            let padding = display.width.saturating_sub(line.len());
            let _ = write!(stderr, "\r{line}{}", " ".repeat(padding));
            let _ = stderr.flush();
            display.width = line.len();
        } else {
            let _ = writeln!(stderr, "{line}");
        }
        display.rendered = Instant::now();
    }

    fn finish_phase(&self, display: &mut Display) {
        if matches!(display.phase, Phase::Hashing { .. }) {
            self.render(display);
            if self.live {
                let _ = writeln!(io::stderr().lock());
                display.width = 0;
            }
        } else {
            self.clear(display);
        }
    }

    fn clear(&self, display: &mut Display) {
        if self.live && display.width > 0 {
            let mut stderr = io::stderr().lock();
            let _ = write!(stderr, "\r{}\r", " ".repeat(display.width));
            let _ = stderr.flush();
            display.width = 0;
        }
    }
}

fn protocol_name(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::V1 => "v1",
        Protocol::V2 => "v2",
        Protocol::Hybrid => "hybrid",
    }
}

fn hash_line(protocol: Protocol, bytes: u64, total: u64, elapsed: f64) -> String {
    let bytes = bytes.min(total);
    let percent = if bytes == total {
        100.0
    } else {
        (bytes as f64 / total as f64 * 100.0).min(99.9)
    };
    let rate = if elapsed > 0.0 {
        bytes as f64 / elapsed
    } else {
        0.0
    };
    let eta = if bytes == total {
        "00:00".into()
    } else if rate > 0.0 {
        duration(((total - bytes) as f64 / rate).ceil() as u64)
    } else {
        "--:--".into()
    };
    format!(
        "Hashing {} {percent:5.1}%  {}/{}  {}/s  ETA {eta}",
        protocol_name(protocol),
        byte_size(bytes as f64),
        byte_size(total as f64),
        byte_size(rate),
    )
}

fn byte_size(bytes: f64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = bytes;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

fn duration(seconds: u64) -> String {
    if seconds >= 3600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
    } else {
        format!("{:02}:{:02}", seconds / 60, seconds % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiny_empty_and_partial_payloads_have_finite_truthful_progress() {
        assert_eq!(
            hash_line(Protocol::V1, 0, 3, 0.0),
            "Hashing v1   0.0%  0 B/3 B  0 B/s  ETA --:--"
        );
        assert_eq!(
            hash_line(Protocol::V2, 1, 3, 1.0),
            "Hashing v2  33.3%  1 B/3 B  1 B/s  ETA 00:02"
        );
        assert_eq!(
            hash_line(Protocol::V1, 0, 0, 0.0),
            "Hashing v1 100.0%  0 B/0 B  0 B/s  ETA 00:00"
        );
    }

    #[test]
    fn sizes_and_eta_cover_small_files_and_long_jobs() {
        assert_eq!(byte_size(1024.0), "1.00 KiB");
        assert_eq!(byte_size(1073741824.0), "1.00 GiB");
        assert_eq!(duration(3661), "1:01:01");
        assert!(hash_line(Protocol::V1, 999_999, 1_000_000, 1.0).contains("99.9%"));
    }
}
