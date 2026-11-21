mod dialect;
use dialect::fields;
mod report;
use rayon::prelude::*;
use report::Report;
// Exact Shopping Sessions processing. All state is derived from mounted input.
use std::{
    collections::HashMap,
    env,
    fs::{self, File},
    hash::{BuildHasherDefault, Hasher},
    io::{self, BufRead, BufReader},
    path::{Path, PathBuf},
    time::Instant,
};
#[derive(Default)]
struct FastHasher(u64);
impl Hasher for FastHasher {
    fn finish(&self) -> u64 {
        let mut h = self.0;
        h ^= h >> 33;
        h = h.wrapping_mul(0xff51afd7ed558ccd);
        h ^= h >> 33;
        h = h.wrapping_mul(0xc4ceb9fe1a85ec53);
        h ^ (h >> 33)
    }
    fn write(&mut self, mut bytes: &[u8]) {
        while bytes.len() >= 8 {
            self.0 = (self.0.rotate_left(5) ^ u64::from_le_bytes(bytes[..8].try_into().unwrap()))
                .wrapping_mul(0x517cc1b727220a95);
            bytes = &bytes[8..];
        }
        for &b in bytes {
            self.0 = (self.0.rotate_left(5) ^ u64::from(b)).wrapping_mul(0x517cc1b727220a95);
        }
    }
}
type Map<K, V> = HashMap<K, V, BuildHasherDefault<FastHasher>>;
const WEEK: i64 = 604800000;
const DAY: i64 = 86400000;
fn files(folder: &Path) -> io::Result<Vec<PathBuf>> {
    let mut paths = fs::read_dir(folder)?
        .map(|e| e.map(|e| e.path()))
        .collect::<io::Result<Vec<_>>>()?;
    paths.retain(|p| p.extension().is_some_and(|e| e == "csv"));
    paths.sort();
    Ok(paths)
}
// Only LF terminates a record. Every comma separates fields, even inside quotes.
fn rows<const N: usize>(paths: &[PathBuf], mut visit: impl FnMut([&[u8]; N])) -> io::Result<()> {
    let mut line = Vec::with_capacity(256);
    for path in paths {
        let mut reader = BufReader::with_capacity(1024 * 1024, File::open(path)?);
        line.clear();
        reader.read_until(b'\n', &mut line)?;
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            if line.last() == Some(&b'\n') {
                line.pop();
            }
            let mut fields: [&[u8]; N] = [&[]; N];
            let mut count = 0;
            let mut valid = true;
            for raw in line.split(|&c| c == b',') {
                if count == N {
                    valid = false;
                    break;
                }
                let value = if raw.len() >= 2 && raw[0] == b'"' && raw[raw.len() - 1] == b'"' {
                    &raw[1..raw.len() - 1]
                } else {
                    raw
                };
                if value.contains(&b'"') {
                    valid = false;
                    break;
                }
                fields[count] = value;
                count += 1;
            }
            if valid && count == N {
                visit(fields);
            }
        }
    }
    Ok(())
}
fn number(s: &[u8]) -> Option<i64> {
    if s.is_empty() {
        return None;
    }
    let mut n = 0_i64;
    for &c in s {
        if !c.is_ascii_digit() {
            return None;
        }
        n = n.checked_mul(10)?.checked_add(i64::from(c - b'0'))?;
    }
    Some(n)
}
#[inline]
fn pair(s: &[u8], i: usize) -> Option<i64> {
    let a = s[i].wrapping_sub(b'0');
    let b = s[i + 1].wrapping_sub(b'0');
    if a > 9 || b > 9 {
        return None;
    }
    Some(i64::from(a) * 10 + i64::from(b))
}
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = y - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719468
}
fn timestamp(s: &[u8]) -> Option<i64> {
    if !matches!(s.len(), 20 | 25) || s[4] != b'-' {
        return number(s);
    }
    if s[4] != b'-' || s[7] != b'-' || s[10] != b'T' || s[13] != b':' || s[16] != b':' {
        return None;
    }
    let y = pair(s, 0)? * 100 + pair(s, 2)?;
    let m = pair(s, 5)?;
    let d = pair(s, 8)?;
    let h = pair(s, 11)?;
    let minute = pair(s, 14)?;
    let sec = pair(s, 17)?;
    if !(1..=12).contains(&m) || h > 23 || minute > 59 || sec > 59 {
        return None;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let md = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if d < 1 || d > md[(m - 1) as usize] {
        return None;
    }
    let offset = if s.len() == 20 {
        if s[19] != b'Z' {
            return None;
        }
        0
    } else {
        if !matches!(s[19], b'+' | b'-') || s[22] != b':' {
            return None;
        }
        let oh = pair(s, 20)?;
        let om = pair(s, 23)?;
        if oh > 23 || om > 59 {
            return None;
        }
        (oh * 60 + om) * if s[19] == b'+' { 1 } else { -1 }
    };
    Some(days_from_civil(y, m, d) * DAY + (h * 3600 + minute * 60 + sec - offset * 60) * 1000)
}
struct People {
    ids: Map<u64, u32>,
    names: Vec<u64>,
    parent: Vec<u32>,
    size: Vec<u32>,
    smallest: Vec<u32>,
}
impl People {
    fn new() -> Self {
        Self {
            ids: Map::default(),
            names: Vec::new(),
            parent: Vec::new(),
            size: Vec::new(),
            smallest: Vec::new(),
        }
    }
    fn intern(&mut self, s: u64) -> u32 {
        if let Some(&id) = self.ids.get(&s) {
            return id;
        }
        let id = self.names.len() as u32;
        self.ids.insert(s, id);
        self.names.push(s);
        self.parent.push(id);
        self.smallest.push(id);
        self.size.push(1);
        id
    }
    fn root(&mut self, mut id: u32) -> u32 {
        while self.parent[id as usize] != id {
            let p = self.parent[id as usize];
            self.parent[id as usize] = self.parent[p as usize];
            id = self.parent[id as usize];
        }
        id
    }
    fn union(&mut self, a: u32, b: u32, keys: &Keys) {
        let mut a = self.root(a);
        let mut b = self.root(b);
        if a == b {
            return;
        }
        if self.size[a as usize] < self.size[b as usize] {
            std::mem::swap(&mut a, &mut b);
        }
        self.parent[b as usize] = a;
        self.size[a as usize] += self.size[b as usize];
        let sa = self.smallest[a as usize];
        let sb = self.smallest[b as usize];
        if keys.less(self.names[sb as usize], self.names[sa as usize]) {
            self.smallest[a as usize] = sb;
        }
    }
}
#[derive(Clone, Copy)]
struct Event {
    person: u64,
    category: u32,
    ms: i64,
    revenue: u32,
}

struct Click<'a> {
    person: u64,
    campaign: u32,
    ms: i64,
    id: &'a [u8],
}
#[derive(Default)]
struct Totals {
    sessions: i64,
    purchases: i64,
    revenue: i64,
    assists: i64,
}
fn write_user(
    out: &mut Report,
    person: &[u8],
    day: i64,
    sessions: i64,
    events: i64,
    revenue: i64,
    category: Option<&[u8]>,
) -> io::Result<()> {
    out.text(0, person);
    out.day(1, day);
    out.count(2, sessions);
    out.count(3, events);
    out.count(4, revenue);
    out.optional(5, category);
    out.row()
}
// Short identifiers have a reversible seven-bit representation. Other UTF-8
// identifiers use a disjoint namespace with exact byte comparisons.
#[derive(Default)]
struct Keys {
    unusual: std::sync::Mutex<(Map<Vec<u8>, u64>, Vec<Vec<u8>>)>,
}
impl Keys {
    fn key(&self, value: &[u8]) -> u64 {
        if value.len() == 9 && value[0] > 0 && value[0] < 128 {
            let bytes = u64::from_be_bytes(value[1..].try_into().unwrap());
            // Compact eight nonzero ASCII bytes to 56 bits in three steps.
            if bytes & 0x8080808080808080 == 0
                && (bytes.wrapping_sub(0x0101010101010101) & !bytes & 0x8080808080808080) == 0
            {
                let pairs = (bytes & 0x007f007f007f007f) | ((bytes >> 1) & 0x3f803f803f803f80);
                let quads = (pairs & 0x00003fff00003fff) | ((pairs >> 2) & 0x0fffc0000fffc000);
                return (u64::from(value[0]) << 56)
                    | (quads & 0x0fffffff)
                    | ((quads >> 4) & 0x00fffffff0000000);
            }
        }
        if value.len() <= 9 && value.iter().all(|&b| b > 0 && b < 128) {
            return value
                .iter()
                .fold(0u64, |key, &byte| (key << 7) | u64::from(byte));
        }
        self.unusual(value)
    }
    fn unusual(&self, value: &[u8]) -> u64 {
        let mut guard = self.unusual.lock().unwrap();
        let (map, names) = &mut *guard;
        if let Some(&key) = map.get(value) {
            return key;
        }
        let key = (1u64 << 63) | names.len() as u64;
        map.insert(value.to_vec(), key);
        names.push(value.to_vec());
        key
    }
    fn less(&self, a: u64, b: u64) -> bool {
        if (a | b) >> 63 == 0 {
            let align = |key: u64| {
                let bits = (64 - key.leading_zeros()).div_ceil(7) * 7;
                key << (63 - bits)
            };
            return align(a) < align(b);
        }
        self.bytes(a) < self.bytes(b)
    }
    fn bytes(&self, mut key: u64) -> Vec<u8> {
        if key >> 63 != 0 {
            return self.unusual.lock().unwrap().1[(key & !(1u64 << 63)) as usize].clone();
        }
        let mut name = Vec::with_capacity(11);
        while key != 0 {
            name.push((key & 127) as u8);
            key >>= 7;
        }
        name.reverse();
        name
    }
}
fn scalar_fields<const N: usize>(line: &[u8]) -> Option<[&[u8]; N]> {
    let mut out = [&[][..]; N];
    let mut count = 0;
    for raw in line.split(|&c| c == b',') {
        if count == N {
            return None;
        }
        let v = if raw.len() >= 2 && raw[0] == b'"' && raw[raw.len() - 1] == b'"' {
            &raw[1..raw.len() - 1]
        } else {
            raw
        };
        if v.contains(&b'"') {
            return None;
        }
        out[count] = v;
        count += 1;
    }
    (count == N).then_some(out)
}
fn identity_slot(raw: u64, mask: usize) -> usize {
    let hash = (raw ^ (raw >> 33)).wrapping_mul(0xff51afd7ed558ccd);
    ((hash ^ (hash >> 33)) as usize) & mask
}
fn person_bucket(person: u64) -> usize {
    (person.wrapping_mul(11400714819323198485) >> 58) as usize
}
fn transpose<T>(parts: Vec<[Vec<T>; 64]>) -> Vec<Vec<Vec<T>>> {
    let mut result: Vec<Vec<Vec<T>>> = (0..64).map(|_| Vec::with_capacity(parts.len())).collect();
    for part in parts {
        for (bucket, rows) in result.iter_mut().zip(part) {
            bucket.push(rows);
        }
    }
    result
}
fn concatenate<T>(chunks: Vec<Vec<T>>) -> Vec<T> {
    let mut rows = Vec::with_capacity(chunks.iter().map(Vec::len).sum());
    for mut chunk in chunks {
        rows.append(&mut chunk);
    }
    rows
}
fn main() -> io::Result<()> {
    let began = Instant::now();
    let mut phase = began;
    let mut checkpoint = |label: &str| {
        let now = Instant::now();
        eprintln!("{label}={:.3}", now.duration_since(phase).as_secs_f64());
        phase = now;
    };
    let data_dir = env::var("DATA_DIR").unwrap_or_else(|_| "/data".into());
    let data = Path::new(&data_dir);
    let out_dir = env::var("OUTPUT_DIR").unwrap_or_else(|_| "/output".into());
    let out = Path::new(&out_dir);
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(data.join("manifest.json"))?)?;
    let bound = |key: &str| {
        manifest[key]
            .as_str()
            .and_then(|s| timestamp(s.as_bytes()))
            .ok_or_else(|| io::Error::other("invalid manifest timestamp"))
    };
    let start = bound("window_start")?;
    let end = bound("window_end")?;
    rayon::ThreadPoolBuilder::new()
        .num_threads(
            env::var("NCPU")
                .unwrap_or_else(|_| "8".into())
                .parse()
                .unwrap(),
        )
        .build_global()
        .unwrap();
    let keys = Keys::default();
    let mut categories = vec![b"unknown".to_vec()];
    let mut cat_ids: Map<Vec<u8>, u32> = Map::default();
    cat_ids.insert(b"unknown".to_vec(), 0);
    let mut products: Map<Vec<u8>, u32> = Map::default();
    rows::<2>(&[data.join("products.csv")], |f| {
        let category = if f[1].is_empty() {
            0
        } else if let Some(&id) = cat_ids.get(f[1]) {
            id
        } else {
            let id = categories.len() as u32;
            categories.push(f[1].to_vec());
            cat_ids.insert(f[1].to_vec(), id);
            id
        };
        products.insert(f[0].to_vec(), category);
    })?;
    let mut campaign_names = vec![b"organic".to_vec()];
    let mut channels = vec![b"organic".to_vec()];
    let mut campaigns: Map<Vec<u8>, u32> = Map::default();
    rows::<2>(&[data.join("campaigns.csv")], |f| {
        let id = campaign_names.len() as u32;
        campaign_names.push(f[0].to_vec());
        channels.push(f[1].to_vec());
        campaigns.insert(f[0].to_vec(), id);
    })?;

    checkpoint("dimensions");
    let event_maps = maps(data, "events")?;
    let click_maps = maps(data, "clicks")?;
    let event_keys = Keys::default();
    checkpoint("map_inputs");
    let (identity, chunks) = rayon::join(
        || -> io::Result<_> {
            let mut people = People::new();
            let links = maps(data, "links")?;
            let pairs: Vec<_> = blocks(&links)
                .into_par_iter()
                .map(|block| {
                    memchr::memchr_iter(b'\n', block)
                        .chain((block.last() != Some(&b'\n')).then_some(block.len()))
                        .scan(0, |pos, end| {
                            let row = &block[*pos..end];
                            *pos = end + 1;
                            Some(row)
                        })
                        .filter_map(|line| {
                            let [a, b] = fields::<2>(line)?;
                            if a.is_empty() || b.is_empty() {
                                return None;
                            }
                            Some((keys.key(a), keys.key(b)))
                        })
                        .collect::<Vec<_>>()
                })
                .collect();
            people.ids.reserve(pairs.iter().map(Vec::len).sum());
            for (a, b) in pairs.into_iter().flatten() {
                let a = people.intern(a);
                let b = people.intern(b);
                people.union(a, b, &keys);
            }
            let mut table = vec![(0u64, 0u64); (people.names.len() * 2 + 1).next_power_of_two()];
            let mask = table.len() - 1;
            for id in 0..people.names.len() {
                let root = people.root(id as u32);
                let raw = people.names[id];
                let rep = people.names[people.smallest[root as usize] as usize];
                if raw == rep {
                    continue;
                }
                let mut pos = identity_slot(raw, mask);
                while table[pos].0 != 0 {
                    pos = (pos + 1) & mask;
                }
                table[pos] = (raw, rep);
            }
            Ok(table)
        },
        || {
            let chunks: Vec<_> = blocks(&event_maps)
                .par_iter()
                .map(|block| {
                    let mut bins: [Vec<_>; 128] =
                        std::array::from_fn(|_| Vec::with_capacity(block.len() / 8192));
                    memchr::memchr_iter(b'\n', block)
                        .chain((block.last() != Some(&b'\n')).then_some(block.len()))
                        .scan(0, |pos, end| {
                            let row = &block[*pos..end];
                            *pos = end + 1;
                            Some(row)
                        })
                        .filter_map(|line| {
                            let f = fields::<7>(line)?;
                            if f[0].is_empty() {
                                return None;
                            }
                            let ingested = timestamp(f[3])?;
                            let ms = timestamp(f[2]).unwrap_or(i64::MIN);
                            let purchase = f[4] == b"purchase";
                            let price = if purchase {
                                number(f[6]).unwrap_or(0)
                            } else {
                                0
                            };
                            let valid = ms >= start
                                && ms < end
                                && ingested - ms <= DAY
                                && !f[1].is_empty()
                                && !f[5].is_empty()
                                && (matches!(f[4], b"cart" | b"view")
                                    || purchase && (1..=10000000).contains(&price));
                            let user = keys.key(f[1]);
                            let person = user;
                            Some((
                                event_keys.key(f[0]),
                                ingested,
                                person,
                                if valid { ms } else { i64::MIN },
                                if valid { price as u32 } else { 0 },
                                if purchase {
                                    *products.get(f[5]).unwrap_or(&0)
                                } else {
                                    0
                                },
                            ))
                        })
                        .for_each(|row| {
                            let bin = (row.0.wrapping_mul(11400714819323198485) >> 57) as usize;
                            bins[bin].push(row);
                        });
                    bins
                })
                .collect();
            chunks
        },
    );
    let table = identity?;
    let lookup = |raw: u64| {
        let mask = table.len() - 1;
        let mut pos = identity_slot(raw, mask);
        loop {
            let (key, rep) = table[pos];
            if key == raw {
                return rep;
            }
            if key == 0 {
                return raw;
            }
            pos = (pos + 1) & mask;
        }
    };
    checkpoint("identity_and_event_parse");
    let mut partitions: Vec<Vec<_>> = (0..128).map(|_| Vec::new()).collect();
    for chunk in chunks {
        for (part, rows) in partitions.iter_mut().zip(chunk) {
            part.push(rows);
        }
    }
    let (event_parts, click_parts) = rayon::join(
        || {
            let parts: Vec<_> = partitions
                .into_par_iter()
                .map(|chunks| {
                    // A unique hash slot proves that an ID occurs exactly once. A
                    // shared slot only identifies candidates; resolve those by full ID.
                    let size =
                        (chunks.iter().map(Vec::len).sum::<usize>() * 16 + 64).next_power_of_two();
                    let mut seen = vec![0u64; size / 64];
                    let mut shared = vec![0u64; size / 64];
                    let slot = |id: u64| {
                        let h = (id ^ (id >> 33)).wrapping_mul(0xff51afd7ed558ccd);
                        let i = (h ^ (h >> 33)) as usize & (size - 1);
                        (i >> 6, 1u64 << (i & 63))
                    };
                    for rows in &chunks {
                        for row in rows {
                            let (i, bit) = slot(row.0);
                            shared[i] |= seen[i] & bit;
                            seen[i] |= bit;
                        }
                    }
                    let per_bucket = chunks.iter().map(Vec::len).sum::<usize>() / 64;
                    let mut buckets: [Vec<Event>; 64] =
                        std::array::from_fn(|_| Vec::with_capacity(per_bucket));
                    let mut add = |(_, _, person, ms, revenue, category)| {
                        if ms != i64::MIN {
                            let person = lookup(person);
                            buckets[person_bucket(person)].push(Event {
                                person,
                                ms,
                                revenue,
                                category,
                            });
                        }
                    };
                    let mut ambiguous = Vec::new();
                    for row in chunks.into_iter().flatten() {
                        let (i, bit) = slot(row.0);
                        if shared[i] & bit == 0 {
                            add(row);
                        } else {
                            ambiguous.push(row);
                        }
                    }
                    ambiguous.sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
                    ambiguous.dedup_by(|a, b| a.0 == b.0);
                    ambiguous.into_iter().for_each(add);
                    buckets
                })
                .collect();
            parts
        },
        || {
            let click_parts: Vec<_> = blocks(&click_maps)
                .par_iter()
                .map(|block| {
                    let mut buckets: [Vec<Click>; 64] = std::array::from_fn(|_| Vec::new());
                    memchr::memchr_iter(b'\n', block)
                        .chain((block.last() != Some(&b'\n')).then_some(block.len()))
                        .scan(0, |pos, end| {
                            let row = &block[*pos..end];
                            *pos = end + 1;
                            Some(row)
                        })
                        .filter_map(|line| {
                            let f = fields::<4>(line)?;
                            if f[0].is_empty() || f[1].is_empty() {
                                return None;
                            }
                            let ms = timestamp(f[3])?;
                            if ms < start - WEEK || ms >= end {
                                return None;
                            }
                            let user = keys.key(f[1]);
                            Some(Click {
                                person: lookup(user),
                                ms,
                                campaign: *campaigns.get(f[2])?,
                                id: f[0],
                            })
                        })
                        .for_each(|click| buckets[person_bucket(click.person)].push(click));
                    buckets
                })
                .collect();
            click_parts
        },
    );
    checkpoint("dedup_and_clicks");
    let events = transpose(event_parts);
    let clicks = transpose(click_parts);
    let groups: Vec<_> = events
        .into_par_iter()
        .zip(clicks)
        .map(|(events, clicks)| {
            let mut events = concatenate(events);
            let mut clicks = concatenate(clicks);
            events.sort_unstable_by_key(|e| (e.person, e.ms));
            clicks.sort_unstable_by(|a, b| {
                a.person
                    .cmp(&b.person)
                    .then_with(|| a.ms.cmp(&b.ms))
                    .then_with(|| b.id.cmp(a.id))
            });
            (events, clicks)
        })
        .collect();
    checkpoint("sort_and_collect");
    let shards: io::Result<Vec<Map<(u32, i64), Totals>>> = groups
        .into_par_iter()
        .enumerate()
        .filter(|(part, (events, _))| *part == 0 || !events.is_empty())
        .map(|(part, (events, clicks))| {
            let mut output = Report::part(
                out,
                "user_daily",
                part,
                &[
                    ("user_id", "text"),
                    ("day", "day"),
                    ("sessions", "count"),
                    ("events", "count"),
                    ("revenue_cents", "count"),
                    ("top_category", "optional"),
                ],
            )?;
            let mut totals: Map<(u32, i64), Totals> = Map::default();
            let mut event_pos = 0;
            let mut click_pos = 0;
            let mut active = vec![0_u32; campaign_names.len()];
            let mut active_campaigns: Vec<u32> = Vec::new();
            let mut active_position = vec![usize::MAX; campaign_names.len()];
            while event_pos < events.len() {
                let person = events[event_pos].person;
                let name = keys.bytes(person);
                let person_begin = event_pos;
                while event_pos < events.len() && events[event_pos].person == person {
                    event_pos += 1;
                }
                while click_pos < clicks.len() && clicks[click_pos].person < person {
                    click_pos += 1;
                }
                let click_begin = click_pos;
                while click_pos < clicks.len() && clicks[click_pos].person == person {
                    click_pos += 1;
                }
                let mut left = click_begin;
                let mut right = click_begin;
                for c in active_campaigns.drain(..) {
                    active[c as usize] = 0;
                    active_position[c as usize] = usize::MAX;
                }
                let mut i = person_begin;
                let mut current_day = None;
                let mut day_sessions = 0;
                let mut day_events = 0;
                let mut day_revenue = 0;
                let mut category_revenue: Map<u32, i64> = Map::default();
                while i < event_pos {
                    let session_start = events[i].ms;
                    let day = session_start.div_euclid(DAY);
                    if current_day.is_some_and(|d| d != day) {
                        let category = category_revenue
                            .iter()
                            .min_by(|(a, ar), (b, br)| {
                                br.cmp(ar)
                                    .then(categories[**a as usize].cmp(&categories[**b as usize]))
                            })
                            .map(|(&id, _)| categories[id as usize].as_slice());
                        write_user(
                            &mut output,
                            &name,
                            current_day.unwrap(),
                            day_sessions,
                            day_events,
                            day_revenue,
                            category,
                        )?;
                        day_sessions = 0;
                        day_events = 0;
                        day_revenue = 0;
                        category_revenue.clear();
                    }
                    current_day = Some(day);
                    let mut revenue = 0;
                    let begin = i;
                    loop {
                        let event = events[i];
                        revenue += i64::from(event.revenue);
                        if event.revenue > 0 {
                            *category_revenue.entry(event.category).or_default() +=
                                i64::from(event.revenue);
                        }
                        i += 1;
                        if i == event_pos || events[i].ms - event.ms > 1800000 {
                            break;
                        }
                    }
                    day_sessions += 1;
                    day_events += (i - begin) as i64;
                    day_revenue += revenue;
                    while right < click_pos && clicks[right].ms <= session_start {
                        let c = clicks[right].campaign as usize;
                        if active[c] == 0 {
                            active_position[c] = active_campaigns.len();
                            active_campaigns.push(c as u32);
                        }
                        active[c] += 1;
                        right += 1;
                    }
                    while left < right && clicks[left].ms < session_start - WEEK {
                        let c = clicks[left].campaign as usize;
                        active[c] -= 1;
                        if active[c] == 0 {
                            let pos = active_position[c];
                            active_campaigns.swap_remove(pos);
                            if pos < active_campaigns.len() {
                                active_position[active_campaigns[pos] as usize] = pos;
                            }
                            active_position[c] = usize::MAX;
                        }
                        left += 1;
                    }
                    let credited = if right > left {
                        clicks[right - 1].campaign
                    } else {
                        0
                    };
                    let total = totals.entry((credited, day)).or_default();
                    total.sessions += 1;
                    total.revenue += revenue;
                    total.purchases += i64::from(revenue > 0);
                    for &c in &active_campaigns {
                        totals.entry((c, day)).or_default().assists += 1;
                    }
                }
                let category = category_revenue
                    .iter()
                    .min_by(|(a, ar), (b, br)| {
                        br.cmp(ar)
                            .then(categories[**a as usize].cmp(&categories[**b as usize]))
                    })
                    .map(|(&id, _)| categories[id as usize].as_slice());
                write_user(
                    &mut output,
                    &name,
                    current_day.unwrap(),
                    day_sessions,
                    day_events,
                    day_revenue,
                    category,
                )?;
            }
            output.close()?;
            Ok(totals)
        })
        .collect();
    let mut totals: Map<(u32, i64), Totals> = Map::default();
    for shard in shards? {
        for (key, v) in shard {
            let t = totals.entry(key).or_default();
            t.sessions += v.sessions;
            t.revenue += v.revenue;
            t.purchases += v.purchases;
            t.assists += v.assists;
        }
    }
    checkpoint("session_attribution_and_user_parquet");
    let mut output = Report::new(
        out,
        "campaign_daily",
        &[
            ("campaign_id", "text"),
            ("channel", "text"),
            ("day", "day"),
            ("sessions", "count"),
            ("purchasing_sessions", "count"),
            ("revenue_cents", "count"),
            ("assisted_sessions", "count"),
        ],
    )?;
    for ((campaign, day), total) in totals {
        output.text(0, &campaign_names[campaign as usize]);
        output.text(1, &channels[campaign as usize]);
        output.day(2, day);
        output.count(3, total.sessions);
        output.count(4, total.purchases);
        output.count(5, total.revenue);
        output.count(6, total.assists);
        output.row()?;
    }
    output.close()?;
    eprintln!("native_seconds={:.3}", began.elapsed().as_secs_f64());
    Ok(())
}
fn maps(data: &Path, folder: &str) -> io::Result<Vec<memmap2::Mmap>> {
    let mut mapped = Vec::new();
    for path in files(&data.join(folder))? {
        let file = File::open(path)?;
        if file.metadata()?.len() > 0 {
            // Scorer mounts these files read-only for the duration of a run.
            mapped.push(unsafe { memmap2::Mmap::map(&file)? });
        }
    }
    Ok(mapped)
}
fn blocks(files: &[memmap2::Mmap]) -> Vec<&[u8]> {
    let mut result = Vec::new();
    for file in files {
        let mut p = memchr::memchr(b'\n', file).map_or(file.len(), |n| n + 1);
        while p < file.len() {
            let approx = (p + 1048576).min(file.len());
            let end = memchr::memchr(b'\n', &file[approx..]).map_or(file.len(), |n| approx + n + 1);
            result.push(&file[p..end]);
            p = end;
        }
    }
    result
}
