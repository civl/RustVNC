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

9. **`src/codec/tight.rs`** — lint only, no behaviour change: the two
   `for i in 0..4` loops were rewritten (`std::array::from_fn` and
   `iter_mut().enumerate()`) because `clippy::needless_range_loop` fails the
   workspace build with `-D warnings`. The vendored crate is part of the
   workspace, so upstream's style has to satisfy our lint gate.

10. **`src/client/connection/tests.rs` and `resize_tests.rs`** — the `event()`
    helper and the one array-index assertion filter out
    `VncEvent::UpdateComplete`, which patch 3 introduced: those tests assert on
    semantic events and would otherwise be off by one marker.

11. **`src/limits.rs`, `src/codec/{zlib_rect,zrle,trle}.rs`** — compressed
    payloads are bounded by the pixels they must expand to
    (`limits::max_compressed_for`) instead of the flat 64 MiB
    `MAX_COMPRESSED`; `MAX_PSEUDO_PAYLOAD` drops to 16 KiB because the wire
    length is a `u16` and the old 64 KiB cap could never fire; new
    `MAX_PSEUDO_BUDGET` bounds total Apple metadata per update.

12. **`src/client/connection.rs`** — `pseudo_bytes` accumulates Apple metadata
    per `FramebufferUpdate` and rejects a batch over `MAX_PSEUDO_BUDGET`.

13. **`src/client/connection/proptests.rs`** (new, `#[cfg(test)]`) — `proptest`
    properties over the read loop, `VncEncoding::from_wire`, `apple::parse` and
    `PixelFormat::try_from`.

14. **`Cargo.toml`** — `proptest` added as a dev-dependency (tests only, not
    linked into the client binary).

## Bounded allocations (2026-11-19)

Two paths let a server decide how much memory the client allocates, which
matters because this decoder reads an untrusted peer:

1. **Apple metadata had a dead cap.** `MAX_PSEUDO_PAYLOAD` was 64 KiB, but the
   length is read as a `u16` (max 65535), so `len > 64 KiB` was unreachable. A
   single `FramebufferUpdate` advertises its rectangle count as a `u16`, and each
   Apple rectangle carries its own length prefix — 65535 × 64 KiB ≈ 4 GiB of
   metadata from one message. The per-message cap is now 16 KiB (the three
   observed bodies are 20, 31 and 45 bytes) and `MAX_PSEUDO_BUDGET` (1 MiB)
   bounds the whole batch.
2. **`zlib`/`ZRLE`/`TRLE` allocated before validating.** Each read a `u32`
   length, accepted anything up to `MAX_COMPRESSED` (64 MiB) and allocated it
   before comparing against the rectangle's own size. A 1×1 rectangle could
   therefore force a 64 MiB allocation. The bound is now derived from the output
   the rectangle must produce (`expected + 12.5 % + 64 B`), which a real zlib
   stream never approaches; `MAX_COMPRESSED` remains as the absolute ceiling.

Regression tests: `limits::tests::{pseudo_payload_cap_can_actually_fire,
compressed_bound_follows_the_expected_pixels}` and
`codec::zlib_rect::tests::oversized_length_is_rejected_before_allocating`.
Property tests in `client::connection::proptests` pin the general contract:
arbitrary server traffic terminates without panicking, `from_wire` is total,
`apple::parse` never panics, and no 16-byte pixel format is rejected by a panic.

## Verifying this file against the code

`git diff --no-index <cargo-registry>/vnc-rs-0.6.1 vendor/vnc-rs` lists exactly
this set (touch points above plus `src/apple.rs`, `src/codec/zlib_rect.rs`,
`src/codec/cursor.rs`, `src/codec/mod.rs`, `src/lib.rs`, `src/limits.rs`,
`src/client/connection/proptests.rs` and the two new test files). Anything else
in the diff is a documentation bug here.

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

`src/apple.rs` now decodes 1109 (`Keyboard`), 1110 (`Machine`), 1104
(`Cursor` geometry from the rectangle) and 1105 (`Display`). 1105's exact field
order is still undocumented; the parser accepts the common
`u16 width, u16 height, f64 BE scale, u32 BE display id` layout and a `u32`
variant, validates every field (plausible size, finite scale in 0.5–8.0), keeps
the raw body, and returns `Unknown` rather than inventing values. The client logs
cursor hotspot/size and display pixel size/scale/id; nothing is applied to the
viewport.

**Standard-RFB limit (2026-11-18):** with a BetterDisplay virtual display
(8960×2880 combined), PointerPos(-232)/1104/1105/1107/1109/1110 requested, remote
pointer motion and a physical display-mode change, the server still emitted only
1107/1109/1110. It did not emit 1104 or 1105. 1105 therefore appears to belong to
Apple's high-performance extension (`SetDisplayConfiguration` 0x1d /
`AppleDisplayLayout` 0x451, encrypted record layer), not the standard RFB path.
The inferred 1105 parser is retained only as a validated diagnostic for a future
capture; it is not used for viewport geometry. 1104's shape is provided by the
standard Cursor pseudo-encoding (-239) when the server sends it.

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

`cargo test -p vnc-rs` — 72 library tests (65 before the bounded-allocation
round), 6 `authentication` integration tests and 2 doc-tests. Notable additions:

- `codec::zlib_rect::tests::decodes_two_rectangles_from_one_shared_stream` locks
  in the shared-stream behaviour.
- `limits::tests` and `codec::zlib_rect::tests` cover the allocation bounds.
- `client::connection::proptests` is the property layer the design asked for:
  hand-written streams pin down what a real Mac sends, these pin down what *any*
  stream may do.

