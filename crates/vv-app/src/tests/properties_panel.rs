use super::*;

struct FadePanel {
    app: VenturiApp,
    ctx: egui::Context,
    targets: Vec<PanelTarget>,
    time: f64,
}

impl FadePanel {
    fn new(fps: vv_core::Rational) -> Self {
        let mut app = VenturiApp::default();
        let timeline = app.create_timeline("Fades".into(), fps, (1280, 720));
        app.open_timeline(timeline);
        app.properties_tab = PropertiesTab::Audio;
        app.settings.panels.inspector_open = true;
        app.settings.panels.inspector_width = 400.0;
        let track = app.session.project.timelines[timeline]
            .tracks
            .iter()
            .position(|t| t.kind == vv_core::TrackKind::Audio)
            .unwrap();
        let mut targets = Vec::new();
        for (start, len) in [(0, 10), (20, 100)] {
            let clip_id = app.session.project.alloc_clip_id();
            app.session.project.timelines[timeline].tracks[track]
                .clips
                .push(vv_core::Clip::from_source_range(
                    clip_id,
                    vv_core::ClipSource::SolidColor,
                    0,
                    len,
                    start,
                    vv_core::Rational::one(),
                ));
            targets.push(PanelTarget {
                timeline,
                track_index: track,
                clip_id,
                source_frame: 0,
                timeline_start: start,
                is_solid_color: false,
                is_text: false,
            });
        }
        app.timeline_state.set_selection(
            targets.iter().map(|t| (t.track_index, t.clip_id)).collect(),
            None,
        );
        Self {
            app,
            ctx: egui::Context::default(),
            targets,
            time: 0.0,
        }
    }

    fn frame(&mut self, events: Vec<egui::Event>) -> egui::FullOutput {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1280.0, 900.0),
            )),
            time: Some(self.time),
            events,
            ..Default::default()
        };
        self.time += 1.0 / 60.0;
        let mut output = self.ctx.run_ui(input, |ui| {
            let (commands, _) = self.app.show_properties_panel(ui, &[], &self.targets);
            self.app
                .apply_effect_changes(commands, ui.input(|i| i.pointer.any_down()));
        });
        output.textures_delta.clear();
        output
    }

    fn field(&mut self, label: &str) -> egui::Pos2 {
        self.frame(vec![]);
        let output = self.frame(vec![]);
        let text = output.shapes.iter().find_map(|shape| match &shape.shape {
            egui::Shape::Text(text) if text.galley.text() == label => Some(text),
            _ => None,
        });
        let text = text.unwrap_or_else(|| panic!("{label:?} is visible in the Audio tab"));
        let y = text.pos.y + text.galley.size().y / 2.0;
        output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(text)
                    if text.galley.text().ends_with(" s")
                        && (text.pos.y + text.galley.size().y / 2.0 - y).abs() < 2.0 =>
                {
                    Some(text.pos + text.galley.size() / 2.0)
                }
                _ => None,
            })
            .expect("the fade field is beside its label")
    }

    fn fades(&self, edge: vv_core::FadeEdge) -> Vec<FrameIdx> {
        self.targets
            .iter()
            .map(|t| {
                let clip = self.app.session.project.timelines[t.timeline]
                    .clip(t.track_index, t.clip_id)
                    .unwrap();
                match edge {
                    vv_core::FadeEdge::In => clip.fade_in,
                    vv_core::FadeEdge::Out => clip.fade_out,
                }
            })
            .collect()
    }

    fn button(pos: egui::Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        }
    }

    fn drag(&mut self, label: &str, steps: usize, motion: f32) {
        let pos = self.field(label);
        self.frame(vec![egui::Event::PointerMoved(pos)]);
        self.frame(vec![Self::button(pos, true)]);
        self.frame(vec![egui::Event::PointerMoved(pos + egui::vec2(8.0, 0.0))]);
        for _ in 0..steps {
            self.frame(vec![egui::Event::MouseMoved(egui::vec2(motion, 0.0))]);
        }
        self.frame(vec![Self::button(pos, false)]);
    }
}

#[test]
fn fade_fields_accumulate_slow_drags_and_undo_both_clips() {
    for (edge, label) in [
        (vv_core::FadeEdge::In, t!("props.fade_in")),
        (vv_core::FadeEdge::Out, t!("props.fade_out")),
    ] {
        let mut panel = FadePanel::new(vv_core::Rational::new(24, 1));
        let before = panel.app.session.history.position();
        panel.drag(&label, 20, 1.0);
        assert_eq!(panel.fades(edge), [10, 10]);
        assert_eq!(panel.app.session.history.position(), before + 1);
        panel.app.undo();
        assert_eq!(panel.fades(edge), [0, 0]);
        panel.app.redo();
        assert_eq!(panel.fades(edge), [10, 10]);
    }
}

#[test]
fn fade_drag_continues_past_the_first_clips_length() {
    let mut panel = FadePanel::new(vv_core::Rational::new(24, 1));
    panel.drag(&t!("props.fade_in"), 50, 1.0);
    assert_eq!(panel.fades(vv_core::FadeEdge::In), [10, 24]);
}

#[test]
fn fade_fields_convert_drags_below_one_frame_per_second() {
    let mut panel = FadePanel::new(vv_core::Rational::new(1, 2));
    panel.drag(&t!("props.fade_in"), 4, 50.0);
    assert_eq!(panel.fades(vv_core::FadeEdge::In), [2, 2]);
}

#[test]
fn fade_fields_display_seconds_below_one_frame_per_second() {
    let mut panel = FadePanel::new(vv_core::Rational::new(1, 2));
    for target in &panel.targets {
        let clip = &mut panel.app.session.project.timelines[target.timeline].tracks
            [target.track_index]
            .clips
            .iter_mut()
            .find(|clip| clip.id == target.clip_id)
            .unwrap();
        clip.fade_in = 2;
    }
    panel.frame(vec![]);
    let output = panel.frame(vec![]);
    let values: Vec<_> = output
        .shapes
        .iter()
        .filter_map(|shape| match &shape.shape {
            egui::Shape::Text(text) => Some(text.galley.text().to_owned()),
            _ => None,
        })
        .collect();
    assert!(values.iter().any(|v| v == "4.00"), "{values:?}");
}
