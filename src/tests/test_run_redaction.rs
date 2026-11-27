//! The OCR fixture reflects the mock field actually written by Turbotap. All
//! credentials and device responses are synthetic and all CLI output is captured.
use crate::testutil::as_bin::{exe, AS_BIN_ENV};
use crate::testutil::capture::Capture;
use crate::testutil::{isolated_cache, set_env, Dyn, MockState, MockWda, Seq, TempDir};
use serde_json::{json, Value};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const FAKE: &str = "synthetic-öCR-47692";
const FAKE_CMD: &str = "731928";
const SPLIT_PIN: &str = "73-19";
const KEY_ENV: &str = "TURBOTAP_REDACTION_KEY";
const TEST_KEY: &str = "a24da950574d624439776e2909c208331fcbff58aea8b101982508eb1875adc4";
const WRONG_KEY: &str = "655ffb62e9dd81a99699ebcf0017208c422510f92b3c075648c68643f8926092";
const RUN_ENV: &str = "TURBOTAP_REDACTION_DIR";
const ENV_STEP: &str = "type:env:TT_SYNTHETIC";
const CMD_STEP: &str = "type:cmd:/bin/sh -c 'printf \"%s\" \"$TT_SYNTHETIC_CMD\"'";
const MASK: &str = "<redacted>";

fn phone() -> MockWda {
    let mut state = MockState {
        screenshot_b64: Some(Seq::one("c3ludGhldGljLXBpeGVscw==".into())),
        ..MockState::default()
    };
    state.elements_map.insert("XCUIElementTypeTextField".into(), vec![json!({"ELEMENT": "input"})]);
    state.set_element_value("input", Dyn::one(json!("")));
    let phone = MockWda::start(state);
    phone.set_custom_handler(|st, method, path, _| {
        if method != "POST" || path != "/wda/fastOCR" {
            return None;
        }
        let field = st.element_values.get_mut("input")?.next(&Value::Null);
        let field = field.as_str().unwrap_or_default();
        let line = |text: &str, y| json!({
            "text": text, "x": 0.125, "y": y, "width": 0.5, "height": 0.1, "confidence": 0.94
        });
        let mut lines = vec![line("Public label", 0.1)];
        if !field.is_empty() {
            let fragment = if field.bytes().all(|b| b.is_ascii_digit()) && field.len() >= 4 {
                format!("{}-{}", &field[..2], &field[2..4])
            } else {
                field.chars().rev().take(6).collect::<Vec<_>>().into_iter().rev().collect::<String>().to_uppercase()
            };
            lines.push(line(field, 0.4));
            lines.push(line(&fragment, 0.6));
        }
        Some(json!({"value": {
            "mode": "accurate",
            "lines": lines,
            "image": {"capture": "cgimage", "scale": 0.5, "sourcePixels": [1206, 2622], "inputPixels": [603, 1311]}
        }}))
    });
    phone
}

fn private_dir(parent: &Path, name: &str) -> PathBuf {
    let path = parent.join(name);
    std::fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
    path
}

fn argv(phone: &MockWda, args: &[&str]) -> Vec<String> {
    ["--no-daemon", "--url", phone.url.as_str()].into_iter().chain(args.iter().copied()).map(str::to_string).collect()
}

fn subprocess(tmp: &Path, phone: &MockWda, scope: Option<&Path>, args: &[&str]) -> Output {
    subprocess_with_key(tmp, phone, scope, Some(TEST_KEY), args)
}

fn subprocess_with_key(tmp: &Path, phone: &MockWda, scope: Option<&Path>, key: Option<&str>, args: &[&str]) -> Output {
    let mut command = Command::new(exe());
    command
        .args(argv(phone, args))
        .env_clear()
        .env(AS_BIN_ENV, "1")
        .env("HOME", tmp)
        .env("WDA_CONFIG", tmp.join("config.json"))
        .env("TURBOTAP_TEST_TIMINGS_FILE", tmp.join("timings.jsonl"))
        .env("TT_SYNTHETIC", FAKE)
        .env("TT_SYNTHETIC_CMD", FAKE_CMD)
        .current_dir(tmp);
    if let Some(scope) = scope {
        command.env(RUN_ENV, scope);
        if let Some(key) = key {
            command.env(KEY_ENV, key);
        }
    }
    command.output().unwrap()
}

