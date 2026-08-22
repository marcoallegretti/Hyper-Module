use std::cell::RefCell;
use std::rc::Rc;

use i_slint_backend_testing::ElementHandle;
use slint::platform::{Key, PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, Model, ModelRc, VecModel};

slint::include_modules!();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MixerIntent {
    Gain(i32, u32),
    Pan(i32, u32),
    Mute(i32),
    Solo(i32),
    SendGain(i32, u32),
    SendPan(i32, u32),
    SendToggle(i32),
    MainGain(u32),
    MainMute,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LoopIntent {
    In,
    Out,
    Clear,
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one generated-window contract keeps shortcut routing and recovery-enabled controls together"
)]
fn generated_transport_shortcuts_are_focus_aware_and_repeat_free() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = OperatorWindow::new().unwrap();
    ui.set_transport_start_enabled(true);
    ui.set_transport_stop_enabled(false);
    ui.set_transport_return_to_start_enabled(true);
    let toggles = Rc::new(RefCell::new(0_usize));
    let toggle_requested = Rc::clone(&toggles);
    ui.on_toggle_transport_requested(move || {
        *toggle_requested.borrow_mut() += 1;
    });
    let returns = Rc::new(RefCell::new(0_usize));
    let return_requested = Rc::clone(&returns);
    ui.on_return_to_start_requested(move || {
        *return_requested.borrow_mut() += 1;
    });
    let seek_intents = configure_transport_seek(&ui);
    ui.show().unwrap();
    let start = find_control(&ui, "Start");
    let stop = find_control(&ui, "Stop");
    let return_to_start = find_control(&ui, "Return to start");
    assert_eq!(start.accessible_enabled(), Some(true));
    assert_eq!(stop.accessible_enabled(), Some(false));
    assert_eq!(return_to_start.accessible_enabled(), Some(true));

    dispatch_key(
        &ui,
        WindowEvent::KeyPressed {
            text: Key::Space.into(),
        },
    );
    dispatch_key(
        &ui,
        WindowEvent::KeyPressRepeated {
            text: Key::Space.into(),
        },
    );
    dispatch_key(
        &ui,
        WindowEvent::KeyReleased {
            text: Key::Space.into(),
        },
    );
    assert_eq!(*toggles.borrow(), 1);

    ui.set_transport_start_enabled(false);
    ui.set_transport_stop_enabled(true);
    assert_eq!(start.accessible_enabled(), Some(false));
    assert_eq!(stop.accessible_enabled(), Some(true));
    dispatch_key(
        &ui,
        WindowEvent::KeyPressed {
            text: Key::Space.into(),
        },
    );
    dispatch_key(
        &ui,
        WindowEvent::KeyPressRepeated {
            text: Key::Space.into(),
        },
    );
    dispatch_key(
        &ui,
        WindowEvent::KeyReleased {
            text: Key::Space.into(),
        },
    );
    assert_eq!(
        *toggles.borrow(),
        2,
        "Space remains available while rolling"
    );

    dispatch_key(
        &ui,
        WindowEvent::KeyPressed {
            text: Key::Home.into(),
        },
    );
    dispatch_key(
        &ui,
        WindowEvent::KeyPressRepeated {
            text: Key::Home.into(),
        },
    );
    dispatch_key(
        &ui,
        WindowEvent::KeyReleased {
            text: Key::Home.into(),
        },
    );
    assert_eq!(*returns.borrow(), 1);

    let seek = find_control(&ui, "Committed transport position percent");
    click_committed_marker(&ui, &seek, 0.2);
    dispatch_key(
        &ui,
        WindowEvent::KeyPressed {
            text: Key::Home.into(),
        },
    );
    dispatch_key(
        &ui,
        WindowEvent::KeyPressRepeated {
            text: Key::Home.into(),
        },
    );
    dispatch_key(
        &ui,
        WindowEvent::KeyReleased {
            text: Key::Home.into(),
        },
    );
    assert_eq!(*returns.borrow(), 1, "the focused slider owns Home");
    assert_eq!(seek_intents.borrow().len(), 2);
    assert!((f32::from_bits(seek_intents.borrow()[0]) - 20.0).abs() <= 0.001);
    assert_eq!(seek_intents.borrow()[1], 0.0_f32.to_bits());

    ui.set_transport_start_enabled(false);
    ui.set_transport_stop_enabled(false);
    ui.set_transport_return_to_start_enabled(false);
    ui.set_transport_seek_enabled(false);
    assert_eq!(start.accessible_enabled(), Some(false));
    assert_eq!(stop.accessible_enabled(), Some(false));
    assert_eq!(return_to_start.accessible_enabled(), Some(false));
    dispatch_key(
        &ui,
        WindowEvent::KeyPressed {
            text: Key::Space.into(),
        },
    );
    dispatch_key(
        &ui,
        WindowEvent::KeyPressed {
            text: Key::Home.into(),
        },
    );
    dispatch_key(
        &ui,
        WindowEvent::KeyPressRepeated {
            text: Key::Home.into(),
        },
    );
    dispatch_key(
        &ui,
        WindowEvent::KeyReleased {
            text: Key::Home.into(),
        },
    );
    assert_eq!(*toggles.borrow(), 2);
    assert_eq!(*returns.borrow(), 1);
}

