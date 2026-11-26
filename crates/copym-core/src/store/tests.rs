use super::*;

fn rm(p: &std::path::Path) {
    for ext in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{ext}", p.display()));
    }
}

fn tmp(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("copym-test-{tag}-{}-{}.sqlite", std::process::id(), now_secs()));
    rm(&p);
    p
}

fn names(s: &mut Store, g: &GroupMatcher) -> Vec<String> {
    s.list_group(&q(""), Some(g), None, 10).items.into_iter().map(|m| m.preview).collect()
}

fn q(s: &str) -> Query {
    Query::parse(s, false).unwrap()
}

#[test]
fn insert_dedup_evict_search() {
    let p = tmp("basic");
    let mut s = Store::open(&p, 3).unwrap();
    for t in ["alpha", "Beta", "gamma", "delta"] {
        s.insert(Content::Text(t.into()), None).unwrap();
    }
    assert_eq!(s.len(), 3); // alpha evicted
    let ids: Vec<_> = s.list(&q(""), Filter::All, 10).iter().map(|m| m.preview.clone()).collect();
    assert_eq!(ids, ["delta", "gamma", "Beta"]);
    // dedup moves to top
    s.insert(Content::Text("Beta".into()), None).unwrap();
    assert_eq!(s.list(&q(""), Filter::All, 10)[0].preview, "Beta");
    assert_eq!(s.len(), 3);
    // case-insensitive search
    assert_eq!(s.list(&q("bet"), Filter::All, 10).len(), 1);
    // pin survives eviction
    let gamma = s.list(&q("gamma"), Filter::All, 1)[0].id;
    s.toggle_pin(gamma).unwrap();
    for t in ["x1", "x2", "x3", "x4"] {
        s.insert(Content::Text(t.into()), None).unwrap();
    }
    let l = s.list(&q(""), Filter::All, 10);
    assert_eq!(l[0].preview, "gamma");
    assert_eq!(l.len(), 4);
    drop(s);
    // persistence
    let mut s = Store::open(&p, 3).unwrap();
    assert_eq!(s.len(), 4);
    assert_eq!(s.list(&q("中"), Filter::All, 10).len(), 0);
    drop(s);
    rm(&p);
}

#[test]
fn groups_by_rules() {
    let p = tmp("groups");
    let mut s = Store::open(&p, 50).unwrap();
    s.insert(Content::Text("https://example.com/a".into()), Some("Firefox".into())).unwrap();
    s.insert(Content::Text("fn main() {}".into()), Some("Code".into())).unwrap();
    let code = s.insert(Content::Text("SELECT 1;".into()), None).unwrap();
    s.toggle_pin(code).unwrap();
    let png = imgutil::encode_png(2, 2, &[255; 16]).unwrap();
    s.insert(Content::Image { png, width: 2, height: 2 }, None).unwrap();

    let rule = |f: &dyn Fn(&mut GroupRule)| {
        let mut r = GroupRule::default();
        f(&mut r);
        GroupMatcher::new(&r).unwrap()
    };
    let link = rule(&|r| r.patterns = vec![r"^\s*https?://".into()]);
    let from_code = rule(&|r| r.apps = vec!["(?i)^code$".into()]);
    let images = rule(&|r| r.kind = Some(Kind::Image));
    let pinned = rule(&|r| r.pinned = true);
    let other = rule(&|r| {
        r.kind = Some(Kind::Text);
        r.exclude = vec![r"^\s*https?://".into()];
        r.exclude_apps = vec!["(?i)^code$".into()];
    });
    assert_eq!(names(&mut s, &link), ["https://example.com/a"]);
    assert_eq!(names(&mut s, &from_code), ["fn main() {}"]);
    assert_eq!(s.list_group(&q(""), Some(&images), None, 10).items.len(), 1);
    assert_eq!(names(&mut s, &pinned), ["SELECT 1;"]);
    assert_eq!(names(&mut s, &other), ["SELECT 1;"]);
    assert_eq!(s.list_group(&q(""), None, None, 10).items.len(), 4);

    let groups = vec![Some(link), Some(images), None];
    assert_eq!(s.group_counts(&q(""), &groups).counts, vec![Some(1), Some(1), None]);
    assert_eq!(s.group_counts(&q("example"), &groups).counts, vec![Some(1), Some(0), None]);
    assert!(GroupMatcher::new(&GroupRule { patterns: vec!["(".into()], ..Default::default() }).is_err());
    drop(s);
    rm(&p);
}

