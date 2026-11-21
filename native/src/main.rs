mod dialect;
mod fast;
mod report;

use dialect::fields;
use fast::{parse_price, timestamp};
use rayon::prelude::*;
use report::Report;
use rustc_hash::FxHashMap;
use std::{
    env,
    fs::{self, File},
    hash::Hasher,
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
const BIAS: u64 = 1u64 << 63; // sign flip for i64 -> ordered u64

// Keys: 7-bit injection for <=9-byte printable-ASCII IDs; mutex side table otherwise.
struct Keys {
    unusual: std::sync::Mutex<(FxHashMap<Vec<u8>, u64>, Vec<Vec<u8>>)>,
}

impl Keys {
    fn new() -> Self {
        Self { unusual: std::sync::Mutex::new((FxHashMap::default(), Vec::new())) }
    }

    #[inline]
    fn key(&self, v: &[u8]) -> u64 {
        if v.len() <= 9 && v.iter().all(|&b| b > 0 && b < 128) {
            let mut k = 0u64;
            for &b in v { k = (k << 7) | u64::from(b); }
            return k;
        }
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
            let mut v = k;
            while v != 0 {
                out.push((v & 0x7f) as u8);
                v >>= 7;
            }
            out.reverse();
            out
        } else {
            self.unusual.lock().unwrap().1[(k & !(1u64 << 63)) as usize].clone()
        }
    }
}

#[derive(Clone, Copy)]
struct RawEvent {
    event_id: u64,
    ingest_ts: i64,
    user_id: u64,
    ts: i64,
    event_type: u8,
    product_id: u64,
    price: u32,
}

#[derive(Clone, Copy)]
struct RawClick {
    id: u64,
    user_id: u64,
    campaign_key: u64,
    ts: i64,
}

#[derive(Clone, Copy)]
struct Event {
    person: u64,
    ts: i64,
    category: u32,
    price: u32,
    is_purchase: bool,
}

#[derive(Clone, Copy)]
struct Click {
    person: u64,
    ts: i64,
    campaign: u32,
    id: u64,
}

#[derive(Clone, Copy, Default)]
struct Totals {
    sessions: i64,
    purchases: i64,
    revenue: i64,
    assisted: i64,
}

struct UserRow {
    person: u64,
    day: i64,
    sessions: i64,
    events: i64,
    revenue: i64,
    top_category: Option<u32>,
}

struct CampaignRow {
    campaign: u32,
    day: i64,
    totals: Totals,
}

