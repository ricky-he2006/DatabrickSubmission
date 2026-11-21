mod dialect;
mod fast;
mod report;

use dialect::fields;
use fast::{parse_price, timestamp};
use rayon::prelude::*;
use report::Report;
use rustc_hash::FxHashMap;
use std::{
    collections::hash_map::Entry,
    env,
    fs::{self, File},
    hash::BuildHasherDefault,
    io::{self, BufRead, BufReader},
    path::{Path, PathBuf},
    time::Instant,
};
use voracious_radix_sort::RadixSort;

#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

const WEEK: i64 = 604_800_000;
const DAY: i64 = 86_400_000;
const SESSION_GAP: i64 = 1_800_000;

// ── Key encoding: preserves byte order for ≤9 printable-ASCII IDs ─────
struct Keys {
    unusual: std::sync::Mutex<(FxHashMap<Vec<u8>, u64>, Vec<Vec<u8>>)>,
}
impl Keys {
    fn new() -> Self {
        Self { unusual: std::sync::Mutex::new((FxHashMap::default(), Vec::new())) }
    }
    fn key(&self, v: &[u8]) -> u64 {
        // Fast path: ≤9 bytes, all in 0x01..0x7f. Preserves byte order.
        if v.len() <= 9 && v.iter().all(|&b| b > 0 && b < 128) {
            let mut k = 0u64;
            for &b in v { k = (k << 7) | u64::from(b); }
            return k;
        }
        // Unusual: high bit set, exact match via side table.
        let mut g = self.unusual.lock().unwrap();
        if let Some(&k) = g.0.get(v) { return k; }
        let k = (1u64 << 63) | g.1.len() as u64;
        g.0.insert(v.to_vec(), k);
        g.1.push(v.to_vec());
        k
    }
    fn bytes(&self, k: u64) -> Vec<u8> {
        if k >> 63 == 0 {
            let mut out = Vec::with_capacity(9);
            let mut rest = k;
            while rest != 0 { out.push((rest & 0x7f) as u8); rest >>= 7; }
            out.reverse();
            out
        } else {
            self.unusual.lock().unwrap().1[(k & !(1u64 << 63)) as usize].clone()
        }
    }
}

// ── Identity union-find ──────────────────────────────────────────────
struct People {
    ids: FxHashMap<u64, u32>,
    names: Vec<u64>,
    parent: Vec<u32>,
    size: Vec<u32>,
    smallest: Vec<u64>,
    keys: *const Keys,
}
impl People {
    fn new(keys: &Keys) -> Self {
        Self {
            ids: FxHashMap::default(),
            names: Vec::new(),
            parent: Vec::new(),
            size: Vec::new(),
            smallest: Vec::new(),
            keys: keys as *const Keys,
        }
    }
    fn intern(&mut self, s: u64) -> u32 {
        if let Some(&id) = self.ids.get(&s) { return id; }
        let id = self.names.len() as u32;
        self.ids.insert(s, id);
        self.names.push(s);
        self.parent.push(id);
        self.smallest.push(s);
        self.size.push(1);
        id
    }
    fn root(&mut self, mut id: u32) -> u32 {
        while self.parent[id as usize] != id {
            let g = self.parent[self.parent[id as usize] as usize];
            self.parent[id as usize] = g;
            id = g;
        }
        id
    }
    fn union(&mut self, a: u32, b: u32) {
        let (mut ra, mut rb) = (self.root(a), self.root(b));
        if ra == rb { return; }
        if self.size[ra as usize] < self.size[rb as usize] { std::mem::swap(&mut ra, &mut rb); }
        self.parent[rb as usize] = ra;
        self.size[ra as usize] += self.size[rb as usize];
        let sa = self.smallest[ra as usize];
        let sb = self.smallest[rb as usize];
        // u64 order matches byte order for standard IDs; unusual IDs have bit 63 set,
        // so they sort after all standard IDs byte-wise as well.
        self.smallest[ra as usize] = sa.min(sb);
    }
}

