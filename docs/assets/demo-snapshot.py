#!/usr/bin/env python3
"""Fabricate a realistic tfa snapshot.json for the README screenshot.
The daemon loads it at boot (TFA_STATE_DIR points at the demo dir); with
scanner disabled the states stay exactly as crafted."""
import json, sys, time

now = int(time.time() * 1000)
M = 60_000

def sess(pane, name, w, p, state, since_ago, task=None, reason=None,
         model="claude-fable-5", pct=None, used=0, agent="claude",
         branch="main", pid=4242, consumed=0):
    s = {
        "pane_id": pane, "agent": agent, "session_name": name,
        "state": state, "state_since_ms": now - since_ago,
        "current_task": task, "cwd": f"/Users/dev/{name}",
        "last_activity_ms": now - min(since_ago, 5_000),
        "source": "both", "pid": pid, "model": model,
        "context": {"used_tokens": used, "max_tokens": 200_000, "percent": pct} if pct is not None else None,
        "tokens": {"input": 1200, "output": 5400, "cache_read": used, "cache_creation": 900,
                   "total": used + 7500} if pct is not None else None,
        "git_branch": branch, "transcript_path": None, "agent_session_id": None,
        "consumed_tokens": consumed, "window_index": w, "pane_index": p,
    }
    if reason is not None:
        s["reason"] = reason
    return s

store = {"sessions": {
    "%1": sess("%1", "api", 0, 0, "waiting_input", 11 * M,
               reason="needs permission to run Bash(git push)",
               task="ship the payments refactor", pct=62, used=124_000, consumed=310_000),
    "%2": sess("%2", "api", 2, 1, "waiting_input", 4 * M,
               reason="plan approval required",
               task="add a rate limiter to /v1/orders", pct=41, used=82_000, consumed=150_000),
    "%3": sess("%3", "web", 1, 0, "working", 30_000,
               task="fix the flaky checkout e2e", pct=38, used=76_000, consumed=95_000),
    "%4": sess("%4", "data", 0, 2, "working", 2 * M,
               task="backfill the events table", model="gpt-5.3-codex",
               agent="codex", pct=55, used=110_000, pid=5151, consumed=210_000),
    "%5": sess("%5", "infra", 0, 0, "done", 2 * M,
               task="terraform plan for the new VPC", pct=23, used=46_000, consumed=64_000),
}}

out = sys.argv[1]
with open(out, "w") as f:
    json.dump(store, f)
print(f"wrote {out} at now={now}")
