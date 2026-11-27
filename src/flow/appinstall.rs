//! App Store installation through the live accessibility tree. Flat controls
//! lose the product ancestry needed to distinguish repeated Get/Open buttons.
use super::{AlertStatus, Flow};
use crate::{
    clock,
    error::{Error, Result},
    guard,
    ocr::Reader,
    ui,
    wda::check_wda_ok,
};
use serde_json::{json, Value};

// Keep visibility and ancestry, which the general flattened UI feed discards.
const SOURCE: &str = "/source?format=json&excluded_attributes=accessible,index,nativeAccessibilityElement,frame,nativeFrame,focused,traits,placeholderValue,minValue,maxValue";
const STORE: &str = "com.apple.AppStore";
const SEARCH: &[&str] = &["Search", "Søk"];
const GET: &[&str] = &[
    "Get",
    "Hent",
    "Download",
    "Last ned",
    "Redownload",
    "Last ned igjen",
];
const OPEN: &[&str] = &["Open", "Åpne"];

fn attr<'a>(node: &'a Value, key: &str) -> &'a str {
    node[key].as_str().unwrap_or("")
}

fn label(node: &Value) -> &str {
    for key in ["label", "name", "identifier"] {
        let value = attr(node, key).trim();
        if !value.is_empty() {
            return value;
        }
    }
    ""
}

fn children(node: &Value) -> impl Iterator<Item = &Value> {
    node["children"].as_array().into_iter().flatten()
}

fn rect(node: &Value) -> Option<(f64, f64, f64, f64)> {
    let r = &node["rect"];
    let (x, y, w, h) = (
        r["x"].as_f64()?,
        r["y"].as_f64()?,
        r["width"].as_f64()?,
        r["height"].as_f64()?,
    );
    (w > 0.0 && h > 0.0).then_some((x, y, w, h))
}

fn point(node: &Value, screen: (i64, i64)) -> Option<(i64, i64)> {
    let (x, y, w, h) = rect(node)?;
    let (x, y) = ((x + w / 2.0) as i64, (y + h / 2.0) as i64);
    (0 < x && x < screen.0 && 0 < y && y < screen.1).then_some((x, y))
}

fn visible(node: &Value) -> bool {
    !matches!(&node["visible"], Value::Bool(false))
        && !matches!(attr(node, "visible"), "false" | "0")
}

fn walk<'a>(node: &'a Value, screen: (i64, i64), visit: &mut impl FnMut(&'a Value) -> bool) {
    let kind = attr(node, "type");
    if !visible(node) || matches!(kind, "Alert" | "Sheet" | "Keyboard" | "SecureTextField") {
        return;
    }
    if kind == "Cell"
        && rect(node).is_some_and(|(x, y, w, h)| {
            x >= screen.0 as f64 || x + w <= 0.0 || y >= screen.1 as f64 || y + h <= 0.0
        })
    {
        return;
    }
    if visit(node) {
        for child in children(node) {
            walk(child, screen, visit);
        }
    }
}

fn has_modal(node: &Value) -> bool {
    visible(node)
        && (matches!(attr(node, "type"), "Alert" | "Sheet" | "SecureTextField")
            || children(node).any(has_modal))
}

fn unique<T>(mut hits: Vec<T>, what: &str) -> Result<Option<T>> {
    if hits.len() > 1 {
        return Err(Error::flow(format!("appinstall: ambiguous {what}")));
    }
    Ok(hits.pop())
}

fn has_bundle(node: &Value, bundle: &str) -> bool {
    ["identifier", "name", "bundleId"]
        .iter()
        .any(|key| attr(node, key) == bundle)
}

/// A Cell may carry the bundle itself or expose it on its exact title node.
/// A detail container can carry its own bundle. Never borrow controls across
/// nested result cells or expand the scope to the surrounding list.
fn offer<'a>(
    tree: &'a Value,
    screen: (i64, i64),
    title: &str,
    bundle: &str,
) -> Result<Option<&'a Value>> {
    let mut products = Vec::new();
    walk(tree, screen, &mut |scope| {
        let is_cell = attr(scope, "type") == "Cell";
        let scope_id = matches!(attr(scope, "type"), "Cell" | "Other") && has_bundle(scope, bundle);
        if !is_cell && !scope_id {
            return true;
        }
        let mut has_title = label(scope) == title;
        let mut identity = scope_id;
        let mut actions = Vec::new();
        for child in children(scope) {
            walk(child, screen, &mut |node| {
                let kind = attr(node, "type");
                if kind == "Cell" {
                    return false;
                }
                if matches!(kind, "StaticText" | "Text" | "Button" | "Other")
                    && label(node) == title
                    && point(node, screen).is_some()
                {
                    has_title = true;
                    identity |= has_bundle(node, bundle);
                }
                if kind == "Button"
                    && point(node, screen).is_some()
                    && (GET.contains(&label(node)) || OPEN.contains(&label(node)))
                {
                    actions.push(node);
                }
                true
            });
        }
        if identity && has_title {
            products.push(actions);
            return false;
        }
        true
    });
    let Some(actions) = unique(products, "exact product")? else {
        return Ok(None);
    };
    Ok(
        unique(actions, "install/Open control in the exact product")?.filter(|node| {
            !ui::is_explicitly_disabled(node)
                && (node["value"].is_null()
                    || node["value"].as_str().is_some_and(|value| {
                        value.trim().is_empty() || value.trim() == label(node)
                    }))
        }),
    )
}

