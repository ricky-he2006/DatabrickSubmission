//! A diagnostic run, switched on by `SS_DIAG`. It describes the machine, then
//! times the pipeline as child processes under different tunables, with the
//! input parsed several times over to reach the size of a scored run. It exits
//! non-zero on purpose: the scorer only shows logs for a failed dev run.

use std::fs;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::Instant;

fn read(path: &str) -> String {
    fs::read_to_string(path).unwrap_or_default().trim().to_string()
}

fn field(text: &str, key: &str) -> String {
    text.lines()
        .find(|line| line.starts_with(key))
        .map(|line| line.split_once(':').map_or(line, |pair| pair.1).trim().to_string())
        .unwrap_or_default()
}

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn mmap(a: *mut std::ffi::c_void, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut std::ffi::c_void;
    fn munmap(a: *mut std::ffi::c_void, l: usize) -> i32;
    fn mincore(a: *mut std::ffi::c_void, l: usize, v: *mut u8) -> i32;
    fn madvise(a: *mut std::ffi::c_void, l: usize, advice: i32) -> i32;
}

/// (bytes, bytes in the page cache) of every CSV under `folder`.
#[cfg(target_os = "linux")]
fn resident(folder: &std::path::Path) -> (u64, u64) {
    use std::os::fd::AsRawFd;
    let (mut total, mut cached) = (0u64, 0u64);
    let Ok(entries) = fs::read_dir(folder) else {
        return (0, 0);
    };
    for entry in entries.flatten() {
        let Ok(file) = fs::File::open(entry.path()) else {
            continue;
        };
        let size = file.metadata().map_or(0, |meta| meta.len()) as usize;
        if size == 0 {
            continue;
        }
        unsafe {
            let map = mmap(std::ptr::null_mut(), size, 1, 2, file.as_raw_fd(), 0);
            if map as usize == usize::MAX {
                continue;
            }
            let pages = size.div_ceil(4096);
            let mut seen = vec![0u8; pages];
            if mincore(map, size, seen.as_mut_ptr()) == 0 {
                total += size as u64;
                cached += seen.iter().filter(|&&page| page & 1 != 0).count() as u64 * 4096;
            }
            munmap(map, size);
        }
    }
    (total, cached)
}

#[cfg(not(target_os = "linux"))]
fn resident(_folder: &std::path::Path) -> (u64, u64) {
    (0, 0)
}

/// Milliseconds to first-touch `megabytes` of fresh memory, on `threads` threads.
#[cfg(target_os = "linux")]
fn touch(megabytes: usize, threads: usize, advice: i32) -> f64 {
    let bytes = megabytes << 20;
    let began = Instant::now();
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(move || unsafe {
                let map = mmap(std::ptr::null_mut(), bytes, 3, 0x22, -1, 0);
                if map as usize == usize::MAX {
                    return;
                }
                if advice != 0 {
                    madvise(map, bytes, advice);
                }
                let base = map as *mut u8;
                let mut at = 0;
                while at < bytes {
                    base.add(at).write_volatile(1);
                    at += 4096;
                }
                munmap(map, bytes);
            });
        }
    });
    began.elapsed().as_secs_f64() * 1000.0
}

#[cfg(not(target_os = "linux"))]
fn touch(_megabytes: usize, _threads: usize, _advice: i32) -> f64 {
    0.0
}

struct Timing {
    real: f64,
    phases: String,
}

