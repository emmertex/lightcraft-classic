//! The Presets column (opens to the left of the Edit panel): grouped presets with hover preview
//! (the loupe shows the photo with the preset while the pointer rests on it; nothing is
//! committed), optional live thumbnails, amount slider, create/favourite, import/export of preset
//! files.

use std::collections::BTreeMap;
use std::sync::Arc;

use egui::{Align2, Color32, Rect, Sense, pos2, vec2};
use lcc_develop::{ControlSpec, Preset, Section, Track};
use lcc_engine::presets::PresetList;
use serde_json::json;

use crate::icons::{Icon, paint};
use crate::module::PanelId;
use crate::theme::Tokens;
use crate::widgets::{divider, icon_button, register, slider};
use crate::{HoverPreview, LccApp};

/// Long edge of preset thumbnails (px).
const THUMB_EDGE: usize = 128;

/// Height of the panel's Classic header ([`crate::panels::classic::header`]): folded, it is all
/// the panel takes in the left column.
pub const HEADER_H: f32 = 28.0;

/// The state the Presets panel of Develop's left column folds under. The presets column (Window ▸ Presets)
/// is the same panel somewhere else and folds under the panel's usual `panel:presets`: a state of
/// its own here means folding the panel in the column cannot leave that column open and empty (a
/// dead end), and closing the column brings the panel back as it was.
pub const IN_COLUMN_KEY: &str = "panel:presets-inline";

/// The preset amount slider may replace only the undo step it created.
#[derive(Clone)]
struct AppliedPreset {
    id: String,
    amount: f64,
    selected: Vec<lcc_catalog::PhotoId>,
    active: Option<lcc_catalog::PhotoId>,
    commits: u64,
}

impl AppliedPreset {
    fn new(session: &lcc_engine::Session, id: String, amount: f64) -> Self {
        Self { id, amount, selected: session.selection.ids.clone(), active: session.active(), commits: session.commits() }
    }

    fn is_current(&self, session: &lcc_engine::Session) -> bool {
        self.selected == session.selection.ids && self.active == session.active() && !self.is_stale(session)
    }

    /// A later edit, undo or redo (on any photo) owns the undo step: the slider may never replace it.
    fn is_stale(&self, session: &lcc_engine::Session) -> bool {
        !(self.commits == session.commits()
            && session.interaction.is_none()
            && session.redo.is_empty()
            && session
                .undo
                .last()
                .and_then(|entry| entry.label.strip_prefix("Preset: "))
                .is_some_and(|name| session.presets.iter().any(|preset| preset.id == self.id && preset.name == name)))
    }
}

/// The preset each photo was given, with its settings right after: the panel marks it while the
/// photo still has exactly those settings, so it is still marked on coming back to the photo
/// (issue #2392) and drops once the photo is edited further.
#[derive(Clone, Default)]
struct AppliedByPhoto(std::collections::HashMap<lcc_catalog::PhotoId, (String, Arc<lcc_develop::DevelopSettings>)>);

impl AppliedByPhoto {
    const ID: &'static str = "preset-applied-by-photo";

    /// Remember `preset` on the photos `preset.apply` just changed (the selection, else the active
    /// photo; an active photo outside the selection wasn't changed and isn't marked).
    fn remember(ctx: &egui::Context, session: &lcc_engine::Session, preset: &str) {
        let id = egui::Id::new(Self::ID);
        let mut map: Self = ctx.data(|d| d.get_temp(id)).unwrap_or_default();
        for photo in session.selected() {
            if let Some(d) = session.develop_of(photo) {
                map.0.insert(photo, (preset.to_string(), d));
            }
        }
        ctx.data_mut(|d| d.insert_temp(id, map));
    }

    /// The preset to mark for `photo`: the one it was given, while its settings are unchanged since.
    fn of(ctx: &egui::Context, session: &lcc_engine::Session, photo: Option<lcc_catalog::PhotoId>) -> Option<String> {
        let photo = photo?;
        let map: Self = ctx.data(|d| d.get_temp(egui::Id::new(Self::ID)))?;
        let (preset, then) = map.0.get(&photo)?;
        let now = session.develop_of(photo)?;
        (Arc::ptr_eq(then, &now) || **then == *now).then(|| preset.clone())
    }
}

