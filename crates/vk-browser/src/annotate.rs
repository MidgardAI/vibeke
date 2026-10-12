//! Page annotations (06 B11): the pick engine shared by browser panes (`cdp::Page`) and agent
//! browser sessions. It is generic over a CDP connection and a flattened session id.
//!
//! - **Hover**: `DOM.getNodeForLocation`, then `Overlay.highlightNode`. Nothing is added to the
//!   page DOM, so the page cannot see the highlight.
//! - **Pick an element**: the node at a point is described by [`DESCRIBE_JS`], run with
//!   `Runtime.callFunctionOn` in an isolated world ([`WORLD`]): a selector path, the role and
//!   accessible name, its text, the first 2 KB of `outerHTML` and its rectangle. A screenshot of
//!   the rectangle (8 px margin, at most 1600 px on a side) is taken with
//!   `Page.captureScreenshot {clip}`.
//! - **Pick a region**: [`REGION_JS`] samples `elementsFromPoint` over a grid, takes the smallest
//!   element containing the region and up to 10 covered elements with their text.
//! - [`Gesture`]: what pointer input means while annotating (move = hover, click = pick,
//!   shift-drag or a longer drag = region).
//!
//! The JS sources are plain `pub const`s so the desktop app's TypeScript port can copy them.

#[cfg(not(target_arch = "wasm32"))]
use crate::cdp::Cdp;
#[cfg(not(target_arch = "wasm32"))]
use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
#[cfg(not(target_arch = "wasm32"))]
use std::sync::Arc;
use vk_proto::input::{MouseButton, MouseKind};

/// The isolated world's name (`Page.createIsolatedWorld`).
pub const WORLD: &str = "vibeke-annotate";
/// Margin around a picked element's screenshot, in CSS px.
pub const SHOT_MARGIN: f64 = 8.0;
/// Largest screenshot side in device px.
pub const SHOT_MAX: f64 = 1600.0;
/// Covered elements reported for a region.
pub const MAX_COVERS: usize = 10;
/// A drag shorter than this (CSS px) is a click.
pub const DRAG_MIN: f64 = 6.0;

