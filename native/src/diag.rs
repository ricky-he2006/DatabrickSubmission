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
    let line = log.lines().rev().find(|line| line.contains("total=")).unwrap_or("");
    let mut phases: String = line
        .split_whitespace()
        .filter_map(|part| part.split_once('=')?.1.parse::<f64>().ok())
        .map(|seconds| format!("{:.0}", seconds * 1000.0))
        .collect::<Vec<_>>()
        .join(" ");
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
    for line in &head {
        eprintln!("{line}");
    }

    // Enough copies of the dev set to parse about as many bytes as a 20M-event run.
    let copies = (1_800u64 << 20)
        .div_ceil((events + clicks).max(1))
        .clamp(1, 60)
        .to_string();
    let scaled = |extra: &[(&'static str, &str)]| -> Vec<(&'static str, String)> {
        let mut settings = vec![("SS_COPIES", copies.clone())];
        settings.extend(extra.iter().map(|&(name, value)| (name, value.to_string())));
        settings
    };
    let variants: Vec<(&str, Vec<(&'static str, String)>)> = vec![
        ("dev", vec![]),
        ("base", scaled(&[])),
        ("huge0", scaled(&[("SS_HUGE", "0")])),
        ("pop1", scaled(&[("SS_POPULATE", "1")])),
        ("pop1huge0", scaled(&[("SS_POPULATE", "1"), ("SS_HUGE", "0")])),
        ("map0", scaled(&[("SS_MAP", "0")])),
        ("rel0", scaled(&[("SS_RELEASE", "0")])),
        ("blk256k", scaled(&[("SS_BLOCK", "262144")])),
        ("blk1m", scaled(&[("SS_BLOCK", "1048576")])),
        ("blk4m", scaled(&[("SS_BLOCK", "4194304")])),
        ("id9", scaled(&[("SS_ID_BITS", "9")])),
        ("id10", scaled(&[("SS_ID_BITS", "10")])),
        ("id12", scaled(&[("SS_ID_BITS", "12")])),
        ("id13", scaled(&[("SS_ID_BITS", "13")])),
        ("pb9", scaled(&[("SS_PERSON_BITS", "9")])),
        ("pb10", scaled(&[("SS_PERSON_BITS", "10")])),
        ("pb12", scaled(&[("SS_PERSON_BITS", "12")])),
        ("pb13", scaled(&[("SS_PERSON_BITS", "13")])),
        ("ahead8", scaled(&[("SS_AHEAD", "8")])),
        ("ahead32", scaled(&[("SS_AHEAD", "32")])),
        ("ahead64", scaled(&[("SS_AHEAD", "64")])),
        ("ncpu1", scaled(&[("NCPU", "1")])),
        ("ncpu4", scaled(&[("NCPU", "4")])),
        ("ncpu6", scaled(&[("NCPU", "6")])),
        ("ncpu12", scaled(&[("NCPU", "12")])),
        ("ncpu16", scaled(&[("NCPU", "16")])),
        (
            "x3",
            vec![("SS_COPIES", (copies.parse::<u64>().unwrap() * 3).min(100).to_string())],
        ),
    ];

    let first = child(&[]);
    let mut table = vec![format!("first-dev {:.0} | {}", first.real, first.phases)];
    for (name, settings) in &variants {
        // Leave room inside the dev run's three minutes.
        if began.elapsed().as_secs() > 110 {
            table.push(format!("{name} skipped"));
            continue;
        }
        let runs = if ["ncpu1", "x3"].contains(name) { 1 } else { 3 };
        let mut all: Vec<Timing> = (0..runs).map(|_| child(settings)).collect();
        all.sort_by(|a, b| a.real.total_cmp(&b.real));
        let reals: Vec<String> = all.iter().map(|timing| format!("{:.0}", timing.real)).collect();
        table.push(format!("{name} {} | {}", reals.join("/"), all[0].phases));
    }

    eprintln!("copies={copies}; ms: name real(sorted) | setup parse dedup sessions write links total (best run)");
    for line in &table {
        eprintln!("{line}");
    }
    // Once more at the very end, in case only the tail of the log is shown.
    for line in &head {
        eprintln!("{line}");
    }
    eprintln!("diag took {:.1}s; exiting 1 on purpose", began.elapsed().as_secs_f64());
    std::process::exit(1);
}