fn doc(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).expect("expected a CLI JSON document")
}

fn no_fake(bytes: &[u8]) {
    // Check raw and JSON-escaped output without printing failed command output.
    let s = String::from_utf8_lossy(bytes);
    assert!(
        !s.contains("CR-47692") && !s.contains("-47692") && !s.contains(FAKE_CMD) && !s.contains(SPLIT_PIN),
        "synthetic credential escaped masking"
    );
}

fn read_text(v: &Value, index: usize) -> &Value {
    &v["reads"][0]["lines"][index]["text"]
}

#[test]
fn separate_cli_processes_mask_visible_credentials_only_for_the_same_run_and_device() {
    let tmp = TempDir::new("tt-run-redaction-");
    std::fs::write(tmp.path().join("config.json"), "{}").unwrap();
    let scope = private_dir(tmp.path(), "round");
    let other_scope = private_dir(tmp.path(), "another-round");
    let phone = phone();
    let other_phone = self::phone();

    // Before input, the configured scope behaves like a normal invocation.
    let r = subprocess(tmp.path(), &phone, Some(&scope), &["flow", "read:vision"]);
    assert!(r.status.success(), "clean read failed");
    assert!(read_text(&doc(&r.stdout), 0).as_str() == Some("Public label"));

    // Establish the baseline leak via the actual CLI: the first flow masks its
    // own read, but its successor has no credential context if scope is unset.
    let r = subprocess(tmp.path(), &phone, None, &["flow", ENV_STEP, "read:vision"]);
    assert!(r.status.success(), "unscoped input failed");
    no_fake(&r.stdout);
    no_fake(&r.stderr);
    let r = subprocess(tmp.path(), &phone, None, &["flow", "read:vision"]);
    assert!(r.status.success(), "unscoped read failed");
    assert!(read_text(&doc(&r.stdout), 1).as_str() == Some(FAKE), "mock OCR did not see the written value");

    // Register the text credential in one invocation; a later invocation must
    // also mask its differently cased OCR fragment and retain ordinary labels.
    let r = subprocess(tmp.path(), &phone, Some(&scope), &["flow", ENV_STEP]);
    assert!(r.status.success(), "scoped environment input failed");
    let r = subprocess(tmp.path(), &phone, Some(&scope), &["flow", "read:vision"]);
    assert!(r.status.success(), "scoped text read failed");
    no_fake(&r.stdout);
    let v = doc(&r.stdout);
    assert!(read_text(&v, 0).as_str() == Some("Public label"));
    assert!(read_text(&v, 1).as_str() == Some(MASK));
    assert!(read_text(&v, 2).as_str() == Some(MASK));

    let r = subprocess(tmp.path(), &phone, Some(&scope), &["flow", CMD_STEP, "read:vision"]);
    assert!(r.status.success(), "scoped command input failed");
    no_fake(&r.stdout);
    no_fake(&r.stderr);
    assert!(read_text(&doc(&r.stdout), 1).as_str() == Some(MASK));

    for args in [&["flow", "read:vision"][..], &["vision-ocr"][..]] {
        let r = subprocess(tmp.path(), &phone, Some(&scope), args);
        assert!(r.status.success(), "scoped successor failed");
        no_fake(&r.stdout);
        no_fake(&r.stderr);
        let v = doc(&r.stdout);
        assert_eq!(v["ok"], true);
        assert_eq!(v["redacted"], true);
        let lines = if args[0] == "flow" { &v["reads"][0]["lines"] } else { &v["lines"] };
        assert!(lines[0]["text"].as_str() == Some("Public label"));
        assert!(lines[1]["text"].as_str() == Some(MASK));
        assert!(lines[2]["text"].as_str() == Some(MASK), "numeric fragment with an OCR separator escaped");
        assert!(lines[1]["x"].is_number() && lines[1]["y"].is_number());
    }

    let r = subprocess(tmp.path(), &other_phone, Some(&scope), &["flow", "read:vision"]);
    assert!(r.status.success(), "other-device read failed");
    assert!(read_text(&doc(&r.stdout), 0).as_str() == Some("Public label"));
    let r = subprocess(tmp.path(), &phone, Some(&other_scope), &["flow", "read:vision"]);
    assert!(r.status.success(), "other-run read failed");
    assert!(
        read_text(&doc(&r.stdout), 1).as_str() == Some(FAKE_CMD),
        "another run inherited the first run's state, or command input did not reach the field"
    );

    let image = tmp.path().join("screen.png");
    let r = subprocess(tmp.path(), &phone, Some(&scope), &["screenshot", image.to_str().unwrap()]);
    assert!(!r.status.success(), "scoped screenshot was exported");
    assert!(!image.exists(), "scoped screenshot left a file");
    no_fake(&r.stderr);
    assert!(String::from_utf8_lossy(&r.stderr).contains("image export refused"));

    // A parse error quotes its input in both stderr and the timing diagnostics.
    // The output sink and the timing writer each own their filtering boundary.
    let timing = tmp.path().join("timings.jsonl");
    std::fs::write(&timing, b"").unwrap();
    let sensitive = format!("sleep:{SPLIT_PIN}");
    let r = subprocess(tmp.path(), &phone, Some(&scope), &["flow", &sensitive]);
    assert_eq!(r.status.code(), Some(1));
    no_fake(&r.stderr);
    assert!(String::from_utf8_lossy(&r.stderr).contains(MASK));
    let r = subprocess(tmp.path(), &phone, Some(&scope), &["flow", "sleep:not-a-duration"]);
    assert_eq!(r.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&r.stderr).contains("not-a-duration"));
    let records = std::fs::read_to_string(&timing).unwrap();
    no_fake(records.as_bytes());
    let mut records = records.lines().map(|line| doc(line.as_bytes()));
    let sensitive = records.next().expect("missing scoped timing record");
    assert!(sensitive["step"].as_str() == Some("sleep"));
    assert!(sensitive["arg"].as_str() == Some(MASK) && sensitive["error"].as_str() == Some(MASK));
    let unrelated = records.next().expect("missing unrelated timing record");
    assert!(unrelated["error"].as_str().is_some_and(|e| e.contains("not-a-duration")));

    phone.clear_calls();
    for key in [None, Some(WRONG_KEY)] {
        let r = subprocess_with_key(tmp.path(), &phone, Some(&scope), key, &["flow", "read:vision"]);
        assert_eq!(r.status.code(), Some(1), "invalid key was accepted");
        no_fake(&r.stderr);
        assert!(phone.paths().is_empty(), "invalid key reached the device");
    }

    // Corrupt only the final batch and keep the valid authenticated header.
    let file = std::fs::read_dir(&scope).unwrap().next().unwrap().unwrap().path();
    let mut contents = std::fs::read(&file).unwrap();
    no_fake(&contents);
    assert!(!contents.windows(TEST_KEY.len()).any(|window| window == TEST_KEY.as_bytes()));
    contents.pop().expect("expected registry contents");
    std::fs::write(file, contents).unwrap();
    let r = subprocess(tmp.path(), &phone, Some(&scope), &["flow", "read:vision"]);
    assert_eq!(r.status.code(), Some(1), "torn registry was accepted");
    assert!(phone.paths().is_empty(), "invalid registry reached the device");
    no_fake(&r.stderr);
}

