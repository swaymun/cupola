#!/usr/bin/env python3
"""Read-only rendering inventory. Emits counts only, never text, paths or IDs.

Raw transcript counts are candidate-bearing text records, not proven UI bugs.
Do not count tool arguments/results as chat messages or event mirrors twice.
RPC uses only initialize, projects.list, threads.list and threads.read; it never
attaches a session. A GET of Cupola's history/activity identifies T3 ownership
because threads.read carries no holder field. No direct T3 access is used.
"""
import argparse
import collections
import datetime
import json
from pathlib import Path
import re
import runpy
import time

TAGS = re.compile(r"<(system-reminder|command-name|command-message|command-args|task-notification|pasted_content|bash-input|bash-stdout|bash-stderr|local-command-stdout|local-command-stderr|[a-z]+(?:-[a-z]+)+)(?:\s[^>]*|)>")
KNOWN_TAGS = set("system-reminder command-name command-message command-args task-notification pasted_content bash-input bash-stdout bash-stderr local-command-stdout local-command-stderr oai-mem-citation".split())
PATTERNS = {
    "xml_tags": TAGS,
    "ansi_csi": re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]"),
    "ansi_osc": re.compile(r"\x1b\]"),
    "ansi_other": re.compile(r"\x1b(?![\[\]])|[\x90\x9b\x9d]"),
    "orphan_osc_candidate": re.compile(r"(?:^|\n|\s)\](?:0|1|2|8|52|133|1337);"),
    "tool_json": re.compile(r'"(?:tool_call_id|tool_use_id|function_call|tool_calls|recipient_name)"\s*:'),
    "internal_ids": re.compile(r"\b(?:call_[A-Za-z0-9_-]{8,}|toolu_[A-Za-z0-9_-]{8,}|[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})\b"),
    "tables": re.compile(r"(?m)^\s*\|?\s*:?-{3,}:?\s*\|[\s|:\-]*$"),
    "escaped_table_pipe": re.compile(r"(?m)^.*\|.*\\\|.*$"),
    "nested_list": re.compile(r"(?m)^[ \t]{2,}(?:[-*+] |\d+[.)] )"),
    "indented_fence": re.compile(r"(?m)^ {4,}(?:`{3,}|~{3,})"),
    "html_entities": re.compile(r"&(?:amp|lt|gt|quot|apos|nbsp|#\d+|#x[\da-fA-F]+);"),
    "long_line": re.compile(r"(?m)^[^\n]{1000,}$"),
    "inline_base64": re.compile(r"data:image/[^;,\s]+;base64,|(?<![A-Za-z0-9+/])[A-Za-z0-9+/]{512,}={0,2}(?![A-Za-z0-9+/])"),
    "memory_citation_trailer": re.compile(r"\n<oai-mem-citation>\n<citation_entries>\n[\s\S]*\n</rollout_ids>\n</oai-mem-citation>\s*$"),
}


class Inventory:
    def __init__(self):
        self.groups = {}
        self.errors = collections.Counter()

    def add(self, group, role, text):
        if not isinstance(text, str) or not text:
            return
        key = group + "/" + role
        stat = self.groups.setdefault(key, {"records": 0, "utf8_bytes": 0, "patterns": collections.Counter(), "tags": collections.Counter()})
        stat["records"] += 1
        stat["utf8_bytes"] += len(text.encode())
        for name, pattern in PATTERNS.items():
            if pattern.search(text):
                stat["patterns"][name] += 1
        stat["tags"].update({tag if tag in KNOWN_TAGS else "other_xml_tag" for tag in TAGS.findall(text)})
        paragraphs = [s.strip() for s in text.split("\n\n") if s.strip()]
        if any(a == b and len(a) >= 40 for a, b in zip(paragraphs, paragraphs[1:])):
            stat["patterns"]["adjacent_repeated_paragraph"] += 1
        # An unfinished stream or intentionally shown Markdown can match this.
        opened = None
        lines = text.splitlines()
        table = False
        escaped_table = False
        for index, line in enumerate(lines):
            if opened is None:
                if table and "|" in line and re.search(r"(?<!\\)(?:\\\\)*\\\|", line):
                    escaped_table = True
                if "|" not in line:
                    table = False
                if "|" in line and index + 1 < len(lines) and PATTERNS["tables"].match(lines[index + 1]):
                    table = True
            m = re.match(r"^ {0,3}(`{3,}|~{3,})(.*)$", line)
            if not m:
                continue
            mark, tail = m.groups()
            if opened:
                if mark[0] == opened[0] and len(mark) >= len(opened) and not tail.strip():
                    opened = None
            elif mark[0] != "`" or "`" not in tail:
                opened = mark
        if opened:
            stat["patterns"]["unclosed_fence_candidate"] += 1
        if escaped_table:
            stat["patterns"]["escaped_pipe_in_table"] += 1


