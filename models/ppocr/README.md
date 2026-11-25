# PP-OCRv6 tiny (text detection + recognition)

Embedded in the Linux build of copym (`src/ppocr.rs`) for image text recognition.

| File | Upstream model | SHA-256 |
|---|---|---|
| `PP-OCRv6_det_tiny.onnx` | PP-OCRv6 tiny det | `f42c0fbd294d95eac1a550e131b277dac97462c8025fa4b6c3cec1b7894bd3d5` |
| `PP-OCRv6_rec_tiny.onnx` | PP-OCRv6 tiny rec (character set in the `character` metadata) | `e16e242de5937ad92609223f19bc2aff3727ee40b095f996907c24749bad251b` |

- Models: [PaddleOCR](https://github.com/PaddlePaddle/PaddleOCR) by PaddlePaddle, Apache License 2.0
  (see `LICENSE-APACHE` in the repository root).
- ONNX conversion: [RapidOCR](https://github.com/RapidAI/RapidOCR) v3.10.0, Apache License 2.0,
  from `https://www.modelscope.cn/models/RapidAI/RapidOCR/resolve/v3.10.0/onnx/PP-OCRv6/`.

The files are unmodified.
