//! # copym-sync
//!
//! End-to-end encrypted, serverless clipboard sync over the local network.
//! Platform independent (std + pure-Rust crypto), so the same core can be
//! embedded in the desktop daemon and, later, in an Android app (via UniFFI).
//!
//! * **Discovery**: mDNS / DNS-SD (`_copym._tcp`), or static `ip:port` peers.
//! * **Transport**: TCP + Noise `NNpsk0` (X25519, ChaCha20-Poly1305, BLAKE2s);
//!   the PSK is the 256-bit group key, every connection gets fresh ephemeral
//!   keys (forward secrecy). Devices outside the group cannot even complete a
//!   handshake.
//! * **Pairing**: an 8-digit one-time code + SPAKE2 (password-authenticated key
//!   exchange). The code never crosses the network; a wrong guess burns the
//!   pairing session, so it cannot be brute forced. The group key is then sent
//!   over a Noise channel keyed by the SPAKE2 result.
//! * **Sync model**: items are identified by content hash. On connect peers
//!   exchange inventories (recent items + deletions) and fetch what they miss;
//!   afterwards new copies and deletions are pushed live. The host decides
//!   what to store - copym only adds received items to history.

mod channel;
mod identity;
mod protocol;

pub use identity::{hex, Identity};
pub use protocol::{ItemKind, SyncItem};

use channel::{handshake, read_raw, split, write_raw, SecureSender};
use protocol::{Msg, PairAck, PairOffer, PROTO_VERSION};
use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SERVICE: &str = "_copym._tcp.local.";
const PAIR_SERVICE: &str = "_copym-pair._tcp.local.";
const SYNC_PROLOGUE: &[u8] = b"copym-sync/1";
const PAIR_PROLOGUE: &[u8] = b"copym-pair/1";
const PAIR_IDENTITY: &[u8] = b"copym-pair-v1";
const PAIR_TIMEOUT: Duration = Duration::from_secs(180);
const KEEPALIVE: Duration = Duration::from_secs(25);
const READ_TIMEOUT: Duration = Duration::from_secs(80);
const RECONNECT_EVERY: Duration = Duration::from_secs(15);

/// Callbacks into the application that owns the history.
pub trait Host: Send + Sync + 'static {
    /// Recent items this device offers to peers: (hash, created).
    fn inventory(&self) -> Vec<(u64, i64)>;
    /// Recently deleted items: (hash, deleted_at).
    fn tombstones(&self) -> Vec<(u64, i64)>;
    /// Which of the peer's items we want (not present, not deleted afterwards).
    fn missing(&self, items: &[(u64, i64)]) -> Vec<u64>;
    fn fetch(&self, hash: u64) -> Option<SyncItem>;
    /// An item arrived from device `from`.
    fn receive(&self, item: SyncItem, from: &str);
    /// Items deleted on another device: (hash, deleted_at).
    fn receive_delete(&self, deleted: &[(u64, i64)]);
}

pub struct Config {
    /// Where the device id and group key are stored (keep it private).
    pub identity_path: PathBuf,
    pub device_name: String,
    /// Preferred listening port (0 = any). Falls back to a random port if busy.
    pub port: u16,
    /// Extra peers to connect to without discovery ("192.168.1.20:47800").
    pub static_peers: Vec<SocketAddr>,
    /// Use mDNS for discovery and pairing.
    pub mdns: bool,
}

#[derive(Clone, Debug)]
pub struct PeerInfo {
    pub device_id: String,
    pub name: String,
    pub addr: SocketAddr,
}

#[derive(Clone, Debug)]
pub struct Status {
    pub device_id: String,
    pub device_name: String,
    pub paired: bool,
    pub port: u16,
    pub peers: Vec<PeerInfo>,
    pub pairing_code: Option<String>,
}

#[derive(Clone, Debug)]
pub struct PairingInfo {
    pub code: String,
    /// Addresses a joiner can use when mDNS is blocked: `code@addr`.
    pub addrs: Vec<SocketAddr>,
    pub port: u16,
    pub expires_in: Duration,
}

struct Peer {
    name: String,
    addr: SocketAddr,
    conn_id: u64,
    /// Device id of the side that dialed this connection (tie-breaker).
    dialer: [u8; 16],
    tx: mpsc::Sender<Arc<Vec<u8>>>,
    closer: TcpStream,
}

struct Pairing {
    code: String,
    cancel: Arc<AtomicBool>,
}