// ── Records ─────────────────────────────────────────────────────────
#[derive(Clone, Copy)]
struct RawEvent {
    id: u64,          // 0 if empty
    ingest_ts: i64,
    user_id: u64,     // 0 if empty
    ts: i64,          // i64::MIN if invalid
    event_type: u8,   // 0=view, 1=cart, 2=purchase, 3=invalid
    product_id: u64,  // 0 if empty
    price_cents: u32, // 0 if invalid or not purchase
}

#[derive(Clone, Copy)]
struct RawClick {
    id: u64,          // 0 if empty
    user_id: u64,     // 0 if empty
    campaign: u32,    // u32::MAX if unknown campaign
    ts: i64,          // i64::MIN if invalid
}

#[derive(Clone, Copy)]
struct Event {
    person: u64,
    ts: i64,
    event_type: u8,
    category: u32,
    price: u32,
}

#[derive(Clone, Copy)]
struct Click {
    person: u64,
    ts: i64,
    campaign: u32,
    id: u64,
}

#[derive(Clone, Copy, Default)]
struct DayTotals {
    sessions: i64,
    revenue: i64,
    purchases: i64,
    assisted: i64,
}

// ── File helpers ────────────────────────────────────────────────────
fn files(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut v: Vec<_> = fs::read_dir(dir)?
        .map(|e| e.map(|e| e.path()))
        .collect::<io::Result<Vec<_>>>()?;
    v.retain(|p| p.extension().is_some_and(|e| e == "csv"));
    v.sort();
    Ok(v)
}

fn mmap(path: &Path) -> io::Result<memmap2::Mmap> {
    let f = File::open(path)?;
    let m = unsafe { memmap2::Mmap::map(&f)? };
    let _ = m.advise(memmap2::Advice::Sequential);
    let _ = m.advise(memmap2::Advice::WillNeed);
    Ok(m)
}

/// Iterate LF-terminated records in a memory-mapped block. Only `\n`
/// ends a line; a trailing `\r` is an ordinary byte of the field it is in.
fn records<const N: usize>(block: &[u8], mut visit: impl FnMut([&[u8]; N])) {
    let body = match memchr::memchr(b'\n', block) {
        Some(p) => &block[p + 1..], // skip header
        None => return,
    };
    let mut start = 0;
    for end in memchr::memchr_iter(b'\n', body) {
        if let Some(f) = fields::<N>(&body[start..end]) {
            visit(f);
        }
        start = end + 1;
    }
    if start < body.len() {
        if let Some(f) = fields::<N>(&body[start..]) {
            visit(f);
        }
    }
}

