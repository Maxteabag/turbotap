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