struct Inner {
    cfg: Config,
    host: Arc<dyn Host>,
    identity: Mutex<Identity>,
    port: u16,
    peers: Mutex<HashMap<[u8; 16], Peer>>,
    /// Discovered (mDNS) addresses: device id -> addr.
    known: Mutex<HashMap<[u8; 16], SocketAddr>>,
    dialing: Mutex<std::collections::HashSet<SocketAddr>>,
    mdns: Option<mdns_sd::ServiceDaemon>,
    advertised: Mutex<Option<String>>,
    pairing: Mutex<Option<Pairing>>,
    next_conn: AtomicU64,
}

/// Handle to the running sync engine (cheap to clone).
#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}

fn log(msg: impl AsRef<str>) {
    eprintln!("copym-sync: {}", msg.as_ref());
}

impl Engine {
    pub fn start(cfg: Config, host: Arc<dyn Host>) -> io::Result<Engine> {
        let identity = Identity::load_or_create(&cfg.identity_path)?;
        let listener = TcpListener::bind(("0.0.0.0", cfg.port)).or_else(|_| TcpListener::bind(("0.0.0.0", 0)))?;
        let port = listener.local_addr()?.port();
        let mdns = if cfg.mdns {
            match mdns_sd::ServiceDaemon::new() {
                Ok(d) => Some(d),
                Err(e) => {
                    log(format!("mDNS unavailable: {e}"));
                    None
                }
            }
        } else {
            None
        };
        let inner = Arc::new(Inner {
            cfg,
            host,
            identity: Mutex::new(identity),
            port,
            peers: Mutex::new(HashMap::new()),
            known: Mutex::new(HashMap::new()),
            dialing: Mutex::new(Default::default()),
            mdns,
            advertised: Mutex::new(None),
            pairing: Mutex::new(None),
            next_conn: AtomicU64::new(1),
        });
        let engine = Engine { inner };

        // Accept loop.
        {
            let e = engine.clone();
            std::thread::Builder::new().name("sync-accept".into()).spawn(move || {
                for stream in listener.incoming().flatten() {
                    let e = e.clone();
                    let _ = std::thread::Builder::new().name("sync-in".into()).spawn(move || {
                        if let Err(err) = e.incoming(stream) {
                            if err.kind() != io::ErrorKind::PermissionDenied {
                                log(format!("incoming connection failed: {err}"));
                            }
                        }
                    });
                }
            })?;
        }

        // Discovery.
        if let Some(mdns) = &engine.inner.mdns {
            engine.advertise();
            let rx = mdns.browse(SERVICE).map_err(io::Error::other)?;
            let e = engine.clone();
            std::thread::Builder::new().name("sync-mdns".into()).spawn(move || {
                while let Ok(ev) = rx.recv() {
                    e.on_mdns(ev);
                }
            })?;
        }

        // Reconnect + keepalive housekeeping.
        {
            let e = engine.clone();
            std::thread::Builder::new().name("sync-maint".into()).spawn(move || loop {
                e.reconnect();
                std::thread::sleep(RECONNECT_EVERY);
            })?;
        }
        Ok(engine)
    }

    // ------------------------------------------------------------ public --

    pub fn status(&self) -> Status {
        let id = self.inner.identity.lock().unwrap().clone();
        let mut peers: Vec<PeerInfo> = self
            .inner
            .peers
            .lock()
            .unwrap()
            .iter()
            .map(|(d, p)| PeerInfo { device_id: hex(d), name: p.name.clone(), addr: p.addr })
            .collect();
        peers.sort_by(|a, b| a.name.cmp(&b.name));
        Status {
            device_id: id.device_hex(),
            device_name: self.inner.cfg.device_name.clone(),
            paired: id.group_key.is_some(),
            port: self.inner.port,
            peers,
            pairing_code: self.inner.pairing.lock().unwrap().as_ref().map(|p| p.code.clone()),
        }
    }

    /// Push a newly copied item to all connected devices.
    pub fn broadcast_item(&self, item: SyncItem) {
        self.broadcast(&Msg::Item(item));
    }

    /// Propagate deletions: (hash, deleted_at).
    pub fn broadcast_delete(&self, deleted: Vec<(u64, i64)>) {
        if !deleted.is_empty() {
            self.broadcast(&Msg::Delete(deleted));
        }
    }

