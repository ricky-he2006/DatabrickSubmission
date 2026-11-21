#!/usr/bin/env python3
"""Slow, literal reference for Shopping Sessions. Usage: reference.py DATA_DIR OUT_DIR
Writes OUT_DIR/user_daily.csv and OUT_DIR/campaign_daily.csv (the dev-answer layout)."""
import calendar
import datetime
import glob
import json
import os
import re
import sys
from collections import defaultdict

DAY = 86_400_000
WEEK = 7 * DAY
GAP = 1_800_000

F1 = re.compile(rb"^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2}):(\d{2})Z$", re.A)
F2 = re.compile(rb"^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2}):(\d{2})([+-])(\d{2}):(\d{2})$", re.A)
F3 = re.compile(rb"^[0-9]+$")
DIGITS = re.compile(rb"^[0-9]+$")


def civil(y, mo, d, h, mi, s):
    if not (1 <= mo <= 12 and 1 <= d <= calendar.monthrange(y, mo)[1] if y >= 1 else False):
        return None
    if h > 23 or mi > 59 or s > 59:
        return None
    return (datetime.date(y, mo, d).toordinal() - 719163) * 86400 + h * 3600 + mi * 60 + s


def ts(v):
    if v is None:
        return None
    m = F1.match(v)
    if m and b"\n" not in v:
        sec = civil(*map(int, m.groups()))
        return None if sec is None else sec * 1000
    m = F2.match(v)
    if m and b"\n" not in v:
        g = m.groups()
        sec = civil(*map(int, g[:6]))
        oh, om = int(g[7]), int(g[8])
        if sec is None or oh > 23 or om > 59:
            return None
        off = oh * 3600 + om * 60
        return (sec - off if g[6] == b"+" else sec + off) * 1000
    if F3.match(v) and b"\n" not in v:
        return int(v)
    return None


def rows(folder, n):
    for path in sorted(glob.glob(os.path.join(folder, "*.csv"))):
        with open(path, "rb") as f:
            data = f.read()
        lines = data.split(b"\n")
        if lines and lines[-1] == b"":
            lines.pop()
        for line in lines[1:]:
            parts = line.split(b",")
            if line == b"" or len(parts) != n:
                continue
            out = []
            for p in parts:
                if len(p) >= 2 and p[:1] == b'"' and p[-1:] == b'"':
                    p = p[1:-1]
                if b'"' in p:
                    out = None
                    break
                out.append(p if p != b"" else None)
            if out is not None:
                yield out


def main():
    data, out = sys.argv[1], sys.argv[2]
    man = json.load(open(os.path.join(data, "manifest.json")))
    w0, w1 = ts(man["window_start"].encode()), ts(man["window_end"].encode())

    products = {}
    for line in open(os.path.join(data, "products.csv"), "rb").read().split(b"\n")[1:]:
        if line:
            a, b = line.split(b",")
            products[a] = b
    campaigns = {}
    for line in open(os.path.join(data, "campaigns.csv"), "rb").read().split(b"\n")[1:]:
        if line:
            a, b = line.split(b",")
            campaigns[a] = b

    parent = {}

    def find(x):
        root = x
        while parent[root] != root:
            root = parent[root]
        while parent[x] != root:
            parent[x], x = root, parent[x]
        return root

    for a, b in rows(os.path.join(data, "links"), 2):
        if a is None or b is None:
            continue
        parent.setdefault(a, a)
        parent.setdefault(b, b)
        ra, rb = find(a), find(b)
        if ra != rb:
            if ra < rb:
                parent[rb] = ra
            else:
                parent[ra] = rb

    def person(u):
        return find(u) if u in parent else u

    best = {}
    for r in rows(os.path.join(data, "events"), 7):
        if r[0] is None:
            continue
        ing = ts(r[3])
        cur = best.get(r[0])
        if cur is None or ing < cur[0]:
            best[r[0]] = (ing, r)
    by_person = defaultdict(list)
    for ing, r in best.values():
        _, user, t, _, kind, product, price = r
        t = ts(t)
        if user is None or t is None or not (w0 <= t < w1) or kind not in (b"view", b"cart", b"purchase") or product is None:
            continue
        cents = 0
        if kind == b"purchase":
            if price is None or not DIGITS.match(price) or b"\n" in price or not (1 <= int(price) <= 10_000_000):
                continue
            cents = int(price)
        if ing - t > DAY:
            continue
        by_person[person(user)].append((t, cents, products.get(product, b"unknown") if kind == b"purchase" else None))
    del best

    clicks = {}
    for r in rows(os.path.join(data, "clicks"), 4):
        if r[0] is None:
            continue
        clicks.setdefault(r[0], r)
    by_person_clicks = defaultdict(list)
    for cid, user, campaign, t in clicks.values():
        t = ts(t)
        if user is None or t is None or not (w0 - WEEK <= t < w1) or campaign not in campaigns:
            continue
        by_person_clicks[person(user)].append((t, cid, campaign))

    user_rows = []
    camp = defaultdict(lambda: [0, 0, 0, 0])
    for who, events in by_person.items():
        events.sort(key=lambda e: e[0])
        cl = by_person_clicks.get(who, [])
        sessions = []
        for e in events:
            if sessions and e[0] - sessions[-1][-1][0] <= GAP:
                sessions[-1].append(e)
            else:
                sessions.append([e])
        days = defaultdict(lambda: [0, 0, 0, defaultdict(int)])
        for s in sessions:
            start = s[0][0]
            day = start // DAY
            d = days[day]
            d[0] += 1
            d[1] += len(s)
            revenue = 0
            bought = False
            for _, cents, category in s:
                if category is not None:
                    bought = True
                    revenue += cents
                    d[3][category] += cents
            d[2] += revenue
            reach = [c for c in cl if c[0] <= start and start - c[0] <= WEEK]
            if reach:
                latest = max(c[0] for c in reach)
                credit = min(c for c in reach if c[0] == latest)[2]
            else:
                credit = b"organic"
            t = camp[(credit, day)]
            t[0] += 1
            t[1] += bought
            t[2] += revenue
            for c in {c[2] for c in reach}:
                camp[(c, day)][3] += 1
        for day, (n, ev, revenue, cats) in days.items():
            top = min(cats.items(), key=lambda kv: (-kv[1], kv[0]))[0].decode() if cats else ""
            user_rows.append((who, day, n, ev, revenue, top))

    def date(day):
        return datetime.date.fromordinal(day + 719163).isoformat()

    def q(v):
        v = v.decode("utf-8", "replace")
        return '"' + v.replace('"', '""') + '"' if any(c in v for c in ',"\n\r') else v

    os.makedirs(out, exist_ok=True)
    with open(os.path.join(out, "user_daily.csv"), "w", newline="") as f:
        f.write("user_id,day,sessions,events,revenue_cents,top_category\n")
        for who, day, n, ev, revenue, top in user_rows:
            f.write(f"{q(who)},{date(day)},{n},{ev},{revenue},{top}\n")
    with open(os.path.join(out, "campaign_daily.csv"), "w", newline="") as f:
        f.write("campaign_id,channel,day,sessions,purchasing_sessions,revenue_cents,assisted_sessions\n")
        for (c, day), t in camp.items():
            channel = campaigns.get(c, b"organic")
            f.write(f"{q(c)},{q(channel)},{date(day)},{t[0]},{t[1]},{t[2]},{t[3]}\n")


main()
