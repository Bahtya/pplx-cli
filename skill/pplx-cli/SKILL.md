---
name: pplx-cli
description: Use the local `pplx-cli` tool to reach the live web via Perplexity at exactly the moments you'd otherwise be guessing or relying on possibly-stale memory: (1) choosing between options — picking a library / architecture / approach ("A vs B"); (2) stuck on a bug or design problem and wanting a fresh angle you haven't tried; (3) needing current docs, recent API changes, latest versions, or up-to-date best practices. Also use it for ordinary web search, grounded Q&A with sources, fact-checking a claim against live info, and multi-turn research. Reach for it especially when the user mentions Perplexity/pplx/web search, says "latest"/"current"/a year, or is weighing options. When the question is about the user's own code, attach a code snippet or a desensitized file (--file) so pplx has real context. Two disciplines always apply: (1) in multi-turn research, advance ONE turn at a time with your own thinking between turns — never pipe/dump all questions at once; (2) treat pplx-cli's answers as leads to verify by actually running code or checking docs, not as ground truth.
---

# pplx-cli — local Perplexity search / Q&A / multi-turn research

`pplx-cli` is a local CLI that talks to Perplexity through a Chrome TLS-fingerprint client, so it reaches the live web and returns grounded answers with sources. It has three modes. This skill is about *how to drive it well* — the commands are simple; the value is in two working disciplines below.

**Prerequisites** (already set up on this machine):
- Binary: `pplx-cli` (on `$PATH`).
- Auth: `PERPLEXITY_SESSION_TOKEN` is exported in `~/.bashrc`. If a call fails with "session token" / persistent 403, the token expired — ask the user to refresh it from their browser cookie.
- If a call hangs or times out, retry with `--proxy http://127.0.0.1:7890` (a local proxy is more reliable than the direct path).

## When to use / not use

