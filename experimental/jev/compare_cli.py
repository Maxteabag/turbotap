"""Audit four independent native Codex CLI turns against saved synthetic Jev choices."""
import json
import os
from pathlib import Path
import shutil
import statistics
import subprocess
import tempfile
import time

from planner import TASKS, choices

HERE = Path(__file__).resolve().parent
MODEL = "gpt-6.2-astra"
CASES = [("sum", 0), ("decimal", 3), ("sum", 4), ("missing", 0)]
ARGS = ["exec", "--ephemeral", "--json", "--skip-git-repo-check",
        "--ignore-user-config", "--ignore-rules", "--sandbox", "read-only",
        "--model", MODEL, "-c", 'model_reasoning_effort="low"',
        "-c", "include_environment_context=false", "-c", "project_doc_max_bytes=0",
        "-c", 'web_search="disabled"', "--disable", "hooks", "--disable", "apps", "-"]


def audit(stream):
    lifecycle = []
    messages = []
    usage = {}
    rejected = None
    for line in stream.splitlines():
        event = json.loads(line)
        kind = event["type"]
        if kind in ("thread.started", "turn.started", "turn.completed"):
            lifecycle.append(kind)
            if kind == "turn.completed":
                usage = {k: n for k, n in event.get("usage", {}).items()
                         if k in ("input_tokens", "cached_input_tokens", "output_tokens",
                                  "reasoning_output_tokens") and type(n) is int and n >= 0}
        elif kind in ("item.started", "item.updated", "item.completed"):
            item = event["item"]
            if item["type"] == "agent_message" and kind == "item.completed":
                messages.append(item["text"].strip())
            elif item["type"] not in ("reasoning", "agent_message"):
                rejected = "tool_or_nonmessage_item"
        else:
            rejected = "unexpected_event"
    if lifecycle != ["thread.started", "turn.started", "turn.completed"] or len(messages) != 1:
        rejected = rejected or "incomplete_turn"
    return rejected, messages[0] if not rejected else None, usage


def main():
    binary = shutil.which("codex")
    if not binary:
        raise SystemExit("codex_missing")
    auth_status = subprocess.run([binary, "login", "status"], capture_output=True, text=True)
    if "Logged in using ChatGPT" not in auth_status.stdout + auth_status.stderr:
        raise SystemExit("existing_chatgpt_login_required")
    version = subprocess.check_output([binary, "--version"], text=True).strip()
    auth = Path(os.environ.get("CODEX_HOME", Path.home() / ".codex")) / "auth.json"
    if not auth.is_file():
        raise SystemExit("file_backed_chatgpt_login_required")
    prior = [json.loads(line) for line in (HERE / "live.jsonl").read_text().splitlines()][:-1]
    cases = [next(r for r in prior if (r["task"], r["step"]) == case) for case in CASES]
    stats = dict(requested_model=MODEL, cli=version, attempted=0, accepted=0, correct=0,
                 abstention_correct=0, rejected={}, usage={})
    durations = []
    with tempfile.TemporaryDirectory(prefix="choice-bench-") as tmp:
        root = Path(tmp)
        cli_home = root / "cli"
        cli_home.mkdir(mode=0o700)
        (cli_home / "auth.json").symlink_to(auth.resolve())
        # The bundled catalog can predate the selected model. Seed the clean home
        # with the installed CLI's current discovery cache when available.
        catalog = auth.with_name("models_cache.json")
        if catalog.is_file():
            shutil.copyfile(catalog, cli_home / catalog.name)
        env = {k: os.environ[k] for k in ("PATH", "HOME", "LANG", "XDG_RUNTIME_DIR") if k in os.environ}
        env["CODEX_HOME"] = str(cli_home)
        start = time.perf_counter()
        for i, gold in enumerate(cases):
            options = choices(gold["step"])
            if options != gold["controls"]:
                raise SystemExit("jev_receipt_mismatch")
            prompt = {"task": TASKS[gold["task"]][0], "start": "fresh basic calculator",
                      "keys_already_pressed": gold["history"], "options": options,
                      "instructions": "Use no tools. Select the single next control in order. "
                                      "Choose done after completion. Reply with only its option ID."}
            cwd = root / str(i)
            cwd.mkdir()
            begin = time.perf_counter()
            stats["attempted"] += 1
            try:
                proc = subprocess.run([binary, *ARGS], cwd=cwd, env=env, input=json.dumps(prompt),
                                      capture_output=True, text=True, timeout=60)
            except subprocess.TimeoutExpired:
                durations.append((time.perf_counter() - begin) * 1000)
                stats["stop_reason"] = "cli_timeout"
                break
            durations.append((time.perf_counter() - begin) * 1000)
            if proc.returncode:
                diagnostic = (proc.stdout + proc.stderr).lower()
                reason = next((tag for tag in (
                    "usage_limit_reached", "insufficient_quota", "rate_limit_exceeded",
                    "model_not_found", "unauthorized") if tag in diagnostic), "cli_failed")
                stats["rejected"][reason] = stats["rejected"].get(reason, 0) + 1
                if reason == "model_not_found":
                    continue
                stats["stop_reason"] = reason
                break
            try:
                rejected, answer, usage = audit(proc.stdout)
            except (TypeError, ValueError, KeyError, AttributeError):
                rejected, answer, usage = "invalid_event_stream", None, {}
            for k, n in usage.items():
                stats["usage"][k] = stats["usage"].get(k, 0) + n
            if rejected:
                stats["rejected"][rejected] = stats["rejected"].get(rejected, 0) + 1
            else:
                stats["accepted"] += 1
                correct = answer == gold["expected"]
                stats["correct"] += correct
                stats["abstention_correct"] += correct and gold["expected"] == "unavailable"
        stats["loop_wall_ms"] = round((time.perf_counter() - start) * 1000, 2)
    stats["process_sum_ms"] = round(sum(durations), 2)
    stats["process_median_ms"] = round(statistics.median(durations), 2)
    matched = cases[:stats["attempted"]]
    jev = dict(checkpoints=len(matched), correct=sum(r["correct"] for r in matched),
               abstention_correct=sum(r["correct"] and r["expected"] == "unavailable" for r in matched),
               request_sum_ms=round(sum(r["total_ms"] for r in matched), 2),
               usage={k: sum(r["usage"][k] for r in matched) for k in ("input_tokens", "output_tokens")})
    print(json.dumps({"native_codex": stats, "recorded_jev": jev}))
    return int(stats["accepted"] != len(CASES))


if __name__ == "__main__":
    raise SystemExit(main())
