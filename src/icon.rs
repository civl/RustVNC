use tray_icon::Icon;

/// A tiny template image: a ring. macOS tints template images for light/dark
/// menu bars, so only the alpha channel matters.
pub fn make_icon() -> Icon {
    const SIZE: u32 = 22;
    let mut rgba = vec![0u8; (SIZE * SIZE * 4) as usize];
    let center = (SIZE as f32 - 1.0) / 2.0;

    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            let distance = (dx * dx + dy * dy).sqrt();

            let alpha = if (4.5..=7.0).contains(&distance) {
                255
            } else if (7.0..8.0).contains(&distance) {
                ((8.0 - distance) * 255.0).clamp(0.0, 255.0) as u8
            } else {
                0
            };

            let index = ((y * SIZE + x) * 4) as usize;
            rgba[index] = 0;
            rgba[index + 1] = 0;
            rgba[index + 2] = 0;
            rgba[index + 3] = alpha;
        }
    }

    Icon::from_rgba(rgba, SIZE, SIZE).expect("22x22 RGBA icon is valid")
}
