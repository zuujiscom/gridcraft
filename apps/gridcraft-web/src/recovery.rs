//! Crash recovery in the browser: while a workbook has unsaved changes a copy (an .xlsx) is kept
//! in IndexedDB (craft-libs' `craft-web-recovery`: one store per tab session, held by a Web Lock so
//! a second tab leaves a running tab's copies alone, a write counted only once IndexedDB has
//! committed it). The AutoSave switch only covers workbooks that were saved to a file, which a
//! browser never has.
//!
//! Startup reads every copy before the app is built; the newest is reopened as an unsaved
//! workbook, the others stay stored. A copy goes when its workbook is downloaded, closed or no
//! longer differs from what was last saved.
//!
//! The idea of a recovery copy kept in browser storage also comes from storytold/gridcraft#1944
//! (GhostMitra), which this does not use: it keeps its own storage layer.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use craft_web_recovery::{HiddenHook, Opened, Recovered, Snapshot, Store, UnloadGuard};
use gridcraft_ui_egui::SheetApp;

/// The least time between two copies being written (milliseconds): encoding runs on the page.
const EVERY_MS: f64 = 10_000.0;

/// What [`open`] found, for the app to be built with.
pub struct Startup {
    pub store: Store,
    pub recovered: Vec<Recovered>,
    pub warnings: Vec<String>,
}

pub async fn open() -> Startup {
    let Opened { store, recovered, warnings } = craft_web_recovery::open("gridcraft").await;
    Startup { store, recovered, warnings: warnings.iter().map(ToString::to_string).collect() }
}

/// Reopen the newest readable copy in `app` as an unsaved workbook → the workbook's uid, its store
/// key and a message for the status bar.
pub fn restore(app: &mut SheetApp, recovered: &[Recovered]) -> Option<(u64, String, String)> {
    for r in recovered {
        let b64 = gridcraft_engine::io::base64_encode(&r.bytes);
        let name = if r.name.contains('.') { r.name.clone() } else { format!("{}.xlsx", r.name) };
        if let Err(e) = app.session.execute("file.open", serde_json::json!({"name": name, "base64": b64})) {
            log::warn!("recovery copy “{}” could not be opened: {e}", r.name);
            continue;
        }
        let Some(d) = app.session.active_mut() else { continue };
        // Differs from what was "saved", so it counts as unsaved until it is downloaded.
        d.saved = Arc::new((*d.wb).clone());
        let uid = d.uid;
        let others = recovered.len() - 1;
        let mut message = format!("Recovered “{}” from this browser. Download it to keep it.", r.name);
        if others > 0 {
            message.push_str(&format!(" {others} older recovery cop{} kept.", if others == 1 { "y" } else { "ies" }));
        }
        return Some((uid, r.key.clone(), message));
    }
    None
}

struct Entry {
    key: String,
    /// The (revision, workbook address) last handed to the store.
    queued: Option<(u64, usize)>,
}

/// The copies of the open workbooks, kept up to date once per frame.
pub struct WebRecovery {
    store: Store,
    entries: HashMap<u64, Entry>,
    last: f64,
    /// Set by the download service when an .xlsx went out.
    downloaded: Rc<Cell<bool>>,
    /// The workbook (by address) of each document as it was when it was downloaded.
    clean: HashMap<u64, usize>,
    /// The tab was hidden: write at once at the next frame.
    flush: Rc<Cell<bool>>,
    unsaved: Rc<Cell<bool>>,
    _guard: Option<UnloadGuard>,
    _hidden: Option<HiddenHook>,
}

impl WebRecovery {
    pub fn new(store: Store, adopted: Option<(u64, String)>, downloaded: Rc<Cell<bool>>, ctx: &egui::Context) -> Self {
        let flush = Rc::new(Cell::new(false));
        let unsaved = Rc::new(Cell::new(false));
        let guard_flag = unsaved.clone();
        let guard = UnloadGuard::new(move || guard_flag.get()).map_err(|e| log::warn!("unload guard: {e}")).ok();
        let (hidden_flag, hidden_ctx) = (flush.clone(), ctx.clone());
        let hidden = HiddenHook::new(move || {
            hidden_flag.set(true);
            hidden_ctx.request_repaint();
        })
        .map_err(|e| log::warn!("hidden hook: {e}"))
        .ok();
        let wake = ctx.clone();
        store.set_waker(move || wake.request_repaint());
        let mut entries = HashMap::new();
        if let Some((uid, key)) = adopted {
            entries.insert(uid, Entry { key, queued: None });
        }
        Self { store, entries, last: f64::NEG_INFINITY, downloaded, clean: HashMap::new(), flush, unsaved, _guard: guard, _hidden: hidden }
    }

    pub fn tick(&mut self, app: &SheetApp, ctx: &egui::Context) {
        for w in self.store.take_written() {
            if let Err(e) = w.result {
                log::warn!("recovery copy not written: {e}");
                // Try the same revision again at the next interval.
                if let Some(entry) = self.entries.values_mut().find(|en| en.key == w.key) {
                    entry.queued = None;
                }
            }
        }
        let docs = app.session.documents();
        if self.downloaded.replace(false)
            && let Some(d) = app.session.active()
            && d.is_dirty()
        {
            self.clean.insert(d.uid, Arc::as_ptr(&d.wb) as usize);
        }
        // Workbooks that were closed, saved, or went back to what was saved lose their copy.
        let gone: Vec<u64> = self
            .entries
            .keys()
            .copied()
            .filter(|uid| !docs.iter().any(|d| d.uid == *uid && d.is_dirty() && self.clean.get(uid) != Some(&(Arc::as_ptr(&d.wb) as usize))))
            .collect();
        for uid in gone {
            if let Some(entry) = self.entries.remove(&uid) {
                self.store.remove(&entry.key);
            }
        }
        self.clean.retain(|uid, ptr| docs.iter().any(|d| d.uid == *uid && Arc::as_ptr(&d.wb) as usize == *ptr));

        let mut due = false;
        let now = craft_web_recovery::now_ms();
        let flush = self.flush.get();
        let mut unsaved = false;
        for d in docs {
            let ptr = Arc::as_ptr(&d.wb) as usize;
            if !d.is_dirty() || self.clean.get(&d.uid) == Some(&ptr) {
                continue;
            }
            unsaved = true;
            let sig = (d.revision, ptr);
            if self.entries.get(&d.uid).is_some_and(|e| e.queued == Some(sig)) {
                continue;
            }
            if !flush && now - self.last < EVERY_MS {
                due = true;
                continue;
            }
            let stem = std::path::Path::new(&d.display_title()).file_stem().and_then(std::ffi::OsStr::to_str).unwrap_or("Book").to_string();
            let name = format!("{stem}.xlsx");
            let bytes = match gridcraft_engine::io::save_bytes(&d.wb, &name) {
                Ok(b) => b,
                Err(e) => {
                    log::warn!("recovery copy not encoded: {e}");
                    self.last = now;
                    continue;
                }
            };
            let store = self.store.clone();
            let entry = self.entries.entry(d.uid).or_insert_with(|| Entry { key: store.key(d.uid), queued: None });
            match self.store.put(&entry.key, d.revision, Snapshot { name: &name, path: None, meta: None, bytes: &bytes }) {
                Ok(()) => entry.queued = Some(sig),
                Err(e) => log::warn!("recovery copy not stored: {e}"),
            }
            self.last = now;
        }
        if flush {
            self.flush.set(false);
        }
        self.unsaved.set(unsaved);
        if due {
            ctx.request_repaint_after(std::time::Duration::from_millis((EVERY_MS - (now - self.last)).max(250.0) as u64));
        }
    }
}
