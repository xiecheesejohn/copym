# Maple Mono NF CN

`maple-mono-nf-cn.ttf.zst` (7 MB, 20.6 MB unpacked) is
`MapleMono-NF-CN-Regular.ttf` from [Maple Mono](https://github.com/subframe7536/maple-font)
v7.9 (release asset `MapleMono-NF-CN.zip`, sha256
af913b6322905348b3f50e4397fedc35b3a880db5effcce7969003051dcd3e94), unchanged,
compressed with `zstd -19`. It is the UI font on every platform: monospace
Latin, CJK and Nerd Font icons. SIL Open Font License 1.1, see
`maple-mono-OFL.txt`.

# copym Symbols

`copym-symbols.ttf` (3 KB) holds only the symbols the popup shows
(⌘ ⇧ ⌥ ⌃ ⏎ ↵ ⌫ ⌦ ⎋ ⇥ ⌄ ← ↑ → ↓ ↗ ▣ · …). System fonts lack some of them
(⏎ ⇧ ⌥ on macOS), which egui rendered as boxes.

It is a subset of Adwaita Sans 50.0 (Regular, wght 400, opsz 14), plus ▣ from
Adwaita Mono 50.0 (scaled to the same units per em), renamed;
SIL Open Font License 1.1, see `copym-symbols-OFL.txt`.

# copym Icons

`copym-icons.ttf` (23 KB) is a subset of the [Lucide](https://lucide.dev) icon
font (lucide-static 1.80.0, ISC license, see `copym-icons-LICENSE.txt`) with the
59 icons listed in `src/icons.rs`. To add one: subset `font/lucide.ttf` again
with its code point (from `font/codepoints.json`) after dropping the GSUB, GPOS
and GDEF tables (pyftsubset cannot read Lucide's ligature table), and add the
constant to `src/icons.rs`.
