//! Encrypted, authenticated channel over TCP using the Noise protocol.
//!
//! Pattern `Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s`:
//! * ephemeral X25519 on both sides -> forward secrecy for every connection,
//! * a 32-byte pre-shared key mixed in first -> only holders of the key can
//!   complete the handshake (sync group key, or the SPAKE2 key while pairing).
//!
//! Application messages of any size are split into Noise frames
//! (`[u32 len][ciphertext]`, ciphertext <= 65535 bytes).

use serde::{de::DeserializeOwned, Serialize};
use snow::{Builder, StatelessTransportState};
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

const PATTERN: &str = "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s";
const MAX_NOISE: usize = 65535;
const TAG: usize = 16;
const CHUNK: usize = MAX_NOISE - TAG - 1;
/// Largest application message accepted (images are capped well below this).
pub const MAX_MESSAGE: usize = 64 << 20;

fn bad<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

pub(crate) fn write_raw(w: &mut impl Write, data: &[u8]) -> io::Result<()> {
    if data.len() > MAX_NOISE {
        return Err(bad("frame too large"));
    }
    let mut buf = Vec::with_capacity(data.len() + 4);
    buf.extend_from_slice(&(data.len() as u32).to_be_bytes());
    buf.extend_from_slice(data);
    w.write_all(&buf)
}

pub(crate) fn read_raw(r: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_NOISE {
        return Err(bad("frame too large"));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

/// Run the Noise handshake. `prologue` binds the session to its purpose.
pub fn handshake(stream: &mut TcpStream, psk: &[u8; 32], initiator: bool, prologue: &[u8]) -> io::Result<Arc<StatelessTransportState>> {
    let params = PATTERN.parse().map_err(bad)?;
    let builder = Builder::new(params).prologue(prologue).map_err(bad)?.psk(0, psk).map_err(bad)?;
    let mut hs = if initiator { builder.build_initiator() } else { builder.build_responder() }.map_err(bad)?;
    let mut buf = vec![0u8; MAX_NOISE];
    if initiator {
        let n = hs.write_message(&[], &mut buf).map_err(bad)?;
        write_raw(stream, &buf[..n])?;
        let msg = read_raw(stream)?;
        hs.read_message(&msg, &mut buf).map_err(|_| io::Error::new(io::ErrorKind::PermissionDenied, "handshake rejected (wrong key)"))?;
    } else {
        let msg = read_raw(stream)?;
        hs.read_message(&msg, &mut buf).map_err(|_| io::Error::new(io::ErrorKind::PermissionDenied, "handshake rejected (wrong key)"))?;
        let n = hs.write_message(&[], &mut buf).map_err(bad)?;
        write_raw(stream, &buf[..n])?;
    }
    stream.flush()?;
    Ok(Arc::new(hs.into_stateless_transport_mode().map_err(bad)?))
}

pub struct SecureSender {
    stream: TcpStream,
    state: Arc<StatelessTransportState>,
    nonce: u64,
}

pub struct SecureReceiver {
    stream: TcpStream,
    state: Arc<StatelessTransportState>,
    nonce: u64,
}

pub fn split(stream: TcpStream, state: Arc<StatelessTransportState>) -> io::Result<(SecureSender, SecureReceiver)> {
    let rx = stream.try_clone()?;
    Ok((
        SecureSender { stream, state: state.clone(), nonce: 0 },
        SecureReceiver { stream: rx, state, nonce: 0 },
    ))
}

impl SecureSender {
    pub fn send_bytes(&mut self, msg: &[u8]) -> io::Result<()> {
        let mut chunks: Vec<&[u8]> = msg.chunks(CHUNK).collect();
        if chunks.is_empty() {
            chunks.push(&[]);
        }
        let last = chunks.len() - 1;
        let mut plain = Vec::with_capacity(CHUNK + 1);
        let mut ct = vec![0u8; MAX_NOISE];
        for (i, chunk) in chunks.into_iter().enumerate() {
            plain.clear();
            plain.push(u8::from(i != last)); // 1 = more chunks follow
            plain.extend_from_slice(chunk);
            let n = self.state.write_message(self.nonce, &plain, &mut ct).map_err(bad)?;
            self.nonce += 1;
            write_raw(&mut self.stream, &ct[..n])?;
        }
        self.stream.flush()
    }

    pub fn send<T: Serialize>(&mut self, msg: &T) -> io::Result<()> {
        self.send_bytes(&postcard::to_stdvec(msg).map_err(bad)?)
    }

    pub fn shutdown(&self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

impl SecureReceiver {
    pub fn recv_bytes(&mut self) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut plain = vec![0u8; MAX_NOISE];
        loop {
            let ct = read_raw(&mut self.stream)?;
            let n = self.state.read_message(self.nonce, &ct, &mut plain).map_err(|_| bad("decryption failed"))?;
            self.nonce += 1;
            if n == 0 {
                return Err(bad("empty frame"));
            }
            out.extend_from_slice(&plain[1..n]);
            if out.len() > MAX_MESSAGE {
                return Err(bad("message too large"));
            }
            if plain[0] == 0 {
                return Ok(out);
            }
        }
    }

    pub fn recv<T: DeserializeOwned>(&mut self) -> io::Result<T> {
        postcard::from_bytes(&self.recv_bytes()?).map_err(bad)
    }

    pub fn set_timeout(&self, t: Option<std::time::Duration>) -> io::Result<()> {
        self.stream.set_read_timeout(t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn pair(psk_a: [u8; 32], psk_b: [u8; 32]) -> (io::Result<Arc<StatelessTransportState>>, io::Result<(TcpStream, Arc<StatelessTransportState>)>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let srv = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            handshake(&mut s, &psk_b, false, b"t").map(|st| (s, st))
        });
        let mut c = TcpStream::connect(addr).unwrap();
        let a = handshake(&mut c, &psk_a, true, b"t");
        let b = srv.join().unwrap();
        if let Ok(st) = &a {
            // keep client side around for the roundtrip test
            CLIENT.with(|cell| *cell.borrow_mut() = Some((c, st.clone())));
        }
        (a, b)
    }

    thread_local! {
        static CLIENT: std::cell::RefCell<Option<(TcpStream, Arc<StatelessTransportState>)>> = const { std::cell::RefCell::new(None) };
    }

    #[test]
    fn roundtrip_large_message() {
        let (a, b) = pair([7; 32], [7; 32]);
        a.unwrap();
        let (srv_stream, srv_state) = b.unwrap();
        let (cs, cst) = CLIENT.with(|c| c.borrow_mut().take()).unwrap();
        let (mut tx, _) = split(cs, cst).unwrap();
        let (_, mut rx) = split(srv_stream, srv_state).unwrap();
        let big: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let t = std::thread::spawn(move || {
            tx.send_bytes(&big).unwrap();
            tx.send_bytes(b"").unwrap();
            big
        });
        let got = rx.recv_bytes().unwrap();
        assert_eq!(got, t.join().unwrap());
        assert_eq!(rx.recv_bytes().unwrap(), b"");
    }

    #[test]
    fn wrong_key_is_rejected() {
        let (a, b) = pair([1; 32], [2; 32]);
        assert!(a.is_err() || b.is_err());
    }
}
