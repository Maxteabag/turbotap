---
name: turbotap-retro
description: "Post-task self-optimization loop after any non-trivial turbotap (iPhone automation) task: audit what cost time, extra turns, retries or inline workarounds, then fix it once in turbotap itself or its SKILL.md with generic primitives only, compile-check, commit and push."
---

# turbotap retrospective (`turbotap-retro`)

Run this at the end of every non-trivial `turbotap` task, so each hiccup is fixed once in the tool and never slows a later run.

## 1. Audit the run you just finished

Answer these from the actual task, not in general:

1. **What cost the most time or the most extra agent turns?** For example an idle Wi-Fi timeout, a stale `/source` tree right after `open:`, a tap that hit the status bar or a longer OCR line, a `tree` call only to learn a label, an app that needed `launch:` instead of `open:`, or a `sleep:` where `wait:TEXT` or `gone:TEXT` would have settled immediately.
2. **Did you write an inline workaround?** A heredoc script, a raw `curl` to WDA, hand-computed coordinates, shell `\n` escaping instead of `key:return`, or a loop around `turbotap`. If so, could it be one `flow` verb or a smarter default in an existing verb?
3. **What did the app or iOS do that surprised you?** For example `FLAG_SECURE` blanking a PIN pad, a low-contrast/colored button missed by grayscale OCR, a WebView appending instead of replacing, keyboard-toolbar buttons duplicating form buttons, localized dismiss/back labels, a small UI reaction missed by frame diffing, or noisy `ui` controls.
4. **Did any retry risk a double action?** A send, pay or submit retried only because OCR missed the result has to be made impossible in the tool, not just in prose.

When nothing cost time and nothing surprised you, stop here. Don't patch for the sake of it.

## 2. Fix it in the tool first

- **Prefer code over prose.** Put the fix in the Rust source (`src/flow/` for verbs, `src/ocr.rs`, `src/ui.rs`, `src/wda.rs`) as a generic `flow` verb, a better default in `tap:`/`wait:`/`read`/`ui`, or richer failure output. Add a note to `skills/turbotap/SKILL.md` only for facts that code can't encode, keeping `SKILL.md` tight and scannable.
- **Generic primitives only.** Never hard-code personal names, phone numbers, IPs, UDIDs, accounts, secret entry names, company names or one app's private layout. Generalise instead: `env:VAR`, `cmd:...`, config keys in `~/.config/turbotap/config.json`, region filters (`@below:`, `@y>`), regexes (`re:`), and localized label lists checked in a single predicate.
- **Keep the fast path fast.** A fix must not add an extra `/source` fetch, per-node attribute, or fixed sleep to every step. Pay for fallbacks only when the cheap path misses or is ambiguous (non-exact match, no reaction, colored button missed after initial OCR frames), and check that OCR tweaks don't regress dark-mode or mixed-polarity screens.
- **Fix speed claims at the source.** Only record numbers actually measured on a real device (or label offline microbenchmarks explicitly), and update `README.md` and `docs/HOW_IT_WORKS.md` together.

## 3. Validate without repeating real actions

```bash
cargo build --release && ./target/release/turbotap --help >/dev/null
cargo test                                       # offline mock-WDA suite
turbotap preflight                               # when a phone is reachable
```

- Run a harmless `flow` that touches the changed code path (`read`, navigation, a reversible toggle that you set back) when a device is available, or unit/offline tests for parser/OCR changes when offline.
- Never re-trigger real sends, payments, sign-ins or other external actions to test a retro patch.

## 4. Privacy check, then commit and push

```bash
git diff | grep -nE '([0-9]{1,3}\.){3}[0-9]{1,3}|[0-9A-F]{8}-[0-9A-F]{16}|@[a-z0-9-]+\.[a-z]{2,}' || echo clean
```

Review every hit. Placeholder values such as `192.168.1.50`, `example.com`, or `000081XX-...` are fine, and real ones are not. Verify commit author identity (`git config user.name` / `user.email`), commit only the files you changed with a conventional message (`perf(tap): ...`, `fix(flow): ...`), and push. If you installed `turbotap` from Git, reinstall it so the fix is live (installed skills refresh themselves on the next run):

```bash
cargo install --force --git https://github.com/Maxteabag/turbotap.git
```

Report the bottleneck you fixed, the commit, and anything you left for later, in two or three lines.
