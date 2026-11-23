//! Reading / writing the clipboard through `arboard` (Windows, macOS, X11).

use super::{ClipWriter, MonitorOptions};
use crate::imgutil;
use crate::model::Content;
use std::borrow::Cow;
use std::sync::mpsc;

/// Read the current clipboard: text first, then image.
pub fn read(clip: &mut arboard::Clipboard, opts: &MonitorOptions) -> Option<Content> {
    if let Ok(t) = clip.get_text() {
        if !t.trim().is_empty() && t.len() <= opts.max_text_bytes {
            return Some(Content::Text(t));
        }
        return None;
    }
    if opts.store_images {
        if let Ok(img) = clip.get_image() {
            let (w, h) = (img.width as u32, img.height as u32);
            if (w as usize) * (h as usize) * 4 != img.bytes.len() {
                return None;
            }
            let png = imgutil::encode_png(w, h, &img.bytes)?;
            if png.len() <= opts.max_image_bytes {
                return Some(Content::Image { png, width: w, height: h });
            }
        }
    }
    None
}

type Job = (Content, mpsc::Sender<Result<(), String>>);

/// Owns one `arboard::Clipboard` on a dedicated thread. Keeping it alive matters
/// on X11, where the owning process must keep serving the selection.
pub struct ArboardWriter {
    tx: mpsc::Sender<Job>,
}

impl ArboardWriter {
    pub fn spawn() -> Result<Self, String> {
        let (tx, rx) = mpsc::channel::<Job>();
        let (ready_tx, ready_rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("clip-writer".into())
            .spawn(move || {
                let mut clip = match arboard::Clipboard::new() {
                    Ok(c) => {
                        let _ = ready_tx.send(Ok(()));
                        c
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e.to_string()));
                        return;
                    }
                };
                for (content, reply) in rx {
                    let res = match content {
                        Content::Text(t) => clip.set_text(t).map_err(|e| e.to_string()),
                        Content::Image { png, .. } => match imgutil::decode_png(&png) {
                            Some((w, h, rgba)) => clip
                                .set_image(arboard::ImageData {
                                    width: w as usize,
                                    height: h as usize,
                                    bytes: Cow::Owned(rgba),
                                })
                                .map_err(|e| e.to_string()),
                            None => Err("corrupt image".into()),
                        },
                    };
                    let _ = reply.send(res);
                }
            })
            .map_err(|e| e.to_string())?;
        ready_rx.recv().map_err(|e| e.to_string())??;
        Ok(Self { tx })
    }
}

impl ClipWriter for ArboardWriter {
    fn set(&mut self, content: &Content) -> Result<(), String> {
        let (rtx, rrx) = mpsc::channel();
        self.tx.send((content.clone(), rtx)).map_err(|e| e.to_string())?;
        rrx.recv().map_err(|e| e.to_string())?
    }
}