    /// Become (or stay) a group member and open a pairing window for a new
    /// device. Returns the one-time code to type on the other device.
    pub fn pair_start(&self) -> io::Result<PairingInfo> {
        let key = {
            let mut id = self.inner.identity.lock().unwrap();
            if id.group_key.is_none() {
                id.group_key = Some(identity::random());
                id.save(&self.inner.cfg.identity_path)?;
            }
            id.group_key.unwrap()
        };
        self.advertise();
        if let Some(p) = self.inner.pairing.lock().unwrap().take() {
            p.cancel.store(true, Ordering::Relaxed);
        }
        let n = u64::from_le_bytes(identity::random()) % 100_000_000;
        let code = format!("{:04}-{:04}", n / 10_000, n % 10_000);
        let listener = TcpListener::bind(("0.0.0.0", 0))?;
        let port = listener.local_addr()?.port();
        listener.set_nonblocking(true)?;
        let cancel = Arc::new(AtomicBool::new(false));
        *self.inner.pairing.lock().unwrap() = Some(Pairing { code: code.clone(), cancel: cancel.clone() });

        let pair_fullname = self.inner.mdns.as_ref().and_then(|m| {
            let id = self.inner.identity.lock().unwrap().device_hex();
            let props = [("d", id.as_str()), ("n", self.inner.cfg.device_name.as_str())];
            let info = mdns_sd::ServiceInfo::new(PAIR_SERVICE, &id, &format!("copym-{}.local.", &id[..8]), "", port, &props[..])
                .ok()?
                .enable_addr_auto();
            let name = info.get_fullname().to_string();
            m.register(info).ok().map(|_| name)
        });

        let e = self.clone();
        let code2 = code.clone();
        std::thread::Builder::new().name("sync-pair".into()).spawn(move || {
            let deadline = Instant::now() + PAIR_TIMEOUT;
            let result = loop {
                if cancel.load(Ordering::Relaxed) {
                    break None;
                }
                if Instant::now() > deadline {
                    log("pairing window expired");
                    break None;
                }
                match listener.accept() {
                    Ok((stream, _)) => break Some(stream),
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(100)),
                    Err(_) => break None,
                }
            };
            if let Some(stream) = result {
                // One attempt only: a wrong code ends the session (no brute force).
                match pair_responder(stream, &code2, key, &e.inner.cfg.device_name) {
                    Ok(name) => log(format!("paired with {name}")),
                    Err(err) => log(format!("pairing failed: {err}")),
                }
            }
            if let (Some(m), Some(n)) = (&e.inner.mdns, pair_fullname) {
                let _ = m.unregister(&n);
            }
            let mut p = e.inner.pairing.lock().unwrap();
            if p.as_ref().is_some_and(|p| p.code == code2) {
                *p = None;
            }
        })?;