#[test]
fn regex_and_ocr_search() {
    let p = tmp("regex");
    let mut s = Store::open(&p, 50).unwrap();
    s.insert(Content::Text("订单号 A-2024-0042".into()), None).unwrap();
    s.insert(Content::Text("call me at 555-1234".into()), None).unwrap();
    let png = imgutil::encode_png(2, 2, &[255; 16]).unwrap();
    let img = s.insert(Content::Image { png, width: 2, height: 2 }, None).unwrap();
    assert_eq!(s.pending_ocr(), vec![img]);

    let re = |p: &str| Query::parse(p, true).unwrap();
    assert_eq!(s.list(&re(r"\b\d{3}-\d{4}\b"), Filter::All, 10).len(), 1); // not "024-0042"
    assert_eq!(s.list(&re(r"\d{3}-\d{4}"), Filter::All, 10).len(), 2);
    assert_eq!(s.list(&re(r"^订单号\s+a-\d+"), Filter::All, 10).len(), 1); // case-insensitive
    assert!(Query::parse("(unclosed", true).is_err());

    // OCR text becomes searchable, by plain and regex queries
    let h = s.hash_of(img).unwrap();
    s.set_ocr(h, "发票 金额 ¥128.00\nInvoice".into()).unwrap();
    assert!(s.pending_ocr().is_empty());
    assert_eq!(s.list(&q("发票"), Filter::Image, 10).len(), 1);
    assert_eq!(s.list(&q("INVOICE"), Filter::All, 10)[0].kind, Kind::Image);
    assert_eq!(s.list(&re(r"¥\d+\.\d{2}"), Filter::All, 10).len(), 1);
    drop(s);
    // OCR text persists
    let mut s = Store::open(&p, 50).unwrap();
    assert_eq!(s.list(&q("发票"), Filter::All, 10).len(), 1);
    // reset_ocr makes images pending again; info values persist
    assert_eq!(s.info("ocr_rev"), None);
    s.set_info("ocr_rev", 2).unwrap();
    assert_eq!(s.reset_ocr().unwrap(), 1);
    assert_eq!(s.pending_ocr().len(), 1);
    assert_eq!(s.list(&q("发票"), Filter::All, 10).len(), 0);
    drop(s);
    let s = Store::open(&p, 50).unwrap();
    assert_eq!(s.info("ocr_rev"), Some(2));
    assert_eq!(s.pending_ocr().len(), 1);
    drop(s);
    rm(&p);
}

#[test]
fn sync_merge_order_and_tombstones() {
    let p = tmp("sync");
    let mut s = Store::open(&p, 50).unwrap();
    let now = now_secs();
    s.insert(Content::Text("local new".into()), None).unwrap();
    // An older item from another device slots in below, keeps its time.
    let old = Content::Text("from phone".into());
    assert!(s.insert_synced(old.clone(), Some("Phone".into()), now - 3600).unwrap().is_some());
    let l = s.list(&q(""), Filter::All, 10);
    assert_eq!(l[0].preview, "local new");
    assert_eq!((l[1].preview.as_str(), l[1].source.as_deref()), ("from phone", Some("Phone")));
    // Already present -> ignored, not moved.
    assert!(s.insert_synced(old.clone(), None, now).unwrap().is_none());
    assert_eq!(s.list(&q(""), Filter::All, 10)[0].preview, "local new");

    // Local delete produces a tombstone; the peer's stale copy is not fetched back.
    let id = s.list(&q("phone"), Filter::All, 1)[0].id;
    let tombs = s.delete(id).unwrap();
    assert_eq!(tombs.len(), 1);
    assert!(s.sync_missing(&[(old.hash(), now - 3600)]).is_empty());
    assert!(s.insert_synced(old.clone(), None, now - 3600).unwrap().is_none());
    // ...but a newer copy of the same content is accepted.
    assert_eq!(s.sync_missing(&[(old.hash(), now + 10)]), vec![old.hash()]);

    // Remote deletion removes older copies, keeps pinned ones.
    let keep = Content::Text("pinned".into());
    let kid = s.insert(keep.clone(), None).unwrap();
    s.toggle_pin(kid).unwrap();
    let gone = Content::Text("gone".into());
    s.insert(gone.clone(), None).unwrap();
    let n = s.delete_synced(&[(gone.hash(), now + 100), (keep.hash(), now + 100)]).unwrap();
    assert_eq!(n, 1);
    assert_eq!(s.list(&q("gone"), Filter::All, 10).len(), 0);
    assert_eq!(s.list(&q("pinned"), Filter::All, 10).len(), 1);

    // Tombstones survive a restart; copying again revives the content.
    drop(s);
    let mut s = Store::open(&p, 50).unwrap();
    assert!(s.tombstones().iter().any(|(h, _)| *h == gone.hash()));
    s.insert(gone.clone(), None).unwrap();
    assert!(!s.tombstones().iter().any(|(h, _)| *h == gone.hash()));
    assert_eq!(s.sync_inventory(10, true, u64::MAX)[0].0, gone.hash());
    drop(s);
    rm(&p);
}

