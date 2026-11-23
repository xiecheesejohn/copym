//! Length-prefixed postcard frames over a local socket
//! (Unix domain socket in a 0700 directory / Windows named pipe).

use crate::model::{Request, Response};
use interprocess::local_socket::{prelude::*, ListenerOptions, Name, Stream};
use serde::{de::DeserializeOwned, Serialize};
use std::io::{self, Read, Write};

const MAX_FRAME: usize = 256 * 1024 * 1024;

fn socket_name() -> io::Result<Name<'static>> {
    #[cfg(windows)]
    {
        use interprocess::local_socket::GenericNamespaced;
        let user = std::env::var("USERNAME").unwrap_or_else(|_| "user".into());
        let suffix = std::env::var("COPYM_HOME").map(|h| format!("-{:x}", xxhash_rust::xxh3::xxh3_64(h.as_bytes()))).unwrap_or_default();
        format!("copym-{user}{suffix}").to_ns_name::<GenericNamespaced>().map(|n| n.into_owned())
    }
    #[cfg(not(windows))]
    {
        use interprocess::local_socket::GenericFilePath;
        let path = crate::config::runtime_dir().join("copym.sock");
        path.to_fs_name::<GenericFilePath>().map(|n| n.into_owned())
    }
}

pub fn write_frame<W: Write, T: Serialize>(w: &mut W, value: &T) -> io::Result<()> {
    let body = postcard::to_stdvec(value).map_err(io::Error::other)?;
    let mut buf = Vec::with_capacity(body.len() + 4);
    buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
    buf.extend_from_slice(&body);
    w.write_all(&buf)?;
    w.flush()
}

pub fn read_frame<R: Read, T: DeserializeOwned>(r: &mut R) -> io::Result<T> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    postcard::from_bytes(&body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub struct Client {
    stream: Stream,
}

impl Client {
    pub fn connect() -> io::Result<Self> {
        Ok(Self { stream: Stream::connect(socket_name()?)? })
    }

    pub fn request(&mut self, req: &Request) -> io::Result<Response> {
        write_frame(&mut self.stream, req)?;
        read_frame(&mut self.stream)
    }
}

pub fn listen() -> io::Result<interprocess::local_socket::Listener> {
    ListenerOptions::new().name(socket_name()?).try_overwrite(true).create_sync()
}

pub fn serve_connection(mut stream: Stream, handle: impl Fn(Request) -> Response) {
    while let Ok(req) = read_frame::<_, Request>(&mut stream) {
        let quit = matches!(req, Request::Quit);
        let resp = handle(req);
        if write_frame(&mut stream, &resp).is_err() || quit {
            break;
        }
    }
}
