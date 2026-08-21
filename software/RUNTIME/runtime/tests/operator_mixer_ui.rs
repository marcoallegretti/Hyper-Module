use std::cell::RefCell;
use std::rc::Rc;

use i_slint_backend_testing::ElementHandle;
use slint::platform::PointerEventButton;
use slint::{ComponentHandle, LogicalPosition, Model, ModelRc, VecModel};

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
    ui.set_mixer_channels(ModelRc::from(Rc::clone(&rows)));
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

    verify_enabled_controls(&ui, &intents);
    verify_recovery_and_disabled_controls(&ui, &rows, &intents);
}

fn verify_enabled_controls(ui: &OperatorWindow, intents: &RefCell<Vec<MixerIntent>>) {
    click_enabled(ui, "Mute 2 · Group β");
    click_enabled(ui, "Solo 1 · Audio Ω");
    let group_gain = find_control(ui, "2 · Group β live gain");
    assert_eq!(group_gain.accessible_value_minimum(), Some(0.0));
    assert_eq!(group_gain.accessible_value_maximum(), Some(200.0));
    assert_accessible_value(&group_gain, 40.0);
    drag_to_right_edge(&group_gain);
    assert_eq!(intents.borrow().len(), 3, "drag emits only on release");
    let drag_value = match intents.borrow()[2] {
        MixerIntent::Gain(1, bits) => f32::from_bits(bits),
        intent => panic!("unexpected drag intent {intent:?}"),
    };
    assert!((190.0..=200.0).contains(&drag_value));
    assert_accessible_value(&group_gain, 40.0);
    group_gain.set_accessible_value("200");
    assert_eq!(
        intents.borrow()[3],
        MixerIntent::Gain(1, 200.0_f32.to_bits())
    );
    assert_accessible_value(&group_gain, 40.0);
    let audio_pan = find_control(ui, "1 · Audio Ω live pan");
    assert_eq!(audio_pan.accessible_value_minimum(), Some(-100.0));
    assert_eq!(audio_pan.accessible_value_maximum(), Some(100.0));
    assert_accessible_value(&audio_pan, -50.0);
    audio_pan.set_accessible_value("-100");
    assert_eq!(
        intents.borrow()[4],
        MixerIntent::Pan(0, (-100.0_f32).to_bits())
    );
    assert_accessible_value(&audio_pan, -50.0);
    let main_gain = find_control(ui, "Main live gain");
    assert_accessible_value(&main_gain, 40.0);
    main_gain.mock_single_click(PointerEventButton::Left);
    assert_eq!(
        intents.borrow()[5],
        MixerIntent::MainGain(100.0_f32.to_bits())
    );
    assert_accessible_value(&main_gain, 40.0);
    click_enabled(ui, "Mute main");
    let unavailable = find_control(ui, "Solo unavailable for 2 · Group β");
    assert_eq!(unavailable.accessible_enabled(), Some(false));
    unavailable.mock_single_click(PointerEventButton::Left);

    assert_eq!(intents.borrow().len(), 7);
    assert_eq!(intents.borrow()[0], MixerIntent::Mute(1));
    assert_eq!(intents.borrow()[1], MixerIntent::Solo(0));
    assert_eq!(intents.borrow()[6], MixerIntent::MainMute);
    assert!((ui.get_main_gain_percent() - 40.0).abs() <= f32::EPSILON);
    let committed = ui.get_mixer_channels();
    assert!((committed.row_data(1).unwrap().gain_percent - 40.0).abs() <= f32::EPSILON);
    assert!((committed.row_data(0).unwrap().pan_percent + 50.0).abs() <= f32::EPSILON);
}

fn verify_recovery_and_disabled_controls(
    ui: &OperatorWindow,
    rows: &VecModel<OperatorMixerRow>,
    intents: &RefCell<Vec<MixerIntent>>,
) {
    let group_gain = find_control(ui, "2 · Group β live gain");
    let audio_pan = find_control(ui, "1 · Audio Ω live pan");
    let main_gain = find_control(ui, "Main live gain");
    let mut recovered_group = rows.row_data(1).unwrap();
    recovered_group.gain_percent = 130.0;
    rows.set_row_data(1, recovered_group);
    ui.set_main_gain_percent(80.0);
    assert_accessible_value(&group_gain, 130.0);
    assert_accessible_value(&main_gain, 80.0);

    let mut disabled_group = rows.row_data(1).unwrap();
    disabled_group.controls_enabled = false;
    rows.set_row_data(1, disabled_group);
    let mut disabled_audio = rows.row_data(0).unwrap();
    disabled_audio.controls_enabled = false;
    rows.set_row_data(0, disabled_audio);
    ui.set_main_mute_enabled(false);
    assert_eq!(group_gain.accessible_enabled(), Some(false));
    assert_eq!(audio_pan.accessible_enabled(), Some(false));
    assert_eq!(main_gain.accessible_enabled(), Some(false));
    let before_disabled_interaction = intents.borrow().len();
    drag_to_right_edge(&group_gain);
    audio_pan.set_accessible_value("100");
    main_gain.mock_single_click(PointerEventButton::Left);
    assert_eq!(intents.borrow().len(), before_disabled_interaction);
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

fn drag_to_right_edge(element: &ElementHandle) {
    let position = element.absolute_position();
    let size = element.size();
    element.mock_drag(
        LogicalPosition::new(
            position.x + size.width - 1.0,
            position.y + size.height / 2.0,
        ),
        PointerEventButton::Left,
    );
}

fn assert_accessible_value(element: &ElementHandle, expected: f32) {
    let actual = element
        .accessible_value()
        .expect("slider exposes its committed accessible value")
        .parse::<f32>()
        .expect("slider accessible value is numeric");
    assert!((actual - expected).abs() <= f32::EPSILON);
}
