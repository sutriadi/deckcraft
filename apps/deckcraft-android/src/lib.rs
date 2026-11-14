//! DeckCraft for Android, aimed at tablets and foldables.
//!
//! Runs the same [`deckcraft_ui_egui::SlideApp`] as the desktop and web builds, through eframe on
//! a `NativeActivity`. Differences from the other shells:
//!
//! * No native file dialogs: Open, Save and Insert > Pictures go through the Storage Access
//!   Framework, which only Kotlin can reach, so the two sides talk through [`bridge`] files.
//! * A finger drag scrolls (see [`touch`]); a mouse is a real mouse (patched winit in `vendor/`).
//! * No control channel, printing or clipboard hooks yet (egui's own text paste still works).
//!
//! `android_main` is the entry point `NativeActivity` looks for. Everything else compiles on the
//! host too, so `cargo test -p deckcraft-android` covers the bridge without a device.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod bridge;
pub mod touch;

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use bridge::Bridge;
use deckcraft_engine::Session;
use deckcraft_ui_egui::{Inbox, InboxEvent, InboxFile, Services, SlideApp};

/// How often the bridge folder is checked while a picker or a save is open.
const POLL_EVERY: Duration = Duration::from_millis(200);

/// Bridge request id -> the app's request id (from `begin_external_file`), so a late answer is
/// applied to the slide that asked and a cancelled one to nothing.
type Targets = Rc<RefCell<HashMap<u64, u64>>>;

/// Wraps the app to move what the bridge delivers into the right inbox.
struct AndroidShell {
    app: SlideApp,
    inbox: Inbox,
    bridge: Rc<Bridge>,
    /// Open requests shown and not yet answered, by bridge id.
    targets: Targets,
    touch: touch::TouchScroll,
    insets: [f32; 4],
    insets_read: f64,
}

impl AndroidShell {
    /// Keeps the UI clear of the status bar, navigation bar, cutout and keyboard. Targeting API 35+
    /// draws apps edge to edge, so the system bars would otherwise cover the ribbon and status line.
    fn apply_insets(&mut self, raw: &mut egui::RawInput, now: f64) {
        if now - self.insets_read >= 0.25 || self.insets_read > now {
            self.insets_read = now;
            self.insets = self.bridge.insets();
        }
        let ppp = raw.viewports.get(&raw.viewport_id).and_then(|v| v.native_pixels_per_point).unwrap_or(1.0).max(0.1);
        if let Some(full) = raw.screen_rect {
            let [l, t, r, b] = self.insets.map(|px| px / ppp);
            let min = full.min + egui::vec2(l, t);
            let max = full.max - egui::vec2(r, b);
            if max.x - min.x > 100.0 && max.y - min.y > 100.0 {
                raw.screen_rect = Some(egui::Rect::from_min_max(min, max));
            }
        }
    }
}

impl eframe::App for AndroidShell {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Floating scroll bars only show on hover, which a finger never does: keep them drawn and thick.
        ctx.all_styles_mut(|style| {
            let s = &mut style.spacing.scroll;
            s.floating = true;
            s.bar_width = 18.0;
            s.floating_width = 10.0;
            s.handle_min_length = 48.0;
            s.dormant_handle_opacity = 0.55;
            s.dormant_background_opacity = 0.12;
            s.active_handle_opacity = 0.8;
            s.interact_handle_opacity = 0.95;
            s.foreground_color = true;
        });
        let polled = self.bridge.poll();
        for (id, name, bytes) in polled.files {
            if let Some(request) = self.targets.borrow_mut().remove(&id) {
                push(&self.inbox, InboxEvent::File(InboxFile { id: request, name, bytes }));
                ctx.request_repaint();
            }
        }
        for id in polled.cancelled_opens {
            if let Some(request) = self.targets.borrow_mut().remove(&id) {
                push(&self.inbox, InboxEvent::Cancelled { id: request });
                ctx.request_repaint();
            }
        }
        if !self.targets.borrow().is_empty() {
            ctx.request_repaint_after(POLL_EVERY);
        }
        self.app.logic(ctx);
    }

    fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        let now = raw.time.unwrap_or(0.0);
        self.apply_insets(raw, now);
        self.touch.filter(raw, now);
        if self.touch.is_pending() {
            ctx.request_repaint_after(Duration::from_millis(100)); // catches a long press
        }
        self.app.raw_input_hook(ctx, raw);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.app.ui(ui);
    }
}

