//! Built-in OCR for Linux: PP-OCRv6 tiny (PaddleOCR, Apache-2.0) run on
//! tract, pure Rust. The two models (6.3 MB) are embedded in the binary.
//!
//! Pipeline, as in PaddleOCR/RapidOCR: DB text detection -> one box per line
//! fragment -> CTC recognition per box -> boxes grouped into lines.
//! Post-processing assumes horizontal text (screenshots); rotated text is not
//! handled.
//!
//! tract can't infer the detector's shapes symbolically (FPN concat), and runs
//! symbolic widths slowly, so plans are built per concrete input size: the
//! detector input is padded to multiples of 128 px, recognizer widths to
//! multiples of 160 px, and a few recent plans are cached. `trim()` drops
//! everything when OCR goes idle.

use std::sync::{Arc, Mutex};
use tract_onnx::prelude::*;

static DET: &[u8] = include_bytes!("../models/ppocr/PP-OCRv6_det_tiny.onnx");
static REC: &[u8] = include_bytes!("../models/ppocr/PP-OCRv6_rec_tiny.onnx");

/// Larger images are scaled down for detection (recognition always uses the original pixels).
const DET_MAX_SIDE: usize = 1600;
const DET_PAD: usize = 128;
const REC_H: usize = 48;
const REC_MIN_W: usize = 320;
const REC_MAX_W: usize = 3200;
const REC_PAD: usize = 160;

type Plan = Arc<TypedSimplePlan>;
type Rect = (f32, f32, f32, f32); // x, y, w, h

/// A model plus plans compiled for concrete input sizes (most recent first).
struct Net {
    model: InferenceModel,
    plans: Mutex<Vec<((usize, usize), Plan)>>,
    keep: usize,
}

impl Net {
    fn load(bytes: &[u8], keep: usize) -> TractResult<Net> {
        let model = tract_onnx::onnx()
            .with_ignore_output_shapes(true)
            .with_ignore_value_info(true)
            .model_for_read(&mut &bytes[..])?;
        Ok(Net { model, plans: Mutex::new(Vec::new()), keep })
    }

    fn run(&self, h: usize, w: usize, input: Tensor) -> TractResult<TValue> {
        let plan = {
            let mut plans = self.plans.lock().unwrap();
            match plans.iter().position(|p| p.0 == (h, w)) {
                Some(i) => {
                    let p = plans.remove(i);
                    let plan = p.1.clone();
                    plans.insert(0, p);
                    plan
                }
                None => {
                    let mut m = self.model.clone();
                    m.set_input_fact(0, f32::fact([1, 3, h, w]).into())?;
                    let plan = m.into_optimized()?.into_runnable()?;
                    plans.insert(0, ((h, w), plan.clone()));
                    plans.truncate(self.keep);
                    plan
                }
            }
        };
        Ok(plan.run(tvec!(input.into()))?.remove(0))
    }
}

/// The engine. Models are loaded on first use and dropped by `trim()`, so an
/// idle daemon doesn't keep ~30 MB of parsed models around.
pub struct PpOcr {
    models: Mutex<Option<Arc<Models>>>,
}

struct Models {
    det: Net,
    rec: Net,
    chars: Vec<String>,
}

/// An RGBA image (alpha already flattened onto white).
struct Img<'a> {
    w: usize,
    h: usize,
    px: &'a [u8],
}

impl PpOcr {
    /// Loads the models once to make sure they work.
    pub fn new() -> Result<Self, String> {
        Ok(Self { models: Mutex::new(Some(Arc::new(Models::load()?))) })
    }

    /// Drop the models and compiled plans; they are reloaded on demand.
    pub fn trim(&self) {
        if self.models.lock().unwrap().take().is_some() {
            // hand the freed buffers back to the OS, or the daemon's RSS stays at its peak
            #[cfg(target_env = "gnu")]
            unsafe {
                libc::malloc_trim(0);
            }
        }
    }

