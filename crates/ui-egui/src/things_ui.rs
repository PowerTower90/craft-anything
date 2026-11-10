//! The gallery's things: one card per thing (an enemy, a character with its outfits, a voice
//! group) with its preview and what it holds, and the Inspector's view of a thing's files by role.
//! "Show individual files" goes back to the file grid. The grouping is the engine's
//! ([`craft_anything_engine::asset_things`]); the cards read the gallery's list.

use crate::assets_ui::count;
use crate::i18n::{tr, trf};
use crate::project::Thumb;
use crate::theme::{self, Tokens};
use crate::{CraftApp, icons, widgets};
use craft_anything_engine::asset_things::{self, ROLES, Role, Thing};
use craft_anything_engine::inventory::Item;
use egui::{Color32, RichText, Sense, Ui, Vec2};

const CARD_W: f32 = 150.0;
const CARD_H: f32 = 176.0;
const GAP: f32 = 8.0;
/// Files listed per role in the Inspector.
const LISTED: usize = 40;

/// "3 outfits · 2 models · 6 textures · 34 sounds".
pub fn badge_text(t: &Thing) -> String {
    asset_things::badge(t)
        .into_iter()
        .map(|(what, n)| {
            let key = match what.as_str() {
                "outfits" => "{n} outfits",
                "models" => "{n} models",
                "parts" => "{n} parts",
                "textures" => "{n} textures",
                "animation" => "{n} animation",
                "sounds" => "{n} sounds",
                _ => "{n} voice lines",
            };
            trf(key, &[("n", &count(n as u64))])
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// The cards, a row at a time, drawn only for the rows in view.
pub fn grid(app: &mut CraftApp, ui: &mut Ui) {
    let t = Tokens::get(ui.ctx());
    let things: Vec<Thing> = app.assets.things().to_vec();
    if things.is_empty() {
        widgets::empty_state(ui, "search", "No things here", "Show individual files to see every file.");
        return;
    }
    let cols = (((ui.available_width() + GAP) / (CARD_W + GAP)).floor() as usize).max(1);
    let rows = things.len().div_ceil(cols);
    let mut want: Vec<(String, String)> = Vec::new();
    let mut picked = None;
    ui.spacing_mut().item_spacing = Vec2::splat(GAP);
    egui::ScrollArea::vertical().id_salt("thing-grid").auto_shrink([false, false]).show_rows(ui, CARD_H, rows, |ui, range| {
        for r in range {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = GAP;
                for c in 0..cols {
                    let Some(th) = things.get(r * cols + c) else { break };
                    let thumb = th.preview_item.as_ref().and_then(|id| app.assets.thumb(id));
                    if let (None, Some(id), Some(rel)) = (thumb, &th.preview_item, &th.preview) {
                        want.push((id.clone(), rel.clone()));
                    }
                    let focused = app.assets.thing_focus.as_deref() == Some(th.key.as_str());
                    if card(ui, th, thumb, focused, &t).clicked() {
                        picked = Some(th.key.clone());
                    }
                }
            });
        }
    });
    let ctx = ui.ctx().clone();
    crate::assets_ui::want_thumbs(app, &ctx, want);
    if let Some(k) = picked {
        app.assets.thing_focus = Some(k);
    }
}

fn card(ui: &mut Ui, th: &Thing, thumb: Option<&Thumb>, focused: bool, t: &Tokens) -> egui::Response {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(CARD_W, CARD_H), Sense::hover());
    let resp = ui.interact(rect, egui::Id::new(("thing-card", th.key.as_str())), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, focused, &th.name));
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let fill = if focused { t.accent_soft } else { theme::mix(t.card, t.hover, widgets::hover_t(ui, &resp)) };
    let p = ui.painter();
    p.rect(rect, t.radius, fill, egui::Stroke::new(1.0, if focused { t.accent } else { t.card_border }), egui::StrokeKind::Inside);
    let pic = egui::Rect::from_min_size(rect.min + Vec2::new(6.0, 6.0), Vec2::new(CARD_W - 12.0, CARD_H - 62.0));
    p.rect_filled(pic, t.radius_sm, t.raised);
    match thumb {
        Some(Thumb::Ready(tex)) => {
            let [w, h] = tex.size();
            let scale = (pic.width() / w.max(1) as f32).min(pic.height() / h.max(1) as f32);
            let fit = egui::Rect::from_center_size(pic.center(), Vec2::new(w as f32 * scale, h as f32 * scale));
            p.image(tex.id(), fit, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
        }
        Some(Thumb::Loading) => {}
        _ => icons::paint(
            ui,
            pic,
            if th.sounds_only {
                "audio-lines"
            } else if th.has_model {
                "box"
            } else {
                "image"
            },
            32.0,
            t.text_faint,
        ),
    }
    let p = ui.painter();
    let clip = rect.shrink2(Vec2::new(6.0, 0.0));
    let name = p.layout_no_wrap(th.name.clone(), theme::semibold(theme::CAPTION), t.text);
    p.with_clip_rect(clip).galley(egui::pos2(rect.left() + 6.0, pic.bottom() + 6.0), name, t.text);
    let files = p.layout_no_wrap(trf("{n} files", &[("n", &count(th.files as u64))]), egui::FontId::proportional(theme::CAPTION), t.text_dim);
    p.with_clip_rect(clip).galley(egui::pos2(rect.left() + 6.0, pic.bottom() + 24.0), files, t.text_dim);
    let badge = p.layout_no_wrap(badge_text(th), egui::FontId::proportional(theme::CAPTION), t.text_faint);
    p.with_clip_rect(clip).galley(egui::pos2(rect.left() + 6.0, pic.bottom() + 40.0), badge, t.text_faint);
    let tip = if th.variants.is_empty() { badge_text(th) } else { format!("{}\n{}", badge_text(th), th.variants.join(", ")) };
    resp.on_hover_text(tip)
}

