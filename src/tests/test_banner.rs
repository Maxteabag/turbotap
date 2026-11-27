//! Banner dismissal needs fresh screen evidence and follows caller policy.
use crate::flow::Flow;
use crate::testutil::*;
use serde_json::json;

#[test]
fn banner_gesture_requires_an_observed_line_and_reports_disappearance() {
    let _cache = isolated_cache();
    let (_sleep, _clock) = skip_sleep();
    let mut state = MockState::default();
    state.active_app = Seq::one(json!("com.apple.springboard"));
    state.locked = Seq::one(json!(false));
    let mock = MockWda::start(state);
    let img = solid_img(240, (430, 932));
    let background = make_line("Main screen", 70.0, 230.0, 210.0, 252.0);
    let overlay = make_line("Parcel arrived", 100.0, 60.0, 260.0, 78.0);
    let reader = FakeReader::new(vec![
        (img.clone(), vec![background.clone()]),
        (img.clone(), vec![background.clone(), overlay]),
        (img, vec![background]),
    ]);
    let mut flow = Flow::new(mock.client(), reader, 0.5, None, false).unwrap();
    assert!(flow.step("banner:dismiss").is_err());
    assert_eq!(flow.step("banner:dismiss:Parcel arrived").unwrap(), Some(json!("overlay-not-observed")));
    assert!(mock.state().drags.is_empty());
    assert_eq!(flow.step("banner:dismiss:Parcel arrived").unwrap(), Some(json!("banner-text-gone")));
    let state = mock.state();
    assert_eq!(state.drags.len(), 1);
    assert_eq!(state.drags[0]["fromX"], 180);
    assert_eq!(state.drags[0]["fromY"], 69);
    drop(state);
    flow.close();
}

#[test]
fn banner_respects_caller_deny_and_foreground_app_policy() {
    let _cache = isolated_cache();
    let (_sleep, _clock) = skip_sleep();
    let mut state = MockState::default();
    state.active_app = Seq::one(json!("com.apple.springboard"));
    state.locked = Seq::one(json!(false));
    let mock = MockWda::start(state);
    let img = solid_img(240, (430, 932));
    let reader = FakeReader::new(vec![(img, vec![make_line("Parcel arrived", 100.0, 60.0, 260.0, 78.0)])]);
    let mut flow = Flow::new(mock.client(), reader.clone(), 0.5, Some("^Parcel arrived$"), false).unwrap();
    assert!(flow.step("banner:dismiss:Parcel arrived").unwrap_err().to_string().contains("deny"));
    flow.close();
    let mut client = mock.client();
    client.set_allowed_apps(vec!["com.example.app".into()]);
    let mut flow = Flow::new(client, reader, 0.5, None, false).unwrap();
    assert!(flow.step("banner:dismiss:Parcel arrived").is_err());
    assert!(mock.state().drags.is_empty());
    flow.close();
}