#[test]
fn generated_scene_stop_button_respects_recovery_authority() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = OperatorWindow::new().unwrap();
    let scenes = Rc::new(VecModel::from(vec![OperatorSceneRow {
        name: "Interactive layer".into(),
        status: "Ready".into(),
        available: true,
        queued: false,
        last_launched: false,
    }]));
    ui.set_scenes(ModelRc::from(scenes));
    let intents = Rc::new(RefCell::new(0_usize));
    let requested = Rc::clone(&intents);
    ui.on_stop_interactive_scenes_requested(move || {
        *requested.borrow_mut() += 1;
    });
    ui.set_stop_interactive_scenes_enabled(true);
    ui.show().unwrap();
    scroll_down(&ui);

    let stop = find_control(&ui, "Stop interactive scenes");
    assert_eq!(stop.accessible_enabled(), Some(true));
    stop.mock_single_click(PointerEventButton::Left);
    assert_eq!(*intents.borrow(), 1);

    ui.set_stop_interactive_scenes_enabled(false);
    assert_eq!(stop.accessible_enabled(), Some(false));
    stop.mock_single_click(PointerEventButton::Left);
    assert_eq!(*intents.borrow(), 1);
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one generated-window contract keeps actual control mapping and capability evidence together"
)]
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
    let sends = Rc::new(VecModel::from(vec![
        OperatorMixerSendRow {
            source_name: "Audio Ω".into(),
            target_name: "Return β".into(),
            tap: "POST".into(),
            gain_percent: 60.0,
            pan_percent: 25.0,
            enabled: true,
            controls_enabled: true,
        },
        OperatorMixerSendRow {
            source_name: "Audio Ω".into(),
            target_name: "Return β".into(),
            tap: "PRE".into(),
            gain_percent: 30.0,
            pan_percent: -25.0,
            enabled: true,
            controls_enabled: true,
        },
    ]));
    ui.set_mixer_sends(ModelRc::from(Rc::clone(&sends)));
    ui.set_main_gain_percent(40.0);
    ui.set_main_mute_enabled(true);
    let seek_intents = configure_transport_seek(&ui);
    let loop_intents = configure_loop_marks(&ui);

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
    let send_toggle_intents = Rc::clone(&intents);
    ui.on_toggle_mixer_send_requested(move |row| {
        send_toggle_intents
            .borrow_mut()
            .push(MixerIntent::SendToggle(row));
    });
    let send_gain_intents = Rc::clone(&intents);
    ui.on_set_mixer_send_gain_requested(move |row, percent| {
        send_gain_intents
            .borrow_mut()
            .push(MixerIntent::SendGain(row, percent.to_bits()));
    });
    let send_pan_intents = Rc::clone(&intents);
    ui.on_set_mixer_send_pan_requested(move |row, percent| {
        send_pan_intents
            .borrow_mut()
            .push(MixerIntent::SendPan(row, percent.to_bits()));
    });
    ui.show().unwrap();

    verify_committed_transport_seek(&ui, &seek_intents);
    verify_loop_mark_buttons(&ui, &loop_intents);
    verify_enabled_controls(&ui, &intents);
    verify_recovery_and_disabled_controls(&ui, &rows, &sends, &intents);
}