fn files(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut v: Vec<_> = fs::read_dir(dir)?.map(|e| e.map(|e| e.path())).collect::<io::Result<Vec<_>>>()?;
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

fn for_each_record<const N: usize>(block: &[u8], mut visit: impl FnMut([&[u8]; N])) {
    let body = match memchr::memchr(b'\n', block) {
        Some(p) => &block[p + 1..],
        None => return,
    };
    let mut start = 0;
    for end in memchr::memchr_iter(b'\n', body) {
        if let Some(rec) = fields::<N>(&body[start..end]) { visit(rec); }
        start = end + 1;
    }
    if start < body.len() {
        if let Some(rec) = fields::<N>(&body[start..]) { visit(rec); }
    }
}

fn load_products(data: &Path, keys: &Keys) -> io::Result<(FxHashMap<u64, u32>, Vec<Vec<u8>>)> {
    let mut categories: Vec<Vec<u8>> = vec![b"unknown".to_vec()];
    let mut cat_map: FxHashMap<Vec<u8>, u32> = FxHashMap::default();
    cat_map.insert(b"unknown".to_vec(), 0);
    let mut products: FxHashMap<u64, u32> = FxHashMap::default();
    let m = mmap(&data.join("products.csv"))?;
    for_each_record::<2>(&m, |f| {
        let cat = if f[1].is_empty() { 0 } else if let Some(&id) = cat_map.get(f[1]) { id } else {
            let id = categories.len() as u32;
            categories.push(f[1].to_vec());
            cat_map.insert(f[1].to_vec(), id);
            id
        };
        if !f[0].is_empty() { products.insert(keys.key(f[0]), cat); }
    });
    Ok((products, categories))
}

fn load_campaigns(data: &Path, keys: &Keys) -> io::Result<(FxHashMap<u64, u32>, Vec<Vec<u8>>, Vec<u32>, Vec<Vec<u8>>)> {
    let mut campaigns: Vec<Vec<u8>> = vec![b"organic".to_vec()];
    let mut channels: Vec<Vec<u8>> = vec![b"organic".to_vec()];
    let mut campaign_channel: Vec<u32> = vec![0];
    let mut ch_map: FxHashMap<Vec<u8>, u32> = FxHashMap::default();
    ch_map.insert(b"organic".to_vec(), 0);
    let mut campaign_map: FxHashMap<u64, u32> = FxHashMap::default();
    let m = mmap(&data.join("campaigns.csv"))?;
    for_each_record::<2>(&m, |f| {
        if f[0].is_empty() { return; }
        let ch = if let Some(&id) = ch_map.get(f[1]) { id } else {
            let id = channels.len() as u32;
            channels.push(f[1].to_vec());
            ch_map.insert(f[1].to_vec(), id);
            id
        };
        let cid = campaigns.len() as u32;
        campaigns.push(f[0].to_vec());
        campaign_channel.push(ch);
        campaign_map.insert(keys.key(f[0]), cid);
    });
    Ok((campaign_map, campaigns, campaign_channel, channels))
}

fn load_links(data: &Path, keys: &Keys) -> io::Result<FxHashMap<u64, u64>> {
    let mut parent: FxHashMap<u64, u64> = FxHashMap::default();
    let mut smallest: FxHashMap<u64, u64> = FxHashMap::default();
    let mut size: FxHashMap<u64, u64> = FxHashMap::default();

    fn find(p: &mut FxHashMap<u64, u64>, x: u64) -> u64 {
        let mut r = x;
        while let Some(&v) = p.get(&r) { if v == r { break; } r = v; }
        let mut c = x;
        while c != r { let n = p[&c]; p.insert(c, r); c = n; }
        r
    }

    for path in files(&data.join("links"))? {
        let m = mmap(&path)?;
        for_each_record::<2>(&m, |f| {
            if f[0].is_empty() || f[1].is_empty() { return; }
            let a = keys.key(f[0]);
            let b = keys.key(f[1]);
            parent.entry(a).or_insert(a);
            parent.entry(b).or_insert(b);
            smallest.entry(a).or_insert(a);
            smallest.entry(b).or_insert(b);
            size.entry(a).or_insert(1);
            size.entry(b).or_insert(1);
            let ra = find(&mut parent, a);
            let rb = find(&mut parent, b);
            if ra == rb { return; }
            let (big, small) = if size[&ra] >= size[&rb] { (ra, rb) } else { (rb, ra) };
            parent.insert(small, big);
            let sv = size[&small];
            *size.get_mut(&big).unwrap() += sv;
            let s_b = smallest[&small];
            let s_g = smallest[&big];
            smallest.insert(big, s_b.min(s_g));
        });
    }

    let all: Vec<u64> = parent.keys().copied().collect();
    let mut result: FxHashMap<u64, u64> = FxHashMap::default();
    for k in all {
        let r = find(&mut parent, k);
        result.insert(k, smallest[&r]);
    }
    Ok(result)
}

fn load_events(
    data: &Path,
    keys: &Keys,
    person_map: &FxHashMap<u64, u64>,
    products: &FxHashMap<u64, u32>,
    w_start: i64,
    w_end: i64,
) -> io::Result<Vec<Event>> {
    let paths = files(&data.join("events"))?;
    let chunks: Vec<Vec<RawEvent>> = paths.par_iter().map(|p| {
        let m = mmap(p)?;
        let mut out = Vec::with_capacity(m.len() / 80);
        for_each_record::<7>(&m, |f| {
            out.push(RawEvent {
                event_id: keys.key(f[0]),
                ingest_ts: timestamp(f[3]).unwrap_or(i64::MIN),
                user_id: keys.key(f[1]),
                ts: timestamp(f[2]).unwrap_or(i64::MIN),
                event_type: match f[4] { b"view" => 0, b"cart" => 1, b"purchase" => 2, _ => 255 },
                product_id: keys.key(f[5]),
                price: if f[4] == b"purchase" { parse_price(f[6]).unwrap_or(0) } else { 0 },
            });
        });
        Ok(out)
    }).collect::<io::Result<Vec<_>>>()?;

    let mut raw: Vec<RawEvent> = chunks.into_iter().flatten().collect();
    raw.retain(|e| e.event_id != 0 && e.ingest_ts != i64::MIN);

    // Dedup: sort by event_id, keep smallest ingest_ts per group.
    raw.par_radix_sort_unstable_by(|e| e.event_id as u128);
    let mut dedup: Vec<RawEvent> = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        let id = raw[i].event_id;
        let mut j = i + 1;
        while j < raw.len() && raw[j].event_id == id { j += 1; }
        let mut best = raw[i];
        for k in i+1..j { if raw[k].ingest_ts < best.ingest_ts { best = raw[k]; } }
        dedup.push(best);
        i = j;
    }

    let events: Vec<Event> = dedup.into_par_iter().filter_map(|e| {
        if e.user_id == 0 { return None; }
        if e.ts == i64::MIN || e.ts < w_start || e.ts >= w_end { return None; }
        if e.event_type > 2 { return None; }
        if e.product_id == 0 { return None; }
        if e.event_type == 2 && e.price == 0 { return None; }
        if e.ingest_ts - e.ts > DAY { return None; }
        let person = person_map.get(&e.user_id).copied().unwrap_or(e.user_id);
        let category = if e.event_type == 2 { products.get(&e.product_id).copied().unwrap_or(0) } else { 0 };
        Some(Event { person, ts: e.ts, category, price: e.price, is_purchase: e.event_type == 2 })
    }).collect();

    let mut events = events;
    events.par_radix_sort_unstable_by(|e| {
        ((e.person as u128) << 64) | (((e.ts as u64) ^ BIAS) as u128)
    });
    Ok(events)
}