// ── Main ────────────────────────────────────────────────────────────
fn main() -> io::Result<()> {
    let began = Instant::now();
    let mut phase = began;
    let mut cp = |label: &str| {
        let now = Instant::now();
        eprintln!("{label}={:.3}", now.duration_since(phase).as_secs_f64());
        phase = now;
    };

    let data_dir = env::var("DATA_DIR").unwrap_or_else(|_| "/data".into());
    let out_dir = env::var("OUTPUT_DIR").unwrap_or_else(|_| "/output".into());
    let data = Path::new(&data_dir);
    let out = Path::new(&out_dir);

    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(data.join("manifest.json"))?)?;
    let parse_bound = |k: &str| {
        manifest[k].as_str()
            .and_then(|s| timestamp(s.as_bytes()))
            .ok_or_else(|| io::Error::other("bad manifest"))
    };
    let start = parse_bound("window_start")?;
    let end = parse_bound("window_end")?;

    rayon::ThreadPoolBuilder::new()
        .num_threads(env::var("NCPU").unwrap_or_else(|_| "8".into()).parse().unwrap())
        .build_global().ok();

    let keys = Keys::new();

    // ── Dimensions ──────────────────────────────────────────────────
    let mut category_names: Vec<Vec<u8>> = vec![b"unknown".to_vec()];
    let mut category_ids: FxHashMap<Vec<u8>, u32> = FxHashMap::default();
    category_ids.insert(b"unknown".to_vec(), 0);
    let mut product_cat: FxHashMap<u64, u32> = FxHashMap::default();
    {
        let m = mmap(&data.join("products.csv"))?;
        records::<2>(&m, |f| {
            let cid = match category_ids.get(f[1]) {
                Some(&id) => id,
                None => {
                    let id = category_names.len() as u32;
                    category_names.push(f[1].to_vec());
                    category_ids.insert(f[1].to_vec(), id);
                    id
                }
            };
            product_cat.insert(keys.key(f[0]), cid);
        });
    }

    let mut channel_names: Vec<Vec<u8>> = vec![b"organic".to_vec()];
    let mut campaign_channel: Vec<u32> = vec![0];
    let mut campaign_ids: FxHashMap<u64, u32> = FxHashMap::default();
    let mut channel_ids: FxHashMap<Vec<u8>, u32> = FxHashMap::default();
    channel_ids.insert(b"organic".to_vec(), 0);
    {
        let m = mmap(&data.join("campaigns.csv"))?;
        records::<2>(&m, |f| {
            let ch = match channel_ids.get(f[1]) {
                Some(&id) => id,
                None => {
                    let id = channel_names.len() as u32;
                    channel_names.push(f[1].to_vec());
                    channel_ids.insert(f[1].to_vec(), id);
                    id
                }
            };
            let idx = campaign_channel.len() as u32;
            campaign_channel.push(ch);
            campaign_ids.insert(keys.key(f[0]), idx);
        });
    }
    let num_campaigns = campaign_channel.len() as u32;
    cp("dimensions");

    // ── Links → person map ─────────────────────────────────────────
    let mut people = People::new(&keys);
    for path in files(&data.join("links"))? {
        let m = mmap(&path)?;
        records::<2>(&m, |f| {
            if f[0].is_empty() || f[1].is_empty() { return; }
            let a = keys.key(f[0]);
            let b = keys.key(f[1]);
            let ia = people.intern(a);
            let ib = people.intern(b);
            people.union(ia, ib);
        });
    }
    // Resolve root → smallest (person name)
    let mut person_map: FxHashMap<u64, u64> = FxHashMap::default();
    for (id, &name) in people.names.iter().enumerate() {
        let r = people.root(id as u32);
        let canonical = people.smallest[r as usize];
        person_map.insert(name, canonical);
    }
    cp("links");

    // ── Events: parse → dedup → validate → map ─────────────────────
    let event_chunks: Vec<Vec<RawEvent>> = files(&data.join("events"))?
        .par_iter()
        .map(|path| {
            let m = mmap(path)?;
            let mut out = Vec::with_capacity(m.len() / 64);
            records::<7>(&m, |f| {
                // Parse (malformed lines are dropped by `fields`).
                let event_type = match f[4] {
                    b"view" => 0u8,
                    b"cart" => 1,
                    b"purchase" => 2,
                    _ => 3,
                };
                let price = if event_type == 2 {
                    parse_price(f[6]).unwrap_or(0)
                } else { 0 };
                out.push(RawEvent {
                    id: keys.key(f[0]),
                    ingest_ts: timestamp(f[3]).unwrap_or(i64::MIN),
                    user_id: keys.key(f[1]),
                    ts: timestamp(f[2]).unwrap_or(i64::MIN),
                    event_type,
                    product_id: keys.key(f[5]),
                    price_cents: price,
                });
            });
            Ok(out)
        })
        .collect::<io::Result<Vec<_>>>()?;

    let mut raw: Vec<RawEvent> = event_chunks.into_iter().flatten().collect();

    // Dedup on event_id keeping smallest ingest_ts; drop empty event_id.
    raw.retain(|e| e.id != 0 && e.ingest_ts != i64::MIN);
    raw.radix_sort_unstable_by(|e| {
        // Pack (id, ingest_ts) into u128: id high, ingest_ts low.
        ((e.id as u128) << 64) | (e.ingest_ts as u64 as u128)
    });
    let mut dedup: Vec<RawEvent> = Vec::with_capacity(raw.len());
    for ev in raw {
        if dedup.last().map_or(true, |p: &RawEvent| p.id != ev.id) {
            dedup.push(ev);
        }
    }

    // Validate + late filter + map to person.
    let mut events: Vec<Event> = Vec::with_capacity(dedup.len());
    for ev in dedup {
        if ev.user_id == 0 { continue; }
        if ev.ts == i64::MIN || ev.ts < start || ev.ts >= end { continue; }
        if ev.event_type > 2 { continue; }
        if ev.product_id == 0 { continue; }
        if ev.event_type == 2 && ev.price_cents == 0 { continue; }
        if ev.ingest_ts - ev.ts > DAY { continue; }

        let person = person_map.get(&ev.user_id).copied().unwrap_or(ev.user_id);
        let category = if ev.event_type == 2 {
            product_cat.get(&ev.product_id).copied().unwrap_or(0)
        } else { 0 };

        events.push(Event {
            person,
            ts: ev.ts,
            event_type: ev.event_type,
            category,
            price: ev.price_cents,
        });
    }

    events.radix_sort_unstable_by(|e| {
        ((e.person as u128) << 64) | (e.ts as u64 as u128)
    });
    cp("events");

    // ── Clicks: parse → dedup → validate → map ─────────────────────
    let click_chunks: Vec<Vec<RawClick>> = files(&data.join("clicks"))?
        .par_iter()
        .map(|path| {
            let m = mmap(path)?;
            let mut out = Vec::with_capacity(m.len() / 48);
            records::<4>(&m, |f| {
                let campaign = campaign_ids.get(&keys.key(f[2])).copied().unwrap_or(u32::MAX);
                out.push(RawClick {
                    id: keys.key(f[0]),
                    user_id: keys.key(f[1]),
                    campaign,
                    ts: timestamp(f[3]).unwrap_or(i64::MIN),
                });
            });
            Ok(out)
        })
        .collect::<io::Result<Vec<_>>>()?;

    let mut raw: Vec<RawClick> = click_chunks.into_iter().flatten().collect();

    // Dedup on click_id (copies are identical).
    raw.retain(|c| c.id != 0);
    raw.radix_sort_unstable_by(|c| c.id);
    raw.dedup_by(|a, b| a.id == b.id);

    // Validate + map to person.
    let click_lo = start - WEEK;
    let mut clicks: Vec<Click> = Vec::with_capacity(raw.len());
    for c in raw {
        if c.user_id == 0 { continue; }
        if c.ts == i64::MIN || c.ts < click_lo || c.ts >= end { continue; }
        if c.campaign == u32::MAX { continue; }

        let person = person_map.get(&c.user_id).copied().unwrap_or(c.user_id);
        clicks.push(Click { person, ts: c.ts, campaign: c.campaign, id: c.id });
    }

    // Sort by (person, ts, click_id) so the credit tie-break is trivial.
    clicks.radix_sort_unstable_by(|c| {
        // person is high, ts middle, click_id low. Use two passes.
        ((c.person as u128) << 64) | (c.ts as u64 as u128)
    });
    // Within each (person, ts) group, sort click_id ascending.
    let mut i = 0;
    while i < clicks.len() {
        let (p, t) = (clicks[i].person, clicks[i].ts);
        let mut j = i;
        while j < clicks.len() && clicks[j].person == p && clicks[j].ts == t { j += 1; }
        clicks[i..j].sort_unstable_by_key(|c| c.id);
        i = j;
    }
    cp("clicks");

    // ── Sessionize + attribute ─────────────────────────────────────
    let num_categories = category_names.len() as u32;
    let mut user_rows: Vec<UserRow> = Vec::new();
    let mut campaign_totals: FxHashMap<(u32, i64), DayTotals> = FxHashMap::default();

    let mut ei = 0usize;
    let mut ci = 0usize;
    let mut cat_sums: Vec<i64> = vec![0; num_categories as usize];
    let mut cats_touched: Vec<u32> = Vec::new();

    while ei < events.len() {
        let person = events[ei].person;

        // Slice clicks for this person.
        while ci < clicks.len() && clicks[ci].person < person { ci += 1; }
        let click_start = ci;
        while ci < clicks.len() && clicks[ci].person == person { ci += 1; }
        let person_clicks = &clicks[click_start..ci];

        let mut lo = 0usize;
        let mut hi = 0usize;

        // Aggregate per (person, day) in place.
        let mut day = i64::MIN;
        let mut agg_sessions = 0i64;
        let mut agg_events = 0i64;
        let mut agg_revenue = 0i64;

        while ei < events.len() && events[ei].person == person {
            // Start a session at events[ei].ts.
            let session_start = events[ei].ts;
            let session_day = session_start.div_euclid(DAY);

            let mut session_events = 0i64;
            let mut session_revenue = 0i64;
            let mut has_purchase = false;
            let mut prev_ts = session_start;

            // Consume all events of this session.
            while ei < events.len()
                && events[ei].person == person
                && events[ei].ts - prev_ts <= SESSION_GAP
            {
                let ev = events[ei];
                prev_ts = ev.ts;
                session_events += 1;
                if ev.event_type == 2 {
                    has_purchase = true;
                    session_revenue += i64::from(ev.price);
                    if cat_sums[ev.category as usize] == 0 {
                        cats_touched.push(ev.category);
                    }
                    cat_sums[ev.category as usize] += i64::from(ev.price);
                }
                ei += 1;
            }

            // Click window [session_start - WEEK, session_start].
            let wlo = session_start - WEEK;
            while lo < person_clicks.len() && person_clicks[lo].ts < wlo { lo += 1; }
            while hi < person_clicks.len() && person_clicks[hi].ts <= session_start { hi += 1; }

            // Credit: max ts, then min click_id. Clicks in [lo, hi) are sorted
            // by (ts asc, id asc); walk back to the first click of the max-ts group.
            let (credit, assists_start) = if hi > lo {
                let max_ts = person_clicks[hi - 1].ts;
                let mut k = hi - 1;
                while k > lo && person_clicks[k - 1].ts == max_ts { k -= 1; }
                (person_clicks[k].campaign, lo)
            } else {
                (0u32, hi) // organic
            };

            // Assists: every distinct campaign in [lo, hi), excluding organic (index 0).
            // A campaign assists once per session even with multiple clicks.
            let mut assists: Vec<u32> = Vec::with_capacity(8);
            for c in &person_clicks[assists_start..hi] {
                if c.campaign != 0 && !assists.contains(&c.campaign) {
                    assists.push(c.campaign);
                }
            }

            // Roll into (person, day) if the day changed.
            if day != session_day {
                if day != i64::MIN { flush_user(day, agg_sessions, agg_events, agg_revenue,
                                                  &mut cat_sums, &mut cats_touched,
                                                  &category_names, person, &mut user_rows); }
                day = session_day;
                agg_sessions = 0;
                agg_events = 0;
                agg_revenue = 0;
            }
            agg_sessions += 1;
            agg_events += session_events;
            agg_revenue += session_revenue;

            // Campaign credit.
            let ct = campaign_totals.entry((credit, session_day)).or_default();
            ct.sessions += 1;
            if has_purchase { ct.purchases += 1; }
            ct.revenue += session_revenue;

            // Campaign assists.
            for &campaign in &assists {
                campaign_totals.entry((campaign, session_day)).or_default().assisted += 1;
            }
        }

        if day != i64::MIN {
            flush_user(day, agg_sessions, agg_events, agg_revenue,
                       &mut cat_sums, &mut cats_touched,
                       &category_names, person, &mut user_rows);
        }
    }
    cp("sessions");

    // ── Write user_daily ────────────────────────────────────────────
    let mut report = Report::new(out, "user_daily", &[
        ("user_id", "text"),
        ("day", "day"),
        ("sessions", "count"),
        ("events", "count"),
        ("revenue_cents", "count"),
        ("top_category", "optional"),
    ])?;
    for row in &user_rows {
        report.text(0, &row.name);
        report.day(1, row.day);
        report.count(2, row.sessions);
        report.count(3, row.events);
        report.count(4, row.revenue);
        report.optional(5, row.top_category.as_deref());
        report.row()?;
    }
    report.finish()?;

    // ── Write campaign_daily ────────────────────────────────────────
    let mut report = Report::new(out, "campaign_daily", &[
        ("campaign_id", "text"),
        ("channel", "text"),
        ("day", "day"),
        ("sessions", "count"),
        ("purchasing_sessions", "count"),
        ("revenue_cents", "count"),
        ("assisted_sessions", "count"),
    ])?;
    // Need campaign names — store names during dimension parse instead of indices.
    // (See note below; store names in a side vector keyed by campaign index.)
    let mut rows: Vec<(u32, i64, DayTotals)> =
        campaign_totals.into_iter().map(|((c, d), t)| (c, d, t)).collect();
    rows.sort_unstable_by_key(|(c, d, _)| (*c, *d));
    for (c, d, t) in rows {
        report.text(0, campaign_name(c, &keys, data)?);
        report.text(1, &channel_names[campaign_channel[c as usize] as usize]);
        report.day(2, d);
        report.count(3, t.sessions);
        report.count(4, t.purchases);
        report.count(5, t.revenue);
        report.count(6, t.assisted);
        report.row()?;
    }
    report.finish()?;

    eprintln!("total={:.3}", began.elapsed().as_secs_f64());
    Ok(())
}