fn configure_loop_marks(ui: &OperatorWindow) -> Rc<RefCell<Vec<LoopIntent>>> {
    ui.set_loop_in_enabled(true);
    ui.set_loop_out_enabled(true);
    ui.set_loop_clear_enabled(true);
    let intents = Rc::new(RefCell::new(Vec::new()));
    let loop_in = Rc::clone(&intents);
    ui.on_set_loop_in_requested(move || loop_in.borrow_mut().push(LoopIntent::In));
    let loop_out = Rc::clone(&intents);
    ui.on_set_loop_out_requested(move || loop_out.borrow_mut().push(LoopIntent::Out));
    let clear = Rc::clone(&intents);
    ui.on_clear_loop_range_requested(move || clear.borrow_mut().push(LoopIntent::Clear));
    intents
}

fn verify_loop_mark_buttons(ui: &OperatorWindow, intents: &RefCell<Vec<LoopIntent>>) {
    click_enabled(ui, "Loop In");
    click_enabled(ui, "Loop Out");
    click_enabled(ui, "Clear loop");
    assert_eq!(
        intents.borrow().as_slice(),
        [LoopIntent::In, LoopIntent::Out, LoopIntent::Clear]
    );

    ui.set_loop_in_enabled(false);
    ui.set_loop_out_enabled(false);
    ui.set_loop_clear_enabled(false);
    find_control(ui, "Loop In").mock_single_click(PointerEventButton::Left);
    find_control(ui, "Loop Out").mock_single_click(PointerEventButton::Left);
    find_control(ui, "Clear loop").mock_single_click(PointerEventButton::Left);
    assert_eq!(intents.borrow().len(), 3);
}

fn configure_transport_seek(ui: &OperatorWindow) -> Rc<RefCell<Vec<u32>>> {
    ui.set_transport_position_percent(20.0);
    ui.set_transport_seek_enabled(true);
    let intents = Rc::new(RefCell::new(Vec::new()));
    let requested = Rc::clone(&intents);
    ui.on_locate_percent_requested(move |percent| {
        requested.borrow_mut().push(percent.to_bits());
    });
    intents
}

fn verify_committed_transport_seek(ui: &OperatorWindow, intents: &RefCell<Vec<u32>>) {
    let seek = find_control(ui, "Committed transport position percent");
    assert_eq!(seek.accessible_enabled(), Some(true));
    assert_accessible_value(&seek, 20.0);
    let pressed = committed_marker_position(&seek, 0.2);
    let released = committed_marker_position(&seek, 0.4);
    ui.window()
        .dispatch_event(WindowEvent::PointerMoved { position: pressed });
    assert!(intents.borrow().is_empty());
    ui.window().dispatch_event(WindowEvent::PointerPressed {
        position: pressed,
        button: PointerEventButton::Left,
    });
    assert!(intents.borrow().is_empty());
    ui.window()
        .dispatch_event(WindowEvent::PointerMoved { position: released });
    assert!(intents.borrow().is_empty());
    ui.window().dispatch_event(WindowEvent::PointerReleased {
        position: released,
        button: PointerEventButton::Left,
    });
    assert_eq!(intents.borrow().len(), 1);
    assert!((f32::from_bits(intents.borrow()[0]) - 40.0).abs() <= 0.001);
    assert_accessible_value(&seek, 20.0);
    drag_to_right_edge(&seek);
    assert_eq!(intents.borrow().len(), 2, "seek emits only on release");
    assert_eq!(intents.borrow()[1], 100.0_f32.to_bits());
    assert_accessible_value(&seek, 20.0);
    ui.set_transport_position_percent(50.0);
    assert_accessible_value(&seek, 50.0);
    ui.set_transport_seek_enabled(false);
    assert_eq!(seek.accessible_enabled(), Some(false));
    drag_to_right_edge(&seek);
    assert_eq!(intents.borrow().len(), 2);
}