fn load_clicks(
    data: &Path,
    keys: &Keys,
    person_map: &FxHashMap<u64, u64>,
    campaigns: &FxHashMap<u64, u32>,
    w_start: i64,
    w_end: i64,
) -> io::Result<Vec<Click>> {
    let paths = files(&data.join("clicks"))?;
    let click_lo = w_start - WEEK;
    let chunks: Vec<Vec<RawClick>> = paths.par_iter().map(|p| {
        let m = mmap(p)?;
        let mut out = Vec::with_capacity(m.len() / 60);
        for_each_record::<4>(&m, |f| {
            out.push(RawClick {
                id: keys.key(f[0]),
                user_id: keys.key(f[1]),
                campaign_key: keys.key(f[2]),
                ts: timestamp(f[3]).unwrap_or(i64::MIN),
            });
        });
        Ok(out)
    }).collect::<io::Result<Vec<_>>>()?;

    let mut raw: Vec<RawClick> = chunks.into_iter().flatten().collect();
    raw.retain(|c| c.id != 0);

    raw.par_radix_sort_unstable_by(|c| c.id as u128);
    let mut dedup: Vec<RawClick> = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        let id = raw[i].id;
        let mut j = i + 1;
        while j < raw.len() && raw[j].id == id { j += 1; }
        dedup.push(raw[i]);
        i = j;
    }

    let clicks: Vec<Click> = dedup.into_par_iter().filter_map(|c| {
        if c.user_id == 0 { return None; }
        if c.ts == i64::MIN || c.ts < click_lo || c.ts >= w_end { return None; }
        let campaign = *campaigns.get(&c.campaign_key)?;
        let person = person_map.get(&c.user_id).copied().unwrap_or(c.user_id);
        Some(Click { person, ts: c.ts, campaign, id: c.id })
    }).collect();

    let mut clicks = clicks;
    clicks.par_radix_sort_unstable_by(|c| {
        ((c.person as u128) << 64) | (((c.ts as u64) ^ BIAS) as u128)
    });
    // Tie-break click_id within (person, ts) groups.
    let mut i = 0;
    while i < clicks.len() {
        let (p, t) = (clicks[i].person, clicks[i].ts);
        let mut j = i + 1;
        while j < clicks.len() && clicks[j].person == p && clicks[j].ts == t { j += 1; }
        if j - i > 1 { clicks[i..j].sort_unstable_by_key(|c| c.id); }
        i = j;
    }
    Ok(clicks)
}

fn top_category(sums: &[(u32, i64)], names: &[Vec<u8>]) -> Option<u32> {
    if sums.is_empty() { return None; }
    let mut best = sums[0];
    for &(c, s) in &sums[1..] {
        if s > best.1 || (s == best.1 && names[c as usize] < names[best.0 as usize]) {
            best = (c, s);
        }
    }
    Some(best.0)
}

