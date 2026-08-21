use std::cell::RefCell;
use std::rc::Rc;

use i_slint_backend_testing::ElementHandle;
use slint::platform::PointerEventButton;
use slint::{ComponentHandle, ModelRc, VecModel};

slint::include_modules!();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MixerIntent {
    Mute(i32),
    Solo(i32),
    MainMute,
}

#[test]
fn generated_mixer_buttons_forward_exact_rows_and_respect_capability() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = OperatorWindow::new().unwrap();
    let rows = Rc::new(VecModel::from(vec![
        OperatorMixerRow {
            name: "Audio Ω".into(),
            muted: false,
            soloed: false,
            controls_enabled: true,
            solo_available: true,
        },
        OperatorMixerRow {
            name: "Group β".into(),
            muted: false,
            soloed: false,
            controls_enabled: true,
            solo_available: false,
        },
    ]));
    ui.set_mixer_channels(ModelRc::from(rows));
    ui.set_main_mute_enabled(true);

    let intents = Rc::new(RefCell::new(Vec::new()));
    let mute_intents = Rc::clone(&intents);
    ui.on_toggle_mixer_channel_mute_requested(move |row| {
        mute_intents.borrow_mut().push(MixerIntent::Mute(row));
    });
    let solo_intents = Rc::clone(&intents);
    ui.on_toggle_mixer_channel_solo_requested(move |row| {
        solo_intents.borrow_mut().push(MixerIntent::Solo(row));
    });
    let main_intents = Rc::clone(&intents);
    ui.on_toggle_main_mute_requested(move || {
        main_intents.borrow_mut().push(MixerIntent::MainMute);
    });
    ui.show().unwrap();

    click_enabled(&ui, "Mute 2 · Group β");
    click_enabled(&ui, "Solo 1 · Audio Ω");
    click_enabled(&ui, "Mute main");
    let unavailable = find_control(&ui, "Solo unavailable for 2 · Group β");
    assert_eq!(unavailable.accessible_enabled(), Some(false));
    unavailable.mock_single_click(PointerEventButton::Left);

    assert_eq!(
        intents.borrow().as_slice(),
        [
            MixerIntent::Mute(1),
            MixerIntent::Solo(0),
            MixerIntent::MainMute
        ]
    );
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
