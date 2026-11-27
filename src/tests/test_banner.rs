//! Regression for the removed gesture when SpringBoard obscures the app.
use crate::flow::Flow;
use crate::testutil::*;
use serde_json::json;

#[test]
fn banner_step_is_unknown_and_sends_no_gesture_when_springboard_is_in_front() {
    let _cache = isolated_cache();
    let (_sleep, _clock) = skip_sleep();
    let mut state = MockState::default();
    state.active_app = Seq::one(json!("com.apple.springboard"));
    state.locked = Seq::one(json!(false));
    let mock = MockWda::start(state);
    let mut flow = Flow::new(mock.client(), FakeReader::new(vec![]), 0.5, None, false).unwrap();
    let error = flow.step("banner:dismiss").unwrap_err().to_string();
    assert!(error.contains("unknown step"), "{error}");
    assert!(mock.state().drags.is_empty());
    assert!(mock.tapped_points().is_empty());
    flow.close();
}
