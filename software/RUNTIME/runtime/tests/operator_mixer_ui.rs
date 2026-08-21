use std::cell::RefCell;
use std::rc::Rc;

use i_slint_backend_testing::ElementHandle;
use slint::platform::PointerEventButton;
use slint::{ComponentHandle, Model, ModelRc, VecModel};

slint::include_modules!();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MixerIntent {
    Gain(i32, u32),
    Pan(i32, u32),
    Mute(i32),
    Solo(i32),
    MainGain(u32),
    MainMute,
}

#[test]
fn generated_mixer_controls_forward_exact_rows_values_and_capability() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = OperatorWindow::new().unwrap();
    let rows = Rc::new(VecModel::from(vec![
        OperatorMixerRow {
            name: "Audio Ω".into(),
            gain_percent: 20.0,
            pan_percent: -50.0,
            muted: false,
            soloed: false,
            controls_enabled: true,
            solo_available: true,
        },
        OperatorMixerRow {
            name: "Group β".into(),
            gain_percent: 40.0,
            pan_percent: 50.0,
            muted: false,
            soloed: false,
            controls_enabled: true,
            solo_available: false,
        },
    ]));
    ui.set_mixer_channels(ModelRc::from(rows));
    ui.set_main_gain_percent(40.0);
    ui.set_main_mute_enabled(true);

    let intents = Rc::new(RefCell::new(Vec::new()));
    let mute_intents = Rc::clone(&intents);
    ui.on_toggle_mixer_channel_mute_requested(move |row| {
        mute_intents.borrow_mut().push(MixerIntent::Mute(row));
    });
    let gain_intents = Rc::clone(&intents);
    ui.on_set_mixer_channel_gain_requested(move |row, percent| {
        gain_intents
            .borrow_mut()
            .push(MixerIntent::Gain(row, percent.to_bits()));
    });
    let pan_intents = Rc::clone(&intents);
    ui.on_set_mixer_channel_pan_requested(move |row, percent| {
        pan_intents
            .borrow_mut()
            .push(MixerIntent::Pan(row, percent.to_bits()));
    });
    let solo_intents = Rc::clone(&intents);
    ui.on_toggle_mixer_channel_solo_requested(move |row| {
        solo_intents.borrow_mut().push(MixerIntent::Solo(row));
    });
    let main_intents = Rc::clone(&intents);
    ui.on_toggle_main_mute_requested(move || {
        main_intents.borrow_mut().push(MixerIntent::MainMute);
    });
    let main_gain_intents = Rc::clone(&intents);
    ui.on_set_main_gain_requested(move |percent| {
        main_gain_intents
            .borrow_mut()
            .push(MixerIntent::MainGain(percent.to_bits()));
    });
    ui.show().unwrap();

    click_enabled(&ui, "Mute 2 · Group β");
    click_enabled(&ui, "Solo 1 · Audio Ω");
    let group_gain = find_control(&ui, "2 · Group β live gain");
    assert_eq!(group_gain.accessible_value_minimum(), Some(0.0));
    assert_eq!(group_gain.accessible_value_maximum(), Some(200.0));
    group_gain.mock_single_click(PointerEventButton::Left);
    let audio_pan = find_control(&ui, "1 · Audio Ω live pan");
    assert_eq!(audio_pan.accessible_value_minimum(), Some(-100.0));
    assert_eq!(audio_pan.accessible_value_maximum(), Some(100.0));
    audio_pan.mock_single_click(PointerEventButton::Left);
    click_enabled(&ui, "Main live gain");
    click_enabled(&ui, "Mute main");
    let unavailable = find_control(&ui, "Solo unavailable for 2 · Group β");
    assert_eq!(unavailable.accessible_enabled(), Some(false));
    unavailable.mock_single_click(PointerEventButton::Left);

    assert_eq!(
        intents.borrow().as_slice(),
        [
            MixerIntent::Mute(1),
            MixerIntent::Solo(0),
            MixerIntent::Gain(1, 100.0_f32.to_bits()),
            MixerIntent::Pan(0, 0.0_f32.to_bits()),
            MixerIntent::MainGain(100.0_f32.to_bits()),
            MixerIntent::MainMute
        ]
    );
    assert!((ui.get_main_gain_percent() - 40.0).abs() <= f32::EPSILON);
    let committed = ui.get_mixer_channels();
    assert!((committed.row_data(1).unwrap().gain_percent - 40.0).abs() <= f32::EPSILON);
    assert!((committed.row_data(0).unwrap().pan_percent + 50.0).abs() <= f32::EPSILON);
}

fn find_control(ui: &OperatorWindow, accessible_label: &str) -> ElementHandle {
    let mut matches = ElementHandle::find_by_accessible_label(ui, accessible_label);
    let element = matches
        .next()
        .unwrap_or_else(|| panic!("missing generated control {accessible_label:?}"));
    assert!(matches.next().is_none(), "control label must be unique");
    element
}

fn click_enabled(ui: &OperatorWindow, accessible_label: &str) {
    let element = find_control(ui, accessible_label);
    assert_eq!(element.accessible_enabled(), Some(true));
    element.mock_single_click(PointerEventButton::Left);
}