fn push(inbox: &Inbox, event: InboxEvent) {
    inbox.lock().unwrap_or_else(|e| e.into_inner()).push(event);
}

fn services(inbox: Inbox, bridge: Rc<Bridge>, targets: Targets) -> Services {
    let save_bridge = bridge.clone();
    let save_again = bridge.clone();
    Services {
        // `purpose` is `open`, `picture`, `audio` or `video`; Kotlin filters the system picker by it.
        open_async: Some(Box::new(move |purpose: &str, request: u64| match bridge.request_open(purpose) {
            Ok(id) => {
                targets.borrow_mut().insert(id, request);
            }
            Err(e) => log::error!("couldn't ask for the open picker: {e}"),
        })),
        // No `pick_save`: like the web build, saving hands the bytes to the host, which here asks
        // Android's system picker where they go.
        write: Some(Box::new(move |name: &str, bytes: &[u8]| {
            save_bridge.request_save(name, bytes).map(|_| ()).map_err(|e| format!("couldn't hand {name} to Android: {e}"))
        })),
        download: Some(Box::new(move |name: &str, bytes: &[u8]| {
            if let Err(e) = save_again.request_save(name, bytes) {
                log::error!("couldn't hand {name} to Android: {e}");
            }
        })),
        inbox: Some(inbox),
        ..Default::default()
    }
}

/// Starts the app. `bridge_root` is a folder in the app's private storage shared with the Kotlin side.
pub fn run(options: eframe::NativeOptions, bridge_root: PathBuf) -> eframe::Result {
    eframe::run_native(
        "DeckCraft",
        options,
        Box::new(move |_cc| {
            let bridge = Rc::new(Bridge::new(&bridge_root).map_err(|e| format!("bridge folder {}: {e}", bridge_root.display()))?);
            let inbox: Inbox = Inbox::default();
            let targets: Targets = Rc::default();
            let mut app = SlideApp::new(Session::new(), services(inbox.clone(), bridge.clone(), targets.clone()));
            if let Err(e) = app.run("file.new", serde_json::json!({})) {
                log::error!("new presentation: {e}");
            }
            Ok(Box::new(AndroidShell { app, inbox, bridge, targets, touch: touch::TouchScroll::default(), insets: [0.0; 4], insets_read: -1.0 }))
        }),
    )
}

/// The entry point `NativeActivity` calls (named in `AndroidManifest.xml` as `libdeckcraft_android.so`).
#[cfg(target_os = "android")]
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub fn android_main(app: android_activity::AndroidApp) {
    android_logger::init_once(android_logger::Config::default().with_max_level(log::LevelFilter::Info).with_tag("DeckCraft"));
    let data = app.internal_data_path().unwrap_or_else(|| PathBuf::from("/data/local/tmp"));
    install_panic_report(data.join("crash.txt"));
    let root = data.join("bridge");
    let options = eframe::NativeOptions { android_app: Some(app), ..Default::default() };
    if let Err(e) = run(options, root) {
        log::error!("DeckCraft stopped: {e}");
    }
}

/// Records a panic where the Kotlin side can show it on the next launch (there is no console on a tablet).
#[cfg(target_os = "android")]
fn install_panic_report(path: PathBuf) {
    std::panic::set_hook(Box::new(move |info| {
        let report = format!("{info}\n\n{}", std::backtrace::Backtrace::force_capture());
        log::error!("panic: {report}");
        let _ = std::fs::write(&path, report);
    }));
}
