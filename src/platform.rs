//! OS integration: files opened from Finder / the Dock, and a native "Open…" panel.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use eframe::egui;

/// Files the OS asked us to open, waiting for the next frame.
static OPENED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
static CTX: OnceLock<egui::Context> = OnceLock::new();

pub fn set_context(ctx: &egui::Context) {
    let _ = CTX.set(ctx.clone());
}

/// Most recent file the OS asked us to open, if any.
pub fn take_opened() -> Option<PathBuf> {
    OPENED.lock().unwrap().drain(..).last()
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn push_opened(paths: impl IntoIterator<Item = PathBuf>) {
    OPENED.lock().unwrap().extend(paths);
    if let Some(ctx) = CTX.get() {
        ctx.request_repaint();
    }
}

/// Whether `open_dialog` can show a file picker on this system.
pub fn has_open_dialog() -> bool {
    #[cfg(target_os = "linux")]
    return linux::picker().is_some();
    #[cfg(not(target_os = "linux"))]
    cfg!(target_os = "macos")
}

#[cfg(target_os = "macos")]
mod mac {
    use std::path::PathBuf;

    use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
    use objc2::{MainThreadMarker, ffi, sel};
    use objc2_app_kit::{NSApplication, NSModalResponseOK, NSOpenPanel};
    use objc2_foundation::{NSArray, NSURL};

    fn url_path(url: &NSURL) -> Option<PathBuf> {
        url.path().map(|p| PathBuf::from(p.to_string()))
    }

    /// `-[delegate application:openURLs:]`: Finder "Open With", double-click,
    /// or a file dropped on the Dock icon.
    unsafe extern "C-unwind" fn open_urls(
        _this: *mut AnyObject,
        _cmd: Sel,
        _app: *mut NSApplication,
        urls: *mut NSArray<NSURL>,
    ) {
        if let Some(urls) = unsafe { urls.as_ref() } {
            super::push_opened(urls.iter().filter_map(|u| url_path(&u)));
        }
    }

    /// winit owns the NSApplication delegate (and panics if it is replaced), so we
    /// add the open-files handler to winit's delegate class instead. Must run after
    /// the event loop is built and before it starts, so the "open documents" event
    /// sent at launch reaches us.
    pub fn install_open_handler() {
        let Some(mtm) = MainThreadMarker::new() else { return };
        let Some(delegate) = NSApplication::sharedApplication(mtm).delegate() else { return };
        let class: &AnyClass = AsRef::<AnyObject>::as_ref(&*delegate).class();
        let imp = open_urls as unsafe extern "C-unwind" fn(_, _, _, _);
        // SAFETY: the signature matches the type encoding: void (id self, SEL _cmd, id, id).
        unsafe {
            let imp: Imp = std::mem::transmute(imp);
            ffi::class_addMethod(
                class as *const AnyClass as *mut AnyClass,
                sel!(application:openURLs:),
                imp,
                c"v@:@@".as_ptr(),
            );
        }
    }

    pub fn open_dialog() -> Option<PathBuf> {
        let mtm = MainThreadMarker::new()?;
        let panel = NSOpenPanel::openPanel(mtm);
        panel.setCanChooseDirectories(false);
        panel.setAllowsMultipleSelection(false);
        if panel.runModal() != NSModalResponseOK {
            return None;
        }
        url_path(&*panel.URLs().firstObject()?)
    }
}

#[cfg(target_os = "macos")]
pub use mac::{install_open_handler, open_dialog};

#[cfg(not(target_os = "macos"))]
pub fn install_open_handler() {}

/// Linux has no system file-picker API without GTK/Qt, so this runs the
/// desktop's own picker tool (zenity on GNOME and most others, kdialog on KDE).
#[cfg(target_os = "linux")]
mod linux {
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::OnceLock;

    const PICKERS: [(&str, &[&str]); 2] = [
        ("zenity", &["--file-selection", "--title=Open a file with merge conflicts"]),
        ("kdialog", &["--getopenfilename", ".", "--title", "Open a file with merge conflicts"]),
    ];

    pub fn picker() -> Option<&'static (&'static str, &'static [&'static str])> {
        static FOUND: OnceLock<Option<usize>> = OnceLock::new();
        let i = FOUND.get_or_init(|| {
            let dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
            PICKERS.iter().position(|(cmd, _)| dirs.iter().any(|d| d.join(cmd).is_file()))
        });
        i.map(|i| &PICKERS[i])
    }

    pub fn open_dialog() -> Option<PathBuf> {
        let (cmd, args) = picker()?;
        let out = Command::new(cmd).args(*args).output().ok()?;
        let path = String::from_utf8_lossy(&out.stdout).trim_end_matches(['\n', '\r']).to_string();
        (out.status.success() && !path.is_empty()).then(|| PathBuf::from(path))
    }
}

#[cfg(target_os = "linux")]
pub use linux::open_dialog;

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn open_dialog() -> Option<PathBuf> {
    None
}
