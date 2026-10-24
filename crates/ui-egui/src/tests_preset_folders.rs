//! Presets panel: preset folders, subfolders, moving, and hierarchy (issue #468).

use std::time::Duration;

use lcc_develop::Preset;
use serde_json::json;

use crate::headless::Headless;
use crate::{LccApp, Services};

const SETTLE: Duration = Duration::from_secs(120);

fn rect(h: &Headless, id: &str) -> Option<egui::Rect> {
    h.app.widgets.iter().find(|(w, _)| w == id).map(|(_, r)| *r)
}

#[test]
fn preset_folders_and_subfolders_hierarchy() {
    let app = LccApp::new(lcc_engine::Session::with_demo(), Services { png: None, ..Default::default() });
    let mut h = Headless::new(app, [1200.0, 800.0], 1.0);
    h.settle(SETTLE);
    h.app.run("panel.presets", json!({})).unwrap();

    // 1. Create top-level folder via dialog
    h.app.ui.dialog = Some(crate::state::Dialog::CreatePresetFolder { name: "Mastin Labs".into(), parent: None });
    h.step();
    // Confirm dialog
    let r = h.request("ui.dialog.confirm", json!({}), SETTLE);
    assert_eq!(r["ok"], true);
    h.step();
    h.step();

    assert!(rect(&h, "presetGroup:Mastin Labs").is_some(), "Mastin Labs folder header appears");

    // 2. Create subfolder under Mastin Labs
    h.app.run("preset.createGroup", json!({"name": "Portra Original", "parent": "Mastin Labs"})).unwrap();

    // Open Mastin Labs folder
    h.app.ui.group_open.insert("preset:Mastin Labs".into(), true);
    h.step();
    h.step();

    assert!(rect(&h, "presetGroup:Mastin Labs/Portra Original").is_some(), "Subfolder Portra Original appears under Mastin Labs");

    // Subfolder should be indented further than parent
    let parent_r = rect(&h, "presetGroup:Mastin Labs").unwrap();
    let child_r = rect(&h, "presetGroup:Mastin Labs/Portra Original").unwrap();
    assert!(child_r.top() > parent_r.top());

    // 3. Add a preset to the subfolder
    let base = h.app.session.presets.iter().find(|p| p.id == "lc.warm-glow").unwrap().clone();
    h.app.session.presets.push(Preset {
        id: "user.mastin-portra".into(),
        name: "Portra 400".into(),
        group: "Mastin Labs/Portra Original".into(),
        builtin: false,
        ..base
    });

    // Open subfolder
    h.app.ui.group_open.insert("preset:Mastin Labs/Portra Original".into(), true);
    h.step();
    h.step();

    assert!(rect(&h, "preset:user.mastin-portra").is_some(), "Preset appears inside open subfolder");

    // 4. Rename subfolder
    h.app.run("preset.renameGroup", json!({"group": "Mastin Labs/Portra Original", "name": "Portra Pack"})).unwrap();
    h.app.ui.group_open.insert("preset:Mastin Labs/Portra Pack".into(), true);
    h.step();
    h.step();

    assert!(rect(&h, "presetGroup:Mastin Labs/Portra Pack").is_some(), "Renamed subfolder appears");
    assert!(rect(&h, "presetGroup:Mastin Labs/Portra Original").is_none(), "Old subfolder name is gone");

    // 5. Move subfolder to top level
    h.app.run("preset.moveGroup", json!({"group": "Mastin Labs/Portra Pack", "parent": null})).unwrap();
    h.app.ui.group_open.insert("preset:Portra Pack".into(), true);
    h.step();
    h.step();

    assert!(rect(&h, "presetGroup:Portra Pack").is_some(), "Portra Pack folder moved to top level");

    // 6. Delete folder
    h.app.run("preset.deleteGroup", json!({"group": "Portra Pack"})).unwrap();
    h.step();
    h.step();

    assert!(rect(&h, "presetGroup:Portra Pack").is_none(), "Deleted folder is gone");
    assert!(rect(&h, "preset:user.mastin-portra").is_none(), "Preset in deleted folder is gone");
}
