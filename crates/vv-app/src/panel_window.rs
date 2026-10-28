use egui::{Context, Event, Id, Key, Modifiers, RawInput, Rect, Ui, ViewportClass, ViewportId};
use rust_i18n::t;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Placement {
    pub detached: bool,
    pub size: [f32; 2],
}

impl Placement {
    fn size(self, default: [f32; 2]) -> [f32; 2] {
        if self
            .size
            .iter()
            .all(|n| n.is_finite() && *n >= 160.0 && *n <= 8192.0)
        {
            self.size
        } else {
            default
        }
    }
}

#[derive(Clone, Default)]
struct Forwarded {
    events: Vec<Event>,
    keys: Vec<Key>,
    modifiers: Modifiers,
    source: Option<&'static str>,
}

fn record_input(ui: &Ui, id: &'static str) {
    let ctx = ui.ctx();
    let held = ui.input(|i| i.pointer.any_down()) || egui::Popup::is_any_open(ctx);
    ctx.data_mut(|d| d.insert_temp(Id::new(("panel-held", id)), held));
    if !ui.input(|i| i.focused) {
        return;
    }
    let frame = ctx.cumulative_frame_nr();
    if ctx.data(|d| d.get_temp::<u64>(Id::new(("panel-forwarded-frame", id)))) == Some(frame) {
        return;
    }
    let typing = ctx.egui_wants_keyboard_input() || egui::Popup::is_any_open(ctx);
    let forwarded = ui.input(|input| Forwarded {
        events: if typing {
            Vec::new()
        } else {
            input
                .events
                .iter()
                .filter(|event| {
                    matches!(
                        event,
                        Event::Key { .. } | Event::Copy | Event::Cut | Event::Paste(_)
                    )
                })
                .cloned()
                .collect()
        },
        keys: if typing {
            Vec::new()
        } else {
            input.keys_down.iter().copied().collect()
        },
        modifiers: input.modifiers,
        source: Some(id),
    });
    ctx.data_mut(|d| {
        d.insert_temp(Id::new("panel-forwarded"), forwarded);
        d.insert_temp(Id::new(("panel-forwarded-frame", id)), frame);
    });
    if ui.input(|i| !i.keys_down.is_empty()) {
        ctx.request_repaint();
    }
}

pub(crate) fn forward_to_root(ctx: &Context, raw: &mut RawInput) -> (bool, Option<&'static str>) {
    if raw.viewport_id != ViewportId::ROOT {
        return (false, None);
    }
    let forwarded = ctx
        .data_mut(|d| d.remove_temp::<Forwarded>(Id::new("panel-forwarded")))
        .unwrap_or_default();
    let previous = ctx
        .data(|d| d.get_temp::<Vec<Key>>(Id::new("panel-forwarded-keys")))
        .unwrap_or_default();
    for key in &previous {
        if !forwarded.keys.contains(key)
            && !raw
                .events
                .iter()
                .any(|e| matches!(e,Event::Key{key:k,pressed:true,..} if k==key))
        {
            raw.events.push(Event::Key {
                key: *key,
                physical_key: None,
                pressed: false,
                repeat: false,
                modifiers: forwarded.modifiers,
            });
        }
    }
    if forwarded.source.is_some() {
        ctx.memory_mut(|m| m.stop_text_input());
        raw.focused = true;
        raw.events
            .push(Event::ModifiersChanged(forwarded.modifiers));
        for key in &forwarded.keys {
            if !previous.contains(key)
                && !forwarded
                    .events
                    .iter()
                    .any(|e| matches!(e,Event::Key{key:k,pressed:true,..} if k==key))
            {
                raw.events.push(Event::Key {
                    key: *key,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: forwarded.modifiers,
                });
            }
        }
        raw.events.extend(forwarded.events);
    }
    let previous_source = ctx
        .data(|d| d.get_temp::<bool>(Id::new("panel-forwarded-active")))
        .unwrap_or(false);
    let active = forwarded.source.is_some() || previous_source || !previous.is_empty();
    if forwarded.source.is_none() && previous_source && !raw.focused {
        raw.events.push(Event::ModifiersChanged(Modifiers::NONE));
    }
    ctx.data_mut(|d| {
        d.insert_temp(Id::new("panel-forwarded-keys"), forwarded.keys);
        d.insert_temp(
            Id::new("panel-forwarded-active"),
            forwarded.source.is_some(),
        );
    });
    (active, forwarded.source)
}

pub(crate) fn interaction_held(ctx: &Context) -> bool {
    ["audio_mixer", "color_window", "keyframe_editor"]
        .into_iter()
        .any(|id| {
            ctx.data(|d| {
                d.get_temp::<bool>(Id::new(("panel-held", id)))
                    .unwrap_or(false)
            })
        })
}

pub(crate) fn begin_frame(ctx: &Context) {
    ctx.data_mut(|d| {
        for id in ["audio_mixer", "color_window", "keyframe_editor"] {
            d.insert_temp(Id::new(("panel-held", id)), false);
        }
    });
}

pub(crate) fn show(
    ctx: &Context,
    id: &'static str,
    title: &str,
    default_size: [f32; 2],
    open: &mut bool,
    placement: &mut Placement,
    mut contents: impl FnMut(&mut Ui),
) -> Option<Rect> {
    if !*open {
        return None;
    }
    if placement.detached && !ctx.embed_viewports() {
        let size = placement.size(default_size);
        let viewport = ViewportId::from_hash_of(("detached-panel", id));
        let rect = ctx.show_viewport_immediate(
            viewport,
            egui::ViewportBuilder::default()
                .with_title(title)
                .with_inner_size(size)
                .with_min_inner_size([240.0, 160.0]),
            |ui, class| {
                if class == ViewportClass::EmbeddedWindow {
                    placement.detached = false;
                }
                if ui.input(|i| i.viewport().close_requested()) {
                    *open = false;
                    return ui.max_rect();
                }
                egui::CentralPanel::default().show(ui, |ui| {
                    if ui.button(t!("panels.reattach")).clicked() {
                        placement.detached = false;
                        ui.ctx().request_repaint();
                    }
                    ui.separator();
                    contents(ui);
                    record_input(ui, id);
                });
                if let Some(rect) = ui.input(|i| i.viewport().inner_rect) {
                    placement.size = [rect.width(), rect.height()];
                }
                ui.max_rect()
            },
        );
        Some(rect)
    } else {
        placement.detached = false;
        egui::Window::new(title)
            .id(Id::new(id))
            .open(open)
            .default_size(placement.size(default_size))
            .show(ctx, |ui| {
                if !ctx.embed_viewports() && ui.button(t!("panels.detach")).clicked() {
                    placement.detached = true;
                    ctx.request_repaint();
                }
                contents(ui);
            })
            .map(|window| window.response.rect)
    }
}

#[cfg(test)]
#[path = "tests/panel_window.rs"]
mod tests;