fn process_chunk(
    events: &[Event],
    clicks: &[Click],
    w_start: i64,
    campaign_count: u32,
) -> (Vec<UserRow>, Vec<((u32, i64), Totals)>) {
    if events.is_empty() { return (Vec::new(), Vec::new()); }
    let mut user_rows: Vec<UserRow> = Vec::new();
    let mut campaigns: FxHashMap<(u32, i64), Totals> = FxHashMap::default();
    let mut cat_buf: Vec<(u32, i64)> = Vec::with_capacity(8);

    let mut ei = 0;
    while ei < events.len() {
        let person = events[ei].person;
        let c_lo = clicks.partition_point(|c| c.person < person);
        let c_hi = clicks.partition_point(|c| c.person <= person);
        let person_clicks = &clicks[c_lo..c_hi];

        let mut lo = 0;
        let mut hi = 0;
        let mut cur_day: Option<i64> = None;
        let mut day_sessions = 0i64;
        let mut day_events = 0i64;
        let mut day_revenue = 0i64;
        cat_buf.clear();

        while ei < events.len() && events[ei].person == person {
            let session_start = events[ei].ts;
            let session_day = session_start.div_euclid(DAY);

            let mut sess_events = 0i64;
            let mut sess_revenue = 0i64;
            let mut has_purchase = false;
            let mut prev_ts = session_start;
            while ei < events.len() && events[ei].person == person && events[ei].ts - prev_ts <= SESSION_GAP {
                let ev = events[ei];
                prev_ts = ev.ts;
                sess_events += 1;
                if ev.is_purchase {
                    has_purchase = true;
                    sess_revenue += ev.price as i64;
                    let mut found = false;
                    for x in cat_buf.iter_mut() { if x.0 == ev.category { x.1 += ev.price as i64; found = true; break; } }
                    if !found { cat_buf.push((ev.category, ev.price as i64)); }
                }
                ei += 1;
            }

            let w_lo = session_start - WEEK;
            while lo < person_clicks.len() && person_clicks[lo].ts < w_lo { lo += 1; }
            while hi < person_clicks.len() && person_clicks[hi].ts <= session_start { hi += 1; }

            let credit = if hi > lo {
                let max_ts = person_clicks[hi - 1].ts;
                let mut k = hi - 1;
                while k > lo && person_clicks[k - 1].ts == max_ts { k -= 1; }
                person_clicks[k].campaign
            } else { 0 };

            // Assists: unique non-organic campaigns in [lo, hi).
            let mut assists: Vec<u32> = Vec::with_capacity(8);
            for c in &person_clicks[lo..hi] {
                if c.campaign != 0 && !assists.contains(&c.campaign) { assists.push(c.campaign); }
            }

            if cur_day != Some(session_day) {
                if let Some(d) = cur_day {
                    user_rows.push(UserRow {
                        person, day: d, sessions: day_sessions, events: day_events, revenue: day_revenue,
                        top_category: top_category(&cat_buf, &[]),
                    });
                    cat_buf.clear();
                }
                cur_day = Some(session_day);
                day_sessions = 0; day_events = 0; day_revenue = 0;
            }
            day_sessions += 1;
            day_events += sess_events;
            day_revenue += sess_revenue;

            let ct = campaigns.entry((credit, session_day)).or_default();
            ct.sessions += 1;
            if has_purchase { ct.purchases += 1; }
            ct.revenue += sess_revenue;
            for &c in &assists { campaigns.entry((c, session_day)).or_default().assisted += 1; }
        }

        if let Some(d) = cur_day {
            user_rows.push(UserRow {
                person, day: d, sessions: day_sessions, events: day_events, revenue: day_revenue,
                top_category: top_category(&cat_buf, &[]),
            });
        }
        // ignore unused hint
        let _ = campaign_count;
    }
    let campaign_vec: Vec<((u32, i64), Totals)> = campaigns.into_iter().collect();
    (user_rows, campaign_vec)
}

