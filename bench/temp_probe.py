#!/usr/bin/env python3
"""pv bench — real temperature sweep against the Groq API.

For every Groq text model x temperature, fire REPS identical calls at three
deterministically graded supervisor probes and record:

  pass      fraction of calls graded correct (exact rules, no LLM judge)
  answers   canonical answer per call (for self-agreement)
  tokens    mean completion tokens (reasoning inflation shows up here)

Writes temperature.json next to itself. `--report` prints summary tables
from an existing temperature.json without touching the network.
`--smoke` runs a 6-call sanity pass.

Key resolution mirrors pv: $GROQ_API_KEY, then ~/.config/pv/groq_api_key.
The key is never printed or written to disk.
"""

import json
import os
import re
import subprocess
import sys
import time
from collections import Counter
from pathlib import Path

MODELS = [
    "llama-3.1-8b-instant",
    "meta-llama/llama-4-scout-17b-16e-instruct",
    "openai/gpt-oss-20b",
    "qwen/qwen3-32b",
    "qwen/qwen3.6-27b",
    "llama-3.3-70b-versatile",
    "openai/gpt-oss-120b",
    # kimi-k2-instruct-0905 is on the pricing page but not accessible on the
    # developer tier (model_not_found) — excluded from the measured sweep.
]
REASONING = {  # models that think before answering; need token headroom
    "openai/gpt-oss-20b",
    "openai/gpt-oss-120b",
    "qwen/qwen3-32b",
    "qwen/qwen3.6-27b",
    "moonshotai/kimi-k2-instruct-0905",
}
TEMPS = [0.0, 0.2, 0.5, 0.8, 1.0, 1.5]
REPS = 5
OUT = Path(__file__).with_name("temperature.json")


def grade_json(text):
    m = re.search(r"\{.*\}", text, re.S)
    if not m:
        return "parse-fail", False
    try:
        obj = json.loads(m.group(0))
    except Exception:
        return "parse-fail", False
    action = str(obj.get("action", "")).lower()
    app = str(obj.get("app", "")).lower()
    try:
        gb = float(obj.get("gb", 0))
    except Exception:
        gb = 0.0
    ok = action in ("suspend", "freeze") and "firefox" in app and 1.0 <= gb <= 1.6
    return action or "empty", ok


def grade_action(text):
    toks = re.findall(r"SUSPEND_BROWSER|MIGRATE_BUILD|THROTTLE_INDEXER|NONE", text)
    if not toks:
        return "no-token", False
    return toks[-1], toks[-1] == "SUSPEND_BROWSER"


def grade_arith(text):
    nums = re.findall(r"-?\d+", text)
    if not nums:
        return "no-number", False
    return nums[-1], int(nums[-1]) == 94


PROBES = [
    {
        "id": "json",
        "grade": grade_json,
        "system": "You are pv, a process supervisor. Reply with a single JSON object and nothing else.",
        "user": "Firefox has been idle 17 minutes and holds 1.3 GB. RAM is 81% committed. "
                "Emit exactly: {\"action\": ..., \"app\": ..., \"gb\": ...} describing the best action.",
    },
    {
        "id": "action",
        "grade": grade_action,
        "system": "You are pv, a process supervisor. Reply with exactly one token: "
                  "SUSPEND_BROWSER, MIGRATE_BUILD, THROTTLE_INDEXER, or NONE. No other text.",
        "user": "RAM 81% and climbing. Browser idle 17 min holding 1.3 GB. "
                "cargo build active (interruptible). Background indexer busy. "
                "Pick the single most urgent action.",
    },
    {
        "id": "arith",
        "grade": grade_arith,
        "system": "You are pv, a process supervisor. Answer with just the integer, no units, no prose.",
        "user": "The machine has 12 cores and load average 11.23. "
                "What is the load as a percentage of capacity, rounded to the nearest integer?",
    },
]


def api_key():
    key = os.environ.get("GROQ_API_KEY", "").strip()
    if key:
        return key
    try:
        return Path.home().joinpath(".config/pv/groq_api_key").read_text().strip()
    except OSError:
        return ""