/// `in_column`: drawn as the Presets panel of Develop's left column (which gives it a share of the
/// column and lets it fold). The presets column (Window ▸ Presets) draws the same panel whole.
pub fn show(app: &mut LccApp, ui: &mut egui::Ui, side: &[PanelId], in_column: bool) {
    let t = Tokens::get(ui.ctx());
    egui::Frame::NONE.fill(t.chrome).stroke(egui::Stroke::new(1.0, t.divider)).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        // The panel's own Classic header in the left column (issue #75): the chevron folds the
        // list to give the panels below the room, folding under `IN_COLUMN_KEY` so folding there
        // leaves the presets column (Window ▸ Presets) whole. The header keeps room for the Create and More
        // buttons at the right (`header_inset`). Outside the left column (the presets column,
        // Window ▸ Presets, or docking), the panel is drawn whole and does not fold, so it carries no
        // collapse triangle (issue #238).
        let (hr, unfolded) = if in_column {
            super::classic::header_inset(app, ui, PanelId::Presets, side, IN_COLUMN_KEY, 62.0)
        } else {
            let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), HEADER_H), Sense::hover());
            register(ui.ctx(), "panelHeader:presets", r);
            let painter = ui.painter();
            painter.line_segment([pos2(r.left(), r.top()), pos2(r.right(), r.top())], egui::Stroke::new(1.0, t.divider));
            let name = crate::i18n::tr(super::classic::title(PanelId::Presets));
            painter.text(pos2(r.left() + 14.0, r.center().y), Align2::LEFT_CENTER, name, t.semibold(12.5), t.text);
            (r, true)
        };
        let mut hdr = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(Rect::from_min_max(pos2(hr.right() - 80.0, hr.top()), hr.right_bottom()))
                .layout(egui::Layout::right_to_left(egui::Align::Center)),
        );
        let plus = icon_button(&mut hdr, "presetCreate", Icon::Plus, vec2(26.0, 22.0), false, true, crate::i18n::tr("Create Preset or Folder"));
        register(ui.ctx(), "icon:presetCreate", plus.rect);
        egui::Popup::menu(&plus).show(|ui| {
            if ui.add_enabled(app.session.active().is_some(), egui::Button::new(crate::i18n::tr("Create Preset…"))).clicked() {
                app.ui.dialog = Some(crate::state::Dialog::create_preset());
            }
            if ui.button(crate::i18n::tr("Create Folder…")).clicked() {
                app.ui.dialog = Some(crate::state::Dialog::create_preset_folder(None));
            }
        });
        let more = icon_button(&mut hdr, "presetMore", Icon::More, vec2(26.0, 22.0), false, true, crate::i18n::tr("More preset options"));
        register(ui.ctx(), "icon:presetMore", more.rect);
        egui::Popup::menu(&more).show(|ui| {
            let r = ui.checkbox(&mut app.ui.preset_thumbs, crate::i18n::tr("Show Thumbnails"));
            register(ui.ctx(), "presetMenu:thumbnails", r.rect);
            ui.separator();
            if ui.button(crate::i18n::tr("Import Presets…")).clicked() {
                let _ = app.run("file.importPresets", json!({}));
            }
            let any_user = app.session.presets.iter().any(|p| !p.builtin);
            if ui.add_enabled(any_user, egui::Button::new(crate::i18n::tr("Export User Presets…"))).clicked() {
                let _ = app.run("file.exportPresets", json!({}));
            }
        });
        // folded: the header is the whole panel, and the hairline under it closes it off from the
        // panels below
        if !unfolded {
            divider(ui);
            return;
        }
        divider(ui);
        // amount slider (applies to the last applied preset)
        let amt_id = egui::Id::new("preset-amount");
        let last: Option<AppliedPreset> = ui.data(|d| d.get_temp(amt_id));
        if let Some(last) = last.clone().filter(|last| last.is_current(&app.session)) {
            let spec = ControlSpec {
                id: "presetAmount",
                label: "Amount",
                section: Section::Profile,
                min: 0.0,
                max: 200.0,
                default: 100.0,
                step: 1.0,
                decimals: 0,
                track: Track::Plain,
            };
            let out = slider(ui, &spec, last.amount, true, None);
            if let Some(v) = out.value
                && app.run("edit.undo", json!({})).is_ok()
            {
                // Only replace the most recent preset; never undo intervening edits.
                if app.run("preset.apply", json!({"id": &last.id, "amount": v})).is_ok() {
                    AppliedByPhoto::remember(ui.ctx(), &app.session, &last.id);
                    ui.data_mut(|d| d.insert_temp(amt_id, AppliedPreset::new(&app.session, last.id, v)));
                } else {
                    let _ = app.run("edit.redo", json!({}));
                    ui.data_mut(|d| d.remove::<AppliedPreset>(amt_id));
                }
            }
            divider(ui);
        } else if last.as_ref().is_some_and(|last| last.is_stale(&app.session)) {
            // kept across a switch to another photo and back (the slider returns with it); gone once
            // a later edit, undo or redo owns the step
            ui.data_mut(|d| d.remove::<AppliedPreset>(amt_id));
        }
        let active = app.session.active();
        let marked = AppliedByPhoto::of(ui.ctx(), &app.session, active);
        let current = active.and_then(|id| app.session.develop_of(id)).map(|d| (*d).clone());
        let thumbs = app.ui.preset_thumbs && active.is_some();
        let row_h = if thumbs { 54.0 } else { 26.0 };
        let groups = grouped(ui.ctx(), &app.session.presets, &app.session.preset_folders);
        let mut all_user_folders = Vec::new();
        collect_all_user_folders(&groups, &mut all_user_folders);
        all_user_folders.sort();

        let scroll = egui::ScrollArea::vertical().id_salt("presets-scroll").auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            let visible = ui.clip_rect().expand(row_h);
            for node in groups.iter() {
                show_folder_node(app, ui, node, &all_user_folders, row_h, visible, &last, &marked, &current, thumbs, amt_id);
            }
            let (space_r, space_resp) = ui.allocate_exact_size(vec2(ui.available_width(), 40.0), Sense::click());
            if let Some(dragged) = app.ui.dragging_preset_group.as_ref()
                && space_resp.hovered()
            {
                ui.painter().rect_stroke(space_r.shrink(1.0), 2.0, egui::Stroke::new(1.5, t.tool_active), egui::StrokeKind::Inside);
                if ui.input(|i| i.pointer.any_released()) {
                    let _ = app.run("preset.moveGroup", json!({"group": dragged, "parent": null}));
                    app.ui.dragging_preset_group = None;
                }
            }
            crate::widgets::context_menu(&space_resp, |ui| {
                if ui.button(crate::i18n::tr("Create Preset…")).clicked() {
                    app.ui.dialog = Some(crate::state::Dialog::create_preset());
                }
                if ui.button(crate::i18n::tr("Create Folder…")).clicked() {
                    app.ui.dialog = Some(crate::state::Dialog::create_preset_folder(None));
                }
            });
            ui.add_space(30.0);
        });

        // Drop cancelled or ended anywhere
        if ui.input(|i| i.pointer.any_released()) {
            app.ui.dragging_preset = None;
            app.ui.dragging_preset_group = None;
        }

        // Drag feedback tooltip
        if let Some(pos) = ui.ctx().input(|i| i.pointer.hover_pos()) {
            if let Some(pid) = app.ui.dragging_preset.as_ref() {
                if let Some(pr) = app.session.presets.iter().find(|p| &p.id == pid) {
                    egui::Area::new(egui::Id::new("drag-preset"))
                        .order(egui::Order::Tooltip)
                        .interactable(false)
                        .fixed_pos(pos + vec2(14.0, 10.0))
                        .show(ui.ctx(), |ui| {
                            let t = Tokens::get(ui.ctx());
                            egui::Frame::popup(ui.style()).fill(t.chrome).show(ui, |ui| {
                                ui.label(&pr.name);
                            });
                        });
                }
            } else if let Some(grp) = app.ui.dragging_preset_group.as_ref() {
                egui::Area::new(egui::Id::new("drag-preset-group"))
                    .order(egui::Order::Tooltip)
                    .interactable(false)
                    .fixed_pos(pos + vec2(14.0, 10.0))
                    .show(ui.ctx(), |ui| {
                        let t = Tokens::get(ui.ctx());
                        egui::Frame::popup(ui.style()).fill(t.chrome).show(ui, |ui| {
                            ui.label(grp.rsplit_once('/').map(|(_, l)| l).unwrap_or(grp));
                        });
                    });
            }
        }
        let last_hover_id = egui::Id::new("last-preset-hover");
        let pointer_in_scroll = ui.ctx().input(|i| i.pointer.hover_pos()).is_some_and(|pos| scroll.inner_rect.contains(pos));
        if pointer_in_scroll {
            if app.hover_preview.is_none()
                && let (Some(id), Some(curr)) = (active, current.as_ref())
                && let Some((saved_id, saved_preset, saved_label)) = ui.data(|d| d.get_temp::<(lcc_catalog::PhotoId, Preset, String)>(last_hover_id))
                && saved_id == id
            {
                app.hover_preview = Some(HoverPreview { label: saved_label, settings: saved_preset.apply(curr, 1.0) });
            }
        } else {
            ui.data_mut(|d| d.remove::<(lcc_catalog::PhotoId, Preset, String)>(last_hover_id));
        }
    });
}

