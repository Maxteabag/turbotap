# Jev Calculator planner

Run from the repository root with Python 3. The planner emits one JSON proposal per checkpoint and a final summary. Select a single checkpoint for parent-side validation:

```sh
python3 experimental/jev/planner.py --task decimal --step 3
python3 experimental/jev/planner.py
TYPESAFE_API_KEY="$(secret-tool lookup service typesafe)" python3 experimental/jev/planner.py --mode live
```

The default is offline oracle replay. Only live mode sends requests, directly to TypeSafe. Inputs are limited to built-in synthetic tasks and key histories. A parent can look up each proposal in that row's controls map and compare it with the expected ID. IDs are local to the checkpoint. The planner exposes no device interface or action executor.

The three tasks cover addition, decimal multiplication, completion and a missing square-root control. The benchmark supplies the correct preceding key history independently at each checkpoint and shuffles control IDs deterministically. It measures next-control selection under a correct prefix. It does not simulate a display, press keys or measure recovery after a wrong choice.

## Recorded run, 2026-11-25

The JSONL receipts contain all 13 checkpoints. The fixture replayed 13/13. TypeSafe returned jev-1.14.0 and grounded 12/13 correctly. Both arithmetic sequences and their completion decisions matched throughout. On the missing-control task the model proposed 9 despite the explicit requirement to use the square-root key first. A constrained option set alone therefore cannot enforce abstention.

Median time to first response body byte was 225.22 ms. Median time to validated proposal was 225.29 ms. Both include a fresh HTTPS connection per request. The API returns one JSON document, so model token TTFT is unobservable and recorded as null. Usage totaled 7,698 input and 2,318 output tokens. At the published USD 0.042 per million input tokens and free output, this implies USD 0.000323316 at list price. This is an estimate, not an invoice. [TypeSafe model pricing](https://docs.typesafe.ai/models.md)

## Parent device comparison

With basic Calculator already foreground in portrait and an unlocked device configured in TurboTap, run this from the parent Clarp session:

```sh
PYTHONPATH=src TYPESAFE_API_KEY="$(secret-tool lookup service typesafe)" python3 experimental/jev/parent_e2e.py
```

Each arm clears the current Calculator entry, checks for a single zero readout, then enters 7 + 4 =. The first arm selects keys deterministically. The second uses Jev's proposal and checks it against the deterministic next key before tapping. A fresh fastUI snapshot must confirm the foreground bundle and keypad geometry before every tap. Each request sends only the fixed task, prior arithmetic keys and a fresh map of opaque IDs to canonical digit and operator labels. Display text, coordinates, bundle identifiers, unknown labels and reset controls stay local.

Each arm prints its receipt only after observing exactly 11 in the numeric readout above the keypad, at the same vertical position as the verified zero. Unrecognized locales, inaccessible or ambiguous readouts, and model disagreement stop the run. Elapsed time covers selection, taps and final verification after reset. This runner requires the parent to perform device validation.

## Four direct Codex CLI turns

Run from the worktree root using the installed CLI and an existing file-backed ChatGPT login:

```sh
python3 experimental/jev/compare_cli.py
cat experimental/jev/direct_comparison.json
```

Each checkpoint starts a fresh native `codex exec --ephemeral --json --sandbox read-only --model gpt-6.2-astra` process with low reasoning effort in an empty directory. All four share a temporary Codex home with access to the existing login. The CLI constructs and sends its ordinary request. The prompt supplies the same synthetic task, history and option map as the corresponding Jev checkpoint and instructs the model to return one ID without tools.

The runner audits stdout JSONL in memory. An accepted turn requires the normal thread and turn lifecycle, one final agent message and only reasoning or agent-message items. Any tool item or unrecognized event rejects that trial. A wrong or malformed answer on an otherwise accepted trial scores zero. The receipt contains aggregate counts, token usage and wall times. CLI process timing includes startup through exit. Loop timing also includes per-checkpoint setup and auditing. Both exclude the one-time login and version preflight. Cached input tokens, when reported, are part of input tokens.

Jev timings come from prior live receipts and include a fresh HTTPS connection per request. The Jev benchmark did not measure Python process startup. The four cases include a known Jev abstention failure.

The recorded direct run attempted all four checkpoints. Every CLI process failed with a diagnostic containing `model_not_found`. No native answer or usage was available to score. The four failed processes took 11.73228 seconds in total, and the loop took 11.73265 seconds. An earlier single-checkpoint attempt and a catalog-seeding diagnostic retry also failed and are excluded from those timings.

## Historical relay experiment

`comparison.jsonl` preserves the original relay measurements. The relay replaced the CLI-generated request, which limits those numbers to that experiment. Use `direct_comparison.json` for ordinary native CLI turns.

The first digit, multiplication operator, completed task and missing square-root key use identical task text, correct prior key histories and option maps in both arms. Codex CLI 0.164.0 started a new process for each turn. Its one-use local relay replaced the CLI-generated request with the synthetic prompt, empty tools and explicit no-tool choice, and streamed the response from the fixed ChatGPT endpoint back to the CLI. All four responses named gpt-6.2-astra, low effort was requested, and all four processes exited zero.

Codex selected 4/4 including abstention. Jev selected 3/4, choosing 9 on the missing-key checkpoint. Codex process wall times in checkpoint order were 2758.43, 3117.44, 2991.34 and 2730.27 ms. The four processes summed to 11597.48 ms. The comparison loop took 11611.89 ms including relay setup and cleanup, excluding the one-time login and version checks. Median CLI startup to local request was 987.33 ms and median process start to first streamed text was 2458.33 ms. Codex used 809 input and 23 output tokens.

Jev's matching live receipts used 2360 input and 714 output tokens. Its four request-to-validated-proposal times were 225.29, 235.73, 206.21 and 188.65 ms, totaling 855.88 ms. These include fresh HTTPS connections. The original benchmark used one Python process and did not record interpreter startup or a separate four-step process wall time. Reusing the receipts avoided new TypeSafe usage. The samples were taken at different times, and the missing-key case was a known Jev failure.
