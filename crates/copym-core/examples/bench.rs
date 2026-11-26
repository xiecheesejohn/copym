//! Store benchmark with generated history:
//!
//!     cargo run --release -p copym-core --example bench -- <items> [db path]
//!
//! Builds the database once (reused when it already has that many items),
//! then measures open, first page, searches, inserts, group counts, memory.

use copym_core::model::{Content, GroupRule};
use copym_core::store::{GroupMatcher, Query, Store};
use std::time::{Duration, Instant};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

const EN: &[&str] = &[
    "the", "meeting", "roadmap", "invoice", "deploy", "server", "error", "config", "release", "customer", "password", "report", "update",
    "design", "review", "budget", "schedule", "database", "network", "project", "ticket", "feature", "branch", "commit", "merge", "test",
    "build", "pipeline", "kubernetes", "docker", "latency", "timeout", "request", "response", "payload", "token", "session", "cache",
];
const ZH: &[&str] = &[
    "会议", "订单", "发货", "快递", "验证码", "报销", "发票", "合同", "项目", "进度", "需求", "测试", "上线", "服务器", "数据库", "地址",
    "电话", "客户", "预算", "周报", "设计", "评审", "账号", "密码", "通知", "活动", "价格", "优惠", "退款", "物流", "会员", "付款",
];
const APPS: &[&str] = &["Firefox", "Slack", "Code", "Terminal", "WeChat", "Mail", "Notes"];

fn text(r: &mut Rng, i: usize) -> String {
    match r.below(10) {
        0 | 1 => format!("https://{}.example.com/{}/{}?id={i}", EN[r.below(EN.len())], EN[r.below(EN.len())], r.next() % 100_000),
        2 => format!("fn {}_{i}() {{\n    let {} = {};\n}}", EN[r.below(EN.len())], EN[r.below(EN.len())], r.next() % 1000),
        3 => format!("{}", r.next() % 1_000_000),
        4..=6 => {
            let n = 4 + r.below(60);
            (0..n).map(|_| ZH[r.below(ZH.len())]).collect::<Vec<_>>().join("") + &format!(" #{i}")
        }
        _ => {
            // mostly short, sometimes long (log-ish distribution)
            let n = match r.below(10) {
                0 => 300 + r.below(1500),
                1..=3 => 40 + r.below(120),
                _ => 3 + r.below(30),
            };
            (0..n).map(|_| EN[r.below(EN.len())]).collect::<Vec<_>>().join(" ") + &format!(" #{i}")
        }
    }
}

fn rss_mb() -> f64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("VmRSS")).and_then(|l| l.split_whitespace().nth(1)?.parse::<f64>().ok()))
        .unwrap_or(0.0)
        / 1024.0
}

fn ms(d: Duration) -> String {
    format!("{:.1} ms", d.as_secs_f64() * 1000.0)
}

fn time<T>(f: impl FnOnce() -> T) -> (T, Duration) {
    let t = Instant::now();
    let v = f();
    (v, t.elapsed())
}

