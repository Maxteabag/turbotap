use crate::error::Error;
use crate::flow::{Flow, DENY_DEFAULT};
use crate::testutil::*;
use serde_json::{json, Value};

const STORE: &str = "com.apple.AppStore";
const TARGET: &str = "org.example.reader";
const STEP: &str = "appinstall:Reader|org.example.reader";

fn bounds(x: i64, y: i64) -> Value {
    json!({"x": x, "y": y, "width": 60, "height": 40})
}

fn control(kind: &str, label: &str, x: i64, y: i64) -> Value {
    json!({"type": kind, "label": label, "rect": bounds(x, y), "visible": true, "enabled": true})
}

fn result_row(bundle: &str, offer: &str, y: i64) -> Value {
    let mut title = control("StaticText", "Reader", 100, y);
    title["name"] = json!(bundle);
    json!({"type": "Cell", "children": [title, control("Button", offer, 310, y)]})
}

fn tree(children: Value) -> Dyn {
    Dyn::one(json!({"type": "Application", "children": children}))
}

fn mock_search(results: Value) -> MockWda {
    let st = MockState {
        active_app: Seq::one(json!("com.apple.springboard")),
        source_tree: tree(
            json!([{"type": "TabBar", "children": [control("Button", "Søk", 335, 850)]}]),
        ),
        fast_tap_404: true,
        fast_ui_404: true,
        ..Default::default()
    };
    let mock = MockWda::start(st);
    mock.set_on_action(move |st, kind, data| match kind {
        "app" => st.active_app = Seq::one(data["bundleId"].clone()),
        "tap" if data["y"] == 870 => {
            st.source_tree = tree(json!([control("SearchField", "Søk", 25, 70)]));
        }
        "tap" if data["y"] == 90 => {
            st.focused_eid = Some("search".into());
            st.element_rects.insert("search".into(), bounds(25, 70));
            st.element_types.insert("search".into(), json!("XCUIElementTypeSearchField"));
        }
        "value" if data[0] == "search" && data[1] == json!(["\n"]) => {
            st.source_tree = tree(results.clone());
        }
        "tap" if data["y"] == 320 => {
            st.source_tree = Dyn::of(vec![
                json!({"type":"Application", "children":[result_row(TARGET, "Last ned igjen", 300)]}),
                json!({"type":"Application", "children":[result_row(TARGET, "Åpne", 300)]}),
            ]);
        }
        _ => {}
    });
    mock
}

fn flow(mock: &MockWda, allowed: &[&str], timeout: f64) -> Flow<FakeReader> {
    let mut client = mock.client();
    client.set_allowed_apps(allowed.iter().map(|id| id.to_string()).collect());
    Flow::new(
        client,
        FakeReader::with(vec![], (430, 932), 1.0),
        timeout,
        Some(DENY_DEFAULT),
        false,
    )
    .unwrap()
}

#[test]
fn appinstall_presses_only_the_exact_product_and_waits_for_open_before_launching() {
    let _cache = isolated_cache();
    let _clock = virtual_clock();
    let mock = mock_search(json!([
        result_row("org.example.imposter", "Åpne", 200),
        result_row(TARGET, "Last ned igjen", 300),
    ]));
    let mut flow = flow(&mock, &[STORE, TARGET], 8.0);
    assert_eq!(
        flow.step(&format!("{STEP}|open")).unwrap(),
        Some(json!(TARGET))
    );
    assert_eq!(
        mock.tapped_points()
            .iter()
            .map(|(x, y, _)| (*x, *y))
            .collect::<Vec<_>>(),
        [(365, 870), (55, 90), (340, 320)]
    );
    let calls = mock.calls();
    let install = calls
        .iter()
        .position(|(_, p, d)| p.ends_with("/wda/tap") && d["y"] == 320)
        .unwrap();
    let launch = calls
        .iter()
        .position(|(_, p, d)| p.ends_with("/wda/apps/activate") && d["bundleId"] == TARGET)
        .unwrap();
    assert!(install < launch);
    assert!(
        calls[install + 1..launch]
            .iter()
            .filter(|(_, p, _)| p.contains("/source?"))
            .count()
            >= 2
    );
}

#[test]
fn appinstall_refuses_unverified_or_paid_offers_and_unallowed_targets() {
    let _cache = isolated_cache();
    let _clock = virtual_clock();
    let mock = mock_search(json!([
        result_row(TARGET, "$4.99", 240),
        result_row("org.example.imposter", "Get", 360),
    ]));
    let refused = flow(&mock, &[STORE], 8.0).step(STEP);
    assert!(matches!(refused, Err(Error::AppGuard(_))));
    assert!(!mock
        .paths()
        .iter()
        .any(|p| p.ends_with("/wda/apps/activate")));

    let err = flow(&mock, &[STORE, TARGET], 2.0).step(STEP).unwrap_err();
    assert!(
        err.to_string().contains("waiting for an enabled Open"),
        "{err}"
    );
    assert_eq!(
        mock.tapped_points()
            .iter()
            .map(|(_, y, _)| *y)
            .collect::<Vec<_>>(),
        [870, 90]
    );
}
