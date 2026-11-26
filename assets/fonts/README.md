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
