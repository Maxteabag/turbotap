"""Four paired choices using installed ChatGPT-authenticated Codex and prior live Jev receipts."""
import http.client
import json
from pathlib import Path
import secrets
from http.server import BaseHTTPRequestHandler, HTTPServer
import shutil
import subprocess
import threading
import time

from planner import TASKS, choices

HERE = Path(__file__).resolve().parent
MODEL = "gpt-6.2-astra"
CASES = [("sum", 0), ("decimal", 3), ("sum", 4), ("missing", 0)]
CLOCK = time.perf_counter


def elapsed(start):
    return round((CLOCK() - start) * 1000, 2)


def codex_turn(binary, prompt):
    # A one-use loopback endpoint enforces the wire-level tool and prompt boundary.
    # The CLI supplies its own existing ChatGPT authorization. Never persist headers.
    route = "/" + secrets.token_hex(20)
    info = {}
    started = CLOCK()

    class Relay(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_POST(self):
            if self.path != route + "/responses" or info:
                self.send_error(404)
                return
            self.rfile.read(int(self.headers["Content-Length"]))
            info["startup_to_request_ms"] = elapsed(started)
            auth = self.headers.get("Authorization", "")
            if not auth.startswith("Bearer "):
                self.send_error(401)
                return
            req = {"model": MODEL, "store": False, "stream": True, "tools": [],
                   "tool_choice": "none", "instructions": "Select exactly one listed option ID.",
                   "reasoning": {"effort": "low"},
                   "input": [{"role": "user", "content": prompt}]}
            upstream = http.client.HTTPSConnection("chatgpt.com", timeout=35)
            try:
                headers = {"Authorization": auth, "Content-Type": "application/json",
                           "Accept": "text/event-stream"}
                if self.headers.get("ChatGPT-Account-ID"):
                    headers["ChatGPT-Account-ID"] = self.headers["ChatGPT-Account-ID"]
                upstream.request("POST", "/backend-api/codex/responses",
                                 json.dumps(req).encode(), headers)
                response = upstream.getresponse()
                info["http_status"] = response.status
                self.send_response(response.status)
                self.send_header("Content-Type", response.getheader("Content-Type", "text/event-stream"))
                self.end_headers()
                if response.status != 200:
                    data = response.read()
                    try:
                        err = json.loads(data).get("error", {})
                        info["error_code"] = err.get("code") or err.get("type")
                    except (ValueError, AttributeError):
                        pass
                    self.wfile.write(data)
                    return
                for line in response:
                    if line.startswith(b"data: {"):
                        event = json.loads(line[6:])
                        if event.get("type") == "response.output_text.delta":
                            info.setdefault("first_text_ms", elapsed(started))
                        if event.get("type") == "response.completed":
                            result = event["response"]
                            info.update(returned_model=result.get("model"),
                                        usage={k: result["usage"][k] for k in
                                               ("input_tokens", "output_tokens", "total_tokens")},
                                        response_ms=elapsed(started))
                    self.wfile.write(line)
                    self.wfile.flush()
            finally:
                upstream.close()

    with HTTPServer(("127.0.0.1", 0), Relay) as relay:
        thread = threading.Thread(target=relay.serve_forever, kwargs={"poll_interval": .01}, daemon=True)
        thread.start()
        base = f"http://127.0.0.1:{relay.server_port}{route}"
        args = [binary, "exec", "--ephemeral", "--skip-git-repo-check", "--json",
                "--ignore-user-config", "--ignore-rules", "-s", "read-only", "-m", MODEL,
                "-c", 'model_provider="bench"', "-c", 'model_providers.bench.name="bench"',
                "-c", f'model_providers.bench.base_url="{base}"',
                "-c", "model_providers.bench.requires_openai_auth=true",
                "-c", 'model_providers.bench.wire_api="responses"',
                "-c", "model_providers.bench.request_max_retries=0",
                "-c", "model_providers.bench.stream_max_retries=0",
                "-c", "project_doc_max_bytes=0", "-c", "include_environment_context=false",
                "-c", 'web_search="disabled"', "--disable", "hooks", "--disable", "apps",
                "--disable", "enable_request_compression", "-"]
        started = CLOCK()
        try:
            proc = subprocess.run(args, cwd=HERE, input=prompt, capture_output=True,
                                  text=True, timeout=60)
            info["process_ms"] = elapsed(started)
        finally:
            relay.shutdown()
        info["exit_code"] = proc.returncode
        answer = None
        for line in proc.stdout.splitlines():
            event = json.loads(line)
            item = event.get("item", {})
            if event.get("type") == "item.completed" and item.get("type") == "agent_message":
                answer = item["text"].strip()
        if proc.returncode and not info.get("error_code"):
            msg = (proc.stderr + proc.stdout).lower()
            info["error_code"] = next((tag for tag in (
                "usage_limit_reached", "insufficient_quota", "rate_limit_exceeded",
                "model_not_found", "unauthorized", "invalid_request_error",
                "missing field", "permission denied") if tag in msg), "cli_failed")
        return answer, info


def main():
    binary = shutil.which("codex")
    login = subprocess.run([binary, "login", "status"], capture_output=True, text=True)
    if "Logged in using ChatGPT" not in login.stdout + login.stderr:
        raise SystemExit("existing ChatGPT login required")
    version = subprocess.check_output([binary, "--version"], text=True).strip()
    prior = [json.loads(line) for line in (HERE / "live.jsonl").read_text().splitlines()][:-1]
    wall = CLOCK()
    rows = []
    for task, step in CASES:
        gold = next(r for r in prior if r["task"] == task and r["step"] == step)
        assert gold["controls"] == choices(step)
        prompt = json.dumps({"task": TASKS[task][0], "start": "fresh basic calculator",
                             "keys_already_pressed": gold["history"], "options": gold["controls"],
                             "instructions": "Select the single next control in order. Choose done after completion."},
                            ensure_ascii=False)
        answer, metrics = codex_turn(binary, prompt)
        row = {"task": task, "step": step, "prompt": json.loads(prompt), "expected": gold["expected"],
               "codex": metrics | {"version": version, "choice": answer if answer in gold["controls"] else None,
                                   "correct": answer == gold["expected"]},
               "jev": {k: gold[k] for k in ("model", "proposal", "correct", "usage", "total_ms", "first_body_ms")}}
        print(json.dumps(row), flush=True)
        rows.append(row)
        if metrics["exit_code"] or metrics.get("http_status") != 200 or not metrics.get("usage"):
            raise SystemExit("Stopped after unsuccessful inference")
    print(json.dumps({"comparison_wall_ms": elapsed(wall),
                      "codex_process_sum_ms": sum(r["codex"]["process_ms"] for r in rows),
                      "jev_recorded_request_sum_ms": sum(r["jev"]["total_ms"] for r in rows)}))


if __name__ == "__main__":
    main()
