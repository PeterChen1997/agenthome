# Third-Party Licenses

AgentHome is licensed under Apache-2.0 (see `LICENSE` and `NOTICE`). It bundles or links the third-party components below. Binary distributions (the `.app` / `.zip`) must ship this file, `NOTICE`, and the font licenses in `crates/app/assets/fonts/`.

## 1. Rust crates

All crates come from crates.io and are linked statically. As of `Cargo.lock` for v0.1.4 the dependency graph (all platforms, 835 packages) uses only permissive or weak-copyleft licenses:

| License (SPDX expression as declared) | Notes |
|---|---|
| MIT, Apache-2.0, `MIT OR Apache-2.0` and variants | The vast majority, including GPUI (`gpui-pre*`, Apache-2.0, Zed Industries) and GPUI Kit (`gpui-kit*`, `gpui-component*`, `gpui-base`, Apache-2.0, Longbridge). |
| BSD-2-Clause, BSD-3-Clause, ISC, Zlib, 0BSD, CC0-1.0, Unlicense (dual with MIT) | Permissive; attribution required in binary distributions. Examples: `zstd`, `zstd-safe`, `zstd-sys` (BSD-3-Clause), `tiny-skia` (BSD-3-Clause), `rav1e` (BSD-2-Clause), `rustls-webpki` (ISC), `slotmap` / `foldhash` / `zlib-rs` (Zlib). |
| Unicode-3.0 | ICU4X crates (`icu_*`, `zerovec`, `yoke`, `tinystr`, ...) and `unicode-ident`. Attribution required. |
| MPL-2.0 | `option-ext` (via `dirs`, linked unmodified on macOS); `cbindgen` (build-time only, not shipped); `dwrote` (Windows only). File-level copyleft: compatible with Apache-2.0 distribution as long as these files are not modified; their source is available on crates.io. |
| `MIT OR Apache-2.0 OR LGPL-2.1-or-later` (`r-efi`), `Apache-2.0 OR GPL-2.0-only` (`self_cell`) | Used under the permissive option (MIT / Apache-2.0). |

No dependency is available only under GPL, LGPL or AGPL.

To regenerate a full per-crate report with license texts:

```bash
cargo install cargo-about --locked
cargo about generate about.hbs > THIRD_PARTY_CRATES.html
```

## 2. Fonts (embedded in the app binary)

| Font | Files | License |
|---|---|---|
| VT323 | `crates/app/assets/fonts/VT323-Regular.ttf` | SIL Open Font License 1.1, full text in `crates/app/assets/fonts/OFL-VT323.txt` |
| Pixelify Sans | `crates/app/assets/fonts/PixelifySans-{Regular,Medium,SemiBold}.ttf` | SIL Open Font License 1.1, full text in `crates/app/assets/fonts/OFL-PixelifySans.txt` |

The fonts are redistributed unmodified. OFL 1.1 requires the copyright notice and license text to accompany every copy, including the bundled binary.

## 3. Icons: Lucide (via `gpui-kit-assets`)

The default component icons of GPUI Kit are Lucide icons, embedded through the `gpui-kit-assets` crate. License text as shipped with that crate:

```text
ISC License

Copyright (c) 2026 Lucide Icons and Contributors

Permission to use, copy, modify, and/or distribute this software for any
purpose with or without fee is hereby granted, provided that the above
copyright notice and this permission notice appear in all copies.

THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.

---

The following Lucide icons are derived from the Feather project:

airplay, alert-circle, alert-octagon, alert-triangle, aperture, arrow-down-circle, arrow-down-left, arrow-down-right, arrow-down, arrow-left-circle, arrow-left, arrow-right-circle, arrow-right, arrow-up-circle, arrow-up-left, arrow-up-right, arrow-up, at-sign, calendar, cast, check, chevron-down, chevron-left, chevron-right, chevron-up, chevrons-down, chevrons-left, chevrons-right, chevrons-up, circle, clipboard, clock, code, columns, command, compass, corner-down-left, corner-down-right, corner-left-down, corner-left-up, corner-right-down, corner-right-up, corner-up-left, corner-up-right, crosshair, database, divide-circle, divide-square, dollar-sign, download, external-link, feather, frown, hash, headphones, help-circle, info, italic, key, layout, life-buoy, link-2, link, loader, lock, log-in, log-out, maximize, meh, minimize, minimize-2, minus-circle, minus-square, minus, monitor, moon, more-horizontal, more-vertical, move, music, navigation-2, navigation, octagon, pause-circle, percent, plus-circle, plus-square, plus, power, radio, rss, search, server, share, shopping-bag, sidebar, smartphone, smile, square, table-2, tablet, target, terminal, trash-2, trash, triangle, tv, type, upload, x-circle, x-octagon, x-square, x, zoom-in, zoom-out

The MIT License (MIT) (for the icons listed above)

Copyright (c) 2013-present Cole Bemis

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## 4. Theme: macOS Classic

`crates/app/assets/themes/macos-classic.json` is the "macOS Classic" theme by Jason Lee (https://github.com/huacnlee/zed-theme-macos-classic). It is kept in the source tree and is not compiled into the app.

```text
MIT License

Copyright (c) 2023 Jason Lee.

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

`crates/app/assets/themes/pixel-habitat.json` is original work of this project (Apache-2.0).

## 5. Artwork

The pixel-art habitat backgrounds, the three resident characters (fox, penguin, cat), the avatars, and the app icon / logo in `crates/app/assets/` and `docs/design/` were created for this project with an AI image-generation model and then post-processed (chroma-keyed and cropped) by the project author. They are distributed under the project license (Apache-2.0). They do not reproduce third-party artwork, and the characters are not official mascots of Anthropic, OpenAI or Google.

## 6. Trademarks

"Claude" and "Claude Code" are trademarks of Anthropic, PBC. "Codex" and "ChatGPT" are trademarks of OpenAI. "Gemini" is a trademark of Google LLC. AgentHome is an independent project and is not affiliated with, endorsed by, or sponsored by any of them.