fn verify_enabled_controls(ui: &OperatorWindow, intents: &RefCell<Vec<MixerIntent>>) {
    click_enabled(ui, "Mute 2 · Group β");
    click_enabled(ui, "Solo 1 · Audio Ω");
    let group_gain = find_control(ui, "2 · Group β live gain");
    assert_eq!(group_gain.accessible_value_minimum(), Some(0.0));
    assert_eq!(group_gain.accessible_value_maximum(), Some(200.0));
    assert_accessible_value(&group_gain, 40.0);
    click_committed_marker(ui, &group_gain, 0.2);
    assert_eq!(
        intents.borrow()[2],
        MixerIntent::Gain(1, 40.0_f32.to_bits())
    );
    assert_accessible_value(&group_gain, 40.0);
    drag_to_right_edge(&group_gain);
    assert_eq!(intents.borrow().len(), 4, "drag emits only on release");
    assert_eq!(
        intents.borrow()[3],
        MixerIntent::Gain(1, 200.0_f32.to_bits())
    );
    assert_accessible_value(&group_gain, 40.0);
    group_gain.set_accessible_value("200");
    assert_eq!(
        intents.borrow()[4],
        MixerIntent::Gain(1, 200.0_f32.to_bits())
    );
    assert_accessible_value(&group_gain, 40.0);
    let audio_pan = find_control(ui, "1 · Audio Ω live pan");
    assert_eq!(audio_pan.accessible_value_minimum(), Some(-100.0));
    assert_eq!(audio_pan.accessible_value_maximum(), Some(100.0));
    assert_accessible_value(&audio_pan, -50.0);
    audio_pan.set_accessible_value("-100");
    assert_eq!(
        intents.borrow()[5],
        MixerIntent::Pan(0, (-100.0_f32).to_bits())
    );
    assert_accessible_value(&audio_pan, -50.0);
    let main_gain = find_control(ui, "Main live gain");
    assert_accessible_value(&main_gain, 40.0);
    main_gain.mock_single_click(PointerEventButton::Left);
    assert_eq!(
        intents.borrow()[6],
        MixerIntent::MainGain(100.0_f32.to_bits())
    );
    assert_accessible_value(&main_gain, 40.0);
    click_enabled(ui, "Mute main");
    let unavailable = find_control(ui, "Solo unavailable for 2 · Group β");
    assert_eq!(unavailable.accessible_enabled(), Some(false));
    unavailable.mock_single_click(PointerEventButton::Left);

    scroll_down(ui);
    click_enabled(ui, "Disable send 1 · Audio Ω to Return β");
    let send_gain = find_control(ui, "1 · Audio Ω to Return β send gain");
    let send_pan = find_control(ui, "1 · Audio Ω to Return β send pan");
    assert_accessible_value(&send_gain, 60.0);
    assert_accessible_value(&send_pan, 25.0);
    send_gain.set_accessible_value("125");
    send_pan.set_accessible_value("-75");
    assert_accessible_value(&send_gain, 60.0);
    assert_accessible_value(&send_pan, 25.0);

    scroll_down(ui);
    click_enabled(ui, "Disable send 2 · Audio Ω to Return β");
    find_control(ui, "2 · Audio Ω to Return β send gain").set_accessible_value("150");
    find_control(ui, "2 · Audio Ω to Return β send pan").set_accessible_value("75");

    assert_eq!(intents.borrow().len(), 14);
    assert_eq!(intents.borrow()[0], MixerIntent::Mute(1));
    assert_eq!(intents.borrow()[1], MixerIntent::Solo(0));
    assert_eq!(intents.borrow()[7], MixerIntent::MainMute);
    assert_eq!(intents.borrow()[8], MixerIntent::SendToggle(0));
    assert_eq!(
        intents.borrow()[9],
        MixerIntent::SendGain(0, 125.0_f32.to_bits())
    );
    assert_eq!(
        intents.borrow()[10],
        MixerIntent::SendPan(0, (-75.0_f32).to_bits())
    );
    assert_eq!(intents.borrow()[11], MixerIntent::SendToggle(1));
    assert_eq!(
        intents.borrow()[12],
        MixerIntent::SendGain(1, 150.0_f32.to_bits())
    );
    assert_eq!(
        intents.borrow()[13],
        MixerIntent::SendPan(1, 75.0_f32.to_bits())
    );
    assert!((ui.get_main_gain_percent() - 40.0).abs() <= f32::EPSILON);
    let committed = ui.get_mixer_channels();
    assert!((committed.row_data(1).unwrap().gain_percent - 40.0).abs() <= f32::EPSILON);
    assert!((committed.row_data(0).unwrap().pan_percent + 50.0).abs() <= f32::EPSILON);
}

