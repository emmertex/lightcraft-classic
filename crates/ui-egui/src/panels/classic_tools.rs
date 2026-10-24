//! Develop's tool strip as Lightroom Classic has it (fork-owned): a row of the editing tools under
//! the histogram (Crop, Remove, Red Eye, Masking, in the user's order), each opening its drawer
//! above the Develop panels, which stay under it. A click on the tool in use closes its drawer, as
//! Return and the drawer's Close do. The right-edge strip keeps the fold button and Presets. A
//! child module of `right`, so it uses that file's helpers.

use egui::vec2;
use lcc_catalog::PhotoId;
use serde_json::json;

use crate::LccApp;
use crate::module::{ModuleId, PanelId};
use crate::state::RightPanel;
use crate::widgets::{divider, icon_button_named};

/// Height of the row.
const ROW_H: f32 = 34.0;
/// Width of one tool's button.
const BUTTON_W: f32 = 40.0;

/// Whether `right` shows Develop's histogram, tool row and panels (the Edit panel, a tool's drawer
/// over it, or the profile browser in its place).
pub(super) fn develop_tools(app: &LccApp) -> bool {
    app.ui.module == ModuleId::Develop && app.ui.right.is_edit_tool()
}

/// Whether a tool's drawer is open over the panels.
pub(super) fn drawer_open(app: &LccApp) -> bool {
    develop_tools(app) && matches!(app.ui.right, RightPanel::Crop | RightPanel::Remove | RightPanel::Masking | RightPanel::RedEye)
}

/// The row of tools under the histogram, centred, with a rule under it.
pub(super) fn row(app: &mut LccApp, ui: &mut egui::Ui) {
    let has_photo = app.session.active().is_some();
    let tools: Vec<PanelId> = crate::panels::strip::strip_panels(app).into_iter().filter(|p| *p != PanelId::Edit).collect();
    let width = ui.available_width();
    let (row_rect, _) = ui.allocate_exact_size(vec2(width, ROW_H), egui::Sense::hover());
    let total_tools_w = tools.len() as f32 * BUTTON_W + (tools.len().saturating_sub(1)) as f32 * 4.0;
    let group_rect = egui::Rect::from_center_size(row_rect.center(), vec2(total_tools_w, ROW_H));
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(group_rect).layout(egui::Layout::left_to_right(egui::Align::Center)));
    child.spacing_mut().item_spacing.x = 4.0;
    for p in &tools {
        let Some((id, icon, panel, tip, needs_photo)) = crate::panels::strip::entry(*p) else { continue };
        let on = app.ui.right == panel;
        let video = crate::video_ui::blocked(app, &format!("panel.{id}"));
        let sc = crate::shortcuts::shortcut_of(&app.ui.settings.keymap, &format!("panel.{id}"));
        let tip = if video { crate::i18n::tr(crate::video_ui::NOT_FOR_VIDEO).to_string() } else { crate::widgets::command_tooltip(&child, tip, sc) };
        let name = crate::i18n::tr(entry_name(*p));
        let resp = icon_button_named(&mut child, id, icon, vec2(BUTTON_W, ROW_H - 4.0), on, (has_photo || !needs_photo) && !video, name, &tip);
        if resp.clicked() {
            // the tool in use: its drawer closes, back to the panels
            let cmd = if on { "panel.edit".to_string() } else { format!("panel.{id}") };
            let _ = app.run(&cmd, json!({}));
        }
        // the strip's header menu: show/hide, solo, hiding mode
        crate::panels::classic::side_menu(app, &resp, crate::panels::strip::strip_declared(ModuleId::Develop));
    }
    divider(ui);
}

/// The Develop panels under an open drawer.
pub(super) fn panels_under_drawer(app: &mut LccApp, ui: &mut egui::Ui, id: PhotoId) {
    ui.add_space(6.0);
    divider(ui);
    super::super::edit::show_controls(app, ui, id);
}

/// A tool's name for screen readers (its tooltip carries the shortcut).
fn entry_name(p: PanelId) -> &'static str {
    crate::panels::strip::entry(p).map_or("", |e| e.3)
}
