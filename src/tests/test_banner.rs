//! A notification banner over the app blocked a final check. `banner:dismiss` swipes it
//! away only when SpringBoard is in front and the phone is unlocked; otherwise it sends
//! nothing.
use crate::flow::Flow;
use crate::testutil::*;
use serde_json::json;

fn run(front: &str, locked: bool) -> (String, usize) {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    let mut st = MockState::default();
    st.active_app = Seq::one(json!(front));
    st.locked = Seq::one(json!(locked));
    let m = MockWda::start(st);
    let mut f = Flow::new(m.client(), FakeReader::new(vec![]), 0.5, None, false).unwrap();
    let r = f.step("banner:dismiss").unwrap().unwrap();
    f.close();
    let drags = m.state().drags.len();
    (r.as_str().unwrap_or("").to_string(), drags)
}

#[test]
fn app_in_front_sends_nothing() {
    let (r, drags) = run("com.example.app", false);
    assert_eq!((r.as_str(), drags), ("none", 0));
}

#[test]
fn springboard_in_front_swipes_the_banner_up() {
    let (r, drags) = run("com.apple.springboard", false);
    assert!(r.starts_with("swiped") || r == "dismissed", "{r}");
    assert_eq!(drags, 1);
}

#[test]
fn a_locked_phone_is_left_alone() {
    let (r, drags) = run("com.apple.springboard", true);
    assert_eq!((r.as_str(), drags), ("none (locked)", 0));
}