fn verify_recovery_and_disabled_controls(
    ui: &OperatorWindow,
    rows: &VecModel<OperatorMixerRow>,
    sends: &VecModel<OperatorMixerSendRow>,
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
    let send_gain = find_control(ui, "1 · Audio Ω to Return β send gain");
    let send_pan = find_control(ui, "1 · Audio Ω to Return β send pan");
    let mut recovered_send = sends.row_data(0).unwrap();
    recovered_send.gain_percent = 90.0;
    recovered_send.pan_percent = -10.0;
    sends.set_row_data(0, recovered_send);
    assert_accessible_value(&send_gain, 90.0);
    assert_accessible_value(&send_pan, -10.0);

    let mut disabled_group = rows.row_data(1).unwrap();
    disabled_group.controls_enabled = false;
    rows.set_row_data(1, disabled_group);
    let mut disabled_audio = rows.row_data(0).unwrap();
    disabled_audio.controls_enabled = false;
    rows.set_row_data(0, disabled_audio);
    ui.set_main_mute_enabled(false);
    let mut disabled_send = sends.row_data(0).unwrap();
    disabled_send.controls_enabled = false;
    sends.set_row_data(0, disabled_send);
    let mut disabled_second_send = sends.row_data(1).unwrap();
    disabled_second_send.controls_enabled = false;
    sends.set_row_data(1, disabled_second_send);
    assert_eq!(group_gain.accessible_enabled(), Some(false));
    assert_eq!(audio_pan.accessible_enabled(), Some(false));
    assert_eq!(main_gain.accessible_enabled(), Some(false));
    assert_eq!(send_gain.accessible_enabled(), Some(false));
    assert_eq!(send_pan.accessible_enabled(), Some(false));
    let send_toggle = find_control(ui, "Disable send 1 · Audio Ω to Return β");
    assert_eq!(send_toggle.accessible_enabled(), Some(false));
    let before_disabled_interaction = intents.borrow().len();
    drag_to_right_edge(&group_gain);
    audio_pan.set_accessible_value("100");
    main_gain.mock_single_click(PointerEventButton::Left);
    send_gain.set_accessible_value("100");
    send_pan.set_accessible_value("100");
    send_toggle.mock_single_click(PointerEventButton::Left);
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

fn dispatch_key(ui: &OperatorWindow, event: WindowEvent) {
    ui.window().dispatch_event(event);
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

fn click_committed_marker(ui: &OperatorWindow, element: &ElementHandle, normalized: f32) {
    let marker = committed_marker_position(element, normalized);
    ui.window()
        .dispatch_event(WindowEvent::PointerMoved { position: marker });
    ui.window().dispatch_event(WindowEvent::PointerPressed {
        position: marker,
        button: PointerEventButton::Left,
    });
    ui.window()
        .dispatch_event(WindowEvent::PointerMoved { position: marker });
    ui.window().dispatch_event(WindowEvent::PointerReleased {
        position: marker,
        button: PointerEventButton::Left,
    });
}

fn committed_marker_position(element: &ElementHandle, normalized: f32) -> LogicalPosition {
    const THUMB_WIDTH: f32 = 18.0;
    let position = element.absolute_position();
    let size = element.size();
    LogicalPosition::new(
        position.x + THUMB_WIDTH / 2.0 + (size.width - THUMB_WIDTH) * normalized,
        position.y + size.height / 2.0,
    )
}

fn scroll_down(ui: &OperatorWindow) {
    ui.window().dispatch_event(WindowEvent::PointerScrolled {
        position: LogicalPosition::new(440.0, 800.0),
        delta_x: 0.0,
        delta_y: -600.0,
    });
}

fn assert_accessible_value(element: &ElementHandle, expected: f32) {
    let actual = element
        .accessible_value()
        .expect("slider exposes its committed accessible value")
        .parse::<f32>()
        .expect("slider accessible value is numeric");
    assert!((actual - expected).abs() <= f32::EPSILON);
}
