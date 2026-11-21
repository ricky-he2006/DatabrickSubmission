//! A diagnostic run, switched on by `SS_DIAG`. It generates a practice-size
//! dataset with the track's generator, copies it into memory files (shmem,
//! as the scorer's tmpfs `/data` is), and times the pipeline on it as child
//! processes: thread counts, floors (read only, split only, decode only), and
//! CPU time, faults and cgroup throttling per run. It exits non-zero on
//! purpose: the scorer only shows the log of a failed dev run, and only its
//! end, so the most important lines come last.

use std::fs;
use std::io::{Read, Write};
use std::os::fd::FromRawFd;
use std::process::{Command, Stdio};
use std::time::Instant;

fn read(path: &str) -> String {
    fs::read_to_string(path).unwrap_or_default().trim().to_string()
}

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn memfd_create(name: *const u8, flags: u32) -> i32;
}

/// Copies every file under `from` into a memory file and mirrors the tree
/// under `to` as symlinks to `/proc/self/fd/N`: the fds are inherited, so a
/// child opening the link opens its own copy of the memory file. Returns bytes.
#[cfg(target_os = "linux")]
fn into_memory(from: &std::path::Path, to: &std::path::Path, keep: &mut Vec<fs::File>) -> u64 {
    let _ = fs::create_dir_all(to);
    let mut total = 0;
    let Ok(entries) = fs::read_dir(from) else {
        return 0;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let target = to.join(entry.file_name());
        if path.is_dir() {
            total += into_memory(&path, &target, keep);
            continue;
        }
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };
        let fd = unsafe { memfd_create(b"ss\0".as_ptr(), 0) };
        if fd < 0 {
            continue;
        }
        let mut file = unsafe { fs::File::from_raw_fd(fd) };
        if file.write_all(&bytes).is_err() {
            continue;
        }
        total += bytes.len() as u64;
        let _ = std::os::unix::fs::symlink(format!("/proc/self/fd/{fd}"), &target);
        let _ = fs::remove_file(&path);
        keep.push(file);
    }
    total
}

#[cfg(not(target_os = "linux"))]
fn into_memory(_from: &std::path::Path, _to: &std::path::Path, _keep: &mut Vec<fs::File>) -> u64 {
    0
}

/// Wall ms for `threads` threads to each run the same multiply-bound loop:
/// flat up to the number of whole cores, then up to 2x where two threads
/// share a core, and worse where the CPU quota throttles.
fn spin(threads: usize) -> f64 {
    let began = Instant::now();
    std::thread::scope(|scope| {
        for seed in 0..threads {
            scope.spawn(move || {
                // 32 independent lanes: enough to fill the multiply ports of a
                // core whether or not the compiler vectorizes them.
                let mut x = [0u64; 32];
                for (lane, value) in x.iter_mut().enumerate() {
                    *value = (seed * 32 + lane) as u64 | 1;
                }
                for _ in 0..10_000_000u64 {
                    for value in x.iter_mut() {
                        *value = value.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1);
                    }
                }
                std::hint::black_box(x);
            });
        }
    });
    began.elapsed().as_secs_f64() * 1000.0
}

/// (times throttled, ms throttled) by the cgroup so far.
fn throttled() -> (i64, i64) {
    let stat = read("/sys/fs/cgroup/cpu.stat");
    let get = |key: &str| {
        stat.lines()
            .find_map(|line| line.strip_prefix(key)?.trim().parse::<i64>().ok())
            .unwrap_or(0)
    };
    (get("nr_throttled"), get("throttled_usec") / 1000)
}

struct Timing {
    real: f64,
    detail: String,
}

fn child(settings: &[(&str, String)]) -> Timing {
    let exe = std::env::current_exe().unwrap();
    let before = throttled();
    let began = Instant::now();
    let mut command = Command::new(exe);
    command
        .env("SS_CHILD", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    for (name, value) in settings {
        command.env(name, value);
    }
    let mut process = command.spawn().expect("child");
    let mut log = String::new();
    let _ = process.stderr.take().unwrap().read_to_string(&mut log);
    let status = process.wait();
    let real = began.elapsed().as_secs_f64() * 1000.0;
    let after = throttled();
    // "setup=0.008 parse=0.150 ..." -> "8 150 ...", in milliseconds.
    let phases = log
        .lines()
        .rev()
        .find(|line| line.contains("total="))
        .unwrap_or("")
        .split_whitespace()
        .filter_map(|part| part.split_once('=')?.1.parse::<f64>().ok())
        .map(|seconds| format!("{:.0}", seconds * 1000.0))
        .collect::<Vec<_>>()
        .join(" ");
    let cpu = log
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix("cpu= "))
        .unwrap_or("")
        .rsplit_once(" sink")
        .map_or("", |pair| pair.0)
        .to_string();
    let mut detail = format!("{phases} | {cpu} th={}/{}", after.0 - before.0, after.1 - before.1);
    if !status.is_ok_and(|status| status.success()) {
        detail = format!("FAILED {}", log.lines().last().unwrap_or(""));
    }
    Timing { real, detail }
}