impl<R: Reader> Flow<R> {
    fn install_front(&mut self, expected: &str, end: f64) -> Result<()> {
        match self.c.live_foreground_app(Some(end)) {
            Ok(front) if front == expected => Ok(()),
            Ok(front) => Err(Error::flow(format!(
                "appinstall: expected {expected} in front, saw {front}"
            ))),
            Err(reason) => Err(Error::flow(format!(
                "appinstall: could not verify {expected} in front ({reason})"
            ))),
        }
    }

    fn install_tree(&mut self, end: f64) -> Result<Value> {
        self.c.stop_check().map_err(Error::flow)?;
        self.install_front(STORE, end)?;
        let (alert, _) = Self::query_alert_text(&mut self.c, Some(end));
        if alert != AlertStatus::Absent {
            return Err(Error::flow(
                "appinstall: an alert is present or could not be ruled out; inspect it manually",
            ));
        }
        let path = self.c.spath(SOURCE);
        let reply = self.c.req("GET", &path, None, 5.0, Some(end));
        let tree = guard::success_value(&reply)
            .ok()
            .filter(|tree| {
                attr(tree, "type") == "Application" && matches!(attr(tree, "bundleId"), "" | STORE)
            })
            .ok_or_else(|| {
                Error::flow("appinstall: could not read the App Store accessibility tree")
            })?;
        if has_modal(tree) {
            return Err(Error::flow(
                "appinstall: an App Store sheet needs manual attention",
            ));
        }
        Ok(tree.clone())
    }

    fn install_press(&mut self, node: &Value, screen: (i64, i64), end: f64) -> Result<()> {
        let (x, y) =
            point(node, screen).ok_or_else(|| Error::flow("appinstall: control is off screen"))?;
        self.install_front(STORE, end)?;
        self.tap_point_with(x, y, label(node), Some(end), false)
    }

    fn install_poll(end: f64, interval: f64, waiting_for: &str) -> Result<()> {
        clock::sleep((end - clock::now()).clamp(0.0, interval));
        if clock::now() >= end {
            return Err(Error::flow(format!(
                "appinstall: timed out waiting for {waiting_for}"
            )));
        }
        Ok(())
    }

    fn install_search(&mut self, name: &str, screen: (i64, i64), end: f64) -> Result<()> {
        let mut entered_search = false;
        let field_rect = loop {
            let tree = self.install_tree(end)?;
            let mut fields = Vec::new();
            let mut tabs = Vec::new();
            walk(&tree, screen, &mut |node| {
                if point(node, screen).is_some() && !ui::is_explicitly_disabled(node) {
                    let kind = attr(node, "type");
                    if kind == "SearchField"
                        || (kind == "TextField" && SEARCH.contains(&label(node)))
                    {
                        fields.push(node);
                    }
                    if kind == "Tab" && SEARCH.contains(&label(node)) {
                        tabs.push(node);
                    }
                    if kind == "TabBar" {
                        walk(node, screen, &mut |button| {
                            if attr(button, "type") == "Button"
                                && SEARCH.contains(&label(button))
                                && point(button, screen).is_some()
                                && !ui::is_explicitly_disabled(button)
                            {
                                tabs.push(button);
                            }
                            true
                        });
                        return false;
                    }
                }
                true
            });
            if let Some(field) = unique(fields, "search field")? {
                self.install_press(field, screen, end)?;
                break rect(field);
            }
            if !entered_search {
                if let Some(tab) = unique(tabs, "Search control")? {
                    self.install_press(tab, screen, end)?;
                    entered_search = true;
                }
            }
            Self::install_poll(end, 0.3, "an App Store search field")?;
        };

        // Bind clearing and typing to the live search element. A tap that did
        // not focus it must not send keys into a sign-in field.
        let eid = loop {
            self.install_front(STORE, end)?;
            if let Some(eid) = self.install_active_element(end) {
                let path = self.c.spath(&format!("/element/{eid}/attribute/type"));
                let kind = self.c.req("GET", &path, None, 5.0, Some(end));
                let path = self.c.spath(&format!("/element/{eid}/rect"));
                let bounds = self.c.req("GET", &path, None, 5.0, Some(end));
                let same_rect = guard::success_value(&bounds)
                    .ok()
                    .and_then(|bounds| rect(&json!({"rect": bounds})))
                    .is_some_and(|bounds| Some(bounds) == field_rect);
                if same_rect
                    && matches!(
                        guard::success_value(&kind).ok().and_then(Value::as_str),
                        Some("XCUIElementTypeSearchField" | "XCUIElementTypeTextField")
                    )
                {
                    break eid;
                }
            }
            Self::install_poll(end, 0.3, "focus on the App Store search field")?;
        };
        self.install_front(STORE, end)?;
        let path = self.c.spath(&format!("/element/{eid}/clear"));
        let reply = self.c.req("POST", &path, Some(&json!({})), 5.0, Some(end));
        check_wda_ok(reply, "appinstall: clear search", false)?;
        self.install_type_query(&eid, name, end)?;
        self.guard_submit("appinstall: submit search")?;
        let path = self.c.spath(&format!("/element/{eid}/attribute/value"));
        let value = self.c.req("GET", &path, None, 5.0, Some(end));
        if self.install_active_element(end).as_deref() != Some(&eid)
            || guard::success_value(&value).ok().and_then(Value::as_str) != Some(name)
        {
            return Err(Error::flow(
                "appinstall: search field lost focus or query did not match",
            ));
        }
        self.install_type_query(&eid, "\n", end)
    }