/// A preset folder / group node in the hierarchy.
#[derive(Clone, Debug)]
pub struct FolderNode {
    pub name: String,
    pub path: String,
    pub builtin: bool,
    pub user: bool,
    pub favorites: bool,
    pub depth: usize,
    pub items: Vec<usize>,
    pub children: Vec<FolderNode>,
}

fn holds_current_preset(node: &FolderNode, app: &LccApp, last_id: Option<&str>, marked_id: Option<&str>) -> bool {
    let matches = |id: &str| last_id == Some(id) || marked_id == Some(id);
    let has_item = node.items.iter().any(|&i| app.session.presets.get(i).is_some_and(|p| matches(&p.id)));
    has_item || node.children.iter().any(|c| holds_current_preset(c, app, last_id, marked_id))
}

fn collect_all_user_folders(nodes: &[FolderNode], out: &mut Vec<String>) {
    for n in nodes {
        if n.user && !n.builtin && !n.favorites && !out.contains(&n.path) {
            out.push(n.path.clone());
        }
        collect_all_user_folders(&n.children, out);
    }
}

#[allow(clippy::too_many_arguments)]
fn show_folder_node(
    app: &mut LccApp,
    ui: &mut egui::Ui,
    node: &FolderNode,
    all_user_folders: &[String],
    row_h: f32,
    visible: Rect,
    last: &Option<AppliedPreset>,
    marked: &Option<String>,
    current: &Option<lcc_develop::DevelopSettings>,
    thumbs: bool,
    amt_id: egui::Id,
) {
    let t = Tokens::get(ui.ctx());
    let (path, name, builtin, favorites) = (node.path.clone(), node.name.clone(), node.builtin, node.favorites);
    let key = if favorites { "presetFavorites".to_string() } else { format!("preset:{path}") };
    let holds_current = holds_current_preset(node, app, last.as_ref().map(|l| l.id.as_str()), marked.as_deref());
    let open = app.ui.group_is_open(&key, holds_current || favorites);
    let indent = node.depth as f32 * 14.0;

    let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 32.0), Sense::click_and_drag());
    let label = if favorites { crate::i18n::tr("Favorites") } else { crate::i18n::builtin_label(&name, builtin && node.depth == 0) };
    crate::widgets::announce(&resp, label.to_string());
    register(ui.ctx(), if favorites { "presetFavorites".to_string() } else { format!("presetGroup:{path}") }, r);

    let is_drop_target = (app.ui.dragging_preset.is_some()
        || app.ui.dragging_preset_group.as_ref().is_some_and(|g| g != &path && !path.starts_with(&format!("{g}/"))))
        && resp.hovered();

    if is_drop_target {
        ui.painter().rect_stroke(r.shrink(1.0), 2.0, egui::Stroke::new(1.5, t.tool_active), egui::StrokeKind::Inside);
        if ui.input(|i| i.pointer.any_released()) {
            if let Some(dragged_pid) = app.ui.dragging_preset.take() {
                let _ = app.run("preset.move", json!({"id": dragged_pid, "group": path}));
            } else if let Some(dragged_grp) = app.ui.dragging_preset_group.take() {
                let _ = app.run("preset.moveGroup", json!({"group": dragged_grp, "parent": path}));
            }
        }
    } else if resp.hovered() {
        ui.painter().rect_filled(r, 0.0, t.hover.gamma_multiply(0.4));
    }

    if resp.drag_started() && node.user && !builtin && !favorites {
        app.ui.dragging_preset_group = Some(path.clone());
    }

    paint(
        ui.painter(),
        Rect::from_center_size(pos2(r.left() + 22.0 + indent, r.center().y), vec2(16.0, 16.0)),
        if open { Icon::ChevronDown } else { Icon::ChevronRight },
        t.text_label,
    );
    ui.painter().text(pos2(r.left() + 36.0 + indent, r.center().y), Align2::LEFT_CENTER, label, t.semibold(13.0), t.text_label);

    if resp.clicked() {
        app.ui.group_open.insert(key, !open);
    }

    if !favorites {
        crate::widgets::context_menu(&resp, |ui| {
            if ui.button(crate::i18n::tr("Create Preset…")).clicked() {
                app.ui.dialog = Some(crate::state::Dialog::create_preset_in(Some(&path)));
            }
            if ui.button(crate::i18n::tr("Create Subfolder…")).clicked() {
                app.ui.dialog = Some(crate::state::Dialog::create_preset_folder(Some(path.clone())));
            }
            if node.user && !builtin {
                ui.separator();
                if ui.button(crate::i18n::tr("Rename…")).clicked() {
                    crate::panels::dialogs::prompt(app, "Rename Folder", "Folder name", &name, "preset.renameGroup", json!({"group": &path}), "name");
                }
                crate::widgets::menu_button(ui, crate::i18n::tr("Move to"), |ui| {
                    if node.depth > 0 && ui.button(crate::i18n::tr("Top Level")).clicked() {
                        let _ = app.run("preset.moveGroup", json!({"group": &path, "parent": null}));
                    }
                    for g in all_user_folders {
                        if g != &path && !g.starts_with(&format!("{path}/")) && ui.button(g).clicked() {
                            let _ = app.run("preset.moveGroup", json!({"group": &path, "parent": g}));
                        }
                    }
                });
                if ui.button(crate::i18n::tr("Delete Folder")).clicked() {
                    let _ = app.run("preset.deleteGroup", json!({"group": &path}));
                }
            }
            ui.separator();
            if ui.button(crate::i18n::tr("Export Folder…")).clicked() {
                let _ = app.run("file.exportPresets", json!({"group": path}));
            }
        });
    }

    if !open {
        return;
    }

    for child in &node.children {
        show_folder_node(app, ui, child, all_user_folders, row_h, visible, last, marked, current, thumbs, amt_id);
    }

    let active = app.session.active();
    let (first, end) = rows_in_view(ui.cursor().top(), row_h, node.items.len(), visible);
    if first > 0 {
        ui.allocate_space(vec2(ui.available_width(), first as f32 * row_h));
    }
    for &i in node.items.get(first..end).unwrap_or_default() {
        let Some(pr) = app.session.presets.get(i).cloned() else { continue };
        let (pid, pname, fav) = (pr.id.clone(), crate::i18n::builtin_label(&pr.name, pr.builtin).to_string(), pr.favorite);
        let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::click_and_drag());
        crate::widgets::announce(&resp, pname.clone());
        register(ui.ctx(), if favorites { format!("presetFav:{pid}") } else { format!("preset:{pid}") }, r);
        let is_last = marked.as_deref() == Some(pid.as_str());

        if resp.drag_started() && !favorites {
            app.ui.dragging_preset = Some(pid.clone());
        }

        if is_last {
            register(ui.ctx(), "presetApplied", r);
            ui.painter().rect_filled(r, 0.0, t.tool_active);
        } else if resp.hovered() {
            ui.painter().rect_filled(r, 0.0, t.hover.gamma_multiply(0.7));
        }
        let look = if thumbs || resp.hovered() { current.as_ref().map(|d| pr.apply(d, 1.0)) } else { None };
        let mut text_x = r.left() + 40.0 + indent;
        if thumbs && let (Some(id), Some(s)) = (active, look.as_ref()) {
            let tr = Rect::from_min_size(pos2(r.left() + 36.0 + indent, r.top() + 5.0), vec2(66.0, row_h - 10.0));
            ui.painter().rect_filled(tr, 2.0, t.canvas);
            if let Some(job) = app.session.variant_job(id, s, THUMB_EDGE)
                && let Some(tex) = app.renderer.variant(job)
            {
                ui.painter().image(tex.tex.id(), tr, cover_uv(tr, tex.size), Color32::WHITE);
            }
            text_x = tr.right() + 10.0;
        }
        ui.painter().text(pos2(text_x, r.center().y), Align2::LEFT_CENTER, &pname, t.font(13.0), t.text_label);
        if fav {
            paint(ui.painter(), Rect::from_center_size(pos2(r.right() - 20.0, r.center().y), vec2(16.0, 16.0)), Icon::StarFilled, t.star);
        }
        if resp.hovered()
            && let Some(s) = look
        {
            let label = crate::i18n::tr_format!("Preset: {name}", name = pname);
            app.hover_preview = Some(HoverPreview { label: label.clone(), settings: s });
            if let Some(id) = active {
                ui.data_mut(|d| d.insert_temp(egui::Id::new("last-preset-hover"), (id, pr.clone(), label)));
            }
        }
        if resp.clicked() && app.run("preset.apply", json!({"id": pid, "amount": 100})).is_ok() {
            AppliedByPhoto::remember(ui.ctx(), &app.session, &pid);
            ui.data_mut(|d| d.insert_temp(amt_id, AppliedPreset::new(&app.session, pid.clone(), 100.0)));
            ui.data_mut(|d| d.remove::<(lcc_catalog::PhotoId, Preset, String)>(egui::Id::new("last-preset-hover")));
            app.toast(ui.ctx(), crate::i18n::tr_format!("Preset: {name}", name = pname));
        }
        let (p_builtin, p_group) = (pr.builtin, pr.group.clone());
        crate::widgets::context_menu(&resp, |ui| {
            if ui.button(crate::i18n::tr(if fav { "Remove from Favorites" } else { "Add to Favorites" })).clicked() {
                let _ = app.run("preset.favorite", json!({"id": pid}));
            }
            if p_builtin {
                return;
            }
            ui.separator();
            if ui.add_enabled(app.session.active().is_some(), egui::Button::new(crate::i18n::tr("Update with Current Settings"))).clicked() {
                match app.run("preset.update", json!({"id": pid})) {
                    Ok(_) => app.toast(ui.ctx(), crate::i18n::tr_format!("Updated “{name}”", name = pname)),
                    Err(e) => app.toast(ui.ctx(), e),
                }
            }
            if ui.button(crate::i18n::tr("Rename…")).clicked() {
                crate::panels::dialogs::prompt(app, "Rename Preset", "Preset name", &pname, "preset.rename", json!({"id": pid}), "name");
            }
            crate::widgets::menu_button(ui, crate::i18n::tr("Move to Group"), |ui| {
                for g in all_user_folders {
                    if g != &p_group && ui.button(g).clicked() {
                        let _ = app.run("preset.move", json!({"id": pid, "group": g}));
                    }
                }
                if ui.button(crate::i18n::tr("New Folder…")).clicked() {
                    crate::panels::dialogs::prompt(app, "Move Preset to New Folder", "Folder name", "", "preset.move", json!({"id": pid}), "group");
                }
            });
            ui.separator();
            if ui.button(crate::i18n::tr("Delete Preset")).clicked() {
                let _ = app.run("preset.delete", json!({"id": pid}));
            }
        });
    }
    if end < node.items.len() {
        ui.allocate_space(vec2(ui.available_width(), (node.items.len() - end) as f32 * row_h));
    }
}