macro_rules! helpers_js {
    () => {
        r#"
  const esc = (s) => (globalThis.CSS && CSS.escape) ? CSS.escape(s) : String(s).replace(/[^a-zA-Z0-9_-]/g, (c) => '\\' + c);
  const uniq = (sel) => { try { return document.querySelectorAll(sel).length === 1; } catch (_) { return false; } };
  const selectorOf = (el) => {
    if (el.id && uniq('#' + esc(el.id))) return '#' + esc(el.id);
    for (const a of ['data-testid', 'data-test', 'data-cy']) {
      const v = el.getAttribute && el.getAttribute(a);
      if (v) { const s = '[' + a + '="' + v.replace(/"/g, '\\"') + '"]'; if (uniq(s)) return s; }
    }
    const parts = [];
    let n = el;
    while (n && n.nodeType === 1 && parts.length < 8) {
      if (n !== el && n.id && uniq('#' + esc(n.id))) { parts.unshift('#' + esc(n.id)); break; }
      const tag = n.localName;
      if (tag === 'html' || tag === 'body') { parts.unshift(tag); break; }
      let part = tag;
      const p = n.parentElement;
      if (p) {
        const same = Array.prototype.filter.call(p.children, (c) => c.localName === tag);
        if (same.length > 1) part += ':nth-of-type(' + (same.indexOf(n) + 1) + ')';
      }
      parts.unshift(part);
      n = p;
    }
    return parts.join(' > ');
  };
  const ROLES = {button: 'button', nav: 'navigation', main: 'main', header: 'banner', footer: 'contentinfo', aside: 'complementary', form: 'form', img: 'img', ul: 'list', ol: 'list', li: 'listitem', table: 'table', tr: 'row', td: 'cell', th: 'columnheader', select: 'combobox', textarea: 'textbox', h1: 'heading', h2: 'heading', h3: 'heading', h4: 'heading', h5: 'heading', h6: 'heading', dialog: 'dialog', section: 'region', option: 'option', progress: 'progressbar', summary: 'button'};
  const INPUTS = {checkbox: 'checkbox', radio: 'radio', range: 'slider', button: 'button', submit: 'button', reset: 'button', image: 'button', search: 'searchbox', number: 'spinbutton'};
  const roleOf = (el) => {
    const r = el.getAttribute('role');
    if (r) return r.split(' ')[0];
    const t = el.localName;
    if (t === 'a') return el.hasAttribute('href') ? 'link' : null;
    if (t === 'input') return INPUTS[(el.getAttribute('type') || 'text').toLowerCase()] || 'textbox';
    return ROLES[t] || null;
  };
  const clean = (s, n) => { s = String(s || '').replace(/\s+/g, ' ').trim(); return s.length > n ? s.slice(0, n) + '…' : s; };
  const nameOf = (el) => {
    const l = el.getAttribute('aria-label');
    if (l) return clean(l, 120);
    const by = el.getAttribute('aria-labelledby');
    if (by) {
      const t = by.split(/\s+/).map((id) => { const x = document.getElementById(id); return x ? (x.innerText || x.textContent) : ''; }).join(' ');
      if (t.trim()) return clean(t, 120);
    }
    if (el.labels && el.labels.length) return clean(el.labels[0].innerText || el.labels[0].textContent, 120);
    for (const a of ['alt', 'title', 'placeholder']) { const v = el.getAttribute(a); if (v) return clean(v, 120); }
    if (/^(a|button|summary|h[1-6]|label|option)$/.test(el.localName) || /^(button|link|tab|menuitem|heading)$/.test(el.getAttribute('role') || '')) return clean(el.innerText || el.textContent, 120) || null;
    return null;
  };
  const textOf = (el) => clean(el.innerText !== undefined ? el.innerText : el.textContent, 500);
  const rectOf = (el) => { const r = el.getBoundingClientRect(); return {x: r.left, y: r.top, w: r.width, h: r.height}; };
  const view = () => ({sx: scrollX, sy: scrollY, vw: innerWidth, vh: innerHeight, dpr: devicePixelRatio});
"#
    };
}

/// Describe the element `this` (or a text node's parent): `{selector, role, name, text, html,
/// rect: {x, y, w, h}, sx, sy, vw, vh, dpr}` (viewport CSS px; scroll offsets and viewport size
/// for the screenshot clip). Run with `Runtime.callFunctionOn {objectId}` in [`WORLD`].
pub const DESCRIBE_JS: &str = concat!(
    "function () {",
    helpers_js!(),
    r#"
  const el = this && this.nodeType === 1 ? this : (this && this.parentElement);
  if (!el) return null;
  const html = el.outerHTML || '';
  return Object.assign({selector: selectorOf(el), role: roleOf(el), name: nameOf(el), text: textOf(el),
    html: html.length > 2048 ? html.slice(0, 2048) + '…' : html, rect: rectOf(el)}, view());
}"#
);

/// Describe the region `(x, y, w, h)` (viewport CSS px): the smallest element containing it
/// (`selector`, `role`, `name`), the outermost elements at least half inside it (`covers`, at
/// most 10, with their text), the covered text, `rect` and the view (as [`DESCRIBE_JS`]). Run
/// with `Runtime.callFunctionOn {executionContextId, arguments}` in [`WORLD`].
pub const REGION_JS: &str = concat!(
    "function (x, y, w, h) {",
    helpers_js!(),
    r#"
  const inside = (r) => {
    const ix = Math.max(0, Math.min(r.x + r.w, x + w) - Math.max(r.x, x));
    const iy = Math.max(0, Math.min(r.y + r.h, y + h) - Math.max(r.y, y));
    return r.w * r.h > 0 && ix * iy >= 0.5 * r.w * r.h;
  };
  const seen = new Set();
  const hits = [];
  const N = 8;
  for (let i = 0; i < N; i++) for (let j = 0; j < N; j++) {
    for (const e of document.elementsFromPoint(x + (i + 0.5) * w / N, y + (j + 0.5) * h / N)) {
      if (!seen.has(e)) { seen.add(e); hits.push(e); }
    }
  }
  let c = document.elementFromPoint(x + w / 2, y + h / 2) || document.body;
  while (c && c.parentElement) {
    const r = rectOf(c);
    if (r.x <= x && r.y <= y && r.x + r.w >= x + w && r.y + r.h >= y + h) break;
    c = c.parentElement;
  }
  c = c || document.body || document.documentElement;
  const covers = [];
  for (const e of hits) {
    if (e === c || e.contains(c) || /^(html|body)$/.test(e.localName)) continue;
    if (!inside(rectOf(e))) continue;
    if (covers.some((k) => k.contains(e))) continue;
    for (let k = covers.length - 1; k >= 0; k--) if (e.contains(covers[k])) covers.splice(k, 1);
    covers.push(e);
  }
  const list = covers.slice(0, 10);
  return Object.assign({selector: selectorOf(c), role: roleOf(c), name: nameOf(c),
    text: clean(list.map((k) => k.innerText || k.textContent || '').join(' '), 500),
    covers: list.map((k) => ({selector: selectorOf(k), text: clean(k.innerText || k.textContent, 120) || undefined})),
    rect: {x: x, y: y, w: w, h: h}}, view());
}"#
);

/// `Overlay.highlightNode` / `Overlay.setInspectMode` highlight config (JSON).
pub const HIGHLIGHT_CONFIG_JSON: &str = r#"{"showInfo":true,"showAccessibilityInfo":false,"contentColor":{"r":111,"g":168,"b":220,"a":0.35},"paddingColor":{"r":147,"g":196,"b":125,"a":0.3},"borderColor":{"r":255,"g":229,"b":153,"a":0.8},"marginColor":{"r":246,"g":178,"b":107,"a":0.3}}"#;