    /// Recognize text in an RGBA image. Lines separated by '\n', fragments of
    /// one line by ' '.
    pub fn recognize(&self, w: u32, h: u32, rgba: &[u8]) -> Result<String, String> {
        let img = Img { w: w as usize, h: h as usize, px: rgba };
        if img.w < 4 || img.h < 4 {
            return Ok(String::new());
        }
        let models = {
            let mut m = self.models.lock().unwrap();
            if m.is_none() {
                *m = Some(Arc::new(Models::load()?));
            }
            m.clone().expect("just loaded")
        };
        let mut items = Vec::new();
        for b in models.detect(&img).map_err(|e| format!("ppocr det: {e}"))? {
            if let Some((text, conf)) = models.read_box(&img, b).map_err(|e| format!("ppocr rec: {e}"))? {
                if conf >= 0.5 {
                    items.push((b, text));
                }
            }
        }
        Ok(join_lines(items))
    }
}

impl Models {
    fn load() -> Result<Self, String> {
        let e = |e: TractError| format!("ppocr: {e}");
        let proto = tract_onnx::onnx().proto_model_for_read(&mut &REC[..]).map_err(e)?;
        let dict = proto.metadata_props.iter().find(|p| p.key == "character").ok_or("ppocr: no character set")?;
        // CTC blank, the model's characters, then space
        let chars = std::iter::once(String::new())
            .chain(dict.value.split('\n').map(str::to_string))
            .chain(std::iter::once(" ".to_string()))
            .collect();
        Ok(Self { det: Net::load(DET, 3).map_err(e)?, rec: Net::load(REC, 8).map_err(e)?, chars })
    }

    fn detect(&self, img: &Img) -> TractResult<Vec<Rect>> {
        let scale = (DET_MAX_SIDE as f32 / img.w.max(img.h) as f32).min(1.0);
        let (sw, sh) = (((img.w as f32 * scale).round() as usize).max(1), ((img.h as f32 * scale).round() as usize).max(1));
        let (dw, dh) = (sw.div_ceil(DET_PAD) * DET_PAD, sh.div_ceil(DET_PAD) * DET_PAD);
        // pad with white (1.0 after normalisation)
        let input = to_tensor(img, (0.0, 0.0, img.w as f32, img.h as f32), (sw, sh), (dw, dh), [0, 1, 2], 1.0);
        let out = self.det.run(dh, dw, input)?;
        let prob = out.to_plain_array_view::<f32>()?;
        let prob = prob.as_slice().ok_or_else(|| TractError::msg("non-contiguous output"))?;
        Ok(db_boxes(prob, dw, dh)
            .into_iter()
            .filter_map(|(x0, y0, x1, y1)| {
                let (x0, y0) = (x0 / scale, y0 / scale);
                let (x1, y1) = ((x1 / scale).min(img.w as f32), (y1 / scale).min(img.h as f32));
                (x1 - x0 >= 2.0 && y1 - y0 >= 2.0).then_some((x0.max(0.0), y0.max(0.0), x1 - x0.max(0.0), y1 - y0.max(0.0)))
            })
            .collect())
    }

    fn read_box(&self, img: &Img, b: Rect) -> TractResult<Option<(String, f32)>> {
        let ow = ((REC_H as f32 * b.2 / b.3).ceil() as usize).clamp(1, REC_MAX_W);
        let tw = ow.max(REC_MIN_W).div_ceil(REC_PAD) * REC_PAD;
        // BGR, like PaddleOCR's OpenCV input; pad with 0 (mid gray), as PaddleOCR does
        let input = to_tensor(img, b, (ow, REC_H), (tw, REC_H), [2, 1, 0], 0.0);
        let out = self.rec.run(REC_H, tw, input)?;
        let p = out.to_plain_array_view::<f32>()?;
        let (t, n) = (p.shape()[1], p.shape()[2]);
        let p = p.as_slice().ok_or_else(|| TractError::msg("non-contiguous output"))?;
        Ok(ctc_decode(p, t, n, &self.chars))
    }
}