fn page(s: &mut Store, query: &str, before: Option<u64>, n: usize) -> (Vec<String>, Option<u64>) {
    let p = s.list_group(&q(query), None, before, n);
    (p.items.into_iter().map(|m| m.preview).collect(), p.next)
}

#[test]
fn paging_and_trigram_search() {
    let p = tmp("paging");
    let mut s = Store::open(&p, 10_000).unwrap();
    let base = now_secs() - 10_000;
    let texts: Vec<(String, i64, Option<String>)> = (0..2000)
        .map(|i| {
            let t = match i % 4 {
                0 => format!("订单 {i} 已发货，快递单号 SF{i:06}"),
                1 => format!("https://example.com/item/{i}?ref=Mail"),
                2 => format!("Meeting notes #{i}: discuss the roadmap"),
                _ => format!("{}", "长文本".repeat(200) + &i.to_string()),
            };
            (t, base + i, Some(if i % 2 == 0 { "Slack" } else { "Firefox" }.to_string()))
        })
        .collect();
    assert_eq!(s.bulk_insert_texts(texts).unwrap(), 2000);
    assert_eq!(s.len(), 2000);

    // pages continue where the last one stopped, newest first, no overlap
    let (a, next) = page(&mut s, "", None, 3);
    assert!(a[0].starts_with("长文本") && a[0].chars().count() == 600);
    let (b, _) = page(&mut s, "", next, 3);
    assert_eq!(b[0], "订单 1996 已发货，快递单号 SF001996");

    // trigram index: ASCII, CJK, case, multiple terms, source app
    assert_eq!(page(&mut s, "SF000004", None, 50).0, ["订单 4 已发货，快递单号 SF000004"]);
    assert_eq!(page(&mut s, "快递单号", None, 1000).0.len(), 500);
    assert_eq!(page(&mut s, "ROADMAP #1002", None, 50).0, ["Meeting notes #1002: discuss the roadmap"]);
    assert_eq!(page(&mut s, "example firefox", None, 1000).0.len(), 500);
    assert_eq!(page(&mut s, "example slack", None, 1000).0.len(), 0);
    // short terms and regexes scan instead
    assert_eq!(page(&mut s, "单", None, 1000).0.len(), 500);
    let re = s.list_group(&Query::parse(r"#1\d{3}:", true).unwrap(), None, None, 1000).items;
    assert_eq!(re.len(), 250);
    // a search pages too
    let (first, next) = page(&mut s, "roadmap", None, 100);
    let (second, _) = page(&mut s, "roadmap", next, 1000);
    assert_eq!(first.len() + second.len(), 500);
    assert!(!second.contains(&first[99]));
    // compressed long texts come back whole
    let id = s.list_group(&q("长文本"), None, None, 1).items[0].id;
    let Some(Content::Text(t)) = s.get(id).unwrap() else { panic!() };
    assert_eq!(t, "长文本".repeat(200) + "1999");
    drop(s);
    rm(&p);
}