// ── Aggregation helpers ────────────────────────────────────────────
struct UserRow {
    name: Vec<u8>,
    day: i64,
    sessions: i64,
    events: i64,
    revenue: i64,
    top_category: Option<Vec<u8>>,
}

fn flush_user(
    day: i64, sessions: i64, events: i64, revenue: i64,
    cat_sums: &mut [i64], cats_touched: &mut Vec<u32>,
    category_names: &[Vec<u8>], person: u64,
    rows: &mut Vec<UserRow>,
) {
    let mut best_cat: Option<(u32, i64)> = None;
    for &c in cats_touched.iter() {
        let rev = cat_sums[c as usize];
        best_cat = Some(match best_cat {
            None => (c, rev),
            Some((bc, br)) => {
                if rev > br || (rev == br && category_names[c as usize] < category_names[bc as usize]) {
                    (c, rev)
                } else { (bc, br) }
            }
        });
    }
    for &c in cats_touched.iter() { cat_sums[c as usize] = 0; }
    cats_touched.clear();

    // Person name — the keys.bytes round-trip is fine here (one per person-day row).
    // Since we don't have access to Keys in this helper, pass it in or pre-encode.
    // See note: emit_person_name below.
    let name = person_name_for_row(person);
    rows.push(UserRow {
        name,
        day,
        sessions,
        events,
        revenue,
        top_category: best_cat.map(|(c, _)| category_names[c as usize].clone()),
    });
}

// ── Name resolution (see note) ─────────────────────────────────────
fn person_name_for_row(_k: u64) -> Vec<u8> { unimplemented!("see note") }
fn campaign_name(_c: u32, _keys: &Keys, _data: &Path) -> io::Result<Vec<u8>> { unimplemented!("see note") }