/// The presets by folder (hierarchical tree) after a Favorites group of the starred ones, if any;
/// rebuilt only when the preset list changed (its generation).
fn grouped(ctx: &egui::Context, presets: &PresetList, custom_folders: &[String]) -> Arc<Vec<FolderNode>> {
    let id = egui::Id::new("presets-grouped");
    if let Some((generation, groups)) = ctx.data(|d| d.get_temp::<(u64, Arc<Vec<FolderNode>>)>(id))
        && generation == presets.generation()
    {
        return groups;
    }

    let mut folder_data: BTreeMap<String, (bool, bool, Vec<usize>)> = BTreeMap::new();

    for (i, p) in presets.iter().enumerate() {
        let (b, u, items) = folder_data.entry(p.group.clone()).or_insert_with(|| (false, false, Vec::new()));
        *b |= p.builtin;
        *u |= !p.builtin;
        items.push(i);

        let mut cur = p.group.as_str();
        while let Some((par, _)) = cur.rsplit_once('/') {
            let (pb, pu, _) = folder_data.entry(par.to_string()).or_insert_with(|| (false, false, Vec::new()));
            *pb |= p.builtin;
            *pu |= !p.builtin;
            cur = par;
        }
    }

    for f in custom_folders {
        let trimmed = f.trim();
        if !trimmed.is_empty() {
            folder_data.entry(trimmed.to_string()).or_insert_with(|| (false, true, Vec::new()));
            let mut cur = trimmed;
            while let Some((par, _)) = cur.rsplit_once('/') {
                folder_data.entry(par.to_string()).or_insert_with(|| (false, true, Vec::new()));
                cur = par;
            }
        }
    }

    fn build_nodes(parent_path: Option<&str>, depth: usize, folder_data: &BTreeMap<String, (bool, bool, Vec<usize>)>) -> Vec<FolderNode> {
        let mut direct_children: Vec<(String, String)> = Vec::new();
        for path in folder_data.keys() {
            match parent_path {
                None => {
                    if !path.contains('/') {
                        direct_children.push((path.clone(), path.clone()));
                    }
                }
                Some(parent) => {
                    if let Some(rest) = path.strip_prefix(parent)
                        && let Some(sub) = rest.strip_prefix('/')
                        && !sub.contains('/')
                    {
                        direct_children.push((sub.to_string(), path.clone()));
                    }
                }
            }
        }

        direct_children.sort_by_key(|a| a.0.to_lowercase());

        let mut nodes = Vec::new();
        for (name, full_path) in direct_children {
            let (builtin, user, items) = folder_data.get(&full_path).cloned().unwrap_or((false, false, Vec::new()));
            let children = build_nodes(Some(&full_path), depth + 1, folder_data);
            let any_builtin = builtin || children.iter().any(|c| c.builtin);
            let any_user = user || children.iter().any(|c| c.user);
            nodes.push(FolderNode { name, path: full_path, builtin: any_builtin, user: any_user, favorites: false, depth, items, children });
        }
        nodes
    }

    let mut roots = build_nodes(None, 0, &folder_data);

    fn collect_starred(nodes: &[FolderNode], presets: &PresetList) -> Vec<usize> {
        let mut starred = Vec::new();
        for node in nodes {
            for &i in &node.items {
                if presets.get(i).is_some_and(|p| p.favorite) && !starred.contains(&i) {
                    starred.push(i);
                }
            }
            starred.extend(collect_starred(&node.children, presets));
        }
        starred
    }

    let starred = collect_starred(&roots, presets);
    if !starred.is_empty() {
        roots.insert(
            0,
            FolderNode {
                name: "Favorites".into(),
                path: "presetFavorites".into(),
                builtin: false,
                user: false,
                favorites: true,
                depth: 0,
                items: starred,
                children: Vec::new(),
            },
        );
    }

    let groups = Arc::new(roots);
    ctx.data_mut(|d| d.insert_temp(id, (presets.generation(), groups.clone())));
    groups
}

