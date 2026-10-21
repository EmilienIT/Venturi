use super::*;
use crate::{ClipFilter, FilterKind, Interpolation};

fn at(shake: &CameraShakeTracks, frame: FrameIdx) -> Transform {
    let mut transform = Transform::default();
    shake.apply(&mut transform, frame, 25.0, (1920, 1080));
    transform
}

#[test]
fn the_same_frame_is_identical_after_seeks_and_across_sessions() {
    let shake = CameraShakeTracks::default();
    let expected = at(&shake, 41);
    assert_ne!(expected, at(&shake, 42));
    at(&shake, 9000);
    at(&shake, -10);
    assert_eq!(at(&shake, 41), expected);
    let loaded: CameraShakeTracks = ron::from_str(&ron::to_string(&shake).unwrap()).unwrap();
    assert_eq!(at(&loaded, 41), expected);
}

#[test]
fn zero_motion_leaves_every_manual_transform_unchanged() {
    let mut shake = CameraShakeTracks::default();
    shake.track_mut(ShakeParam::MotionScale).default = 0.0;
    let original = Transform {
        position: [123.0, -47.0],
        anchor: [11.0, 20.0],
        rotation: 34.0,
        zoom: [0.7, 1.3],
        crop: [1.0, 2.0, 3.0, 4.0],
        opacity: 37.0,
        ..Default::default()
    };
    let mut transform = original;
    shake.apply(&mut transform, 51, 30.0, (1920, 1080));
    assert_eq!(transform, original);
}

#[test]
fn seed_changes_motion_and_each_axis_can_be_disabled() {
    let mut shake = CameraShakeTracks::default();
    let first = at(&shake, 19);
    shake.seed = 12345;
    assert_ne!(at(&shake, 19), first);
    shake.auto_zoom = false;
    for p in [ShakeParam::Tilt, ShakeParam::Rotation, ShakeParam::Zoom] {
        shake.track_mut(p).default = 0.0;
    }
    let transform = at(&shake, 19);
    assert_ne!(transform.position[0], 0.0);
    assert_eq!(transform.position[1], 0.0);
    assert_eq!(transform.rotation, 0.0);
    assert_eq!(transform.zoom, [1.0; 2]);
}

#[test]
fn the_clock_is_in_seconds_at_different_frame_rates() {
    let shake = CameraShakeTracks::default();
    let mut a = Transform::default();
    let mut b = a;
    shake.apply(&mut a, 25, 25.0, (1920, 1080));
    shake.apply(&mut b, 60, 60.0, (1920, 1080));
    assert_eq!(a, b);
}

#[test]
fn animated_speed_integrates_instead_of_jumping_at_keyframes() {
    let mut speed = Keyframed::constant(1.0);
    speed.upsert(0, 1.0, Interpolation::Linear);
    speed.upsert(100, 3.0, Interpolation::Hold);
    assert!((speed_phase(&speed, 50.0) - 75.0).abs() < 1e-5);
    assert!((speed_phase(&speed, 100.0) - 200.0).abs() < 1e-5);
    assert!((speed_phase(&speed, 101.0) - 203.0).abs() < 1e-5);
    speed.upsert(0, 1.0, Interpolation::Hold);
    assert!((speed_phase(&speed, 100.0) - 100.0).abs() < 1e-5);
    assert!((speed_phase(&speed, -25.0) + 25.0).abs() < 1e-5);
}

#[test]
fn animated_motion_scale_can_return_to_the_original_image() {
    let mut shake = CameraShakeTracks::default();
    let scale = shake.track_mut(ShakeParam::MotionScale);
    scale.upsert(0, 0.0, Interpolation::Linear);
    scale.upsert(20, 1.0, Interpolation::Linear);
    scale.upsert(40, 0.0, Interpolation::Linear);
    assert_eq!(at(&shake, 0), Transform::default());
    assert_ne!(at(&shake, 20), Transform::default());
    assert_eq!(at(&shake, 40), Transform::default());
}

