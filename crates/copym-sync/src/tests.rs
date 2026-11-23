//! End-to-end test: two engines on loopback, pairing + catch-up + live push + delete.

use super::*;
use std::collections::HashMap;

#[derive(Default)]
struct MockHost {
    items: Mutex<HashMap<u64, SyncItem>>,
    tombs: Mutex<Vec<(u64, i64)>>,
    from: Mutex<Vec<String>>,
}

impl Host for MockHost {
    fn inventory(&self) -> Vec<(u64, i64)> {
        self.items.lock().unwrap().values().map(|i| (i.hash, i.created)).collect()
    }
    fn tombstones(&self) -> Vec<(u64, i64)> {
        self.tombs.lock().unwrap().clone()
    }
    fn missing(&self, items: &[(u64, i64)]) -> Vec<u64> {
        let have = self.items.lock().unwrap();
        let tombs = self.tombs.lock().unwrap();
        items
            .iter()
            .filter(|(h, c)| !have.contains_key(h) && !tombs.iter().any(|(t, at)| t == h && at >= c))
            .map(|(h, _)| *h)
            .collect()
    }
    fn fetch(&self, hash: u64) -> Option<SyncItem> {
        self.items.lock().unwrap().get(&hash).cloned()
    }
    fn receive(&self, item: SyncItem, from: &str) {
        self.from.lock().unwrap().push(from.to_string());
        self.items.lock().unwrap().insert(item.hash, item);
    }
    fn receive_delete(&self, deleted: &[(u64, i64)]) {
        let mut items = self.items.lock().unwrap();
        for (h, at) in deleted {
            if items.get(h).is_some_and(|i| i.created <= *at) {
                items.remove(h);
            }
            self.tombs.lock().unwrap().push((*h, *at));
        }
    }
}

fn text(hash: u64, s: &str) -> SyncItem {
    SyncItem { hash, created: 1000 + hash as i64, kind: ItemKind::Text, data: s.as_bytes().to_vec(), width: 0, height: 0 }
}

fn engine(name: &str, peers: Vec<SocketAddr>, host: Arc<MockHost>) -> Engine {
    let dir = std::env::temp_dir().join(format!("copym-sync-test-{}-{name}-{}", std::process::id(), hex(&identity::random::<4>())));
    let cfg = Config { identity_path: dir.join("id.bin"), device_name: name.into(), port: 0, static_peers: peers, mdns: false };
    Engine::start(cfg, host).unwrap()
}

fn wait(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn loopback(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

#[test]
fn pair_sync_push_delete() {
    let ha = Arc::new(MockHost::default());
    ha.items.lock().unwrap().insert(1, text(1, "old item on A"));
    let a = engine("A", vec![], ha.clone());
    let hb = Arc::new(MockHost::default());
    let b = engine("B", vec![loopback(a.status().port)], hb.clone());
    assert!(!a.status().paired && !b.status().paired);

    // Pairing
    let info = a.pair_start().unwrap();
    assert!(a.status().paired);
    let inviter = b.join(&info.code, Some(loopback(info.port))).unwrap();
    assert_eq!(inviter, "A");
    assert!(b.status().paired);

    // Connection + catch-up of the existing item
    wait("connected", || a.status().peers.len() == 1 && b.status().peers.len() == 1);
    wait("catch-up", || hb.items.lock().unwrap().contains_key(&1));
    assert_eq!(hb.from.lock().unwrap()[0], "A");

    // Live push in both directions, including a large (multi-frame) item
    a.broadcast_item(text(2, "live from A"));
    let big = SyncItem { hash: 3, created: 5000, kind: ItemKind::Image, data: vec![42u8; 1_000_000], width: 10, height: 10 };
    hb.items.lock().unwrap().insert(3, big.clone());
    b.broadcast_item(big.clone());
    wait("push A->B", || hb.items.lock().unwrap().contains_key(&2));
    wait("push B->A", || ha.items.lock().unwrap().get(&3) == Some(&big));

    // Delete propagates
    hb.items.lock().unwrap().remove(&2);
    b.broadcast_delete(vec![(2, 9999)]);
    wait("delete", || !ha.items.lock().unwrap().contains_key(&2));

    // Leaving disconnects
    b.leave().unwrap();
    wait("disconnect", || a.status().peers.is_empty());
}

#[test]
fn wrong_code_burns_the_session() {
    let a = engine("A2", vec![], Arc::new(MockHost::default()));
    let c = engine("C2", vec![], Arc::new(MockHost::default()));
    let info = a.pair_start().unwrap();
    let wrong = if info.code.starts_with('0') { "1111-1111" } else { "0000-0000" };
    assert!(c.join(wrong, Some(loopback(info.port))).is_err());
    assert!(!c.status().paired);
    // The session is gone after one failed attempt, even with the right code.
    std::thread::sleep(Duration::from_millis(300));
    assert!(c.join(&info.code, Some(loopback(info.port))).is_err());
    assert!(!c.status().paired);
}

#[test]
fn outsider_cannot_connect() {
    let ha = Arc::new(MockHost::default());
    ha.items.lock().unwrap().insert(7, text(7, "secret"));
    let a = engine("A3", vec![], ha);
    a.pair_start().unwrap();
    // E has its own, different group and points straight at A.
    let he = Arc::new(MockHost::default());
    let e = engine("E3", vec![loopback(a.status().port)], he.clone());
    e.pair_start().unwrap();
    e.reconnect();
    std::thread::sleep(Duration::from_secs(2));
    assert!(a.status().peers.is_empty() && e.status().peers.is_empty());
    assert!(he.items.lock().unwrap().is_empty());
}