#[test]
fn scoped_cleartype_preserves_existing_text_if_secret_registration_fails() {
    let tmp = TempDir::new("tt-run-cleartype-");
    std::fs::write(tmp.path().join("config.json"), "{}").unwrap();
    let scope = private_dir(tmp.path(), "workflow");
    let phone = phone();
    let literal = "ordinary text ".repeat(22);
    let literal_step = format!("cleartype:{literal}");
    let output = subprocess(tmp.path(), &phone, Some(&scope), &["flow", "--quiet", &literal_step]);
    assert!(output.status.success(), "ordinary long text was rejected in scoped mode");

    phone.clear_calls();
    let secret_step = format!("cleartype:cmd:printf '%s' '{}'", "x".repeat(257));
    let rejected = subprocess(tmp.path(), &phone, Some(&scope), &["flow", "--quiet", &secret_step]);
    assert_eq!(rejected.status.code(), Some(1));
    assert!(!phone.paths().iter().any(|p| p.ends_with("/element/input/clear")));
    assert!(!phone.paths().iter().any(|p| p.ends_with("/element/input/value")));
    let field = phone.state().element_values.get_mut("input").unwrap().next(&Value::Null);
    assert_eq!(field, json!(literal));
}

#[test]
fn same_process_cli_calls_share_the_run_and_refuse_writes_when_state_disappears() {
    // This test mutates process-wide CLI configuration. Give its two dispatches
    // one isolated process so unrelated tests cannot see that configuration.
    const ISOLATED: &str = "TURBOTAP_TEST_REDACTION_ISOLATED";
    if std::env::var_os(ISOLATED).is_none() {
        let module = module_path!().split_once("::").unwrap().1;
        let test = format!("{module}::same_process_cli_calls_share_the_run_and_refuse_writes_when_state_disappears");
        let child = Command::new(exe()).env_clear().env(ISOLATED, "1").args(["--exact", &test]).output().unwrap();
        assert!(child.status.success(), "isolated same-process regression failed");
        assert!(String::from_utf8_lossy(&child.stdout).contains("1 passed"), "isolated regression did not run");
        return;
    }
    let cache = isolated_cache();
    let scope = private_dir(cache.path(), "round");
    let config = cache.path().join("config.json");
    std::fs::write(&config, "{}").unwrap();
    let _env = set_env(&[
        (RUN_ENV, scope.to_str()),
        (KEY_ENV, Some(TEST_KEY)),
        ("WDA_CONFIG", config.to_str()),
        ("WDA_DEVICE", None),
        ("WDA_UDID", None),
        ("IPHONE_UDID", None),
        ("TURBOTAP_STOP_FILE", None),
        ("TURBOTAP_ALLOWED_APPS", None),
        ("WDA_PASSCODE", None),
        ("WDA_PASSCODE_CMD", None),
        ("TURBOTAP_TIMINGS", Some("0")),
        ("TT_SYNTHETIC", Some(FAKE)),
    ]);
    let phone = phone();
    let cap = Capture::start();
    assert_eq!(crate::cli::run_cli(&argv(&phone, &["flow", ENV_STEP])), 0);
    no_fake(cap.stdout().as_bytes());
    no_fake(cap.stderr().as_bytes());
    drop(cap);

    let cap = Capture::start();
    assert_eq!(crate::cli::run_cli(&argv(&phone, &["flow", "read:vision"])), 0);
    no_fake(cap.stdout().as_bytes());
    no_fake(cap.stderr().as_bytes());
    assert!(read_text(&doc(cap.stdout().as_bytes()), 1).as_str() == Some(MASK));
    drop(cap);

    // Simulate replacement after CLI scope validation, while looking up the input
    // field. Failure must precede the request that carries the new credential.
    let removed = Arc::new(AtomicBool::new(false));
    let removed_by_wda = removed.clone();
    let moved = cache.path().join("moved");
    phone.set_custom_handler(move |_, method, path, _| {
        if method == "GET" && path.ends_with("/element/active") && !removed_by_wda.swap(true, Ordering::SeqCst) {
            std::fs::rename(&scope, &moved).unwrap();
        }
        None
    });
    phone.clear_calls();
    let cap = Capture::start();
    assert_eq!(crate::cli::run_cli(&argv(&phone, &["flow", ENV_STEP])), 1);
    assert!(removed.load(Ordering::SeqCst), "did not reach field lookup");
    assert!(!phone.paths().iter().any(|path| path.ends_with("/element/input/value")), "typed after losing the shared state");
    no_fake(cap.stdout().as_bytes());
    no_fake(cap.stderr().as_bytes());
    assert!(cap.stdout().contains("credential redaction state is unavailable or invalid"));

    phone.clear_calls();
    assert_eq!(crate::cli::run_cli(&argv(&phone, &["flow", "read:vision"])), 1);
    assert!(phone.paths().is_empty(), "CLI contacted WDA with a missing run directory");
}
