"""Read-only next-control benchmark using built-in synthetic Calculator tasks."""
import argparse
import http.client
import json
import os
import random
import statistics
import sys
import time

KEYS = tuple("0123456789") + ("+", "−", "×", "÷", ".", "=", "AC")
# Gold sequences are used locally for fixture replay and independent scoring.
TASKS = {
    "sum": ("Enter seven plus four and show the result.", ("7", "+", "4", "=")),
    "decimal": ("Enter one point five times two and show the result.",
                ("1", ".", "5", "×", "2", "=")),
    "missing": ("Take the square root of nine using the square root key first.", ("unavailable",)),
}


def choices(step):
    labels = list(KEYS)
    random.Random(243 + step).shuffle(labels)
    controls = {f"k{i}": label for i, label in enumerate(labels)}
    return controls | {"done": "Task complete", "unavailable": "Required key missing"}


def query(task, history, controls, key):
    payload = {"model": "jev-latest", "state": {"task": task,
               "start": "fresh basic calculator", "keys_already_pressed": history},
               "questions": {"next": {"type": "choice",
                   "instructions": "Select the single next control in order. Choose done after completion.",
                   "criteria": controls}}}
    conn = http.client.HTTPSConnection("api.typesafe.ai", timeout=15)
    body = json.dumps(payload)
    start = time.perf_counter()
    try:
        conn.request("POST", "/v1/systemone", body, {"Content-Type": "application/json",
                     "Authorization": "Bearer " + key})
        response = conn.getresponse()
        if response.status != 200:
            raise ValueError(f"TypeSafe returned HTTP {response.status}")
        first = response.read(1)
        first_ms = (time.perf_counter() - start) * 1000
        result = json.loads(first + response.read())
        answer = result["answers"]["next"]
        choice = answer["choice"]
        if answer["type"] != "choice" or choice not in controls:
            raise ValueError("Provider returned an invalid choice")
        usage = {k: result["usage"][k] for k in ("input_tokens", "output_tokens")}
        if any(type(n) is not int or n < 0 for n in usage.values()):
            raise ValueError("Invalid provider usage")
        return choice, {"model": result["model"], "first_body_ms": round(first_ms, 2),
                        "ttft_ms": None, "total_ms": round((time.perf_counter()-start)*1000, 2),
                        "usage": usage}
    finally:
        conn.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mode", choices=("fixture", "live"), default="fixture")
    parser.add_argument("--task", choices=TASKS)
    parser.add_argument("--step", type=int, help="One zero-based checkpoint (requires --task)")
    args = parser.parse_args()
    if args.step is not None and (not args.task or args.step < 0):
        parser.error("--step requires --task and a nonnegative index")
    key = os.environ.get("TYPESAFE_API_KEY")
    if args.mode == "live" and not key:
        parser.error("live mode requires TYPESAFE_API_KEY")
    correct = 0
    rows = []
    for name in ([args.task] if args.task else TASKS):
        task, keys = TASKS[name]
        sequence = keys if name == "missing" else (*keys, "done")
        if args.step is not None and args.step >= len(sequence):
            parser.error("step outside this task")
        for step, expected in enumerate(sequence):
            if args.step is not None and step != args.step:
                continue
            controls = choices(step)
            expected_id = next((k for k, label in controls.items() if label == expected), expected)
            if args.mode == "live":
                selected, metrics = query(task, keys[:step], controls, key)
            else:
                selected = expected_id
                metrics = dict(model="oracle-replay", ttft_ms=None, first_body_ms=None,
                               total_ms=None, usage=None)
            correct += selected == expected_id
            row = dict(task=name, step=step, controls=controls, history=keys[:step],
                       proposal=selected, expected=expected_id, correct=selected == expected_id,
                       **metrics)
            rows.append(row)
            print(json.dumps(row, ensure_ascii=False), flush=True)
    summary = dict(mode=args.mode, correct=correct, checkpoints=len(rows))
    if args.mode == "live":
        for field in ("first_body_ms", "total_ms"):
            summary["median_" + field] = statistics.median(row[field] for row in rows)
        for field in ("input_tokens", "output_tokens"):
            summary[field] = sum(row["usage"][field] for row in rows)
    print(json.dumps({"summary": summary}))


if __name__ == "__main__":
    try:
        main()
    except (OSError, http.client.HTTPException, ValueError, KeyError, TypeError) as exc:
        sys.exit(f"Benchmark stopped: {exc}")