def call(key, model, temp, probe):
    max_tokens = 900 if model in REASONING else 200
    payload = json.dumps({
        "model": model,
        "temperature": temp,
        "stream": False,
        "max_tokens": max_tokens,
        "messages": [
            {"role": "system", "content": probe["system"]},
            {"role": "user", "content": probe["user"]},
        ],
    })
    for attempt in (1, 2):
        r = subprocess.run(
            ["curl", "-sS", "-m", "45", "-X", "POST",
             "https://api.groq.com/openai/v1/chat/completions",
             "-H", f"Authorization: Bearer {key}",
             "-H", "Content-Type: application/json",
             "--data-binary", "@-"],
            input=payload, capture_output=True, text=True)
        try:
            d = json.loads(r.stdout)
        except Exception:
            d = {}
        choices = d.get("choices")
        if choices:
            msg = choices[0].get("message", {})
            text = msg.get("content") or ""
            tokens = d.get("usage", {}).get("completion_tokens", 0)
            return text, tokens, None
        err = json.dumps(d)[:140] if d else (r.stderr.strip()[:140] or "empty response")
        if attempt == 1:
            time.sleep(6)
    return None, 0, err


def run(smoke=False):
    key = api_key()
    if not key:
        sys.exit("no GROQ_API_KEY and no ~/.config/pv/groq_api_key")
    models = MODELS if not smoke else [MODELS[0], MODELS[2]]
    temps = TEMPS if not smoke else [0.2]
    reps = REPS if not smoke else 1

    data = {"fetched": time.strftime("%Y-%m-%d %H:%M UTC", time.gmtime()),
            "reps": reps, "temps": temps, "probes": [p["id"] for p in PROBES],
            "cells": {}}
    total = len(models) * len(temps) * len(PROBES) * reps
    done = 0
    for model in models:
        cells = {}
        for temp in temps:
            tcell = {}
            for probe in PROBES:
                answers, passes, tokens, errs = [], 0, [], 0
                for _ in range(reps):
                    text, tok, err = call(key, model, temp, probe)
                    done += 1
                    if err:
                        errs += 1
                        answers.append("error")
                    else:
                        canon, ok = probe["grade"](text)
                        answers.append(canon)
                        passes += 1 if ok else 0
                        tokens.append(tok)
                    if done % 25 == 0:
                        print(f"  {done}/{total} calls", flush=True)
                    time.sleep(0.1)
                tcell[probe["id"]] = {
                    "pass": passes,
                    "answers": answers,
                    "tokens": round(sum(tokens) / len(tokens)) if tokens else 0,
                    "errors": errs,
                }
            cells[str(temp)] = tcell
        data["cells"][model] = cells
        print(f"done {model}", flush=True)
    OUT.write_text(json.dumps(data, indent=1))
    print(f"wrote {OUT}")


def report():
    data = json.loads(OUT.read_text())
    temps = [float(t) for t in data["temps"]]
    print(f"fetched: {data['fetched']} · reps: {data['reps']} · probes: {', '.join(data['probes'])}\n")
    for model, cells in data["cells"].items():
        print(model)
        print(f"  {'temp':>4}  {'pass':>5}  {'agree':>5}  {'tok/call':>8}")
        best, best_t = -1.0, None
        for t in temps:
            cell = cells[str(t)] if str(t) in cells else cells[f"{t}"]
            passes = sum(c["pass"] for c in cell.values())
            total = sum(len(c["answers"]) for c in cell.values())
            agrees, toks, weighted = [], [], 0
            for c in cell.values():
                top = Counter(c["answers"]).most_common(1)[0][1]
                agrees.append(top / len(c["answers"]))
                toks.append(c["tokens"])
            p = passes / total
            a = sum(agrees) / len(agrees)
            score = p * a
            mark = ""
            if score > best:
                best, best_t = score, t
            print(f"  {t:>4}  {p:>5.2f}  {a:>5.2f}  {sum(toks)/len(toks):>8.0f}{mark}")
        print(f"  -> ideal temp: {best_t} (score {best:.2f})\n")


if __name__ == "__main__":
    if "--report" in sys.argv:
        report()
    elif "--smoke" in sys.argv:
        run(smoke=True)
    else:
        run()
