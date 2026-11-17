# vendored `vnc-rs` — origin and patches

This directory is a copy of **`vnc-rs 0.6.1`** (crates.io, upstream
<https://github.com/HsuJv/vnc-rs>, checksum
`0a6306bed7ebbdcc80c44c838a1c0496b589e496bb3b3750a1ed97d4eb26f4cf`), MIT / Apache-2.0.
Both licence files are kept verbatim.

It is wired in through `[patch.crates-io]` in the workspace `Cargo.toml`. It is
vendored rather than forked upstream because everything below lives inside
`pub(crate)` items and a `#[repr(i32)]` closed enum, so none of it can be added
from a dependent crate.

**Resync policy**: on every upstream release, re-copy the crate and re-apply the
patch list below; the patches are intentionally small and localised.

---

## Why we cannot stay on the published crate

| Gap | Effect before the patch |
|---|---|
| `VncEncoding` had no `Zlib(6)` | zlib rectangles were impossible to negotiate; macOS supports zlib and it compresses flat/text areas better than raw |
| No Apple pseudo-encodings | `from_wire` returned `InvalidImageData`, which aborted the session and (with auto-reconnect on) produced a reconnect loop every second |
| `from_wire` error was unlabelled | the user saw "Image data cannot be decoded correctly" with no encoding number |
| `desktop::UpdateBatch` treated Apple rects as pixel data | a length-prefixed metadata body would have been read as pixels, desynchronising the stream |

Upstream `HsuJv/vnc-rs` @ `main` (checked 2026-11-12) has the identical enum, so
this is not something that can be fixed by waiting.

## Patch list

1. **`src/config.rs`**
   - `VncEncoding` gains `Zlib = 6` and `AppleCursorPseudo = 1104`,
     `AppleDisplayPseudo = 1105`, `AppleCapabilitiesPseudo = 1107`,
     `AppleKeyboardPseudo = 1109`, `AppleMachinePseudo = 1110`.
   - `from_wire` maps them and returns the new `VncError::UnsupportedEncoding(i32)`
     for anything else, so the caller can report *which* code was rejected.
   - `is_apple_pseudo()` / `is_pseudo()` helpers.

2. **`src/error.rs`** — new `UnsupportedEncoding(i32)` variant.

3. **`src/event.rs`** — new `VncEvent::AppleEncoding { code, rect, body }`: the
   engine forwards the raw body and refuses to guess at its layout.

4. **`src/codec/zlib_rect.rs`** (new) — decoder for encoding 6: one `Decompress`
   per connection (the zlib stream is shared by every rectangle), `u32` length
   per rectangle, exact `w*h*bpp` output, capped by `MAX_COMPRESSED`, and it
   fails instead of spinning when the stream stalls.

5. **`src/codec/mod.rs`** — export `ZlibDecoder`.

6. **`src/client/connection.rs`**
   - instantiate `ZlibDecoder` and add the `VncEncoding::Zlib` arm;
   - Apple pseudo-encodings are read as `u16` length + that many bytes (cap
     `MAX_PSEUDO_PAYLOAD`), emitted as `VncEvent::AppleEncoding`, and excluded
     from the framebuffer bounds check and from "this batch changed pixels";
   - unknown encodings now yield `UnsupportedEncoding(code)` instead of a
     generic decode failure.

7. **`src/limits.rs`** — `MAX_PSEUDO_PAYLOAD = 64 KiB`.

8. **`Cargo.toml`** — removed the `[profile.release]` block (non-root workspace
   members cannot set profiles); `.cargo_vcs_info.json`, `Cargo.lock` and
   `.github/` dropped from the vendored copy. `package.homepage` dropped as well:
   it duplicates `package.repository`, which cargo warns about
   (`cargo::redundant_homepage`). `Cargo.toml.orig` is left untouched as the
   upstream reference copy.

## Apple body layout — verified on a real Mac (2026-11-17)

Captured from a Mac mini M4 (`RFB 003.889`, screen sharing) by requesting the
Apple pseudo-encodings and logging `VncEvent::AppleEncoding`:

