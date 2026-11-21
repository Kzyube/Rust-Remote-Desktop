use anyhow::{Result, bail};

fn main() -> Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("relay") => {
            let arguments: Vec<_> = std::env::args().skip(2).collect();
            let address = match arguments.as_slice() {
                [] => "127.0.0.1:47833",
                [flag, address] if flag == "--bind" => address,
                _ => bail!("Usage: rust-remote-desktop relay [--bind IP:PORT]"),
            };
            let listener = std::net::TcpListener::bind(address)?;
            println!(
                "Relay listening on {}; press Ctrl+C to stop",
                listener.local_addr()?
            );
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let interrupt = stop.clone();
            ctrlc::set_handler(move || {
                interrupt.store(true, std::sync::atomic::Ordering::Release)
            })?;
            rust_remote_desktop::relay::run(listener, stop)
        }
        Some("capture-check") => {
            let mut desktop = rust_remote_desktop::platform::Desktop::new()?;
            let frame = desktop.capture()?;
            let jpeg = rust_remote_desktop::session::encode_frame(&frame)?;
            let decoded =
                rust_remote_desktop::session::decode_frame(frame.width, frame.height, &jpeg)?;
            println!(
                "Capture and JPEG decode OK: {} × {} pixels; {} RGBA bytes; {} JPEG bytes",
                decoded.width,
                decoded.height,
                decoded.rgba.len(),
                jpeg.len()
            );
            Ok(())
        }
        Some("--help" | "-h") => {
            println!(
                "Rust Remote Desktop\n\n  cargo run --release           Open the native host/viewer application\n  cargo run -- capture-check    Check local capture and codec without connecting or injecting input\n  cargo run -- relay            Run a self-hosted relay on 127.0.0.1:47833\n  cargo run -- relay --bind IP:PORT\n                               Explicitly choose the relay's listen interface\n  cargo run -- --smoke-test     Open and automatically close the native window"
            );
            Ok(())
        }
        Some("--smoke-test") => run_gui(true),
        None => run_gui(false),
        Some(_) => bail!("Unknown command; use --help"),
    }
}

#[cfg(feature = "gui")]
fn run_gui(smoke_test: bool) -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1080.0, 740.0])
            .with_min_inner_size([760.0, 520.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Rust Remote Desktop",
        options,
        Box::new(move |cc| {
            let mut app = rust_remote_desktop::app::RemoteDesktopApp::new(cc);
            if smoke_test {
                app.close_after(std::time::Duration::from_secs(3));
            }
            Ok(Box::new(app))
        }),
    )
    .map_err(|error| anyhow::anyhow!("Unable to open desktop window: {error}"))
}

#[cfg(not(feature = "gui"))]
fn run_gui(_smoke_test: bool) -> Result<()> {
    bail!("Build with the default gui feature to open the application")
}