/// Bilinear resample of `src` (a rectangle of `img`) to `size`, placed top-left on a
/// `canvas`, as NCHW floats normalised to (v/255 - 0.5)/0.5 with channel order `order`.
fn to_tensor(img: &Img, src: Rect, size: (usize, usize), canvas: (usize, usize), order: [usize; 3], pad: f32) -> Tensor {
    let ((ow, oh), (cw, ch)) = (size, canvas);
    let mut t = vec![pad; 3 * ch * cw];
    let (sx, sy) = (src.2 / ow as f32, src.3 / oh as f32);
    let (maxx, maxy) = (img.w as f32 - 1.0, img.h as f32 - 1.0);
    for y in 0..oh {
        let fy = (src.1 + (y as f32 + 0.5) * sy - 0.5).clamp(0.0, maxy);
        let (y1, ty) = (fy as usize, fy.fract());
        let y2 = (y1 + 1).min(img.h - 1);
        for x in 0..ow {
            let fx = (src.0 + (x as f32 + 0.5) * sx - 0.5).clamp(0.0, maxx);
            let (x1, tx) = (fx as usize, fx.fract());
            let x2 = (x1 + 1).min(img.w - 1);
            let [a, b, c, d] = [(x1, y1), (x2, y1), (x1, y2), (x2, y2)].map(|(xx, yy)| (yy * img.w + xx) * 4);
            for (k, &s) in order.iter().enumerate() {
                let p = |i: usize| img.px[i + s] as f32;
                let v = (p(a) * (1.0 - tx) + p(b) * tx) * (1.0 - ty) + (p(c) * (1.0 - tx) + p(d) * tx) * ty;
                t[(k * ch + y) * cw + x] = v / 127.5 - 1.0;
            }
        }
    }
    Tensor::from_shape(&[1, 3, ch, cw], &t).expect("shape matches data")
}

/// DB post-processing for horizontal text: threshold the probability map,
/// dilate 2x2, take 4-connected components, keep confident ones and expand
/// their bounding boxes by the DB "unclip" distance (ratio 1.6).
/// Returns (x0, y0, x1, y1) in map pixels.
fn db_boxes(prob: &[f32], w: usize, h: usize) -> Vec<(f32, f32, f32, f32)> {
    let mut bit = vec![false; w * h];
    for y in 0..h {
        for x in 0..w {
            if prob[y * w + x] > 0.3 {
                for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    if x + dx < w && y + dy < h {
                        bit[(y + dy) * w + x + dx] = true;
                    }
                }
            }
        }
    }
    let mut out = Vec::new();
    let mut stack = Vec::new();
    for start in 0..w * h {
        if !bit[start] {
            continue;
        }
        bit[start] = false;
        stack.push(start);
        let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0, 0);
        while let Some(i) = stack.pop() {
            let (x, y) = (i % w, i / w);
            (x0, x1, y0, y1) = (x0.min(x), x1.max(x), y0.min(y), y1.max(y));
            let mut visit = |j: usize| {
                if bit[j] {
                    bit[j] = false;
                    stack.push(j);
                }
            };
            if x > 0 {
                visit(i - 1);
            }
            if x + 1 < w {
                visit(i + 1);
            }
            if y > 0 {
                visit(i - w);
            }
            if y + 1 < h {
                visit(i + w);
            }
        }
        let (bw, bh) = ((x1 - x0 + 1) as f32, (y1 - y0 + 1) as f32);
        if bw.min(bh) < 3.0 {
            continue;
        }
        let score: f32 = (y0..=y1).map(|y| prob[y * w + x0..=y * w + x1].iter().sum::<f32>()).sum::<f32>() / (bw * bh);
        if score < 0.5 {
            continue;
        }
        let d = bw * bh * 1.6 / (2.0 * (bw + bh));
        if (bw + 2.0 * d).min(bh + 2.0 * d) < 5.0 {
            continue;
        }
        out.push((x0 as f32 - d, y0 as f32 - d, x1 as f32 + 1.0 + d, y1 as f32 + 1.0 + d));
    }
    out
}