/// `Overlay.highlightRect` colours for a region being dragged (JSON: `color`, `outlineColor`).
pub const RECT_COLORS_JSON: &str = r#"{"color":{"r":111,"g":168,"b":220,"a":0.25},"outlineColor":{"r":40,"g":110,"b":200,"a":0.9}}"#;

pub fn highlight_config() -> Value {
    serde_json::from_str(HIGHLIGHT_CONFIG_JSON).unwrap_or_else(|_| json!({}))
}

/// A rectangle in viewport CSS px.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    /// The rectangle spanned by two corners.
    pub fn span(a: (f64, f64), b: (f64, f64)) -> Rect {
        Rect {
            x: a.0.min(b.0),
            y: a.1.min(b.1),
            w: (a.0 - b.0).abs(),
            h: (a.1 - b.1).abs(),
        }
    }

    pub fn json(&self) -> Value {
        json!({"x": self.x, "y": self.y, "w": self.w, "h": self.h})
    }

    pub fn from_json(v: &Value) -> Option<Rect> {
        let n = |k: &str| v.get(k).and_then(Value::as_f64).filter(|x| x.is_finite());
        Some(Rect {
            x: n("x")?,
            y: n("y")?,
            w: n("w").or_else(|| n("width"))?.max(0.0),
            h: n("h").or_else(|| n("height"))?.max(0.0),
        })
    }
}

/// One covered element of a region pick.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Cover {
    pub selector: String,
    pub text: Option<String>,
}

/// What a pick found (raw page strings: the server redacts and bounds them).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Pick {
    /// `element` | `region`.
    pub kind: String,
    pub selector: Option<String>,
    pub role: Option<String>,
    pub name: Option<String>,
    pub text: Option<String>,
    pub html: Option<String>,
    pub rect: Option<Rect>,
    pub covers: Vec<Cover>,
    /// PNG of the picked rectangle.
    pub image: Option<Vec<u8>>,
}

/// The page's scroll offset, viewport size and device pixel ratio, as the pick JS reports them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct View {
    pub sx: f64,
    pub sy: f64,
    pub vw: f64,
    pub vh: f64,
    pub dpr: f64,
}

impl View {
    pub fn from_json(v: &Value) -> View {
        let n = |k: &str, d: f64| v.get(k).and_then(Value::as_f64).unwrap_or(d);
        View {
            sx: n("sx", 0.0),
            sy: n("sy", 0.0),
            vw: n("vw", f64::INFINITY),
            vh: n("vh", f64::INFINITY),
            dpr: n("dpr", 1.0).max(0.1),
        }
    }
}

impl Pick {
    /// From the result of [`DESCRIBE_JS`] / [`REGION_JS`].
    pub fn from_js(kind: &str, v: &Value) -> Pick {
        let st = |k: &str| {
            v.get(k)
                .and_then(Value::as_str)
                .filter(|x| !x.is_empty())
                .map(str::to_string)
        };
        let covers = v
            .get("covers")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|c| {
                        Some(Cover {
                            selector: c.get("selector")?.as_str()?.to_string(),
                            text: c
                                .get("text")
                                .and_then(Value::as_str)
                                .filter(|x| !x.is_empty())
                                .map(str::to_string),
                        })
                    })
                    .take(MAX_COVERS)
                    .collect()
            })
            .unwrap_or_default();
        Pick {
            kind: kind.to_string(),
            selector: st("selector"),
            role: st("role"),
            name: st("name"),
            text: st("text"),
            html: st("html"),
            rect: v.get("rect").and_then(Rect::from_json),
            covers,
            image: None,
        }
    }

    /// The note fields of `browser.annotation_add` (without the image).
    pub fn fields(&self) -> Value {
        let mut v = json!({"kind": self.kind});
        for (k, x) in [
            ("selector", &self.selector),
            ("role", &self.role),
            ("name", &self.name),
            ("text", &self.text),
            ("html", &self.html),
        ] {
            if let Some(x) = x {
                v[k] = json!(x);
            }
        }
        if let Some(r) = &self.rect {
            v["rect"] = r.json();
        }
        if !self.covers.is_empty() {
            v["covers"] = Value::Array(
                self.covers
                    .iter()
                    .map(|c| match &c.text {
                        Some(t) => json!({"selector": c.selector, "text": t}),
                        None => json!({"selector": c.selector}),
                    })
                    .collect(),
            );
        }
        v
    }
}