| Encoding | Rect | Body | Captured bytes |
|---|---|---|---|
| 1107 capabilities | 0×0 | 20 B | `00 01 00 04 10 08 fd 00 10 08 fd 01 10 08 fd 02 10 08 fd 03` |
| 1109 keyboard | 0×0 | 31 B | `00 01 00 00 00 00 00 17` + `com.apple.keylayout.ABC` |
| 1110 machine | 0×0 | 45 B | `… 00 09` + `Mac16,10\0` + `unknown\0` + `unknown\0` |

Conclusions:

- The `u16` length prefix is real for every Apple pseudo-encoding
  (the 1107 prefix is `00 14` = 20), so the skip-based decoder stays.
- Each of 1107/1109/1110 arrives exactly **once**, at session start, with a
  0×0 rectangle. 1104 (cursor) is not sent while the pointer does not move, and
  1105 (display geometry) was not sent at all on this single-display Mac.
- After the Apple rectangles the session continued normally (256 rects, 51 fps in
  5 s), which is the empirical proof that no desync happens.

`src/apple.rs` now decodes 1109 (`Keyboard`) and 1110 (`Machine`) and keeps 1105
as raw bytes, because its field layout was not observed. The parser matches the
*shape* of the payload (first printable ASCII run) rather than fixed offsets, and
returns `Unknown` instead of inventing a string. The captured byte sequences are
embedded in that module's tests.

## Also fixed here: the 5K framebuffer cap

`MAX_PIXELS` was 8,294,400 — exactly 3840×2160. A Mac mini M4 on a 5120×2880
display reports 14,745,600 pixels, and `limits::dimensions()` rejected it *during
the handshake*, which the client surfaced as `Image data cannot be decoded
correctly`. Every 5K/6K Mac was effectively unreachable.

- `MAX_PIXELS` is now 8192×4320 (covers 6K 6016×3384 with headroom). Worst-case
  32 bpp frame is ~141 MB, and only when a server actually sends such a rectangle.
- A too-large framebuffer now returns `VncError::FramebufferTooLarge { width,
  height, pixels, max_pixels }`, and the client turns it into "远端分辨率
  5120×2880 超过上限…请在 Mac 上降低分辨率" instead of the decoder-sounding error.
- Regression tests: `limits::tests::{accepts_5k_retina_framebuffers,
  rejects_oversized_with_a_distinct_error, rejects_zero_dimensions_as_corruption}`.

## Cursor updates at 16 bpp used to kill the session

`codec::cursor` required 32 bpp and returned `WrongPixelFormat` otherwise. The
client asks for `CursorPseudo` (-239) to get a remote pointer shape, so on a
16 bpp ("极速" quality) session macOS would answer with a cursor rectangle as
soon as the pointer moved, and the session died with "Unknown pixel format"
surfaced as `[INTERNAL]`.

The body is perfectly well defined at any bpp, so the decoder now consumes it
and reports an empty shape instead of failing. Two regression tests cover it
(`sixteen_bpp_cursor_is_skipped_not_fatal`,
`thirty_two_bpp_cursor_still_decodes`), and the client no longer advertises
`CursorPseudo` at 16 bpp, because the UI draws the local pointer anyway and
those updates would only cost bandwidth.
## `VncEvent::UpdateComplete`

The decoder reads a `FramebufferUpdate` rectangle by rectangle and emitted one
event per rectangle with no marker for "this batch is finished". A client cannot
then tell whether the update it asked for is still arriving, so the only options
are to guess with a timer or to send the next request blindly — the latter
queues overlapping updates on a slow link and produces visible tearing.

The engine now emits `VncEvent::UpdateComplete` after the last rectangle of every
batch (including empty batches). The client uses it to keep exactly one request
outstanding (see `tests/frame_pacing.rs`, which fails with up to five overlapping
requests when the guard is removed).

Upstream resize tests filter the marker out: they assert on semantic events only.
## Diagnostics kept in the engine

`RUST_LOG=vnc=trace` now prints the server's framebuffer size at `ServerInit` and
one line per rectangle (geometry + encoding). That is what made the 5K bug a
five-minute diagnosis instead of a decoder hunt, so it stays.

## Tests

`cargo test -p vnc-rs` — the upstream suite (60 tests) plus
`codec::zlib_rect::tests::decodes_two_rectangles_from_one_shared_stream`, which
locks in the shared-stream behaviour.