    fn install_type_query(&mut self, eid: &str, text: &str, end: f64) -> Result<()> {
        self.install_front(STORE, end)?;
        self.mark_action(false, true);
        let path = self.c.spath(&format!("/element/{eid}/value"));
        let chars: Vec<String> = text.chars().map(|c| c.to_string()).collect();
        let reply = self.c.req(
            "POST",
            &path,
            Some(&json!({"text": text, "value": chars})),
            5.0,
            Some(end),
        );
        check_wda_ok(reply, "appinstall: write to search field", false)?;
        Ok(())
    }

    fn install_active_element(&mut self, end: f64) -> Option<String> {
        let path = self.c.spath("/element/active");
        let reply = self.c.req("GET", &path, None, 5.0, Some(end));
        let value = guard::success_value(&reply).ok()?;
        value["ELEMENT"]
            .as_str()
            .or_else(|| value["element-6066-11e4-a52e-4f735466cecf"].as_str())
            .filter(|eid| !eid.is_empty())
            .map(str::to_string)
    }

    pub(super) fn appinstall(&mut self, arg: &str) -> Result<String> {
        let bad_arg = || Error::flow("appinstall: use appinstall:NAME|BUNDLE[|open]");
        let mut parts = arg.split('|').map(str::trim);
        let name = parts
            .next()
            .filter(|s| !s.is_empty() && !s.chars().any(char::is_control))
            .ok_or_else(bad_arg)?;
        let bundle = parts
            .next()
            .filter(|s| crate::apps::looks_like_bundle_id(s) && *s != STORE)
            .ok_or_else(bad_arg)?;
        let open = match (parts.next(), parts.next()) {
            (None, None) => false,
            (Some("open"), None) => true,
            _ => return Err(bad_arg()),
        };

        // This is a target transition like open:, so the starting app need
        // not be allowlisted. Both destinations must pass before any action.
        self.c.ensure_allowed_target("appinstall", Some(bundle))?;
        self.c.ensure_allowed_target("appinstall", Some(STORE))?;
        if !self.timeout.is_finite() || self.timeout <= 0.0 {
            return Err(Error::flow(
                "appinstall: --timeout must be finite and positive",
            ));
        }
        let end = clock::now() + self.timeout;
        self.step_inner(&format!("open:{STORE}"))?;
        let screen = self.r.pt_size(&mut self.c, false, Some(end))?;
        self.install_search(name, screen, end)?;

        let mut requested = false;
        loop {
            let tree = self.install_tree(end)?;
            if let Some(button) = offer(&tree, screen, name, bundle)? {
                if OPEN.contains(&label(button)) {
                    self.install_front(STORE, end)?;
                    if open {
                        self.step_inner(&format!("open:{bundle}"))?;
                        self.install_front(bundle, end)?;
                    }
                    return Ok(bundle.to_string());
                }
                if !requested {
                    self.install_press(button, screen, end)?;
                    requested = true;
                }
            }
            let (interval, waiting_for) = if requested {
                (1.0, "an enabled Open/Åpne control for the verified product after requesting download; allow more time with --timeout")
            } else {
                (0.3, "an enabled free Get/redownload or Open/Åpne control for an exact title and bundle ID in the same product listing")
            };
            Self::install_poll(end, interval, waiting_for)?;
        }
    }
}