/// The bounding rectangle of a CDP quad (`[x1, y1, … x4, y4]`, as `DOM.getBoxModel` returns).
pub fn quad_rect(q: &Value) -> Option<Rect> {
    let a = q.as_array().filter(|a| a.len() == 8)?;
    let n: Vec<f64> = a.iter().map(Value::as_f64).collect::<Option<_>>()?;
    let xs = [n[0], n[2], n[4], n[6]];
    let ys = [n[1], n[3], n[5], n[7]];
    let (x0, x1) = (
        xs.iter().copied().fold(f64::INFINITY, f64::min),
        xs.iter().copied().fold(f64::NEG_INFINITY, f64::max),
    );
    let (y0, y1) = (
        ys.iter().copied().fold(f64::INFINITY, f64::min),
        ys.iter().copied().fold(f64::NEG_INFINITY, f64::max),
    );
    (x0.is_finite() && y0.is_finite() && x1.is_finite() && y1.is_finite()).then(|| Rect {
        x: x0,
        y: y0,
        w: x1 - x0,
        h: y1 - y0,
    })
}

/// The `Page.captureScreenshot` clip for `r` (viewport CSS px): the rectangle plus
/// [`SHOT_MARGIN`], cut to the viewport, in document coordinates, scaled so no side exceeds
/// [`SHOT_MAX`] device px. `None` when nothing of it is visible.
pub fn clip_for(r: &Rect, view: &View) -> Option<Value> {
    let x0 = (r.x - SHOT_MARGIN).max(0.0);
    let y0 = (r.y - SHOT_MARGIN).max(0.0);
    let x1 = (r.x + r.w + SHOT_MARGIN).min(view.vw);
    let y1 = (r.y + r.h + SHOT_MARGIN).min(view.vh);
    let (w, h) = (x1 - x0, y1 - y0);
    if w.is_nan() || h.is_nan() || w < 1.0 || h < 1.0 {
        return None;
    }
    let scale = (SHOT_MAX / (w.max(h) * view.dpr)).min(1.0);
    Some(json!({"x": x0 + view.sx, "y": y0 + view.sy, "width": w, "height": h, "scale": scale}))
}

// ---- gestures -------------------------------------------------------------------------------

/// What one pointer event does while annotating.
#[derive(Debug, Clone, PartialEq)]
pub enum Act {
    /// Highlight the element under the pointer.
    Hover(f64, f64),
    /// Show the region being dragged.
    Rubber(Rect),
    /// Pick the element at a point.
    Point(f64, f64),
    /// Pick a region.
    Region(Rect),
    Nothing,
}

/// Pointer state while annotating (per pane): a press waiting for its release, or a region
/// being dragged. Coordinates are page CSS px.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Gesture {
    press: Option<(f64, f64)>,
    region: Option<(f64, f64)>,
}

impl Gesture {
    /// Move = hover; left click = pick the element; shift + drag (or a drag longer than
    /// [`DRAG_MIN`]) = pick a region. Other buttons cancel.
    pub fn on_mouse(
        &mut self,
        kind: MouseKind,
        button: MouseButton,
        x: f64,
        y: f64,
        shift: bool,
    ) -> Act {
        let pt = (x, y);
        match kind {
            MouseKind::Move => match self.region {
                Some(a) => Act::Rubber(Rect::span(a, pt)),
                None => Act::Hover(x, y),
            },
            MouseKind::Press if button == MouseButton::Left => {
                if shift {
                    self.region = Some(pt);
                    self.press = None;
                    Act::Rubber(Rect::span(pt, pt))
                } else {
                    self.press = Some(pt);
                    self.region = None;
                    Act::Nothing
                }
            }
            MouseKind::Press => {
                *self = Gesture::default();
                Act::Nothing
            }
            MouseKind::Drag => {
                if let Some(a) = self.region {
                    return Act::Rubber(Rect::span(a, pt));
                }
                if let Some(p) = self.press
                    && ((p.0 - x).abs() > DRAG_MIN || (p.1 - y).abs() > DRAG_MIN)
                {
                    self.press = None;
                    self.region = Some(p);
                    return Act::Rubber(Rect::span(p, pt));
                }
                Act::Nothing
            }
            MouseKind::Release => {
                if let Some(a) = self.region.take() {
                    self.press = None;
                    let r = Rect::span(a, pt);
                    if r.w < 4.0 && r.h < 4.0 {
                        return Act::Point(x, y);
                    }
                    return Act::Region(r);
                }
                if self.press.take().is_some() {
                    return Act::Point(x, y);
                }
                Act::Nothing
            }
        }
    }
}

// ---- CDP ------------------------------------------------------------------------------------

/// The pick engine on one page (a browser pane's target or an agent browser session). All calls
/// block on CDP responses: run them off the async runtime.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone)]
pub struct Picker {
    cdp: Arc<Cdp>,
    session: String,
}

#[cfg(not(target_arch = "wasm32"))]
impl Picker {
    pub fn new(cdp: Arc<Cdp>, session: impl Into<String>) -> Picker {
        Picker {
            cdp,
            session: session.into(),
        }
    }

