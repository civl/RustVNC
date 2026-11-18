use std::io::BufWriter;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{info, warn};
use vnc::{
    ClientKeyEvent, ClientMouseEvent, PixelFormat, Rect, Screen, VncConnector, VncEncoding,
    VncEvent, X11Event,
};

#[derive(Debug, Parser)]
#[command(
    name = "rfb-probe",
    version,
    about = "RustVNC M0 probe: RFB handshake/auth/encodings/input against macOS Screen Sharing"
)]
struct Cli {
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    #[arg(long, default_value_t = 5900)]
    port: u16,
    /// macOS account short name for Apple ARD auth (type 30)
    #[arg(long, env = "VNC_USERNAME")]
    user: Option<String>,
    /// macOS account password or VNC password (prefer VNC_PASSWORD env)
    #[arg(long, env = "VNC_PASSWORD", hide_env_values = true)]
    password: Option<String>,
    #[arg(long, value_enum, default_value_t = AuthMode::Auto)]
    auth: AuthMode,
    /// Ask the server to switch to this resolution, e.g. `1920x1080`
    /// (RFB ExtendedDesktopSize; most macOS builds do not implement it).
    #[arg(long)]
    resize: Option<String>,

    /// Also request Apple's private pseudo-encodings (1104/1105/1107/1109/1110)
    /// so their bodies can be captured and decoded.
    #[arg(long)]
    apple: bool,
    #[arg(long, default_value_t = 32)]
    bpp: u8,
    #[arg(long, value_enum, default_value_t = EncodingMode::Zrle)]
    encoding: EncodingMode,
    #[arg(long, default_value_t = 5)]
    seconds: u64,
    #[arg(long)]
    save: Option<PathBuf>,
    /// Show the framebuffer in a small minifb window (requires a GUI session)
    #[arg(long)]
    window: bool,
    /// Send a pointer move/click and an "A" key once (types into the focused app!)
    #[arg(long)]
    input_test: bool,
    /// Move the remote pointer for the first 3 seconds; macOS emits Apple 1104
    /// when the cursor shape changes (not merely when it moves).
    #[arg(long)]
    wiggle: bool,
    /// Optional remote coordinate `x,y` for --wiggle. The script points this at
    /// the terminal's text area so the arrow ⇄ I-beam transition triggers 1104.
    #[arg(long)]
    hover: Option<String>,
    /// Print RFB version and security types, then exit
    #[arg(long)]
    inspect: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum AuthMode {
    Auto,
    Vnc,
    Ard,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum EncodingMode {
    Zrle,
    /// Standard zlib rectangles (RFB encoding 6).
    Zlib,
    Tight,
    Raw,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    if let Err(err) = run(cli).await {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    let addr = format!("{}:{}", cli.host, cli.port);
    if cli.inspect {
        return inspect(&addr).await;
    }
    if !matches!(cli.bpp, 16 | 32) {
        bail!("--bpp must be 16 or 32");
    }

    let password = cli.password.clone().unwrap_or_default();
    let user = cli.user.clone();
    if cli.auth == AuthMode::Vnc && password.is_empty() {
        bail!("--auth vnc requires --password or VNC_PASSWORD");
    }
    if cli.auth == AuthMode::Ard && user.is_none() {
        bail!("--auth ard requires --user");
    }
    if cli.auth == AuthMode::Auto && user.is_none() && password.is_empty() {
        warn!("no credentials supplied; the server will probably reject authentication");
    }

    let tcp = TcpStream::connect(&addr)
        .await
        .with_context(|| format!("connect to {addr}"))?;
    let pixel_format = pixel_format(cli.bpp);
    let mut connector = VncConnector::new(tcp)
        .allow_shared(true)
        .set_pixel_format(pixel_format)
        .add_encoding(VncEncoding::Raw);

    for encoding in encodings(cli.encoding) {
        connector = connector.add_encoding(encoding);
    }

    if cli.resize.is_some() {
        connector = connector.add_encoding(VncEncoding::ExtendedDesktopSizePseudo);
    }

    if cli.apple {
        for encoding in [
            VncEncoding::PointerPosPseudo,
            VncEncoding::AppleCursorPseudo,
            VncEncoding::AppleDisplayPseudo,
            VncEncoding::AppleCapabilitiesPseudo,
            VncEncoding::AppleKeyboardPseudo,
            VncEncoding::AppleMachinePseudo,
        ] {
            connector = connector.add_encoding(encoding);
        }
    }
    if cli.auth != AuthMode::Vnc
        && let Some(user) = user.as_ref()
    {
        connector = connector.set_ard_credentials(user.clone(), password.clone());
    }
    if cli.auth != AuthMode::Ard && !password.is_empty() {
        let password = password.clone();
        connector = connector.set_auth_method(async move { Ok(password) });
    }

    let client = connector
        .build()
        .context("build VNC connector")?
        .try_start()
        .await
        .context("RFB handshake/authentication")?
        .finish()
        .context("finish VNC client")?;

    println!("CONNECTED");
    println!("server_name: {}", client.server_name());
    println!("bpp: {}", cli.bpp);
    println!("encoding: {:?}", cli.encoding);

    if let Some(spec) = cli.resize.as_deref() {
        let (width, height) = spec
            .split_once('x')
            .and_then(|(w, h)| Some((w.parse::<u16>().ok()?, h.parse::<u16>().ok()?)))
            .with_context(|| format!("--resize expects WIDTHxHEIGHT, got {spec}"))?;
        match tokio::time::timeout(Duration::from_secs(5), client.resize_desktop(width, height))
            .await
        {
            Ok(Ok(layout)) => println!("resize: applied {layout:?}"),
            Ok(Err(err)) => println!("resize: refused ({err})"),
            Err(_) => println!("resize: no answer within 5s"),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    let mut framebuffer = FrameBuffer::new(cli.bpp);
    let mut window: Option<minifb::Window> = None;
    let mut rects = 0u64;
    let mut bytes = 0u64;
    let mut input_sent = false;
    let mut next_wiggle = Instant::now();
    let mut wiggle_index = 0i32;
    let hover = cli.hover.as_deref().and_then(|value| {
        let (x, y) = value.split_once(',')?;
        Some((x.trim().parse::<u16>().ok()?, y.trim().parse::<u16>().ok()?))
    });
    let start = Instant::now();
    let deadline = start + Duration::from_secs(cli.seconds);
    let mut last_refresh = Instant::now() - Duration::from_millis(50);
    let mut last_present = Instant::now() - Duration::from_millis(50);
    let mut requested_full = false;

    while Instant::now() < deadline {
        // Wait for an event with a short timeout instead of polling: `poll_event`
        // is a non-blocking `try_recv`, so looping on it would burn a core and
        // distort the very frame-rate numbers this tool exists to produce.
        match tokio::time::timeout(Duration::from_millis(10), client.recv_event()).await {
            Ok(Ok(event)) => {
                if let Some(event) =
                    handle_event(event, &mut framebuffer, &mut window, &mut rects, &mut bytes)?
                {
                    warn!("unhandled event: {event:?}");
                }
            }
            Ok(Err(err)) => {
                eprintln!("vnc error: {err}");
                break;
            }
            Err(_) => {}
        }

        if let Some(window) = window.as_mut()
            && !window.is_open()
        {
            break;
        }

        if cli.input_test && !input_sent && start.elapsed() >= Duration::from_millis(500) {
            send_input_test(&client, &framebuffer).await?;
            input_sent = true;
            println!("input_test_sent");
        }

        if cli.wiggle && start.elapsed() < Duration::from_secs(3) && next_wiggle <= Instant::now() {
            let width = framebuffer.width.max(200) as u16;
            let height = framebuffer.height.max(200) as u16;
            let (x, y) = if wiggle_index % 2 == 0 {
                hover.unwrap_or((width / 2, height / 2))
            } else {
                (10, 10)
            };
            wiggle_index = wiggle_index.wrapping_add(1);
            client
                .input(X11Event::PointerEvent(ClientMouseEvent {
                    position_x: x.min(width.saturating_sub(1)),
                    position_y: y.min(height.saturating_sub(1)),
                    bottons: 0,
                }))
                .await?;
            next_wiggle = Instant::now() + Duration::from_millis(120);
        }

        if last_refresh.elapsed() >= Duration::from_millis(33) {
            // The first request must be non-incremental: macOS answers an
            // incremental request with only the regions it considers changed, so
            // a `--save` captured before anything moves contains black where no
            // pixels ever arrived.
            let event = if requested_full {
                X11Event::Refresh
            } else {
                requested_full = true;
                X11Event::FullRefresh
            };
            let _ = client.input(event).await;
            last_refresh = Instant::now();
        }

        if cli.window && last_present.elapsed() >= Duration::from_millis(33) {
            if framebuffer.width > 0 && framebuffer.height > 0 {
                let buffer = framebuffer.to_rgba_u32();
                if window.is_none() {
                    let options = minifb::WindowOptions {
                        resize: true,
                        ..Default::default()
                    };
                    window = Some(
                        minifb::Window::new(
                            "RustVNC M0",
                            framebuffer.width,
                            framebuffer.height,
                            options,
                        )
                        .context("create minifb window")?,
                    );
                }
                if let Some(window) = window.as_mut() {
                    window
                        .update_with_buffer(&buffer, framebuffer.width, framebuffer.height)
                        .context("update minifb window")?;
                }
            }
            last_present = Instant::now();
        }

        tokio::time::sleep(Duration::from_millis(3)).await;
    }

    let elapsed = start.elapsed();
    println!(
        "STATS resolution={}x{} bpp={} rects={} bytes={} elapsed={:.2}s fps={:.2}",
        framebuffer.width,
        framebuffer.height,
        framebuffer.bpp,
        rects,
        bytes,
        elapsed.as_secs_f64(),
        rects as f64 / elapsed.as_secs_f64().max(0.001)
    );

    if let Some(path) = cli.save.as_ref() {
        framebuffer.save_png(path)?;
        println!("saved: {}", path.display());
    }

    let _ = client.close().await;
    Ok(())
}

fn handle_event(
    event: VncEvent,
    framebuffer: &mut FrameBuffer,
    window: &mut Option<minifb::Window>,
    rects: &mut u64,
    bytes: &mut u64,
) -> Result<Option<VncEvent>> {
    match event {
        VncEvent::SetResolution(Screen { width, height }) => {
            framebuffer.resize(width as usize, height as usize);
            *window = None;
            println!("resolution: {width}x{height}");
        }
        VncEvent::SetPixelFormat(format) => {
            println!(
                "pixel_format: bpp={} depth={} shifts=({},{},{}) max=({},{},{})",
                format.bits_per_pixel,
                format.depth,
                format.red_shift,
                format.green_shift,
                format.blue_shift,
                format.red_max,
                format.green_max,
                format.blue_max
            );
        }
        VncEvent::AppleEncoding { code, rect, body } => {
            let shown: Vec<String> = body.iter().take(48).map(|b| format!("{b:02x}")).collect();
            println!(
                "apple encoding {code}: rect {}x{}+{}+{} body={} bytes [{}]{}",
                rect.width,
                rect.height,
                rect.x,
                rect.y,
                body.len(),
                shown.join(" "),
                if body.len() > 48 { " ..." } else { "" }
            );
        }
        VncEvent::RawImage(rect, data) => {
            framebuffer.blit(rect, &data)?;
            *rects += 1;
            *bytes += data.len() as u64;
        }
        VncEvent::Copy(dst, src) => {
            framebuffer.copy_rect(dst, src)?;
            *rects += 1;
        }
        VncEvent::SetCursor(rect, _) => {
            if rect.width > 0 {
                info!("cursor {}x{}", rect.width, rect.height);
            }
        }
        VncEvent::Bell => info!("bell"),
        VncEvent::Text(text) => println!("clipboard: {} bytes", text.len()),
        VncEvent::Error(err) => warn!("server error event: {err}"),
        other => return Ok(Some(other)),
    }
    Ok(None)
}

async fn send_input_test(client: &vnc::VncClient, framebuffer: &FrameBuffer) -> Result<()> {
    let x = (framebuffer.width / 2) as u16;
    let y = (framebuffer.height / 2) as u16;
    client
        .input(X11Event::PointerEvent(ClientMouseEvent {
            position_x: x,
            position_y: y,
            bottons: 0,
        }))
        .await?;
    client
        .input(X11Event::PointerEvent(ClientMouseEvent {
            position_x: x,
            position_y: y,
            bottons: 1,
        }))
        .await?;
    client
        .input(X11Event::PointerEvent(ClientMouseEvent {
            position_x: x,
            position_y: y,
            bottons: 0,
        }))
        .await?;
    let key = 0x41; // X11 keysym for 'A'
    client
        .input(X11Event::KeyEvent(ClientKeyEvent {
            keycode: key,
            down: true,
        }))
        .await?;
    client
        .input(X11Event::KeyEvent(ClientKeyEvent {
            keycode: key,
            down: false,
        }))
        .await?;
    Ok(())
}

fn pixel_format(bpp: u8) -> PixelFormat {
    let mut format = PixelFormat::default();
    if bpp == 16 {
        format.bits_per_pixel = 16;
        format.depth = 16;
        format.red_max = 31;
        format.green_max = 63;
        format.blue_max = 31;
        format.red_shift = 11;
        format.green_shift = 5;
        format.blue_shift = 0;
    }
    format
}

fn encodings(mode: EncodingMode) -> Vec<VncEncoding> {
    match mode {
        EncodingMode::Zrle => vec![
            VncEncoding::Zrle,
            VncEncoding::CopyRect,
            VncEncoding::CursorPseudo,
            VncEncoding::DesktopSizePseudo,
        ],
        EncodingMode::Zlib => vec![
            VncEncoding::Zlib,
            VncEncoding::CopyRect,
            VncEncoding::CursorPseudo,
            VncEncoding::DesktopSizePseudo,
        ],
        EncodingMode::Tight => vec![
            VncEncoding::Tight,
            VncEncoding::CopyRect,
            VncEncoding::CursorPseudo,
            VncEncoding::DesktopSizePseudo,
        ],
        EncodingMode::Raw => vec![
            VncEncoding::CopyRect,
            VncEncoding::CursorPseudo,
            VncEncoding::DesktopSizePseudo,
        ],
    }
}

async fn inspect(addr: &str) -> Result<()> {
    let mut stream = TcpStream::connect(addr)
        .await
        .with_context(|| format!("connect to {addr}"))?;
    let mut version = [0u8; 12];
    stream.read_exact(&mut version).await?;
    let version = String::from_utf8_lossy(&version).trim().to_string();

    stream.write_all(b"RFB 003.008\n").await?;
    let mut count = [0u8; 1];
    stream.read_exact(&mut count).await?;
    if count[0] == 0 {
        let mut len = [0u8; 4];
        stream.read_exact(&mut len).await?;
        let mut reason = vec![0u8; u32::from_be_bytes(len) as usize];
        stream.read_exact(&mut reason).await?;
        bail!("server refused: {}", String::from_utf8_lossy(&reason));
    }

    let mut types = vec![0u8; count[0] as usize];
    stream.read_exact(&mut types).await?;

    println!("server_version: {version}");
    println!("security_types: {types:?}");
    for security_type in types {
        println!("  {security_type}: {}", security_type_name(security_type));
    }
    Ok(())
}

fn security_type_name(value: u8) -> &'static str {
    match value {
        1 => "None",
        2 => "VNC authentication (DES)",
        18 => "TLS",
        19 => "VeNCrypt",
        30 => "Apple ARD (type 30)",
        33 => "Apple legacy SRP/DH",
        35 => "Apple legacy SRP/DH",
        36 => "Apple SRP",
        _ => "unknown/Apple private",
    }
}

struct FrameBuffer {
    width: usize,
    height: usize,
    bpp: u8,
    data: Vec<u8>,
}

impl FrameBuffer {
    fn new(bpp: u8) -> Self {
        Self {
            width: 0,
            height: 0,
            bpp,
            data: Vec::new(),
        }
    }

    fn stride(&self) -> usize {
        self.width * self.bpp as usize / 8
    }

    fn resize(&mut self, width: usize, height: usize) {
        if self.width != width || self.height != height {
            self.width = width;
            self.height = height;
            self.data.resize(width * height * self.bpp as usize / 8, 0);
        }
    }

    fn blit(&mut self, rect: Rect, source: &[u8]) -> Result<()> {
        if rect.width == 0 || rect.height == 0 {
            return Ok(());
        }
        let bpp = self.bpp as usize / 8;
        let required_width = rect.x as usize + rect.width as usize;
        let required_height = rect.y as usize + rect.height as usize;
        if self.width < required_width || self.height < required_height {
            self.resize(
                required_width.max(self.width),
                required_height.max(self.height),
            );
        }

        let row_bytes = rect.width as usize * bpp;
        if source.len() < row_bytes * rect.height as usize {
            bail!(
                "short raw rect: {} bytes, expected {}",
                source.len(),
                row_bytes * rect.height as usize
            );
        }
        let stride = self.stride();
        for row in 0..rect.height as usize {
            let dst = (rect.y as usize + row) * stride + rect.x as usize * bpp;
            let src = row * row_bytes;
            self.data[dst..dst + row_bytes].copy_from_slice(&source[src..src + row_bytes]);
        }
        Ok(())
    }

    fn copy_rect(&mut self, dst: Rect, src: Rect) -> Result<()> {
        let bpp = self.bpp as usize / 8;
        let width = src.width.min(dst.width) as usize;
        let height = src.height.min(dst.height) as usize;
        if width == 0 || height == 0 {
            return Ok(());
        }
        if src.x as usize + width > self.width
            || src.y as usize + height > self.height
            || dst.x as usize + width > self.width
            || dst.y as usize + height > self.height
        {
            bail!("CopyRect out of bounds");
        }
        let stride = self.stride();
        let mut temp = vec![0u8; width * height * bpp];
        for row in 0..height {
            let src_offset = (src.y as usize + row) * stride + src.x as usize * bpp;
            temp[row * width * bpp..(row + 1) * width * bpp]
                .copy_from_slice(&self.data[src_offset..src_offset + width * bpp]);
        }
        for row in 0..height {
            let dst_offset = (dst.y as usize + row) * stride + dst.x as usize * bpp;
            self.data[dst_offset..dst_offset + width * bpp]
                .copy_from_slice(&temp[row * width * bpp..(row + 1) * width * bpp]);
        }
        Ok(())
    }

    fn to_rgba_u32(&self) -> Vec<u32> {
        let pixels = self.width * self.height;
        let mut out = vec![0u32; pixels];
        match self.bpp {
            32 => {
                for (index, pixel) in self.data.as_chunks::<4>().0.iter().enumerate() {
                    let b = pixel[0] as u32;
                    let g = pixel[1] as u32;
                    let r = pixel[2] as u32;
                    out[index] = (r << 16) | (g << 8) | b;
                }
            }
            16 => {
                for (index, pixel) in self.data.as_chunks::<2>().0.iter().enumerate() {
                    let value = u16::from_le_bytes([pixel[0], pixel[1]]);
                    let r = ((value >> 11) & 0x1f) as u32 * 255 / 31;
                    let g = ((value >> 5) & 0x3f) as u32 * 255 / 63;
                    let b = (value & 0x1f) as u32 * 255 / 31;
                    out[index] = (r << 16) | (g << 8) | b;
                }
            }
            _ => {}
        }
        out
    }

    fn save_png(&self, path: &std::path::Path) -> Result<()> {
        if self.width == 0 || self.height == 0 {
            bail!("no framebuffer captured");
        }
        let file =
            std::fs::File::create(path).with_context(|| format!("create {}", path.display()))?;
        let writer = BufWriter::new(file);
        let mut encoder = png::Encoder::new(writer, self.width as u32, self.height as u32);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().context("write PNG header")?;
        let mut rgb = Vec::with_capacity(self.width * self.height * 3);
        match self.bpp {
            32 => {
                for pixel in self.data.as_chunks::<4>().0 {
                    rgb.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
                }
            }
            16 => {
                for pixel in self.data.as_chunks::<2>().0 {
                    let value = u16::from_le_bytes([pixel[0], pixel[1]]);
                    // Widen before scaling: `channel as u8 * 255` overflows u8 for
                    // any non-zero channel (31 * 255 panics in debug, wraps in
                    // release and produces wrong colours).
                    let r = (((value >> 11) & 0x1f) * 255 / 31) as u8;
                    let g = (((value >> 5) & 0x3f) * 255 / 63) as u8;
                    let b = ((value & 0x1f) * 255 / 31) as u8;
                    rgb.extend_from_slice(&[r, g, b]);
                }
            }
            _ => bail!("unsupported bpp"),
        }
        writer.write_image_data(&rgb).context("write PNG data")?;
        Ok(())
    }
}
