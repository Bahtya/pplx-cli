#!/usr/bin/env python3
"""MCP server exposing the local `pplx-cli` (Perplexity, Chrome TLS fingerprint,
the user's own session token, `--file` code context) as DIRECT tools.

Why this exists: as a *skill*, pplx-cli was never proactively invoked — the model
defaults to the direct WebSearch tool, and an opt-in skill never wins that race
(measured: 0/16 trigger rate). As an MCP tool it is first-class (single call,
always in the toolset), so the model can pick it for web-grounded Q&A on equal
footing with WebSearch.

Tools: perplexity_ask (grounded answer + citations), perplexity_search (links),
perplexity_committee (3-model Council synthesis, slower). pplx-cli reads
PERPLEXITY_SESSION_TOKEN / PERPLEXITY_PROXY from the inherited environment.

Register:
  claude mcp add perplexity -- python /data/data/com.termux/files/home/project/pplx-cli/mcp/server.py
"""
import json
import subprocess
import time

from mcp.server.fastmcp import FastMCP

PPLX = "pplx-cli"


def _run(args, tries: int = 4, per_try_timeout: int = 200) -> dict:
    """Run pplx-cli in JSON mode with warmup-retry.

    pplx-cli occasionally fails its startup warmup GET ("Session warmup failed:
    operation timed out") — that yields empty stdout, so we retry. Returns parsed
    JSON, or {"_error": ...} on persistent failure.
    """
    for _ in range(tries):
        try:
            p = subprocess.run([PPLX, *args], capture_output=True, text=True, timeout=per_try_timeout)
        except subprocess.TimeoutExpired:
            time.sleep(5)
            continue
        out = (p.stdout or "").strip()
        if out:
            try:
                return json.loads(out)
            except json.JSONDecodeError:
                return {"_error": f"non-JSON output: {out[:200]}"}
        # empty stdout → almost always a warmup timeout (details on stderr); retry
        time.sleep(5)
    return {"_error": f"pplx-cli produced no output after {tries} tries (warmup timeout / rate limit / token issue)"}


def _fmt_sources(results) -> str:
    if not results:
        return ""
    lines = ["", "Sources:"]
    for i, r in enumerate(results, 1):
        lines.append(f"[{i}] {r.get('name', '')} — {r.get('url', '')}")
        sn = (r.get("snippet") or "").strip().replace("\n", " ")
        if sn:
            lines.append(f"    {sn[:200]}")
    return "\n".join(lines)


mcp = FastMCP("perplexity")


@mcp.tool()
def perplexity_ask(query: str, model: str = "", sources: str = "web",
                   language: str = "en-US", file: str = "") -> str:
    """Get a synthesized, CITED answer grounded in the live web (via Perplexity).

    PREFER this over WebSearch when you want an ANSWER + sources, not a result
    list — one call returns a grounded answer plus numbered citations. Pass `file`
    (a path to the user's own code/document) to ground the answer in real context.
    Optional: `model` (e.g. claude-5.0-sonnet, gpt-5.5, glm-5.2), `sources`
    (web / scholar / social), `language` (e.g. en-US, zh-CN).
    """
    args = ["ask", "--json"]
    if model:
        args += ["--model", model]
    if file:
        args += ["--file", file]
    args += ["--sources", sources, "--language", language, query]
    j = _run(args, per_try_timeout=200)
    if "_error" in j:
        return f"[pplx-cli error] {j['_error']}"
    return (j.get("answer") or "").strip() + _fmt_sources(j.get("sources") or [])


@mcp.tool()
def perplexity_search(query: str, sources: str = "web") -> str:
    """Quick web search → ranked results (title, URL, snippet).

    Use this when you want URLs / a lay-of-the-land, NOT a synthesized answer
    (use perplexity_ask for that). `sources`: web / scholar / social.
    """
    j = _run(["search", "--json", "--sources", sources, query], per_try_timeout=120)
    if "_error" in j:
        return f"[pplx-cli error] {j['_error']}"
    res = j.get("results") or []
    if not res:
        return "(no results)"
    return _fmt_sources(res).strip()


@mcp.tool()
def perplexity_committee(query: str) -> str:
    """Ask a 3-model committee (claude-5.0-sonnet + glm-5.2 + gpt-5.4-thinking answer
    in parallel) plus a synthesizer — returns a structured consensus / disagreement /
    unique-findings / comprehensive result.

    HIGHER quality on hard or ambiguous questions where a single model might miss
    angles, but SLOWER (~30-60s) than perplexity_ask. Don't use for simple lookups.
    """
    j = _run(["committee", "--json", query], tries=3, per_try_timeout=300)
    if "_error" in j:
        return f"[pplx-cli error] {j['_error']}"
    syn = j.get("synthesis") or {}
    parts = []
    for title, key in (("Consensus", "consensus"), ("Disagreement", "disagreement"),
                       ("Unique findings", "unique_findings"), ("Comprehensive analysis", "comprehensive")):
        body = (syn.get(key) or "").strip()
        if body:
            parts.append(f"## {title}\n{body}")
    out = "\n\n".join(parts) if parts else (syn.get("raw") or "[no synthesis produced]")
    # merged, URL-deduped sources across the 3 members
    seen, dedup = set(), []
    for m in j.get("members") or []:
        for r in m.get("sources") or []:
            u = r.get("url")
            if u and u not in seen:
                seen.add(u)
                dedup.append(r)
    return out + _fmt_sources(dedup)


if __name__ == "__main__":
    mcp.run()