    /// The flattened CDP session this picker drives.
    pub fn session(&self) -> &str {
        &self.session
    }

    fn call(&self, method: &str, params: Value) -> Result<Value> {
        self.cdp.call(Some(&self.session), method, params)
    }

    /// `DOM.enable` and `Overlay.enable` (needed before highlights and node lookups).
    pub fn enable(&self) -> Result<()> {
        self.call("DOM.enable", json!({}))?;
        self.call("Overlay.enable", json!({}))?;
        Ok(())
    }

    /// Hide highlights and leave inspect mode.
    pub fn disable(&self) -> Result<()> {
        let _ = self.call("Overlay.hideHighlight", json!({}));
        self.call(
            "Overlay.setInspectMode",
            json!({"mode": "none", "highlightConfig": highlight_config()}),
        )?;
        Ok(())
    }

    /// The node at a viewport point: (backendNodeId, frameId). `None` when there is none.
    fn node_at(&self, x: f64, y: f64) -> Result<Option<(i64, Option<String>)>> {
        let r = match self.call(
            "DOM.getNodeForLocation",
            json!({"x": x.round() as i64, "y": y.round() as i64,
                   "includeUserAgentShadowDOM": false, "ignorePointerEventsNone": true}),
        ) {
            Ok(r) => r,
            Err(e) if format!("{e:#}").contains("No node") => return Ok(None),
            Err(e) => return Err(e),
        };
        Ok(r["backendNodeId"]
            .as_i64()
            .map(|n| (n, r["frameId"].as_str().map(str::to_string))))
    }

    /// Highlight the element under a point (or hide the highlight when there is none).
    pub fn hover(&self, x: f64, y: f64) -> Result<()> {
        match self.node_at(x, y)? {
            Some((node, _)) => self.call(
                "Overlay.highlightNode",
                json!({"highlightConfig": highlight_config(), "backendNodeId": node}),
            )?,
            None => self.call("Overlay.hideHighlight", json!({}))?,
        };
        Ok(())
    }

    /// Outline a rectangle (viewport CSS px), e.g. a region being dragged.
    pub fn highlight_rect(&self, r: &Rect) -> Result<()> {
        let colors: Value = serde_json::from_str(RECT_COLORS_JSON).unwrap_or_else(|_| json!({}));
        self.call(
            "Overlay.highlightRect",
            json!({"x": r.x.round() as i64, "y": r.y.round() as i64,
                   "width": r.w.round().max(1.0) as i64, "height": r.h.round().max(1.0) as i64,
                   "color": colors["color"], "outlineColor": colors["outlineColor"]}),
        )?;
        Ok(())
    }

    pub fn hide(&self) -> Result<()> {
        self.call("Overlay.hideHighlight", json!({}))?;
        Ok(())
    }

    fn main_frame(&self) -> Result<String> {
        Ok(
            self.call("Page.getFrameTree", json!({}))?["frameTree"]["frame"]["id"]
                .as_str()
                .ok_or_else(|| anyhow!("Page.getFrameTree: no main frame"))?
                .to_string(),
        )
    }

    /// A fresh isolated world in `frame` (the main frame when `None`); returns its context id.
    fn world(&self, frame: Option<&str>) -> Result<i64> {
        let frame = match frame {
            Some(f) => f.to_string(),
            None => self.main_frame()?,
        };
        self.call(
            "Page.createIsolatedWorld",
            json!({"frameId": frame, "worldName": WORLD, "grantUniveralAccess": false}),
        )?["executionContextId"]
            .as_i64()
            .ok_or_else(|| anyhow!("Page.createIsolatedWorld: no context"))
    }

    fn value_of(r: Value) -> Result<Value> {
        if let Some(ex) = r.get("exceptionDetails") {
            bail!("pick script threw: {ex}");
        }
        Ok(r["result"]["value"].clone())
    }

    /// Pick the element at a viewport point. `None` when no element is there.
    pub fn pick_point(&self, x: f64, y: f64) -> Result<Option<Pick>> {
        let Some((node, frame)) = self.node_at(x, y)? else {
            return Ok(None);
        };
        let ctx = match self.world(frame.as_deref()) {
            Ok(c) => c,
            Err(_) => self.world(None)?,
        };
        let obj = self.call(
            "DOM.resolveNode",
            json!({"backendNodeId": node, "executionContextId": ctx}),
        )?;
        let id = obj["object"]["objectId"]
            .as_str()
            .ok_or_else(|| anyhow!("DOM.resolveNode: no object"))?
            .to_string();
        let r = self.call(
            "Runtime.callFunctionOn",
            json!({"functionDeclaration": DESCRIBE_JS, "objectId": id, "returnByValue": true}),
        );
        let _ = self.call("Runtime.releaseObject", json!({"objectId": id}));
        let v = Self::value_of(r?)?;
        if !v.is_object() {
            return Ok(None);
        }
        let mut pick = Pick::from_js("element", &v);
        let mut view = View::from_json(&v);
        // In a child frame the description is frame-local, but the screenshot is of the
        // top-level page: take the rectangle and the view from the top level instead.
        let main = self.main_frame().ok();
        if frame.is_some() && frame != main {
            match self.top_level(node, view.dpr) {
                Ok((r, top)) => {
                    pick.rect = Some(r);
                    view = top;
                }
                Err(_) => pick.rect = None,
            }
        }
        if let Some(r) = pick.rect {
            pick.image = self.capture(&r, &view).ok().flatten();
        }
        let _ = self.call(
            "Overlay.highlightNode",
            json!({"highlightConfig": highlight_config(), "backendNodeId": node}),
        );
        Ok(Some(pick))
    }