/// Greedy CTC decoding of `t` steps x `n` classes (index 0 = blank).
/// Returns the text and its mean confidence.
fn ctc_decode(p: &[f32], t: usize, n: usize, chars: &[String]) -> Option<(String, f32)> {
    let (mut text, mut conf, mut count, mut last) = (String::new(), 0.0, 0, 0);
    for row in p.chunks_exact(n).take(t) {
        let (k, v) = row.iter().enumerate().fold((0, f32::MIN), |a, (k, &v)| if v > a.1 { (k, v) } else { a });
        if k != 0 && k != last {
            text.push_str(chars.get(k).map_or("", String::as_str));
            conf += v;
            count += 1;
        }
        last = k;
    }
    (count > 0).then(|| (text, conf / count as f32))
}

/// Group boxes into lines by vertical overlap; left to right within a line.
fn join_lines(mut items: Vec<(Rect, String)>) -> String {
    items.sort_by(|a, b| (a.0 .1 + a.0 .3 / 2.0).total_cmp(&(b.0 .1 + b.0 .3 / 2.0)));
    let mut lines: Vec<(f32, f32, Vec<(Rect, String)>)> = Vec::new();
    for it in items {
        let (top, bottom) = (it.0 .1, it.0 .1 + it.0 .3);
        match lines.last_mut() {
            Some(l) if bottom.min(l.1) - top.max(l.0) > 0.5 * it.0 .3.min(l.1 - l.0) => {
                l.0 = l.0.min(top);
                l.1 = l.1.max(bottom);
                l.2.push(it);
            }
            _ => lines.push((top, bottom, vec![it])),
        }
    }
    lines
        .into_iter()
        .map(|mut l| {
            l.2.sort_by(|a, b| a.0 .0.total_cmp(&b.0 .0));
            l.2.into_iter().map(|x| x.1).collect::<Vec<_>>().join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_boxes_and_lines() {
        // two blobs on one line, one below
        let (w, h) = (40, 30);
        let mut p = vec![0f32; w * h];
        for (x0, x1, y0, y1) in [(2, 12, 2, 8), (20, 35, 3, 8), (2, 20, 18, 25)] {
            for y in y0..y1 {
                for x in x0..x1 {
                    p[y * w + x] = 0.9;
                }
            }
        }
        let b = db_boxes(&p, w, h);
        assert_eq!(b.len(), 3);
        let items = b.iter().zip(["a", "b", "c"]).map(|(r, s)| ((r.0, r.1, r.2 - r.0, r.3 - r.1), s.to_string())).collect();
        assert_eq!(join_lines(items), "a b\nc");
    }

    #[test]
    fn ctc() {
        let chars: Vec<String> = ["", "你", "好"].iter().map(|s| s.to_string()).collect();
        // 你 你 _ 你 好 -> 你你好
        let rows = [[0.1, 0.8, 0.1], [0.1, 0.8, 0.1], [0.9, 0.05, 0.05], [0.1, 0.8, 0.1], [0.1, 0.1, 0.8]];
        let p: Vec<f32> = rows.iter().flatten().copied().collect();
        let (t, c) = ctc_decode(&p, 5, 3, &chars).unwrap();
        assert_eq!(t, "你你好");
        assert!(c > 0.7);
    }

    /// The embedded models load and read the repository's test image.
    #[test]
    fn reads_test_image() {
        let (w, h, mut rgba) = crate::imgutil::decode_png(include_bytes!("../tests/ocr-zh.png")).unwrap();
        crate::imgutil::flatten_on_white(&mut rgba);
        let text = PpOcr::new().unwrap().recognize(w, h, &rgba).unwrap();
        assert!(text.contains("验证码是482913"), "{text}");
        assert!(text.contains("隐私与安全性"), "{text}");
    }
}