**Use** pplx-cli at these moments (they're the highest-value triggers), and for ordinary lookups:
- **Choosing between options** — "should I use A or B", picking a library / architecture / pattern. Get a grounded comparison instead of guessing from memory.
- **Stuck and need a fresh angle** — a bug or design problem you've been spinning on; pull in approaches you haven't tried.
- **Need current docs / latest data** — exact versions, recent API/breaking changes, up-to-date best practices, or any fact your training might be stale on.
- Plus: plain web search, grounded answers with citations, fact-checking a claim, or a multi-turn dive into a topic.

**Don't use** when the task needs no live data — pure local code edits, reasoning over files already in the repo, math you can do yourself. pplx-cli is a *lookup/research* tool, not a substitute for doing the work.

## The three modes

### `search` — quick web search (links + snippets only)
```bash
pplx-cli search "tokio vs async-std 2026"
```
Returns numbered sources (title / URL / snippet). Best when you want URLs or a lay of the land, not a synthesis. Uses the fast `turbo` model; incognito.

### `ask` — single grounded Q&A (answer + sources)
```bash
pplx-cli ask "how does tokio::select! actually work?"
pplx-cli ask "summarize this" --file report.pdf --sources scholar
pplx-cli ask "quick one" --model gpt-5.5 --language zh-CN
```
One question → one answer + a `Sources:` list. Default model is Claude Sonnet 4.6 (non-thinking, fast). Single-turn, incognito, the temp thread is auto-deleted. Flags: `--file` (attach, repeatable), `--model`, `--sources web,scholar,social`, `--language`.

### `reason` — deep, multi-turn (thinking model)
```bash
pplx-cli reason --query "compare tokio and async-std"
```
Uses a *thinking* model (Claude Sonnet 4.6 thinking), deeper but slower. Interactive: a question may span multiple lines — **press Enter on a blank line to submit**; `/quit` (or Ctrl-D) exits and deletes the thread.

See `references/details.md` for the full model list and troubleshooting.

---

## Ask well: give pplx your code context

When the question is about the user's *own* code, a bare "why doesn't this work" gets a generic answer. Give pplx real context so it can actually reason about your situation:

- **Paste the relevant snippet** into the query — the function or block in question — or **attach the file** with `--file path/to/file` (repeatable for several files).
- **Desensitize first**: strip tokens, API keys, secrets, and internal URLs/identifiers before sending. Whatever you paste or attach leaves the machine and goes to Perplexity.
- Pair the snippet with a *specific* question — "why does this deadlock when the second client connects?" beats "fix this".

`search` doesn't need code context (it just runs a query). `ask` and `reason` benefit the most — they reason over exactly what you give them.

---

## Discipline 1 — Multi-turn: think between turns, never dump

When the user wants a *multi-turn* exploration ("多轮", "深挖", "由浅入深", "iterate on this"), do it **one turn at a time**:

1. Ask the first (shallow, scoping) question with `pplx-cli ask` (fast) or `pplx-cli reason --query` (deep).
2. **Read the answer.** Then, in your reply to the user, write a short reflection: what this established, what's still unclear or under-specified, what the natural deeper question is.
3. Ask that deeper follow-up as a *separate* call, carrying the needed context in the prompt (each call is independent — there's no server-side memory unless you keep one `reason` session alive, see `references/details.md`).
4. Repeat, going shallow → deep, until the question is genuinely answered.

**Never** batch all questions into one call. The failure mode this prevents: piping `q1\nq2\nq3` at once either splits them into separate turns you can't react to, or produces one shallow blob — either way you've stopped *thinking* and just forwarded a script. The whole point of multi-turn is that each follow-up is shaped by the previous answer. If you catch yourself writing a `printf` with several questions separated by newlines, stop — that's the anti-pattern.

Why sequential calls (one `ask`/`reason --query` per turn) instead of one long interactive `reason` session: an agent driving a shell can't reliably hold an interactive process open across turns, and a single server-side thread isn't worth the fragility. Carrying context in each prompt is robust and loses nothing that matters for research. (True thread-continuity is possible — see `references/details.md` — but treat it as optional.)

## Discipline 2 — pplx output is a lead, not truth; verify

`pplx-cli` returns confident, sourced answers — and they can still be **outdated, subtly wrong, or hallucinated**. It can also be silently routed to the fast `turbo` model (you'll see a heads-up on stderr), which lowers quality. So:

- Treat every pplx-cli answer as a **hypothesis**, especially anything you're about to state as a conclusion or act on (API behavior, syntax, "X is faster than Y", version numbers, security claims).
- **Verify before you assert.** Run the code, check the official docs/source, or reproduce the claim yourself. Adapt the verification to the claim: code claim → write a tiny example and run it; version/date claim → check the source; "how it works" claim → confirm against docs.
- In your final answer, **separate "pplx said…" from "I verified…"**. If you couldn't verify something, say so and mark it unverified rather than presenting it as fact.

This matters because the user is relying on *you* for correctness, not on a chatbot's summary. pplx-cli gets you to the right neighborhood fast; testing is what makes the answer trustworthy.

## Environment & troubleshooting (quick)

- **Token**: `PERPLEXITY_SESSION_TOKEN` (in `~/.bashrc`). Expired → "session validation failed" / persistent 403 → ask user to refresh the cookie.
- **Hangs/timeouts**: add `--proxy http://127.0.0.1:7890`.
- **Thinking model is slow**: `reason`/thinking answers sit silent ~20–40s before streaming — that's normal, wait for it. (Streaming timeout is already generous; if it still cuts off, retry, optionally via proxy.)
- **"Server used turbo" notice on stderr**: Perplexity routed a trivial query to the free model — answer quality may be lower; re-ask with more specificity if needed.

Full model list, the downgrade/token-expiry details, and the optional true-multi-turn (FIFO) recipe are in `references/details.md`.

## Sources

When you use pplx-cli for an answer, pass along the `Sources:` list it prints (the user may want to click through). You can trim to the ones you actually relied on or verified.