/// The Inspector for a focused thing: its name, outfits, and its files grouped by role; a file
/// opens in the file grid.
pub fn inspector(app: &mut CraftApp, ui: &mut Ui) -> bool {
    let t = Tokens::get(ui.ctx());
    let Some(key) = app.assets.thing_focus.clone() else { return false };
    let Some(th) = app.assets.things().iter().find(|x| x.key == key).cloned() else { return false };
    let files: Vec<Item> = app.assets.items.iter().filter(|i| i.thing.as_ref().is_some_and(|x| x.key == key)).cloned().collect();
    let mut open = None;
    let mut rebuild = false;
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        widgets::panel(&t).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(&th.name).font(theme::semibold(theme::BODY)));
            ui.label(RichText::new(trf("{n} files", &[("n", &count(th.files as u64))])).color(t.text_dim).size(theme::CAPTION));
            if !th.variants.is_empty() {
                ui.add(egui::Label::new(RichText::new(trf("Outfits: {list}", &[("list", &th.variants.join(", "))])).size(theme::CAPTION)).wrap());
            }
            ui.add_space(6.0);
            if widgets::secondary(ui, Some("hammer"), "Rebuild this…").clicked() {
                rebuild = true;
            }
        });
        ui.add_space(8.0);
        for role in ROLES {
            let of: Vec<&Item> = files.iter().filter(|i| i.role.unwrap_or_else(|| asset_things::role_of(i)) == *role).collect();
            if of.is_empty() {
                continue;
            }
            widgets::caption(ui, role.title());
            ui.label(RichText::new(count(of.len() as u64)).font(theme::mono(theme::CAPTION)).color(t.text_faint));
            if role.technical() {
                ui.label(RichText::new(tr("Made with the model, not on their own.")).color(t.text_faint).size(theme::CAPTION));
            }
            for i in of.iter().take(LISTED) {
                let words = match i.thing.as_ref().and_then(|x| x.variant.as_deref()) {
                    Some(v) => format!("{} · {v}", i.name),
                    None => i.name.clone(),
                };
                if ui.add(egui::Label::new(RichText::new(words).size(theme::CAPTION)).truncate().sense(Sense::click())).clicked() {
                    open = Some(i.id.clone());
                }
            }
            if of.len() > LISTED {
                ui.label(RichText::new(trf("and {n} more", &[("n", &count((of.len() - LISTED) as u64))])).color(t.text_faint).size(theme::CAPTION));
            }
            ui.add_space(8.0);
        }
    });
    if let Some(id) = open {
        app.assets.show_files = true;
        app.assets.selected = [id.clone()].into_iter().collect();
        app.assets.focus = Some(id);
    }
    if rebuild {
        let ids: Vec<String> = files.iter().map(|i| i.id.clone()).collect();
        app.open_rebuild_plan(None, ids);
    }
    true
}

/// The role's title, for the Inspector's headings.
pub fn role_title(r: Role) -> String {
    tr(r.title())
}