/// The presets as the rows of a picker (a combo box's list), in list order with a header row
/// wherever the group changes. Only the rows in view are built (10 000+ presets made an open
/// picker take seconds a frame). Returns the id of the preset clicked.
pub fn picker_rows(ui: &mut egui::Ui, salt: &str, presets: &PresetList, selected: Option<&str>) -> Option<String> {
    let id = egui::Id::new("preset-picker-rows");
    let rows = match ui.ctx().data(|d| d.get_temp::<(u64, Arc<Vec<PickRow>>)>(id)) {
        Some((generation, rows)) if generation == presets.generation() => rows,
        _ => {
            let mut rows = Vec::with_capacity(presets.len() + 16);
            let mut group: Option<&str> = None;
            for (i, p) in presets.iter().enumerate() {
                if group != Some(p.group.as_str()) {
                    group = Some(&p.group);
                    rows.push(PickRow::Header(crate::i18n::builtin_label(&p.group, p.builtin).to_string()));
                }
                rows.push(PickRow::Preset(i));
            }
            let rows = Arc::new(rows);
            ui.ctx().data_mut(|d| d.insert_temp(id, (presets.generation(), rows.clone())));
            rows
        }
    };
    let row_h = ui.spacing().interact_size.y;
    let mut out = None;
    egui::ScrollArea::vertical().id_salt(("preset-picker", salt)).max_height(300.0).auto_shrink([false, true]).show_rows(
        ui,
        row_h,
        rows.len(),
        |ui, range| {
            for row in rows.get(range).unwrap_or_default() {
                let size = vec2(ui.available_width(), row_h);
                match row {
                    PickRow::Header(g) => {
                        ui.add_sized(size, egui::Label::new(egui::RichText::new(g).size(10.5).weak()));
                    }
                    PickRow::Preset(i) => {
                        let Some(p) = presets.get(*i) else { continue };
                        let label = crate::i18n::builtin_label(&p.name, p.builtin);
                        if ui.add_sized(size, egui::Button::selectable(selected == Some(p.id.as_str()), label)).clicked() {
                            out = Some(p.id.clone());
                        }
                    }
                }
            }
        },
    );
    out
}