        Ok(PairingInfo {
            code,
            addrs: local_ipv4().into_iter().map(|ip| SocketAddr::new(IpAddr::V4(ip), port)).collect(),
            port,
            expires_in: PAIR_TIMEOUT,
        })
    }

    /// Join the group of the device showing `code`. `addr` skips discovery.
    /// Returns the inviting device's name.
    pub fn join(&self, code: &str, addr: Option<SocketAddr>) -> io::Result<String> {
        let code: String = code.chars().filter(char::is_ascii_digit).collect();
        if code.len() != 8 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "配对码应为 8 位数字"));
        }
        let code = format!("{}-{}", &code[..4], &code[4..]);
        let candidates = match addr {
            Some(a) => vec![a],
            None => self.discover_pairing(Duration::from_secs(8))?,
        };
        if candidates.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "局域网内没有找到正在配对的设备（可改用 配对码@IP:端口）",
            ));
        }
        let mut last_err = None;
        for a in candidates {
            let stream = match TcpStream::connect_timeout(&a, Duration::from_secs(3)) {
                Ok(s) => s,
                Err(e) => {
                    last_err = Some(e);
                    continue;
                }
            };
            match pair_initiator(stream, &code, &self.inner.cfg.device_name) {
                Ok((offer, name)) => {
                    self.set_group(Some(offer))?;
                    return Ok(name);
                }
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| io::Error::other("pairing failed")))
    }

    /// Leave the sync group (forget the key, disconnect everyone).
    pub fn leave(&self) -> io::Result<()> {
        self.set_group(None)
    }

    // ----------------------------------------------------------- private --

    fn set_group(&self, key: Option<[u8; 32]>) -> io::Result<()> {
        {
            let mut id = self.inner.identity.lock().unwrap();
            id.group_key = key;
            id.save(&self.inner.cfg.identity_path)?;
        }
        for (_, p) in self.inner.peers.lock().unwrap().drain() {
            let _ = p.closer.shutdown(std::net::Shutdown::Both);
        }
        self.inner.known.lock().unwrap().clear();
        self.advertise();
        if key.is_some() {
            let e = self.clone();
            std::thread::spawn(move || {
                // give mDNS a moment, then try to connect right away
                std::thread::sleep(Duration::from_millis(1500));
                e.reconnect();
            });
        }
        Ok(())
    }

    fn advertise(&self) {
        let Some(mdns) = &self.inner.mdns else { return };
        if let Some(old) = self.inner.advertised.lock().unwrap().take() {
            let _ = mdns.unregister(&old);
        }
        let id = self.inner.identity.lock().unwrap().clone();
        let Some(gid) = id.group_id() else { return };
        let dev = id.device_hex();
        let props = [("g", gid.as_str()), ("d", dev.as_str()), ("n", self.inner.cfg.device_name.as_str())];
        match mdns_sd::ServiceInfo::new(SERVICE, &dev, &format!("copym-{}.local.", &dev[..8]), "", self.inner.port, &props[..]) {
            Ok(info) => {
                let info = info.enable_addr_auto();
                let name = info.get_fullname().to_string();
                if mdns.register(info).is_ok() {
                    *self.inner.advertised.lock().unwrap() = Some(name);
                }
            }
            Err(e) => log(format!("mDNS register failed: {e}")),
        }
    }

    fn on_mdns(&self, ev: mdns_sd::ServiceEvent) {
        let mdns_sd::ServiceEvent::ServiceResolved(info) = ev else { return };
        let (my_gid, my_id) = {
            let id = self.inner.identity.lock().unwrap();
            (id.group_id(), id.device_id)
        };
        let Some(my_gid) = my_gid else { return };
        if info.get_property_val_str("g") != Some(my_gid.as_str()) {
            return;
        }
        let Some(dev) = info.get_property_val_str("d").and_then(identity::parse_hex16) else { return };
        if dev == my_id {
            return;
        }
        let Some(ip) = info.get_addresses_v4().into_iter().next() else { return };
        let addr = SocketAddr::new(IpAddr::V4(ip), info.get_port());
        self.inner.known.lock().unwrap().insert(dev, addr);
        // Only the device with the smaller id dials, to avoid duplicate links.
        if my_id < dev && !self.inner.peers.lock().unwrap().contains_key(&dev) {
            self.dial(addr);
        }
    }

    fn reconnect(&self) {
        let (paired, my_id) = {
            let id = self.inner.identity.lock().unwrap();
            (id.group_key.is_some(), id.device_id)
        };
        if !paired {
            return;
        }
        let connected: Vec<SocketAddr> = self.inner.peers.lock().unwrap().values().map(|p| p.addr).collect();
        let mut targets: Vec<SocketAddr> = self
            .inner
            .known
            .lock()
            .unwrap()
            .iter()
            .filter(|(d, _)| my_id < **d && !self.inner.peers.lock().unwrap().contains_key(*d))
            .map(|(_, a)| *a)
            .collect();
        targets.extend(self.inner.cfg.static_peers.iter().filter(|a| !connected.iter().any(|c| c.ip() == a.ip())));
        for a in targets {
            self.dial(a);
        }
    }

    fn dial(&self, addr: SocketAddr) {
        if !self.inner.dialing.lock().unwrap().insert(addr) {
            return; // already dialing
        }
        let e = self.clone();
        let _ = std::thread::Builder::new().name("sync-out".into()).spawn(move || {
            let r = e.outgoing(addr);
            e.inner.dialing.lock().unwrap().remove(&addr);
            if let Err(err) = r {
                if !matches!(err.kind(), io::ErrorKind::ConnectionRefused | io::ErrorKind::TimedOut) {
                    log(format!("connection to {addr} failed: {err}"));
                }
            }
        });
    }

    fn group_key(&self) -> Option<[u8; 32]> {
        self.inner.identity.lock().unwrap().group_key
    }

    fn outgoing(&self, addr: SocketAddr) -> io::Result<()> {
        let key = self.group_key().ok_or_else(|| io::Error::other("not paired"))?;
        let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(4))?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        let state = handshake(&mut stream, &key, true, SYNC_PROLOGUE)?;
        let me = self.inner.identity.lock().unwrap().device_id;
        self.run_peer(stream, state, addr, Some(me))
    }

    fn incoming(&self, mut stream: TcpStream) -> io::Result<()> {
        let key = self.group_key().ok_or_else(|| io::Error::new(io::ErrorKind::PermissionDenied, "not paired"))?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        let addr = stream.peer_addr()?;
        let state = handshake(&mut stream, &key, false, SYNC_PROLOGUE)?;
        self.run_peer(stream, state, addr, None)
    }

    /// `dialer`: Some(my id) when we dialed; None when the peer dialed.
    fn run_peer(
        &self,
        stream: TcpStream,
        state: Arc<snow::StatelessTransportState>,
        addr: SocketAddr,
        dialer: Option<[u8; 16]>,
    ) -> io::Result<()> {
        let closer = stream.try_clone()?;
        let (mut tx, mut rx) = split(stream, state)?;
        let me = self.inner.identity.lock().unwrap().device_id;
        tx.send(&Msg::Hello { device_id: me, name: self.inner.cfg.device_name.clone(), proto: PROTO_VERSION })?;
        let Msg::Hello { device_id, name, proto } = rx.recv()? else {
            return Err(io::Error::other("protocol error: expected hello"));
        };
        if proto != PROTO_VERSION {
            return Err(io::Error::other(format!("{name} uses an incompatible protocol version {proto}")));
        }
        if device_id == me {
            return Ok(()); // connected to ourselves
        }
        rx.set_timeout(Some(READ_TIMEOUT))?;
        let dialer = dialer.unwrap_or(device_id);

        // Register, resolving duplicate connections deterministically.
        let conn_id = self.inner.next_conn.fetch_add(1, Ordering::Relaxed);
        let (msg_tx, msg_rx) = mpsc::channel::<Arc<Vec<u8>>>();
        {
            let mut peers = self.inner.peers.lock().unwrap();
            if let Some(existing) = peers.get(&device_id) {
                let keep_new = dialer < existing.dialer || dialer == existing.dialer;
                if !keep_new {
                    return Ok(());
                }
                let _ = existing.closer.shutdown(std::net::Shutdown::Both);
            }
            peers.insert(device_id, Peer { name: name.clone(), addr, conn_id, dialer, tx: msg_tx.clone(), closer: closer.try_clone()? });
        }
        log(format!("connected to {name} ({addr})"));

        // Writer thread: queued messages + keepalive.
        let writer = std::thread::Builder::new().name("sync-writer".into()).spawn(move || writer_loop(tx, msg_rx))?;

        // Catch-up: tell the peer what we have and what we deleted.
        let host = self.inner.host.clone();
        send_msg(&msg_tx, &Msg::Inventory { items: host.inventory(), deleted: host.tombstones() });

        let result = loop {
            let msg: Msg = match rx.recv() {
                Ok(m) => m,
                Err(e) => break Err(e),
            };
            match msg {
                Msg::Inventory { items, deleted } => {
                    host.receive_delete(&deleted);
                    let want = host.missing(&items);
                    if !want.is_empty() {
                        send_msg(&msg_tx, &Msg::Want(want));
                    }
                }
                Msg::Want(hashes) => {
                    for h in hashes {
                        if let Some(item) = host.fetch(h) {
                            send_msg(&msg_tx, &Msg::Item(item));
                        }
                    }
                }
                Msg::Item(item) => host.receive(item, &name),
                Msg::Delete(d) => host.receive_delete(&d),
                Msg::Ping | Msg::Hello { .. } => {}
            }
        };

        // Cleanup (only if this connection is still the registered one).
        {
            let mut peers = self.inner.peers.lock().unwrap();
            if peers.get(&device_id).is_some_and(|p| p.conn_id == conn_id) {
                peers.remove(&device_id);
                log(format!("disconnected from {name}"));
            }
        }
        let _ = closer.shutdown(std::net::Shutdown::Both);
        drop(msg_tx);
        let _ = writer.join();
        match result {
            Err(e) if matches!(e.kind(), io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset) => Ok(()),
            other => other.map(|_: ()| ()),
        }
    }

    fn broadcast(&self, msg: &Msg) {
        let Ok(bytes) = postcard::to_stdvec(msg) else { return };
        let bytes = Arc::new(bytes);
        for p in self.inner.peers.lock().unwrap().values() {
            let _ = p.tx.send(bytes.clone());
        }
    }

    fn discover_pairing(&self, wait: Duration) -> io::Result<Vec<SocketAddr>> {
        let Some(mdns) = &self.inner.mdns else { return Ok(Vec::new()) };
        let rx = mdns.browse(PAIR_SERVICE).map_err(io::Error::other)?;
        let my = self.inner.identity.lock().unwrap().device_hex();
        let deadline = Instant::now() + wait;
        let mut found = Vec::new();
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match rx.recv_timeout(left) {
                Ok(mdns_sd::ServiceEvent::ServiceResolved(info)) => {
                    if info.get_property_val_str("d") == Some(my.as_str()) {
                        continue;
                    }
                    for ip in info.get_addresses_v4() {
                        found.push(SocketAddr::new(IpAddr::V4(ip), info.get_port()));
                    }
                    if !found.is_empty() {
                        // Give other interfaces/devices a brief moment, then go.
                        std::thread::sleep(Duration::from_millis(300));
                        break;
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        let _ = mdns.stop_browse(PAIR_SERVICE);
        Ok(found)
    }
}

fn send_msg(tx: &mpsc::Sender<Arc<Vec<u8>>>, msg: &Msg) {
    if let Ok(b) = postcard::to_stdvec(msg) {
        let _ = tx.send(Arc::new(b));
    }
}

fn writer_loop(mut tx: SecureSender, rx: mpsc::Receiver<Arc<Vec<u8>>>) {
    let ping = postcard::to_stdvec(&Msg::Ping).unwrap_or_default();
    loop {
        let res = match rx.recv_timeout(KEEPALIVE) {
            Ok(bytes) => tx.send_bytes(&bytes),
            Err(mpsc::RecvTimeoutError::Timeout) => tx.send_bytes(&ping),
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if res.is_err() {
            break;
        }
    }
    tx.shutdown();
}

// ------------------------------------------------------------------ pairing --

/// Inviting side. Returns the joiner's name.
fn pair_responder(mut stream: TcpStream, code: &str, group_key: [u8; 32], my_name: &str) -> io::Result<String> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(15)))?;
    let (spake, outbound) = spake2::Spake2::<spake2::Ed25519Group>::start_symmetric(
        &spake2::Password::new(code.as_bytes()),
        &spake2::Identity::new(PAIR_IDENTITY),
    );
    let theirs = read_raw(&mut stream)?;
    write_raw(&mut stream, &outbound)?;
    let key = spake_key(spake.finish(&theirs))?;
    let state = handshake(&mut stream, &key, false, PAIR_PROLOGUE)
        .map_err(|_| io::Error::new(io::ErrorKind::PermissionDenied, "wrong pairing code"))?;
    let (mut tx, mut rx) = split(stream, state)?;
    tx.send(&PairOffer { group_key, name: my_name.to_string() })?;
    let ack: PairAck = rx.recv()?;
    Ok(ack.name)
}