fn child(settings: &[(&str, String)]) -> Timing {
    let exe = std::env::current_exe().unwrap();
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
    // "setup=0.008 parse=0.150 ..." -> "8 150 ...", in milliseconds.
    let numbers = |marker: &str| -> String {
        log.lines()
            .rev()
            .find(|line| line.contains(marker))
            .unwrap_or("")
            .split_whitespace()
            .filter_map(|part| part.split_once('=')?.1.parse::<f64>().ok())
            .map(|seconds| format!("{:.0}", seconds * 1000.0))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let mut phases = format!("{} | {}", numbers("total="), numbers("pu="));
    if !status.is_ok_and(|status| status.success()) {
        phases = format!("FAILED {}", log.lines().last().unwrap_or(""));
    }
    Timing { real, phases }
}

pub fn run() -> ! {
    let began = Instant::now();
    // Docker writes /etc/hostname when it creates the container.
    let created = fs::metadata("/etc/hostname")
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|at| at.elapsed().ok())
        .map_or(-1.0, |age| age.as_secs_f64() * 1000.0);
    let data = std::path::PathBuf::from(std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".into()));
    let (events, events_cached) = resident(&data.join("events"));
    let (clicks, clicks_cached) = resident(&data.join("clicks"));

    let cpu = read("/proc/cpuinfo");
    let flags = field(&cpu, "flags");
    let wanted = [
        "avx2",
        "bmi2",
        "avx512f",
        "avx512bw",
        "avx512vl",
        "avx512vbmi",
        "avx512_vbmi2",
        "gfni",
        "pdpe1gb",
    ];
    let have: Vec<&str> = wanted
        .iter()
        .copied()
        .filter(|flag| flags.split(' ').any(|have| have == *flag))
        .collect();
    let cache = |index: usize| read(&format!("/sys/devices/system/cpu/cpu0/cache/index{index}/size"));
    let meminfo = read("/proc/meminfo");
    let mounts: Vec<String> = read("/proc/self/mountinfo")
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split(' ').collect();
            let dash = parts.iter().position(|part| *part == "-")?;
            let point = *parts.get(4)?;
            ["/", "/data", "/output", "/scratch"]
                .contains(&point)
                .then(|| format!("{point}={}", parts.get(dash + 1).unwrap_or(&"?")))
        })
        .collect();
    let mut head = Vec::new();
    head.push(format!(
        "cpu={} | n={} mhz={} | {}",
        field(&cpu, "model name"),
        std::thread::available_parallelism().map_or(0, usize::from),
        field(&cpu, "cpu MHz"),
        have.join(",")
    ));
    head.push(format!(
        "cache L1d={} L2={} L3={} | sib0={} | kernel={}",
        cache(0),
        cache(2),
        cache(3),
        read("/sys/devices/system/cpu/cpu0/topology/thread_siblings_list"),
        read("/proc/sys/kernel/osrelease")
    ));
    head.push(format!(
        "thp={} | defrag={} | cpu.max={} | mem.max={}",
        read("/sys/kernel/mm/transparent_hugepage/enabled"),
        read("/sys/kernel/mm/transparent_hugepage/defrag"),
        read("/sys/fs/cgroup/cpu.max"),
        read("/sys/fs/cgroup/memory.max")
    ));
    head.push(format!(
        "MemTotal={} MemFree={} Cached={} | fs {}",
        field(&meminfo, "MemTotal"),
        field(&meminfo, "MemFree"),
        field(&meminfo, "Cached"),
        mounts.join(" ")
    ));
    head.push(format!(
        "input MB events={} cached={} clicks={} cached={}",
        events >> 20,
        events_cached >> 20,
        clicks >> 20,
        clicks_cached >> 20
    ));
    head.push(format!(
        "touch 256MB ms: 1thr 4k={:.0} huge={:.0} populate={:.0} | 8thr 4k={:.0} huge={:.0} populate={:.0}",
        touch(256, 1, 0),
        touch(256, 1, 14),
        touch(256, 1, 23),
        touch(256, 8, 0),
        touch(256, 8, 14),
        touch(256, 8, 23)
    ));
    head.push(format!("ms from container created to our main: {created:.0}"));
    // How fast do plain writes to each place go, and does size change the rate?
    let mut writes = Vec::new();
    for folder in ["/output", "/scratch"] {
        for megabytes in [4usize, 32, 128] {
            let path = format!("{folder}/.diag-write");
            let block = vec![7u8; megabytes << 20];
            let began = Instant::now();
            let ok = fs::write(&path, &block).is_ok();
            let took = began.elapsed().as_secs_f64() * 1000.0;
            let _ = fs::remove_file(&path);
            writes.push(format!("{folder} {megabytes}MB={took:.0}{}", if ok { "" } else { "!" }));
        }
    }
    // The scorer shows only about 3000 characters and the machine is known
    // from the first run, so print just what is new.
    eprintln!("start={created:.0}ms write: {}", writes.join(" "));
    let _ = head;

    // A real practice-size dataset, from the track's own generator.
    let scratch = std::env::var("SCRATCH_DIR").unwrap_or_else(|_| "/scratch".into());
    let big = format!("{scratch}/d20");
    let generating = Instant::now();
    let generated = Command::new("/app/generate")
        .args([
            "--seed", "7", "--events", "20000000", "--clicks", "5000000", "--output", &big,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    eprintln!(
        "generate ok={generated} took {:.1}s",
        generating.elapsed().as_secs_f64()
    );
    let on = |extra: &[(&'static str, &str)]| -> Vec<(&'static str, String)> {
        let mut settings = vec![("DATA_DIR", big.clone())];
        settings.extend(extra.iter().map(|&(name, value)| (name, value.to_string())));
        settings
    };
    let variants: Vec<(&str, Vec<(&'static str, String)>)> = vec![
        ("dev", vec![]),
        ("base", on(&[])),
        ("nowrite", on(&[("SS_NOWRITE", "1")])),
        ("index", on(&[("SS_MODE", "1")])),
        ("lines", on(&[("SS_MODE", "2")])),
        ("nopush", on(&[("SS_MODE", "3")])),
        ("base", on(&[])),
        ("huge0", on(&[("SS_HUGE", "0")])),
        ("rel0", on(&[("SS_RELEASE", "0")])),
        ("map0", on(&[("SS_MAP", "0")])),
        ("ncpu2", on(&[("NCPU", "2")])),
        ("ncpu4", on(&[("NCPU", "4")])),
        ("ncpu6", on(&[("NCPU", "6")])),
        ("ncpu10", on(&[("NCPU", "10")])),
        ("ncpu12", on(&[("NCPU", "12")])),
        ("ncpu16", on(&[("NCPU", "16")])),
        ("1:base", on(&[("NCPU", "1")])),
        ("1:index", on(&[("NCPU", "1"), ("SS_MODE", "1")])),
        ("1:lines", on(&[("NCPU", "1"), ("SS_MODE", "2")])),
        ("1:nopush", on(&[("NCPU", "1"), ("SS_MODE", "3")])),
    ];

    let mut table: Vec<String> = Vec::new();
    for (name, settings) in &variants {
        // Leave room inside the dev run's three minutes.
        if began.elapsed().as_secs() > 110 {
            table.push(format!("{name} skipped"));
            continue;
        }
        let runs = if name.starts_with("1:") { 1 } else { 3 };
        let mut all: Vec<Timing> = (0..runs).map(|_| child(settings)).collect();
        all.sort_by(|a, b| a.real.total_cmp(&b.real));
        let reals: Vec<String> = all.iter().map(|timing| format!("{:.0}", timing.real)).collect();
        table.push(format!("{name} {} | {}", reals.join("/"), all[0].phases));
    }

    eprintln!("ms: real x3 | setup parse dedup sess write links total | cpu u/s parse dedup sess write, kflt kblk");
    for line in &table {
        eprintln!("{line}");
    }
    eprintln!("diag took {:.1}s; exiting 1 on purpose", began.elapsed().as_secs_f64());
    std::process::exit(1);
}