pub fn run() -> ! {
    let began = Instant::now();
    // Docker writes /etc/hostname when it creates the container.
    let created = fs::metadata("/etc/hostname")
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|at| at.elapsed().ok())
        .map_or(-1.0, |age| age.as_secs_f64() * 1000.0);
    let cpus = std::thread::available_parallelism().map_or(0, usize::from);
    let affinity = read("/sys/fs/cgroup/cpuset.cpus.effective");
    let spins: Vec<String> = [1usize, 2, 4, 6, 8, 12, 16]
        .iter()
        .map(|&threads| format!("{threads}:{:.0}", spin(threads)))
        .collect();

    // A practice-size dataset (20M events, 5M clicks), from the track's own
    // generator, then moved into memory.
    let scratch = std::env::var("SCRATCH_DIR").unwrap_or_else(|_| "/scratch".into());
    let raw = std::path::PathBuf::from(format!("{scratch}/raw20"));
    let big = std::path::PathBuf::from(format!("{scratch}/m20"));
    let events: u64 = std::env::var("SS_DIAG_EVENTS").ok().and_then(|v| v.parse().ok()).unwrap_or(20_000_000);
    let generating = Instant::now();
    let generated = Command::new("/app/generate")
        .args(["--seed", "7", "--events", &events.to_string(), "--clicks", &(events / 4).to_string(), "--output"])
        .arg(&raw)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    let generate_s = generating.elapsed().as_secs_f64();
    let copying = Instant::now();
    let mut keep = Vec::new();
    let megabytes = into_memory(&raw, &big, &mut keep) >> 20;
    let copy_s = copying.elapsed().as_secs_f64();
    let big = big.to_string_lossy().to_string();

    let on = |extra: &[(&'static str, &str)]| -> Vec<(&'static str, String)> {
        let mut settings = vec![("DATA_DIR", big.clone()), ("OUTPUT_DIR", format!("{scratch}/out"))];
        settings.extend(extra.iter().map(|&(name, value)| (name, value.to_string())));
        settings
    };
    let _ = fs::create_dir_all(format!("{scratch}/out/user_daily"));
    let _ = fs::create_dir_all(format!("{scratch}/out/campaign_daily"));
    let variants: Vec<(&str, usize, Vec<(&'static str, String)>)> = vec![
        ("dev", 2, vec![("OUTPUT_DIR", format!("{scratch}/out"))]),
        ("base", 3, on(&[])),
        ("n12", 3, on(&[("NCPU", "12")])),
        ("n16", 2, on(&[("NCPU", "16")])),
        ("n4", 2, on(&[("NCPU", "4")])),
        ("n2", 1, on(&[("NCPU", "2")])),
        ("n1", 1, on(&[("NCPU", "1")])),
        ("map0", 2, on(&[("SS_MAP", "0")])),
        ("rd", 2, on(&[("SS_MODE", "1")])),
        ("split", 2, on(&[("SS_MODE", "2")])),
        ("dec", 2, on(&[("SS_MODE", "3")])),
        ("1rd", 1, on(&[("SS_MODE", "1"), ("NCPU", "1")])),
        ("1split", 1, on(&[("SS_MODE", "2"), ("NCPU", "1")])),
        ("1dec", 1, on(&[("SS_MODE", "3"), ("NCPU", "1")])),
        ("4dec", 1, on(&[("SS_MODE", "3"), ("NCPU", "4")])),
        ("base", 3, on(&[])),
    ];

    let mut table: Vec<String> = Vec::new();
    for (name, runs, settings) in &variants {
        // Leave room inside the dev run's three minutes.
        if began.elapsed().as_secs() > 120 {
            table.push(format!("{name} skipped"));
            continue;
        }
        let mut all: Vec<Timing> = (0..*runs).map(|_| child(settings)).collect();
        let reals: Vec<String> = all.iter().map(|timing| format!("{:.0}", timing.real)).collect();
        all.sort_by(|a, b| a.real.total_cmp(&b.real));
        table.push(format!("{name} {} | {}", reals.join("/"), all[0].detail));
    }

    eprintln!("ms real | setup parse dedup sess write links total | u/s ms: setup parse dedup sess write, kfaults, invol | throttled n/ms");
    for line in &table {
        eprintln!("{line}");
    }
    eprintln!(
        "gen ok={generated} {generate_s:.1}s copy {megabytes}MB {copy_s:.1}s | cpus={cpus} set={affinity} max={} NCPU={} | start={created:.0}ms",
        read("/sys/fs/cgroup/cpu.max"),
        std::env::var("NCPU").unwrap_or_default()
    );
    eprintln!("spin ms by threads {}", spins.join(" "));
    eprintln!("diag took {:.1}s; exiting 1 on purpose", began.elapsed().as_secs_f64());
    std::process::exit(1);
}
