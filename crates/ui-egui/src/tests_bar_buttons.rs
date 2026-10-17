//! Issue #221: the buttons in the bars under the right column share its width, as the left column's
//! Import… / Export… do, instead of sitting at their own text's width.

use std::time::Duration;

use serde_json::json;

use crate::headless::Headless;
use crate::{LccApp, Services};

const T: Duration = Duration::from_secs(20);
const SETTLE: Duration = Duration::from_secs(120);

fn ask(h: &mut Headless, method: &str, params: serde_json::Value) {
    let r = h.request(method, params.clone(), T);
    assert_eq!(r["ok"], true, "{method} {params}: {r}");
    h.settle(SETTLE);
}

fn rect(h: &Headless, id: &str) -> egui::Rect {
    h.app.widgets.iter().find(|(w, _)| w == id).map(|(_, r)| *r).unwrap_or_else(|| panic!("no {id} on screen"))
}

/// Two buttons of one width side by side, together nearly as wide as `span` (what is left of the bar).
fn share(a: egui::Rect, b: egui::Rect, span: f32, what: &str) {
    assert!((a.width() - b.width()).abs() <= 1.0, "{what}: the same width ({} and {})", a.width(), b.width());
    assert!(a.right() < b.left() && (a.center().y - b.center().y).abs() < 1.0, "{what}: side by side");
    let used = b.right() - a.left();
    assert!(used >= span - 40.0, "{what}: {used} of {span} points used");
}

#[test]
fn right_column_bar_buttons_fill_its_width() {
    let app = LccApp::new(lcc_engine::Session::with_demo(), Services { png: None, ..Default::default() });
    let mut h = Headless::new(app, [1400.0, 1000.0], 1.0);
    h.settle(SETTLE);
    let ids: Vec<u64> = h.app.session.visible_cloned().iter().take(2).map(|p| p.0).collect();
    ask(&mut h, "engine.execute", json!({"command": "library.select", "params": {"ids": ids}}));
    ask(&mut h, "engine.execute", json!({"command": "module.library", "params": {}}));
    ask(&mut h, "ui.set", json!({"leftPanel": true}));
    let (meta, settings) = (rect(&h, "button:syncMetadata"), rect(&h, "button:syncSettings"));
    let left = rect(&h, "button:leftBar-import");
    share(meta, settings, h.app.ui.right_width, "Library");
    assert!(meta.width() > left.width() * 0.6, "about as wide as the left column's buttons");
    ask(&mut h, "engine.execute", json!({"command": "module.develop", "params": {}}));
    let (sync, reset) = (rect(&h, "button:syncBar"), rect(&h, "button:reset"));
    let switch = rect(&h, "button:autoSyncSwitch");
    share(sync, reset, h.app.ui.right_width - (switch.right() - switch.left()), "Develop");
}
