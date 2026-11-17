//! Minimal eframe window, used to tell "our UI is broken" apart from
//! "eframe/GL on this machine is broken".
//!
//! ```text
//! cargo run -p rustvnc-client --example egui_smoke
//! ```
//!
//! With `RUSTVNC_SMOKE_SHOT=<path>` it grabs the framebuffer back through
//! `ViewportCommand::Screenshot` (i.e. `glReadPixels` inside the app) and writes
//! a PNG. If that PNG shows the UI but the screen does not, the problem is
//! presentation, not drawing.

fn main() -> eframe::Result {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default().with_inner_size([640.0, 360.0]),
        vsync: std::env::var("RUSTVNC_VSYNC")
            .map(|v| v != "0")
            .unwrap_or(true),
        multisampling: std::env::var("RUSTVNC_MSAA")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
        ..Default::default()
    };
    eframe::run_native(
        "egui smoke",
        options,
        Box::new(|cc| {
            cc.egui_ctx.set_visuals(eframe::egui::Visuals::dark());
            Ok(Box::new(Smoke::default()))
        }),
    )
}

#[derive(Default)]
struct Smoke {
    frames: u32,
    shot_requested: bool,
    shot_path: Option<std::path::PathBuf>,
}

impl eframe::App for Smoke {
    fn update(&mut self, ctx: &eframe::egui::Context, _frame: &mut eframe::Frame) {
        self.frames += 1;
        if self.shot_path.is_none() {
            self.shot_path = std::env::var_os("RUSTVNC_SMOKE_SHOT").map(Into::into);
        }

        eframe::egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("HELLO SMOKE");
            ui.separator();
            ui.label(format!("frame {}", self.frames));
            if ui.button("a button").clicked() {
                ctx.request_repaint();
            }
        });

        // Ask egui to read its own framebuffer back once things have settled.
        if self.frames == 20
            && !self.shot_requested
            && let Some(path) = self.shot_path.clone()
        {
            self.shot_requested = true;
            println!("requesting screenshot -> {}", path.display());
            ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Screenshot(Default::default()));
        }
        for event in ctx.input(|input| input.events.clone()) {
            if let eframe::egui::Event::Screenshot { image, .. } = event
                && let Some(path) = self.shot_path.as_ref()
            {
                let [width, height] = image.size;
                let mut rgba = Vec::with_capacity(width * height * 4);
                for pixel in &image.pixels {
                    rgba.extend_from_slice(&pixel.to_array());
                }
                match image::save_buffer(
                    path,
                    &rgba,
                    width as u32,
                    height as u32,
                    image::ColorType::Rgba8,
                ) {
                    Ok(()) => println!("screenshot saved: {} ({width}x{height})", path.display()),
                    Err(err) => println!("screenshot save failed: {err}"),
                }
            }
        }

        // Keep repainting so a stuck present is visible as a frame counter that
        // does not advance on screen.
        ctx.request_repaint_after(std::time::Duration::from_millis(500));
    }
}