#[test]
fn views_count_and_persist() {
    let p = tmp("views");
    let mut s = Store::open(&p, 10_000).unwrap();
    let base = now_secs() - 10_000;
    s.bulk_insert_texts((0..5000).map(|i| (if i % 10 == 0 { format!("https://x.org/{i}") } else { format!("note {i}") }, base + i, None)))
        .unwrap();
    let link = GroupMatcher::new(&GroupRule { patterns: vec![r"^https?://".into()], ..Default::default() }).unwrap();
    let text = GroupMatcher::new(&GroupRule { kind: Some(Kind::Text), ..Default::default() }).unwrap();
    let all = GroupMatcher::new(&GroupRule::default()).unwrap();
    let groups = vec![Some(all), Some(link.clone()), Some(text)];
    assert_eq!(s.group_counts(&q(""), &groups), Counts { counts: vec![Some(5000), Some(500), Some(5000)], more: false });
    // writes keep the views current
    s.insert(Content::Text("https://new.example".into()), None).unwrap();
    let id = s.list_group(&q("x.org/4990"), None, None, 1).items[0].id;
    s.delete(id).unwrap();
    assert_eq!(s.group_counts(&q(""), &groups).counts, vec![Some(5000), Some(500), Some(5000)]);
    // a sparse group lists from its bitmap
    let l = s.list_group(&q(""), Some(&link), None, 3).items;
    assert_eq!(l.iter().map(|m| m.preview.as_str()).collect::<Vec<_>>(), ["https://new.example", "https://x.org/4980", "https://x.org/4970"]);
    // during a search, counts cover the hits
    assert_eq!(s.group_counts(&q("x.org/49"), &groups).counts, vec![Some(10), Some(10), Some(10)]);
    // saved views come back without a rebuild
    drop(s);
    let mut s = Store::open(&p, 10_000).unwrap();
    assert!(s.views.get(&link.fp).is_some_and(|v| v.cursor == 0 && v.count == 500));
    assert_eq!(s.group_counts(&q(""), &groups).counts, vec![Some(5000), Some(500), Some(5000)]);
    // clear keeps pinned items only
    let keep = s.list_group(&q("x.org/10"), None, None, 1).items[0].id;
    s.toggle_pin(keep).unwrap();
    s.clear().unwrap();
    assert_eq!(s.group_counts(&q(""), &groups).counts, vec![Some(1), Some(1), Some(1)]);
    assert_eq!(page(&mut s, "x.org", None, 10).0, ["https://x.org/1090"]);
    drop(s);
    rm(&p);
}

#[test]
fn pin_groups() {
    let p = tmp("boards");
    let mut s = Store::open(&p, 3).unwrap();
    let a = s.insert(Content::Text("reply: thanks".into()), None).unwrap();
    let b = s.insert(Content::Text("ssh prod-1".into()), None).unwrap();
    s.insert(Content::Text("latest".into()), None).unwrap();
    s.set_pin(a, 2).unwrap();
    s.set_pin(b, 3).unwrap();
    // the global shortcut puts the newest item into a group
    let latest = s.pin_latest(2).unwrap().unwrap();
    let board = |n: u32| GroupMatcher::new(&GroupRule { board: Some(n), ..Default::default() }).unwrap();
    let any = GroupMatcher::new(&GroupRule { pinned: true, ..Default::default() }).unwrap();
    let names = |s: &mut Store, g: &GroupMatcher| -> Vec<String> { s.list_group(&q(""), Some(g), None, 10).items.into_iter().map(|m| m.preview).collect() };
    assert_eq!(names(&mut s, &board(2)), ["latest", "reply: thanks"]);
    assert_eq!(names(&mut s, &board(3)), ["ssh prod-1"]);
    let groups = vec![Some(board(2)), Some(board(3)), Some(any)];
    assert_eq!(s.group_counts(&q(""), &groups).counts, vec![Some(2), Some(1), Some(3)]);
    let m = s.list_group(&q("ssh"), None, None, 1).items.remove(0);
    assert_eq!((m.pinned, m.board), (true, 3));
    // pinned items survive eviction and re-copies keep their group
    for t in ["x1", "x2", "x3", "x4", "x5"] {
        s.insert(Content::Text(t.into()), None).unwrap();
    }
    s.insert(Content::Text("ssh prod-1".into()), None).unwrap();
    assert_eq!(names(&mut s, &board(3)), ["ssh prod-1"]);
    // unpin; toggle uses group 1
    s.set_pin(latest, 0).unwrap();
    assert_eq!(names(&mut s, &board(2)), ["reply: thanks"]);
    let id = s.list_group(&q("x5"), None, None, 1).items[0].id;
    s.toggle_pin(id).unwrap();
    assert_eq!(names(&mut s, &board(1)), ["x5"]);
    drop(s);
    rm(&p);
}