fn pct(mut v: Vec<Duration>, p: f64) -> Duration {
    v.sort();
    v[((v.len() as f64 - 1.0) * p).round() as usize]
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let n: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(1_000_000);
    let path = args.get(2).cloned().unwrap_or_else(|| std::env::temp_dir().join(format!("copym-bench-{n}.sqlite")).display().to_string());
    let path = std::path::Path::new(&path);

    {
        let mut s = Store::open(path, usize::MAX).unwrap();
        if s.len() < n {
            let have = s.len();
            println!("generating {} items into {} ...", n - have, path.display());
            let mut r = Rng(0x9E37_79B9_7F4A_7C15 ^ have as u64);
            let start = copym_core::model::now_secs() - n as i64; // one item per second
            let t = Instant::now();
            let mut done = have;
            while done < n {
                let chunk = (n - done).min(200_000);
                let items: Vec<_> = (done..done + chunk).map(|i| (text(&mut r, i), start + i as i64, Some(APPS[r.below(APPS.len())].to_string()))).collect();
                s.bulk_insert_texts(items).unwrap();
                done += chunk;
                println!("  {done} / {n}  ({:.0} items/s)", (done - have) as f64 / t.elapsed().as_secs_f64());
            }
        }
    }
    let size: u64 = ["", "-wal"].iter().filter_map(|x| std::fs::metadata(format!("{}{x}", path.display())).ok()).map(|m| m.len()).sum();

    let (s, open) = time(|| Store::open(path, usize::MAX).unwrap());
    let mut s = s;
    println!("items          {}", s.len());
    println!("db size        {:.0} MB", size as f64 / 1e6);
    println!("open           {}  (rss {:.0} MB)", ms(open), rss_mb());
    let (p, first) = time(|| s.list_group(&Query::All, None, None, 200));
    println!("first page     {}  ({} items)", ms(first), p.items.len());
    let (p2, deep) = time(|| s.list_group(&Query::All, None, p.next, 200));
    println!("next page      {}  ({} items)", ms(deep), p2.items.len());

    let mut r = Rng(42);
    let run = |label: &str, queries: Vec<String>, regex: bool, s: &mut Store| {
        let mut times = Vec::new();
        let mut hits = 0;
        for q in &queries {
            let query = Query::parse(q, regex).unwrap();
            let (p, d) = time(|| s.list_group(&query, None, None, 200));
            hits += p.items.len();
            if d > Duration::from_millis(120) {
                println!("  slow: {q:?} {}", ms(d));
            }
            times.push(d);
        }
        println!(
            "{label:<14} p50 {}  p95 {}  max {}  (avg {:.1} hits/query, e.g. {:?})",
            ms(pct(times.clone(), 0.5)),
            ms(pct(times.clone(), 0.95)),
            ms(pct(times, 1.0)),
            hits as f64 / queries.len() as f64,
            queries[0]
        );
    };
    let common: Vec<String> = (0..40).map(|_| EN[r.below(EN.len())].to_string()).collect();
    run("search common", common, false, &mut s);
    let rare: Vec<String> = (0..40).map(|_| format!("#{}", r.below(n))).collect();
    run("search rare", rare, false, &mut s);
    let two: Vec<String> = (0..40).map(|_| format!("{} {}", EN[r.below(EN.len())], ZH[r.below(ZH.len())])).collect();
    run("search 2 terms", two, false, &mut s);
    let zh: Vec<String> = (0..40).map(|_| format!("{}{}", ZH[r.below(ZH.len())], ZH[r.below(ZH.len())])).collect();
    run("search zh", zh, false, &mut s);
    let short: Vec<String> = (0..40).map(|_| ZH[r.below(ZH.len())].chars().next().unwrap().to_string()).collect();
    run("search 1 char", short, false, &mut s);
    let missing: Vec<String> = (0..10).map(|i| format!("zzqx{i}")).collect();
    run("search none", missing, false, &mut s);
    run("regex", vec![r"id=\d{5}$".into(), r"^\d{6}$".into(), r"会议.*发票".into()], true, &mut s);

    for (q, regex) in [("config 服务器", false), (r"会议.*发票.*退款$", true)] {
        let query = Query::parse(q, regex).unwrap();
        let (t, mut pages, mut hits, mut next) = (Instant::now(), 0, 0, None);
        loop {
            let p = s.list_group(&query, None, next, 200);
            pages += 1;
            hits += p.items.len();
            next = p.next;
            if next.is_none() || hits >= 200 {
                break;
            }
        }
        println!("full pass      {q:?}: {} over {pages} pages, {hits} hits", ms(t.elapsed()));
    }
    println!("rss            {:.0} MB after searches", rss_mb());
    let mut times = Vec::new();
    for i in 0..1000 {
        if i % 100 == 0 {
            s.background_work(Duration::ZERO); // the daemon's upkeep thread does this every second
        }
        let t = text(&mut r, n + i);
        let (_, d) = time(|| s.insert(Content::Text(t), Some("Bench".into())).unwrap());
        times.push(d);
    }
    println!("insert         p50 {}  p95 {}  max {}", ms(pct(times.clone(), 0.5)), ms(pct(times.clone(), 0.95)), ms(pct(times, 1.0)));

    let rules = [
        GroupRule::default(),
        GroupRule { kind: Some(copym_core::model::Kind::Text), ..Default::default() },
        GroupRule { patterns: vec![r"^\s*https?://".into()], ..Default::default() },
        GroupRule { apps: vec!["(?i)^code$".into()], ..Default::default() },
    ];
    let gs: Vec<_> = rules.iter().map(|r| GroupMatcher::new(r).ok()).collect();
    let (c, d) = time(|| s.group_counts(&Query::All, &gs));
    println!("counts (cold)  {}  {:?} more={}", ms(d), c.counts, c.more);
    let t = Instant::now();
    while s.background_work(Duration::from_millis(15)) {}
    println!("views built    {}  (rss {:.0} MB)", ms(t.elapsed()), rss_mb());
    let (c, d) = time(|| s.group_counts(&Query::All, &gs));
    println!("counts         {}  {:?}", ms(d), c.counts);
    let (c, d) = time(|| s.group_counts(&Query::parse("deploy", false).unwrap(), &gs));
    println!("counts search  {}  {:?} more={}", ms(d), c.counts, c.more);
    let link = gs[2].as_ref().unwrap();
    let (p, d) = time(|| s.list_group(&Query::All, Some(link), None, 200));
    println!("group page     {}  ({} items)", ms(d), p.items.len());
    println!("rss            {:.0} MB", rss_mb());
}
