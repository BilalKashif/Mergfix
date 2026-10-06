mod app;
mod doc;
mod find;
mod platform;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

fn main() {
    // Finder may pass `-psn_…` process-serial-number args to bundled apps; ignore flags.
    let path = std::env::args_os().skip(1).find(|a| !a.to_string_lossy().starts_with("-psn_"));
    if path.as_deref().is_some_and(|p| p == "-h" || p == "--help") {
        println!("usage: mergefix <file>\n\nExit code is 0 when no conflicts remain, 1 otherwise.");
        return;
    }

    // Read + index the file while the window and GL context are being created.
    let loader = path.map(PathBuf::from).map(|p| std::thread::spawn(move || doc::Doc::load(p)));
    let unresolved = Arc::new(AtomicUsize::new(0));
    let out = unresolved.clone();

    if let Err(e) = run(loader, out) {
        eprintln!("mergefix: {e}");
        std::process::exit(2);
    }
    // Non-zero exit lets `git mergetool` (trustExitCode = true) know conflicts remain.
    std::process::exit(if unresolved.load(Ordering::Relaxed) > 0 { 1 } else { 0 });
}

type Loader = Option<std::thread::JoinHandle<std::io::Result<doc::Doc>>>;

fn run(loader: Loader, out: Arc<AtomicUsize>) -> Result<(), Box<dyn std::error::Error>> {
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon-256.png"))?;
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("mergefix")
            .with_app_id("mergefix")
            .with_inner_size([1200.0, 850.0])
            .with_drag_and_drop(true)
            .with_icon(icon),
        ..Default::default()
    };

    // We own the event loop (instead of `eframe::run_native`) so the macOS
    // open-files handler can be installed between creating NSApp and starting it.
    let event_loop = winit::event_loop::EventLoop::<eframe::UserEvent>::with_user_event().build()?;
    platform::install_open_handler();
    let mut app = eframe::create_native(
        "mergefix",
        options,
        Box::new(move |cc| {
            platform::set_context(&cc.egui_ctx);
            let loaded = loader.map(|h| h.join().expect("loader thread panicked"));
            Ok(Box::new(app::App::new(&cc.egui_ctx, loaded, out)))
        }),
        &event_loop,
    );
    event_loop.run_app(&mut app)?;
    Ok(())
}