    /// A node's border box in top-level viewport CSS px (`DOM.getBoxModel` answers in the
    /// main frame's viewport for nodes in any frame), and the top-level view from
    /// `Page.getLayoutMetrics`. `dpr` is the frame's, which is the page's.
    fn top_level(&self, node: i64, dpr: f64) -> Result<(Rect, View)> {
        let bm = self.call("DOM.getBoxModel", json!({"backendNodeId": node}))?;
        let r = quad_rect(&bm["model"]["border"])
            .ok_or_else(|| anyhow!("DOM.getBoxModel: no border quad"))?;
        let m = self.call("Page.getLayoutMetrics", json!({}))?;
        let vv = m
            .get("cssVisualViewport")
            .filter(|v| v.is_object())
            .unwrap_or(&m["visualViewport"]);
        let n = |k: &str| {
            vv[k]
                .as_f64()
                .ok_or_else(|| anyhow!("Page.getLayoutMetrics: no {k}"))
        };
        Ok((
            r,
            View {
                sx: n("pageX")?,
                sy: n("pageY")?,
                vw: n("clientWidth")?,
                vh: n("clientHeight")?,
                dpr,
            },
        ))
    }

    /// Pick a region (viewport CSS px).
    pub fn pick_region(&self, r: &Rect) -> Result<Pick> {
        let ctx = self.world(None)?;
        let v = Self::value_of(self.call(
            "Runtime.callFunctionOn",
            json!({"functionDeclaration": REGION_JS, "executionContextId": ctx, "returnByValue": true,
                   "arguments": [{"value": r.x}, {"value": r.y}, {"value": r.w}, {"value": r.h}]}),
        )?)?;
        let mut pick = Pick::from_js("region", &v);
        pick.rect = Some(*r);
        pick.image = self.capture(r, &View::from_json(&v)).ok().flatten();
        let _ = self.highlight_rect(r);
        Ok(pick)
    }

    /// PNG of a viewport rectangle (see [`clip_for`]).
    pub fn capture(&self, r: &Rect, view: &View) -> Result<Option<Vec<u8>>> {
        use base64::Engine as _;
        let Some(clip) = clip_for(r, view) else {
            return Ok(None);
        };
        let v = self.call(
            "Page.captureScreenshot",
            json!({"format": "png", "clip": clip, "captureBeyondViewport": false}),
        )?;
        let data = v["data"].as_str().unwrap_or("");
        if data.is_empty() {
            return Ok(None);
        }
        Ok(Some(
            base64::engine::general_purpose::STANDARD.decode(data)?,
        ))
    }
}

