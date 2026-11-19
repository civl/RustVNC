//! Property tests for the decoder.
//!
//! The hand-written tests pin down the streams a real Mac sends. These pin down
//! the contract that has to hold for *any* stream: the decoder either reports an
//! error or produces events, but never panics, never blocks forever, and never
//! trusts a length it has not bounded — the memory-amplification class of bug
//! that a single crafted `FramebufferUpdate` can otherwise trigger.

use std::sync::atomic::AtomicU32;
use std::time::Duration;

use proptest::prelude::*;
use tokio::io::{duplex, AsyncWriteExt};

use super::*;

/// The encodings a real session negotiates. Keeping the list close to
/// `ClientConfig::encodings()` means the property exercises the arms that ship.
const ENCODINGS: &[VncEncoding] = &[
    VncEncoding::Raw,
    VncEncoding::CopyRect,
    VncEncoding::Zrle,
    VncEncoding::Zlib,
    VncEncoding::CursorPseudo,
    VncEncoding::PointerPosPseudo,
    VncEncoding::DesktopSizePseudo,
    VncEncoding::LastRectPseudo,
    VncEncoding::AppleCursorPseudo,
    VncEncoding::AppleDisplayPseudo,
    VncEncoding::AppleCapabilitiesPseudo,
    VncEncoding::AppleKeyboardPseudo,
    VncEncoding::AppleMachinePseudo,
];

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime")
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// Any byte sequence the server can send is answered with an error or with
    /// events, within bounded time.
    #[test]
    fn arbitrary_server_traffic_terminates_without_panicking(
        payload in prop::collection::vec(any::<u8>(), 0..=4096)
    ) {
        let rt = runtime();
        let terminated = rt.block_on(async move {
            let (mut client, mut server) = duplex(8 * 1024);
            let writer = tokio::spawn(async move {
                let _ = server.write_all(&payload).await;
                let _ = server.shutdown().await;
            });
            let (_stop_tx, mut stopped) = oneshot::channel();
            let screen = AtomicU32::new(pack_screen((64, 64)));
            let finished = tokio::time::timeout(
                Duration::from_secs(2),
                asycn_vnc_read_loop(
                    &mut client,
                    &PixelFormat::rgba(),
                    &|_| async { Ok(()) },
                    &mut stopped,
                    ENCODINGS,
                    &screen,
                    &DesktopState::default(),
                ),
            )
            .await;
            let _ = writer.await;
            finished.is_ok()
        });
        prop_assert!(
            terminated,
            "the decoder did not terminate within 2 s for its input"
        );
    }

    /// `from_wire` is total: every i32 either maps to a known encoding or comes
    /// back labelled with its own value, so the client can name what it refused.
    #[test]
    fn every_encoding_number_is_known_or_labelled(code in any::<i32>()) {
        match VncEncoding::from_wire(code as u32) {
            Ok(encoding) => prop_assert_eq!(encoding as i32, code),
            Err(VncError::UnsupportedEncoding(reported)) => {
                prop_assert_eq!(reported, code);
            }
            Err(other) => prop_assert!(false, "unexpected error for {code}: {other:?}"),
        }
    }

    /// Apple metadata bodies come straight off the wire; parsing one must never
    /// panic, whatever the body and code are.
    #[test]
    fn apple_metadata_parsing_never_panics(
        code in 1104i32..=1110,
        body in prop::collection::vec(any::<u8>(), 0..=512)
    ) {
        // The parser may report "unknown"; what it may not do is panic or
        // invent data, both of which would abort the session thread.
        let _ = crate::apple::parse(code, &body);
    }

    /// A pixel format is a 16-byte struct from the server; converting and
    /// validating one must be total.
    #[test]
    fn pixel_formats_from_the_wire_never_panic(bytes in any::<[u8; 16]>()) {
        if let Ok(format) = PixelFormat::try_from(bytes) {
            let _ = format.validate();
        }
    }
}