/// Joining side. Returns (group key, inviter name).
fn pair_initiator(mut stream: TcpStream, code: &str, my_name: &str) -> io::Result<([u8; 32], String)> {
    stream.set_read_timeout(Some(Duration::from_secs(15)))?;
    let (spake, outbound) = spake2::Spake2::<spake2::Ed25519Group>::start_symmetric(
        &spake2::Password::new(code.as_bytes()),
        &spake2::Identity::new(PAIR_IDENTITY),
    );
    write_raw(&mut stream, &outbound)?;
    let theirs = read_raw(&mut stream)?;
    let key = spake_key(spake.finish(&theirs))?;
    let state = handshake(&mut stream, &key, true, PAIR_PROLOGUE)
        .map_err(|_| io::Error::new(io::ErrorKind::PermissionDenied, "配对码错误或配对已过期"))?;
    let (mut tx, mut rx) = split(stream, state)?;
    let offer: PairOffer = rx.recv()?;
    tx.send(&PairAck { name: my_name.to_string() })?;
    Ok((offer.group_key, offer.name))
}

fn spake_key(r: Result<Vec<u8>, spake2::Error>) -> io::Result<[u8; 32]> {
    let k = r.map_err(|_| io::Error::new(io::ErrorKind::PermissionDenied, "pairing key exchange failed"))?;
    k.try_into().map_err(|_| io::Error::other("unexpected SPAKE2 key size"))
}

/// Best-effort list of this machine's LAN IPv4 addresses.
fn local_ipv4() -> Vec<Ipv4Addr> {
    // Connecting a UDP socket sends nothing; it just selects the outgoing interface.
    let mut out = Vec::new();
    if let Ok(s) = UdpSocket::bind("0.0.0.0:0") {
        if s.connect("10.255.255.255:9").is_ok() || s.connect("192.168.255.255:9").is_ok() {
            if let Ok(SocketAddr::V4(a)) = s.local_addr() {
                if !a.ip().is_loopback() && !a.ip().is_unspecified() {
                    out.push(*a.ip());
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests;