def message_text(content):
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "\n".join(b.get("text", "") for b in content if isinstance(b, dict) and b.get("type") in ("text", "input_text", "output_text") and isinstance(b.get("text", ""), str))
    return ""


def transcripts(inv, excluded_cwd=None):
    totals = {}
    for provider, root in [("codex", Path.home()/".codex/sessions"), ("claude", Path.home()/".claude/projects")]:
        files = list(root.rglob("*.jsonl"))
        total = {"files": len(files), "bytes_at_start": sum(p.stat().st_size for p in files), "lines": 0, "excluded_worker_files": 0}
        for path in files:
            # Files are read to their size at open; active sessions can keep growing.
            size = path.stat().st_size
            read = 0
            try:
                with path.open() as source:
                    for line in source:
                        read += len(line.encode())
                        if read > size:
                            break
                        total["lines"] += 1
                        try:
                            row = json.loads(line)
                        except ValueError:
                            inv.errors[provider + ":invalid_json"] += 1
                            continue
                        cwd = row.get("cwd")
                        if row.get("type") == "session_meta":
                            cwd = (row.get("payload") or {}).get("cwd")
                        if excluded_cwd and cwd == excluded_cwd:
                            total["excluded_worker_files"] += 1
                            break
                        if provider == "codex":
                            p = row.get("payload", {})
                            if row.get("type") == "response_item" and isinstance(p, dict) and p.get("type") == "message" and p.get("role") in ("user", "assistant"):
                                inv.add("raw/codex", p["role"], message_text(p.get("content")))
                            elif row.get("type") == "response_item" and isinstance(p, dict) and p.get("type") in ("function_call_output", "custom_tool_call_output"):
                                output = p.get("output")
                                inv.add("raw/codex", "tool", output if isinstance(output, str) else message_text(output))
                        elif row.get("type") in ("user", "assistant"):
                            p = row.get("message", {})
                            role = row["type"]
                            if row.get("isMeta") or row.get("is_meta") or row.get("isCompactSummary") or (row.get("origin") or {}).get("kind") not in (None, "human"):
                                role += "_metadata"
                            inv.add("raw/claude", role, message_text(p.get("content")))
                            if isinstance(p.get("content"), list):
                                for block in p["content"]:
                                    if isinstance(block, dict) and block.get("type") == "tool_result":
                                        inv.add("raw/claude", "tool", message_text(block.get("content")))
            except OSError:
                inv.errors[provider + ":read_error"] += 1
        totals[provider] = total
    return totals