/// Canned answers of the test fakes ([`crate::fake_chromium`], [`crate::fake`]) for the CDP
/// calls of the pick engine: a node at every point, an isolated world, and a description built
/// from the call (so a test can check what was asked). `None` for other methods.
#[cfg(not(target_arch = "wasm32"))]
pub fn fake_reply(method: &str, params: &Value) -> Option<Value> {
    Some(match method {
        "DOM.enable"
        | "Overlay.enable"
        | "Overlay.disable"
        | "Overlay.highlightNode"
        | "Overlay.highlightRect"
        | "Overlay.hideHighlight"
        | "Overlay.setInspectMode"
        | "Runtime.releaseObject" => json!({}),
        "DOM.getNodeForLocation" => {
            let x = params["x"].as_i64().unwrap_or(0);
            let y = params["y"].as_i64().unwrap_or(0);
            // x = 777: an element inside a child frame (`DOM.getBoxModel` places it).
            let frame = if x == 777 { "child" } else { "F" };
            json!({"backendNodeId": 1000 + x * 7 + y, "frameId": frame})
        }
        "Page.getFrameTree" => json!({"frameTree": {"frame": {"id": "F", "url": "about:blank"}}}),
        // An element of a child frame: its top-level box is offset by the iframe's position.
        // (`Page.getLayoutMetrics` is answered by each fake: `fake_chromium` scrolls 50 px.)
        "DOM.getBoxModel" => {
            let q = json!([110, 220, 190, 220, 190, 244, 110, 244]);
            json!({"model": {"content": q, "padding": q, "border": q, "margin": q, "width": 80, "height": 24}})
        }
        "Page.createIsolatedWorld" => json!({"executionContextId": 900}),
        "DOM.resolveNode" => {
            json!({"object": {"type": "object", "subtype": "node",
                              "objectId": format!("node-{}", params["backendNodeId"])}})
        }
        "Runtime.callFunctionOn" => {
            let f = params["functionDeclaration"].as_str().unwrap_or("");
            let v = if f == REGION_JS {
                let a = |i: usize| params["arguments"][i]["value"].as_f64().unwrap_or(0.0);
                json!({"selector": "main > section", "role": "region", "name": null,
                       "text": "Plans Pricing",
                       "covers": [{"selector": "#plans", "text": "Plans"}, {"selector": "#pricing", "text": "Pricing"}],
                       "rect": {"x": a(0), "y": a(1), "w": a(2), "h": a(3)},
                       "sx": 0, "sy": 0, "vw": 800, "vh": 600, "dpr": 1})
            } else if f == DESCRIBE_JS {
                json!({"selector": "#save", "role": "button", "name": "Save",
                       "text": "Save changes", "html": "<button id=\"save\">Save changes</button>",
                       "rect": {"x": 10, "y": 20, "w": 80, "h": 24},
                       "sx": 0, "sy": 0, "vw": 800, "vh": 600, "dpr": 1})
            } else {
                Value::Null
            };
            json!({"result": {"type": "object", "value": v}})
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gesture_maps_moves_clicks_and_drags() {
        let mut g = Gesture::default();
        let l = MouseButton::Left;
        assert_eq!(
            g.on_mouse(MouseKind::Move, MouseButton::None, 5.0, 6.0, false),
            Act::Hover(5.0, 6.0)
        );
        assert_eq!(
            g.on_mouse(MouseKind::Press, l, 10.0, 10.0, false),
            Act::Nothing
        );
        assert_eq!(
            g.on_mouse(MouseKind::Release, l, 11.0, 10.0, false),
            Act::Point(11.0, 10.0)
        );
        // Shift + drag: a region.
        g.on_mouse(MouseKind::Press, l, 10.0, 10.0, true);
        assert!(matches!(
            g.on_mouse(MouseKind::Drag, l, 50.0, 40.0, true),
            Act::Rubber(_)
        ));
        assert_eq!(
            g.on_mouse(MouseKind::Release, l, 50.0, 40.0, true),
            Act::Region(Rect {
                x: 10.0,
                y: 10.0,
                w: 40.0,
                h: 30.0
            })
        );
        // A longer plain drag also selects a region; a tiny one is a click.
        g.on_mouse(MouseKind::Press, l, 100.0, 100.0, false);
        assert_eq!(
            g.on_mouse(MouseKind::Drag, l, 102.0, 101.0, false),
            Act::Nothing
        );
        g.on_mouse(MouseKind::Drag, l, 60.0, 80.0, false);
        assert_eq!(
            g.on_mouse(MouseKind::Release, l, 60.0, 80.0, false),
            Act::Region(Rect {
                x: 60.0,
                y: 80.0,
                w: 40.0,
                h: 20.0
            })
        );
        // Another button cancels.
        g.on_mouse(MouseKind::Press, l, 1.0, 1.0, false);
        g.on_mouse(MouseKind::Press, MouseButton::Right, 1.0, 1.0, false);
        assert_eq!(
            g.on_mouse(MouseKind::Release, l, 1.0, 1.0, false),
            Act::Nothing
        );
    }

    #[test]
    fn clip_is_cut_to_the_viewport_scrolled_and_bounded() {
        let view = View {
            sx: 0.0,
            sy: 300.0,
            vw: 800.0,
            vh: 600.0,
            dpr: 2.0,
        };
        let c = clip_for(
            &Rect {
                x: 4.0,
                y: 100.0,
                w: 50.0,
                h: 20.0,
            },
            &view,
        )
        .unwrap();
        assert_eq!(c["x"], 0.0);
        assert_eq!(c["y"], 392.0);
        assert_eq!(c["width"], 62.0);
        assert_eq!(c["height"], 36.0);
        assert_eq!(c["scale"], 1.0);
        let big = clip_for(
            &Rect {
                x: 0.0,
                y: 0.0,
                w: 2000.0,
                h: 2000.0,
            },
            &view,
        )
        .unwrap();
        assert_eq!(big["width"], 800.0);
        assert_eq!(big["scale"], 1.0);
        let wide = View { vw: 4000.0, ..view };
        let s = clip_for(
            &Rect {
                x: 0.0,
                y: 0.0,
                w: 3000.0,
                h: 10.0,
            },
            &wide,
        )
        .unwrap()["scale"]
            .as_f64()
            .unwrap();
        assert!((s - 1600.0 / (3008.0 * 2.0)).abs() < 1e-9, "{s}");
        assert!(
            clip_for(
                &Rect {
                    x: 900.0,
                    y: 0.0,
                    w: 10.0,
                    h: 10.0
                },
                &view
            )
            .is_none()
        );
    }

    #[test]
    fn quad_rect_bounds_any_quad() {
        let r = quad_rect(&json!([10, 20, 30, 20, 30, 50, 10, 50])).unwrap();
        assert_eq!(
            r,
            Rect {
                x: 10.0,
                y: 20.0,
                w: 20.0,
                h: 30.0
            }
        );
        // A rotated quad: its bounding box.
        let r = quad_rect(&json!([5, 0, 10, 5, 5, 10, 0, 5])).unwrap();
        assert_eq!((r.x, r.y, r.w, r.h), (0.0, 0.0, 10.0, 10.0));
        assert!(quad_rect(&json!([1, 2, 3])).is_none());
        assert!(quad_rect(&json!(null)).is_none());
    }

    #[test]
    fn pick_fields_round_trip_the_js_result() {
        let v = json!({"selector": "#a", "role": "button", "name": "", "text": "Go",
                       "covers": [{"selector": "#b", "text": ""}],
                       "rect": {"x": 1, "y": 2, "w": 3, "h": 4}});
        let p = Pick::from_js("region", &v);
        assert_eq!(p.name, None);
        assert_eq!(p.covers[0].text, None);
        let f = p.fields();
        assert_eq!(f["kind"], "region");
        assert_eq!(f["rect"]["h"], 4.0);
        assert_eq!(f["covers"][0], json!({"selector": "#b"}));
        assert!(f.get("name").is_none());
        // The JS sources are complete functions.
        for js in [DESCRIBE_JS, REGION_JS] {
            assert!(js.starts_with("function ("), "{js}");
            assert!(js.trim_end().ends_with('}'));
            assert!(js.contains("selectorOf"));
        }
        assert!(highlight_config()["contentColor"].is_object());
    }

    #[test]
    fn picker_against_the_fake_browser() {
        let (cdp, _events, state) = crate::fake_chromium::spawn_pair(1.0);
        let t = cdp
            .call(None, "Target.createTarget", json!({"url": "http://x/"}))
            .unwrap()["targetId"]
            .as_str()
            .unwrap()
            .to_string();
        let s = cdp
            .call(
                None,
                "Target.attachToTarget",
                json!({"targetId": t, "flatten": true}),
            )
            .unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string();
        let p = Picker::new(cdp, s.clone());
        p.enable().unwrap();
        p.hover(5.0, 6.0).unwrap();
        let pick = p.pick_point(30.0, 30.0).unwrap().unwrap();
        assert_eq!(pick.selector.as_deref(), Some("#save"));
        assert_eq!(pick.role.as_deref(), Some("button"));
        assert!(
            pick.image
                .as_ref()
                .is_some_and(|i| i.starts_with(b"\x89PNG"))
        );
        let r = Rect {
            x: 10.0,
            y: 10.0,
            w: 100.0,
            h: 50.0,
        };
        let reg = p.pick_region(&r).unwrap();
        assert_eq!(reg.kind, "region");
        assert_eq!(reg.covers.len(), 2);
        assert_eq!(reg.rect, Some(r));
        p.disable().unwrap();
        let st = state.lock().unwrap();
        // The description ran in the isolated world, on the resolved node.
        let worlds = st.calls("Page.createIsolatedWorld");
        assert!(worlds.iter().all(|(p, _)| p["worldName"] == WORLD));
        let shots = st.calls("Page.captureScreenshot");
        assert_eq!(shots[0].0["clip"]["x"], 2.0);
        assert_eq!(shots[0].0["clip"]["width"], 96.0);
        assert_eq!(st.count("Input.dispatchMouseEvent"), 0);
        // The main frame needs no translation.
        assert_eq!(st.count("DOM.getBoxModel"), 0);
        drop(st);
        // In a child frame: the rectangle and the clip come from the top level (the box model
        // and the layout viewport), not the frame-local description.
        let child = p.pick_point(777.0, 30.0).unwrap().unwrap();
        assert_eq!(
            child.rect,
            Some(Rect {
                x: 110.0,
                y: 220.0,
                w: 80.0,
                h: 24.0
            })
        );
        let st = state.lock().unwrap();
        let shots = st.calls("Page.captureScreenshot");
        let clip = &shots.last().unwrap().0["clip"];
        assert_eq!(clip["x"], 102.0);
        assert_eq!(clip["y"], 262.0);
        assert_eq!(clip["width"], 96.0);
        assert_eq!(clip["height"], 40.0);
        assert!(st.count("Overlay.highlightNode") >= 1);
        assert_eq!(st.count("Overlay.setInspectMode"), 1);
    }
}