#[test]
fn automatic_zoom_keeps_full_frame_corners_covered() {
    let mut shake = CameraShakeTracks::default();
    shake.track_mut(ShakeParam::MotionScale).default = 2.0;
    for p in [
        ShakeParam::Pan,
        ShakeParam::Tilt,
        ShakeParam::Rotation,
        ShakeParam::Zoom,
    ] {
        shake.track_mut(p).default = *p.range().end();
    }
    for (w, h) in [(1920, 1080), (1080, 1920), (400, 100)] {
        for frame in 0..300 {
            let mut transform = Transform::default();
            shake.apply(&mut transform, frame, 25.0, (w, h));
            let (sin, cos) = transform.rotation.to_radians().sin_cos();
            for x in [-0.5 * w as f32, 0.5 * w as f32] {
                for y in [-0.5 * h as f32, 0.5 * h as f32] {
                    let x = x - transform.position[0];
                    let y = y - transform.position[1];
                    let source_x = (cos * x - sin * y) / transform.zoom[0];
                    let source_y = (sin * x + cos * y) / transform.zoom[1];
                    assert!(source_x.abs() <= w as f32 * 0.5 + 0.001);
                    assert!(source_y.abs() <= h as f32 * 0.5 + 0.001);
                }
            }
        }
    }
}

#[test]
fn old_filters_load_and_new_filters_round_trip_in_ron_and_otio_metadata() {
    let mut filter = ClipFilter::new(FilterKind::CameraShake);
    filter.shake.seed = 42;
    filter.shake.auto_zoom = false;
    filter
        .shake
        .track_mut(ShakeParam::Pan)
        .upsert(12, 2.5, Interpolation::EaseInOut);
    let loaded: ClipFilter = ron::from_str(&ron::to_string(&filter).unwrap()).unwrap();
    assert_eq!(loaded, filter);
    let mut json = serde_json::to_value(&filter).unwrap();
    assert_eq!(
        serde_json::from_value::<ClipFilter>(json.clone()).unwrap(),
        filter
    );
    json.as_object_mut().unwrap().remove("shake");
    json["kind"] = serde_json::json!("Exposure");
    let old: ClipFilter = serde_json::from_value(json).unwrap();
    assert_eq!(old.shake, CameraShakeTracks::default());
}

#[test]
fn invalid_parameters_never_produce_invalid_transforms() {
    let mut shake = CameraShakeTracks::default();
    for track in shake.tracks_mut() {
        track.default = f32::NAN;
    }
    assert_eq!(at(&shake, i64::MAX), Transform::default());
    for fps in [0.0, -25.0, f64::INFINITY, f64::NAN] {
        let mut transform = Transform::default();
        shake.apply(&mut transform, 10, fps, (1920, 1080));
        assert_eq!(transform, Transform::default());
    }
}

#[test]
fn shifting_and_rescaling_effects_include_camera_shake_keyframes() {
    let mut effects = crate::EffectStack::default();
    let mut filter = ClipFilter::new(FilterKind::CameraShake);
    filter
        .shake
        .track_mut(ShakeParam::Pan)
        .upsert(10, 2.0, Interpolation::Linear);
    effects.filters.push(filter);
    effects.shift_keyframes(5);
    assert_eq!(
        effects.filters[0].shake.track(ShakeParam::Pan).keyframes()[0].0,
        15
    );
    effects.rescale_keyframe_times(crate::Rational::new(25, 1), crate::Rational::new(50, 1));
    assert_eq!(
        effects.filters[0].shake.track(ShakeParam::Pan).keyframes()[0].0,
        30
    );
}

#[test]
fn procedural_motion_is_not_quantized_to_decoded_source_frames() {
    let shake = CameraShakeTracks::default();
    let mut a = Transform::default();
    let mut b = a;
    shake.apply_at(&mut a, 10.0, 25.0, (1920, 1080));
    shake.apply_at(&mut b, 10.5, 25.0, (1920, 1080));
    assert_ne!(
        a, b,
        "a 50 fps timeline must animate a 25 fps source every frame"
    );
}