def rpc(inv, root, limit):
    host = runpy.run_path(str(root/"scripts/cupola-worker"), run_name="render_audit")["Host"]()
    totals = collections.Counter()
    seen = set()
    try:
        projects = host.call("projects.list")["projects"]
        totals["projects"] = len(projects)
        for project in projects:
            try:
                page = host.call("threads.list", {"projectId": project["id"], "limit": 100}, timeout=30)
                if page.get("loading"):
                    page = host.call("threads.list", {"projectId": project["id"], "limit": 100}, timeout=30)
                totals["projects_with_more_threads"] += bool(page.get("nextCursor"))
                totals["projects_still_loading"] += bool(page.get("loading"))
                eligible = [t for t in page.get("threads", []) if t.get("conversationId")]
                totals["attached_listed"] += len(eligible)
                totals["unattached_not_read"] += len(page.get("threads", [])) - len(eligible)
                for thread in eligible[:limit]:
                    ident = thread["conversationId"]
                    if ident in seen:
                        continue
                    seen.add(ident)
                    try:
                        snapshot = host.call("threads.read", {"threadId": ident, "limit": 100}, timeout=30)
                    except Exception:
                        inv.errors["rpc:threads.read"] += 1
                        continue
                    totals["threads_read"] += 1
                    projection = snapshot.get("thread", {})
                    totals["threads_with_older_turns"] += bool(projection.get("nextCursor"))
                    items = [i for t in projection.get("turns", []) for i in t.get("items", [])]
                    # Only positive routing metadata identifies T3-held records.
                    t3 = bool(snapshot.get("messageRecords"))
                    try:
                        activity = host.http("GET", f"/api/v1/conversations/{ident}/history/activity")
                        holder = activity.get("heldBy") or {}
                        t3 = t3 or holder.get("t3Code") is not None or activity.get("runsWithoutT3Code") is True
                        totals["holder:" + holder.get("kind", "none")] += 1
                    except Exception:
                        inv.errors["http:history/activity"] += 1
                    group = "cupola/" + thread.get("family", "unknown")
                    totals["provider:" + thread.get("family", "unknown")] += 1
                    totals["t3_marked_threads"] += bool(t3)
                    used = set()
                    for item in items:
                        if item.get("id") in used:
                            continue
                        used.add(item.get("id"))
                        kind = item.get("type", "unknown")
                        role = {"userMessage": "user", "agentMessage": "assistant"}.get(kind, "activity")
                        inv.add(group, role, item.get("text"))
                        if t3:
                            inv.add("t3_via_" + group, role, item.get("text"))
                        if role == "activity":
                            payload = item.get("payload") or {}
                            detail = {v for key in ("output", "aggregatedOutput", "stdout", "stderr", "detail")
                                      if isinstance(v := payload.get(key), str)}
                            result = payload.get("result")
                            if isinstance(result, str):
                                detail.add(result)
                            elif isinstance(result, dict):
                                detail.add(message_text(result.get("content")))
                            for value in detail:
                                inv.add(group, "activity_detail", value)
                                if t3:
                                    inv.add("t3_via_" + group, "activity_detail", value)
                    # Local outbound rows and legacy assistant rows are separate
                    # delivery surfaces; not summed into native item counts.
                    for message in snapshot.get("messages", []):
                        inv.add(group, "outbound_receipt", message.get("body"))
                    for message in snapshot.get("assistantMessages", []):
                        inv.add(group, "legacy_assistant", message.get("text"))
            except Exception:
                inv.errors["rpc:threads.list"] += 1
    finally:
        host.proc.terminate()
        host.proc.wait(timeout=5)
    return totals


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", choices=("transcripts", "rpc"), required=True)
    parser.add_argument("--threads-per-project", type=int, default=20)
    parser.add_argument("--exclude-cwd", help="Exclude this worker's own transcript from the inventory")
    args = parser.parse_args()
    started = time.monotonic()
    inventory = Inventory()
    totals = transcripts(inventory, args.exclude_cwd) if args.source == "transcripts" else rpc(inventory, Path(__file__).resolve().parents[4], args.threads_per_project)
    print(json.dumps({"measured_at": datetime.datetime.now(datetime.timezone.utc).isoformat(), "seconds": round(time.monotonic()-started, 2), "source": args.source, "scope": totals, "errors": inventory.errors, "groups": inventory.groups}, indent=2, sort_keys=True))
