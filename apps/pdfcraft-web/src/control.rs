//! `request(method, params)`: the UI control channel (`pdfcraft_ui_egui::control`) for the page,
//! as a promise. On the desktop `--control <file>` serves it on loopback; in the browser an agent
//! (WebMCP) or a test calls the same methods on the wasm module's export, the shape EffectCraft's
//! `request` and FilmCraft's `window.filmcraft.request` have.
//!
//! ```js
//! const { commands } = await bindings.request("ui.commands");
//! await bindings.request("ui.command", { id: commands[0].id });
//! ```
//!
//! Still opt-in: the channel keeps a copy of the AccessKit tree from every frame, so [`WebShell`]
//! attaches it on the first request, not at start. The other ArtCraft web builds name two methods
//! differently, and those names work here too: `engine.commands` is `ui.commands` (the list
//! itself), `engine.execute {command, params}` is `ui.command {id, params}`.
//!
//! A result resolves with the method's result; an error rejects with an `Error` carrying its
//! message. The answers are polled on a short timer while any is outstanding, and the app is
//! asked to repaint meanwhile.

use std::cell::{Cell, RefCell};
use std::sync::mpsc::{Receiver, TryRecvError};

use pdfcraft_ui_egui::PdfCraftApp;
use pdfcraft_ui_egui::control::{ControlClient, Reply};
use serde_json::{Value, json};
use wasm_bindgen::JsCast as _;
use wasm_bindgen::prelude::*;

/// A request waiting for the next frame: method, params, resolve, reject.
type Queued = (String, Value, js_sys::Function, js_sys::Function);
/// A request the app has: its reply, whether the answer is the command list, resolve, reject.
type Pending = (Receiver<Reply>, bool, js_sys::Function, js_sys::Function);

thread_local! {
    static QUEUE: RefCell<Vec<Queued>> = const { RefCell::new(Vec::new()) };
    static CTX: RefCell<Option<egui::Context>> = const { RefCell::new(None) };
    static PENDING: RefCell<Vec<Pending>> = const { RefCell::new(Vec::new()) };
    static POLLING: Cell<bool> = const { Cell::new(false) };
}

/// How often the outstanding answers are looked for, in milliseconds.
const POLL_MS: i32 = 10;

/// [`PdfCraftApp`] for the web runner, with the page's control channel once it is used.
pub struct WebShell {
    app: PdfCraftApp,
    client: Option<ControlClient>,
}

impl WebShell {
    pub fn new(app: PdfCraftApp, ctx: &egui::Context) -> Self {
        CTX.with(|c| *c.borrow_mut() = Some(ctx.clone()));
        Self { app, client: None }
    }

    /// Hand the page's requests to the app, attaching the channel on the first one.
    fn forward(&mut self, ctx: &egui::Context) {
        let queued = QUEUE.with(|q| std::mem::take(&mut *q.borrow_mut()));
        if queued.is_empty() {
            return;
        }
        let client = self.client.get_or_insert_with(|| self.app.attach_control(ctx));
        for (method, params, resolve, reject) in queued {
            let (method, params, list) = shared_names(method, params);
            let rx = client.send(&method, params);
            PENDING.with(|p| p.borrow_mut().push((rx, list, resolve, reject)));
        }
        wake();
    }
}

// Every method the app implements, passed on; the rest keep eframe's defaults, as the app does.
impl eframe::App for WebShell {
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        eframe::App::raw_input_hook(&mut self.app, ctx, raw);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::App::save(&mut self.app, storage);
    }

    // Before the app's `logic`, which answers the control requests.
    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.forward(ctx);
        eframe::App::logic(&mut self.app, ctx, frame);
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        eframe::App::ui(&mut self.app, ui, frame);
    }
}

/// The names the other ArtCraft web builds share, in PdfCraft's terms; `true` when the answer is
/// the command list, which they return bare.
fn shared_names(method: String, params: Value) -> (String, Value, bool) {
    match method.as_str() {
        "engine.commands" => ("ui.commands".into(), params, true),
        "engine.execute" => {
            let id = params.get("command").cloned().unwrap_or(Value::Null);
            let inner = params.get("params").cloned().unwrap_or_else(|| json!({}));
            ("ui.command".into(), json!({ "id": id, "params": inner }), false)
        }
        _ => (method, params, false),
    }
}

/// Send one control request; the promise settles when the app has answered it.
#[wasm_bindgen]
pub fn request(method: String, params: JsValue) -> js_sys::Promise {
    let params = from_js(&params);
    js_sys::Promise::new(&mut |resolve, reject| {
        if CTX.with(|c| c.borrow().is_none()) {
            let _ = reject.call1(&JsValue::NULL, &js_sys::Error::new("PdfCraft is not running yet"));
            return;
        }
        QUEUE.with(|q| q.borrow_mut().push((method.clone(), params.clone(), resolve, reject)));
        repaint();
    })
}

fn to_js(v: &Value) -> JsValue {
    js_sys::JSON::parse(&v.to_string()).unwrap_or(JsValue::NULL)
}

fn from_js(v: &JsValue) -> Value {
    if v.is_undefined() || v.is_null() {
        return json!({});
    }
    js_sys::JSON::stringify(v).ok().and_then(|s| s.as_string()).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(json!({}))
}

fn repaint() {
    CTX.with(|c| {
        if let Some(ctx) = c.borrow().as_ref() {
            ctx.request_repaint();
        }
    });
}

/// Ask for a frame and, unless it already runs, start looking for the answers.
fn wake() {
    repaint();
    if !POLLING.with(|p| p.replace(true)) {
        schedule();
    }
}

fn schedule() {
    let tick = wasm_bindgen::closure::Closure::once_into_js(poll);
    let set = web_sys::window().is_some_and(|w| w.set_timeout_with_callback_and_timeout_and_arguments_0(tick.unchecked_ref(), POLL_MS).is_ok());
    if !set {
        POLLING.with(|p| p.set(false));
    }
}

/// Settle the promises whose answers arrived; keep looking while any is outstanding.
fn poll() {
    let mut settled = Vec::new();
    let left = PENDING.with(|p| {
        let mut p = p.borrow_mut();
        let mut keep = Vec::with_capacity(p.len());
        for (rx, list, resolve, reject) in p.drain(..) {
            match rx.try_recv() {
                Ok(r) => settled.push((Some(r), list, resolve, reject)),
                Err(TryRecvError::Empty) => keep.push((rx, list, resolve, reject)),
                Err(TryRecvError::Disconnected) => settled.push((None, list, resolve, reject)),
            }
        }
        *p = keep;
        p.len()
    });
    // Outside the borrow: settling runs no script now, but a later `request` must find the list free.
    for (answer, list, resolve, reject) in settled {
        match answer {
            Some(Ok(v)) => {
                let v = if list { v.get("commands").cloned().unwrap_or(v) } else { v };
                let _ = resolve.call1(&JsValue::NULL, &to_js(&v));
            }
            Some(Err(msg)) => {
                let _ = reject.call1(&JsValue::NULL, &js_sys::Error::new(&msg));
            }
            None => {
                let _ = reject.call1(&JsValue::NULL, &js_sys::Error::new("request dropped"));
            }
        }
    }
    if left == 0 {
        POLLING.with(|p| p.set(false));
    } else {
        repaint();
        schedule();
    }
}
