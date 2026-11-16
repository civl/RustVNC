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

    let mut framebuffer = FrameBuffer::new(cli.bpp);
    let mut window: Option<minifb::Window> = None;
    let mut rects = 0u64;
    let mut bytes = 0u64;
    let mut input_sent = false;
    let start = Instant::now();
    let deadline = start + Duration::from_secs(cli.seconds);
    let mut last_refresh = Instant::now() - Duration::from_millis(50);
    let mut last_present = Instant::now() - Duration::from_millis(50);

    while Instant::now() < deadline {
        match client.poll_event().await {
            Ok(Some(event)) => {
                if let Some(event) =
                    handle_event(event, &mut framebuffer, &mut window, &mut rects, &mut bytes)?
                {
                    warn!("unhandled event: {event:?}");
                }
            }
            Ok(None) => {}
            Err(err) => {
                eprintln!("vnc error: {err}");
                break;
            }
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

        if last_refresh.elapsed() >= Duration::from_millis(33) {
            let _ = client.input(X11Event::Refresh).await;
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
                    let r = ((value >> 11) & 0x1f) as u8 * 255 / 31;
                    let g = ((value >> 5) & 0x3f) as u8 * 255 / 63;
                    let b = (value & 0x1f) as u8 * 255 / 31;
                    rgb.extend_from_slice(&[r, g, b]);
                }
            }
            _ => bail!("unsupported bpp"),
        }
        writer.write_image_data(&rgb).context("write PNG data")?;
        Ok(())
    }
}