#[derive(Clone)]
enum PickRow {
    /// A group's name (in the UI language for stock groups).
    Header(String),
    /// An index into the session's presets.
    Preset(usize),
}

/// The rows `first..end` of a block of `n` rows `row_h` high starting at `top` that meet
/// `visible`.
fn rows_in_view(top: f32, row_h: f32, n: usize, visible: Rect) -> (usize, usize) {
    if row_h <= 0.0 || !top.is_finite() {
        return (0, n);
    }
    // `as usize` saturates (negative and NaN give 0)
    let first = (((visible.top() - top) / row_h).floor() as usize).min(n);
    let end = (((visible.bottom() - top) / row_h).ceil() as usize).clamp(first, n);
    (first, end)
}

/// The part of a `size` image that covers `r` (centre crop), as texture coordinates.
pub fn cover_uv(r: Rect, size: [usize; 2]) -> Rect {
    let a = size[0] as f32 / size[1].max(1) as f32;
    let ra = r.width() / r.height().max(1.0);
    if a > ra {
        let f = ra / a;
        Rect::from_min_max(pos2((1.0 - f) / 2.0, 0.0), pos2((1.0 + f) / 2.0, 1.0))
    } else {
        let f = a / ra;
        Rect::from_min_max(pos2(0.0, (1.0 - f) / 2.0), pos2(1.0, (1.0 + f) / 2.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amount_slider_does_not_undo_a_later_edit() {
        let mut session = lcc_engine::Session::with_demo();
        let pid = "lc.golden-hour";
        session.execute("preset.apply", &json!({"id": pid})).unwrap();
        let state = AppliedPreset::new(&session, pid.into(), 100.0);
        assert!(state.is_current(&session));

        session.execute("develop.set", &json!({"values": {"light.exposure": 0.75}})).unwrap();
        assert!(!state.is_current(&session), "a newer edit owns the undo step");
    }

    /// Issue #1999: starred presets are listed first, in a Favorites group, in the order the panel
    /// shows them below, and stay in their own groups; a user group named "Favorites" is not it.
    #[test]
    fn favorites_group_comes_first_in_panel_order() {
        let mut session = lcc_engine::Session::with_demo();
        let base = session.presets.iter().find(|p| p.id == "lc.warm-glow").unwrap().clone();
        for (id, group, favorite) in [("user.mine", "Mine", true), ("user.plain", "Favorites", false)] {
            session.presets.push(Preset { id: id.into(), name: id.into(), group: group.into(), favorite, builtin: false, ..base.clone() });
        }
        for id in ["lc.warm-glow", "lc.bw-selenium"] {
            session.execute("preset.favorite", &json!({"id": id})).unwrap();
        }
        let index = |id: &str| session.presets.iter().position(|p| p.id == id).unwrap();
        let (warm, bw, mine, plain) = (index("lc.warm-glow"), index("lc.bw-selenium"), index("user.mine"), index("user.plain"));
        assert!(warm < bw, "list order differs from panel order");

        let ctx = egui::Context::default();
        let groups = grouped(&ctx, &session.presets, &session.preset_folders);
        assert_eq!(groups[0].name, "Favorites");
        assert!(!groups[0].user && !groups[0].builtin, "not a Move to Group target");
        // B&W, then Color, then the user groups
        assert_eq!(groups[0].items, vec![bw, warm, mine]);
        for i in [bw, warm, mine] {
            assert!(groups[1..].iter().any(|g| g.items.contains(&i)), "a favourite stays in its own group");
        }
        let named: Vec<&FolderNode> = groups[1..].iter().filter(|g| g.name == "Favorites").collect();
        assert_eq!(named.len(), 1);
        assert!(named[0].user);
        assert_eq!(named[0].items, vec![plain]);

        for id in ["lc.warm-glow", "lc.bw-selenium", "user.mine"] {
            session.execute("preset.favorite", &json!({"id": id})).unwrap();
        }
        let groups = grouped(&ctx, &session.presets, &session.preset_folders);
        assert_eq!(groups[0].name, "B&W", "no Favorites group without favourites");
    }

    #[test]
    fn amount_slider_does_not_undo_on_another_photo() {
        let mut session = lcc_engine::Session::with_demo();
        let pid = "lc.golden-hour";
        session.execute("preset.apply", &json!({"id": pid})).unwrap();
        let state = AppliedPreset::new(&session, pid.into(), 100.0);
        let active = session.active();
        let other = session.visible().iter().copied().find(|id| Some(*id) != active).unwrap();
        session.execute("library.select", &json!({"ids": [other.0]})).unwrap();
        assert!(!state.is_current(&session), "the preset belongs to the earlier selection");
    }

    /// Issue #468: Preset folders and nested subfolders.
    #[test]
    fn nested_preset_folders_structure_and_hierarchy() {
        let mut session = lcc_engine::Session::with_demo();
        let base = session.presets.iter().find(|p| p.id == "lc.warm-glow").unwrap().clone();
        session.presets.push(Preset {
            id: "user.mastin-direct".into(),
            name: "Direct Mastin".into(),
            group: "Mastin Labs".into(),
            builtin: false,
            ..base.clone()
        });
        session.presets.push(Preset {
            id: "user.mastin-portra".into(),
            name: "Portra 400".into(),
            group: "Mastin Labs/Portra Original".into(),
            builtin: false,
            ..base.clone()
        });
        session.presets.push(Preset {
            id: "user.mastin-adv".into(),
            name: "Adventure 01".into(),
            group: "Mastin Labs/Adventure Everyday".into(),
            builtin: false,
            ..base.clone()
        });

        let ctx = egui::Context::default();
        let groups = grouped(&ctx, &session.presets, &session.preset_folders);

        let mastin = groups.iter().find(|g| g.name == "Mastin Labs").expect("Mastin Labs folder at root");
        assert_eq!(mastin.depth, 0);
        assert_eq!(mastin.path, "Mastin Labs");
        assert_eq!(mastin.items.len(), 1, "direct preset in Mastin Labs");
        assert_eq!(mastin.children.len(), 2, "two subfolders under Mastin Labs");
        assert_eq!(mastin.children[0].name, "Adventure Everyday");
        assert_eq!(mastin.children[0].path, "Mastin Labs/Adventure Everyday");
        assert_eq!(mastin.children[0].depth, 1);
        assert_eq!(mastin.children[1].name, "Portra Original");
        assert_eq!(mastin.children[1].path, "Mastin Labs/Portra Original");
        assert_eq!(mastin.children[1].depth, 1);
    }

    /// Issue #468: Create, rename, move and delete preset folders.
    #[test]
    fn preset_folder_commands_crud() {
        let mut session = lcc_engine::Session::with_demo();
        let base = session.presets.iter().find(|p| p.id == "lc.warm-glow").unwrap().clone();

        // 1. Create folders (including empty parent and child)
        session.execute("preset.createGroup", &json!({"name": "Mastin Labs"})).unwrap();
        session.execute("preset.createGroup", &json!({"name": "Portra Original", "parent": "Mastin Labs"})).unwrap();
        assert!(session.preset_folders.contains(&"Mastin Labs".to_string()));
        assert!(session.preset_folders.contains(&"Mastin Labs/Portra Original".to_string()));

        // Add a preset into the folder
        session.presets.push(Preset {
            id: "user.p400".into(),
            name: "Portra 400".into(),
            group: "Mastin Labs/Portra Original".into(),
            builtin: false,
            ..base
        });

        // 2. Rename folder
        session.execute("preset.renameGroup", &json!({"group": "Mastin Labs/Portra Original", "name": "Portra Pack"})).unwrap();
        assert!(session.preset_folders.contains(&"Mastin Labs/Portra Pack".to_string()));
        let p = session.presets.iter().find(|p| p.id == "user.p400").unwrap();
        assert_eq!(p.group, "Mastin Labs/Portra Pack");

        // 3. Move folder to top level
        session.execute("preset.moveGroup", &json!({"group": "Mastin Labs/Portra Pack", "parent": null})).unwrap();
        let p = session.presets.iter().find(|p| p.id == "user.p400").unwrap();
        assert_eq!(p.group, "Portra Pack");

        // 4. Move folder back into Mastin Labs
        session.execute("preset.moveGroup", &json!({"group": "Portra Pack", "parent": "Mastin Labs"})).unwrap();
        let p = session.presets.iter().find(|p| p.id == "user.p400").unwrap();
        assert_eq!(p.group, "Mastin Labs/Portra Pack");

        // 5. Delete folder deletes presets inside it
        session.execute("preset.deleteGroup", &json!({"group": "Mastin Labs"})).unwrap();
        assert!(!session.presets.iter().any(|p| p.id == "user.p400"));
        assert!(!session.preset_folders.iter().any(|f| f.starts_with("Mastin Labs")));
    }
}