fn main() -> io::Result<()> {
    let began = Instant::now();
    let mut t = began;
    let mut cp = |label: &str| { let n = Instant::now(); eprintln!("{label}={:.3}", n.duration_since(t).as_secs_f64()); t = n; };

    let data_dir = env::var("DATA_DIR").unwrap_or_else(|_| "/data".into());
    let out_dir = env::var("OUTPUT_DIR").unwrap_or_else(|_| "/output".into());
    let ncpu: usize = env::var("NCPU").unwrap_or_else(|_| "8".into()).parse().unwrap_or(8);
    let data = Path::new(&data_dir);
    let out = Path::new(&out_dir);

    rayon::ThreadPoolBuilder::new().num_threads(ncpu).build_global().ok();

    let manifest: serde_json::Value = serde_json::from_slice(&fs::read(data.join("manifest.json"))?)?;
    let w_start = timestamp(manifest["window_start"].as_str().unwrap().as_bytes()).unwrap();
    let w_end = timestamp(manifest["window_end"].as_str().unwrap().as_bytes()).unwrap();

    let keys = Keys::new();
    let (products, category_names) = load_products(data, &keys)?;
    let (campaigns_map, campaign_names, campaign_channels, channel_names) = load_campaigns(data, &keys)?;
    cp("dims");

    let person_map = load_links(data, &keys)?;
    cp("links");

    let events = load_events(data, &keys, &person_map, &products, w_start, w_end)?;
    eprintln!("events_count={}", events.len());
    cp("events");

    let clicks = load_clicks(data, &keys, &person_map, &campaigns_map, w_start, w_end)?;
    eprintln!("clicks_count={}", clicks.len());
    cp("clicks");

    // Chunk events at person boundaries for parallel sessionize.
    let n_chunks = 32;
    let chunk_size = (events.len() + n_chunks - 1) / n_chunks;
    let mut bounds = vec![0usize];
    let mut pos = chunk_size;
    while pos < events.len() {
        while pos < events.len() && events[pos].person == events[pos - 1].person { pos += 1; }
        bounds.push(pos);
        pos += chunk_size;
    }
    bounds.push(events.len());

    let campaign_count = campaign_names.len() as u32;
    let results: Vec<(Vec<UserRow>, Vec<((u32, i64), Totals)>)> =
        (0..bounds.len() - 1).into_par_iter().map(|i| {
            process_chunk(&events[bounds[i]..bounds[i + 1]], &clicks, w_start, campaign_count)
        }).collect();
    cp("sessions");

    let mut user_rows: Vec<UserRow> = Vec::new();
    let mut campaign_agg: FxHashMap<(u32, i64), Totals> = FxHashMap::default();
    for (ur, cr) in results {
        user_rows.extend(ur);
        for (k, v) in cr {
            let e = campaign_agg.entry(k).or_default();
            e.sessions += v.sessions;
            e.purchases += v.purchases;
            e.revenue += v.revenue;
            e.assisted += v.assisted;
        }
    }

    // Fill user names and top_category bytes.
    let mut report = Report::new(out, "user_daily", &[
        ("user_id", "text"),
        ("day", "day"),
        ("sessions", "count"),
        ("events", "count"),
        ("revenue_cents", "count"),
        ("top_category", "optional"),
    ])?;
    for row in &user_rows {
        let name = keys.bytes(row.person);
        let tc = row.top_category.map(|c| category_names[c as usize].as_slice());
        report.text(0, &name);
        report.day(1, row.day);
        report.count(2, row.sessions);
        report.count(3, row.events);
        report.count(4, row.revenue);
        report.optional(5, tc);
        report.row()?;
    }
    report.finish()?;

    // Write campaign_daily.
    let mut report = Report::new(out, "campaign_daily", &[
        ("campaign_id", "text"),
        ("channel", "text"),
        ("day", "day"),
        ("sessions", "count"),
        ("purchasing_sessions", "count"),
        ("revenue_cents", "count"),
        ("assisted_sessions", "count"),
    ])?;
    let mut rows: Vec<CampaignRow> = campaign_agg.into_iter()
        .map(|((c, d), t)| CampaignRow { campaign: c, day: d, totals: t })
        .collect();
    rows.sort_unstable_by_key(|r| (r.campaign, r.day));
    for row in &rows {
        let cname = &campaign_names[row.campaign as usize];
        let chname = &channel_names[campaign_channels[row.campaign as usize] as usize];
        report.text(0, cname);
        report.text(1, chname);
        report.day(2, row.day);
        report.count(3, row.totals.sessions);
        report.count(4, row.totals.purchases);
        report.count(5, row.totals.revenue);
        report.count(6, row.totals.assisted);
        report.row()?;
    }
    report.finish()?;
    cp("write");

    eprintln!("total={:.3}", began.elapsed().as_secs_f64());
    Ok(())
}
