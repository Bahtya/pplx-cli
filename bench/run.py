#!/usr/bin/env python3
"""
pplx-cli committee benchmark.

Validates the core claim — does a 3-model committee (parallel fan-out + Council
synthesis) beat the best single model? — via blind LLM-as-judge ranking over a
curated question set.

It shells out to the built `pplx-cli` binary (no in-process coupling):
  - single models : `pplx-cli ask   --json --model <m>  <q>`
  - committee     : `pplx-cli committee --json          <q>`
  - judge         : `pplx-cli ask   --json --model <j>  <rubric>`

Design:
  - Sequential across questions (Perplexity rate limits). Retries transient
    warmup/timeouts. Resumable: re-run picks up where it stopped.
  - Blind judge: candidates are shuffled and labeled A.. each question, so the
    judge never sees which model/system produced which answer.
  - Metric: per-question RANK (1=best). Committee "wins" a question when it
    holds the best (lowest) rank, ties allowed. Reported as win-rate vs the
    best single model, overall + per category.

Outputs: bench/results/<ts>/{runs.jsonl, judge.jsonl, scores.csv, summary.md}.
"""

import argparse
import json
import os
import random
import subprocess
import sys
import threading
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
DEFAULT_PPLX = REPO / "target" / "debug" / "pplx-cli"
DEFAULT_QUESTIONS = Path(__file__).resolve().parent / "questions.jsonl"
DEFAULT_MEMBERS = ["claude-5.0-sonnet", "glm-5.2", "gpt-5.4-thinking"]
DEFAULT_JUDGE = "gemini-3.1-pro"   # PRO-tier, non-member judge (opus needs Max)
DEFAULT_STRONG_SINGLE = ""         # opus (strongest single) needs Max; members are the singles


# --------------------------------------------------------------------------- #
# CLI driver
# --------------------------------------------------------------------------- #
def run_pplx(pplx, args, proxy, tries=4, per_try_timeout=300):
    """Run pplx-cli (JSON mode), retrying transient warmup/timeout failures.

    Returns (parsed_json | None, latency_ms_of_successful_attempt | None)."""
    base = [str(pplx)]
    if proxy:
        base += ["--proxy", proxy]
    for attempt in range(1, tries + 1):
        t0 = time.time()
        try:
            p = subprocess.run(base + args, capture_output=True, text=True,
                               timeout=per_try_timeout)
        except subprocess.TimeoutExpired:
            sys.stderr.write(f"    [timeout] attempt {attempt}/{tries}\n")
            time.sleep(5); continue
        elapsed = int((time.time() - t0) * 1000)
        out = (p.stdout or "").strip()
        if not out:
            err = (p.stderr or "").strip().splitlines()
            tail = err[-1] if err else f"rc={p.returncode}"
            # transient: warmup timeout, session init — retry
            sys.stderr.write(f"    [no output] attempt {attempt}/{tries}: {tail[:140]}\n")
            time.sleep(5); continue
        try:
            return json.loads(out), elapsed
        except json.JSONDecodeError:
            sys.stderr.write(f"    [bad json] attempt {attempt}/{tries}: {out[:160]}\n")
            time.sleep(3)
    return None, None


def extract_answer(system, j):
    """Pull the human-meaningful answer text from a system's JSON."""
    if j is None:
        return None
    if system == "committee":
        syn = j.get("synthesis") or {}
        return (syn.get("comprehensive") or syn.get("raw") or "").strip()
    return (j.get("answer") or "").strip()


# --------------------------------------------------------------------------- #
# Judge
# --------------------------------------------------------------------------- #
def build_judge_prompt(question, labeled_answers):
    """labeled_answers: list of (label, answer_text). Labels are random A.."""
    parts = [
        "You are an impartial judge evaluating candidate answers to ONE question.",
        "Rank every candidate from BEST (rank 1) to WORST (highest number) by overall",
        "answer quality: correctness, completeness, helpfulness, and absence of",
        "hallucination. Lower rank = better. Ties are allowed (give equal ranks).",
        "",
        f"Question: {question}",
        "",
    ]
    for label, ans in labeled_answers:
        parts.append(f"=== Candidate {label} ===")
        parts.append(ans.strip() or "(empty)")
        parts.append("")
    last = labeled_answers[-1][0]
    parts.append(
        f"Output ONLY a single JSON object mapping each candidate label to its integer "
        f"rank (1 = best). No prose, no code fence. Example shape: "
        f'{{"A": 1, "B": 3, "{last}": 2}}. Use ALL labels A..{last}.'
    )
    return "\n".join(parts)


def parse_ranks(text):
    """Extract {label: int_rank} from the judge's (possibly prose-wrapped) reply."""
    if not text:
        return {}
    # grab the outermost {...} block
    start = text.find("{")
    end = text.rfind("}")
    if start < 0 or end <= start:
        return {}
    try:
        obj = json.loads(text[start:end + 1])
    except json.JSONDecodeError:
        return {}
    ranks = {}
    for k, v in obj.items():
        label = k.strip().upper()
        try:
            ranks[label] = int(v)
        except (ValueError, TypeError):
            continue
    return ranks


def judge_question(pplx, proxy, judge_model, q, system_answers):
    """system_answers: {system: answer_text}. Returns {system: rank} or None."""
    items = [(s, a) for s, a in system_answers.items() if a]
    if len(items) < 2:
        return None
    random.shuffle(items)  # blind: random label assignment per question
    labels = [chr(ord("A") + i) for i in range(len(items))]
    label_to_system = {labels[i]: items[i][0] for i in range(len(items))}
    labeled = [(labels[i], items[i][1]) for i in range(len(items))]

    prompt = build_judge_prompt(q["question"], labeled)
    j, _ = run_pplx(pplx, ["ask", "--json", "--model", judge_model, prompt],
                    proxy, per_try_timeout=240)
    if not j:
        return None
    ranks = parse_ranks(j.get("answer", ""))
    if len(ranks) < len(items) / 2:  # judge gave too little to trust
        sys.stderr.write(f"    [judge] thin ranking for {q['id']}: {ranks}\n")
    return {label_to_system[lbl]: r for lbl, r in ranks.items() if lbl in label_to_system}


# --------------------------------------------------------------------------- #
# Persistence / resume
# --------------------------------------------------------------------------- #
def load_jsonl(path):
    rows = []
    if path.exists():
        for line in path.read_text().splitlines():
            line = line.strip()
            if line:
                try:
                    rows.append(json.loads(line))
                except json.JSONDecodeError:
                    pass
    return rows


def append_jsonl(path, obj):
    with path.open("a") as f:
        f.write(json.dumps(obj, ensure_ascii=False) + "\n")


# --------------------------------------------------------------------------- #
# Main
# --------------------------------------------------------------------------- #
def process_question(q, pplx, proxy, singles, judge_model,
                     runs_path, judge_path, prior_runs, prior_judges, lock):
    """Collect this question's candidates, then blind-judge it. Thread-safe:
    reads the shared prior_* snapshots (read-only) and appends records under `lock`."""
    qid, cat, text = q["id"], q["category"], q["question"]
    print(f"\n[{qid}/{cat}] {text[:60]}")
    collected = {}  # system -> json (or None)
    for system in singles + ["committee"]:
        if (qid, system) in prior_runs:
            collected[system] = prior_runs[(qid, system)]["json"]
            print(f"  [{qid}] {system}: cached", flush=True)
            continue
        if system == "committee":
            cli_args, tout = ["committee", "--json", text], 400
        else:
            cli_args, tout = ["ask", "--json", "--model", system, text], 300
        print(f"  [{qid}] {system}: running...", flush=True)
        j, ms = run_pplx(pplx, cli_args, proxy, per_try_timeout=tout)
        status = "FAILED" if j is None else f"ok ({len(extract_answer(system, j) or '')} chars, {ms}ms)"
        print(f"  [{qid}] {system}: {status}", flush=True)
        rec = {"qid": qid, "category": cat, "system": system,
               "latency_ms": ms, "ok": j is not None, "json": j}
        with lock:
            append_jsonl(runs_path, rec)
        collected[system] = j

    if qid in prior_judges:
        return
    sys_answers = {s: extract_answer(s, collected[s]) for s in collected if collected.get(s)}
    if "committee" not in sys_answers:
        print(f"  [{qid}] no committee answer — skipping judgment", flush=True)
        return
    print(f"  [{qid}] judging {len(sys_answers)} candidates...", flush=True)
    ranks = judge_question(pplx, proxy, judge_model, q, sys_answers)
    if ranks is None:
        print(f"  [{qid}] judge FAILED", flush=True)
        return
    print(f"  [{qid}] ranks: {ranks}", flush=True)
    with lock:
        append_jsonl(judge_path, {"qid": qid, "category": cat, "ranks": ranks})


def main():
    ap = argparse.ArgumentParser(description="pplx-cli committee benchmark")
    ap.add_argument("--pplx", default=str(DEFAULT_PPLX))
    ap.add_argument("--questions", default=str(DEFAULT_QUESTIONS))
    ap.add_argument("--judge", default=DEFAULT_JUDGE)
    ap.add_argument("--members", default=",".join(DEFAULT_MEMBERS),
                    help="comma list of single-model baselines")
    ap.add_argument("--strong-single", default=DEFAULT_STRONG_SINGLE,
                    help="extra strongest-individual baseline (empty to skip)")
    ap.add_argument("--limit", type=int, default=0, help="run only first N questions (0=all)")
    ap.add_argument("--proxy", default=os_environ_proxy())
    ap.add_argument("--out-dir", default="")
    ap.add_argument("--no-resume", action="store_true")
    ap.add_argument("--concurrency", type=int, default=1,
                    help="questions to process in parallel (1 = sequential)")
    args = ap.parse_args()

    pplx = Path(args.pplx)
    if not pplx.exists():
        sys.exit(f"pplx-cli binary not found at {pplx} (build it first: cargo build)")

    members = [m.strip() for m in args.members.split(",") if m.strip()]
    singles = list(members)
    if args.strong_single:
        singles.append(args.strong_single)

    questions = load_jsonl(Path(args.questions))
    if args.limit:
        questions = questions[:args.limit]
    print(f"Loaded {len(questions)} questions. Singles={singles} judge={args.judge}")

    out_dir = Path(args.out_dir) if args.out_dir else \
        Path(__file__).resolve().parent / "results" / time.strftime("%Y%m%d-%H%M%S")
    out_dir.mkdir(parents=True, exist_ok=True)
    runs_path = out_dir / "runs.jsonl"
    judge_path = out_dir / "judge.jsonl"
    if args.no_resume and runs_path.exists():
        runs_path.unlink()
    if args.no_resume and judge_path.exists():
        judge_path.unlink()

    prior_runs = {} if args.no_resume else {(r["qid"], r["system"]): r for r in load_jsonl(runs_path)}
    prior_judges = {} if args.no_resume else {r["qid"]: r for r in load_jsonl(judge_path)}
    print(f"Resuming: {len(prior_runs)} cached runs, {len(prior_judges)} cached judgments. -> {out_dir}")

    # ---- per-question: collect candidates, then blind-judge (optionally parallel) ----
    lock = threading.Lock()
    workers = max(1, min(args.concurrency, len(questions)))
    print(f"Concurrency: {workers} question(s) in parallel")
    if args.concurrency > 3:
        sys.stderr.write("WARNING: --concurrency > 3 reliably triggers Perplexity 429s; consider 1-3.\n")

    def do(q):
        process_question(q, pplx, args.proxy, singles, args.judge,
                         runs_path, judge_path, prior_runs, prior_judges, lock)

    if workers > 1:
        with ThreadPoolExecutor(max_workers=workers) as ex:
            list(ex.map(do, questions))
    else:
        for q in questions:
            do(q)

    # ---- aggregate (re-read disk so it sees everything written by all workers) ----
    runs_all = load_jsonl(runs_path)
    judges_all = load_jsonl(judge_path)
    summarize(out_dir, questions,
              {(r["qid"], r["system"]): r for r in runs_all},
              {j["qid"]: j for j in judges_all}, singles, args.judge)


def os_environ_proxy():
    return os.environ.get("PERPLEXITY_PROXY", "")


def summarize(out_dir, questions, runs, judges, singles, judge_model):
    all_systems = singles + ["committee"]
    # per-system rank stats
    rank_lists = {s: [] for s in all_systems}
    wins_committee = 0
    wins_committee_solo = 0
    judged_qs = 0
    by_cat = {}  # cat -> {sys: [ranks]}

    for q in questions:
        qid = q["id"]
        j = judges.get(qid)
        if not j or "committee" not in (j.get("ranks") or {}):
            continue
        judged_qs += 1
        ranks = j["ranks"]
        best = min(ranks.values())
        cat = q["category"]
        by_cat.setdefault(cat, {s: [] for s in all_systems})
        for sysname in all_systems:
            r = ranks.get(sysname)
            if r is not None:
                rank_lists[sysname].append(r)
                by_cat[cat][sysname].append(r)
        if ranks.get("committee") == best:
            wins_committee += 1
            if list(ranks.values()).count(best) == 1:
                wins_committee_solo += 1

    # latency
    lat = {s: [] for s in all_systems}
    for (qid, sysname), rec in runs.items():
        if rec.get("ok") and rec.get("latency_ms") and sysname in lat:
            lat[sysname].append(rec["latency_ms"])

    def avg(xs):
        return sum(xs) / len(xs) if xs else 0.0

    # best single by avg rank
    single_avg = {s: avg(rank_lists[s]) for s in singles if rank_lists[s]}
    best_single = min(single_avg, key=single_avg.get) if single_avg else None

    rows = []
    for s in all_systems:
        rl = rank_lists[s]
        rows.append({
            "system": s,
            "n_judged": len(rl),
            "avg_rank": round(avg(rl), 3),
            "top1_count": sum(1 for r in rl if r == 1),
            "avg_latency_ms": round(avg(lat[s])),
        })

    # write scores.csv
    csv_path = out_dir / "scores.csv"
    with csv_path.open("w") as f:
        f.write("system,n_judged,avg_rank,top1_count,avg_latency_ms\n")
        for r in rows:
            f.write(f"{r['system']},{r['n_judged']},{r['avg_rank']},{r['top1_count']},{r['avg_latency_ms']}\n")

    # write summary.md
    md = []
    md.append("# pplx-cli committee benchmark\n")
    md.append(f"**Questions judged:** {judged_qs} / {len(questions)}\n")
    synth = "gemini-3.1-pro"  # CLI default committee synthesizer
    md.append(f"**Judge:** {judge_model} (blind, shuffled labels). Note: the committee synthesizer "
              f"is {synth} by default; if the judge is also a committee member (e.g. glm-5.2) that is "
              f"a self-preference confound — blinding mitigates positional bias, rerun with "
              f"--judge <other> to check sensitivity.\n\n")
    md.append("## Overall (lower avg_rank = better; top1 = #questions ranked #1)\n")
    md.append("| system | n | avg_rank | top1 | avg_latency_ms |\n|---|---|---|---|---|")
    for r in sorted(rows, key=lambda x: x["avg_rank"]):
        md.append(f"| {r['system']} | {r['n_judged']} | {r['avg_rank']} | {r['top1_count']} | {r['avg_latency_ms']} |")
    md.append("")
    md.append("## Committee vs best single model\n")
    if best_single:
        c_avg = avg(rank_lists["committee"])
        md.append(f"- best single model: **{best_single}** (avg rank {single_avg[best_single]:.3f})")
        md.append(f"- committee avg rank: **{c_avg:.3f}**")
        delta = single_avg[best_single] - c_avg
        verdict = "committee WINS" if delta > 0.05 else ("tie" if abs(delta) <= 0.05 else "committee LOSES")
        md.append(f"- delta (best_single - committee): **{delta:+.3f}** → **{verdict}**")
        if judged_qs:
            md.append(f"- committee ranked #1 (incl. ties): **{wins_committee}/{judged_qs}** "
                      f"= {wins_committee / judged_qs:.0%}")
            md.append(f"- committee ranked #1 (solo, no tie): **{wins_committee_solo}/{judged_qs}** "
                      f"= {wins_committee_solo / judged_qs:.0%}")
    md.append("")
    md.append("## By category (avg rank)\n")
    md.append("| category | " + " | ".join(all_systems) + " |")
    md.append("|---|" + "|".join(["---"] * len(all_systems)) + "|")
    for cat in sorted(by_cat):
        cells = [f"{avg(by_cat[cat][s]):.2f}" if by_cat[cat][s] else "-" for s in all_systems]
        md.append(f"| {cat} | " + " | ".join(cells) + " |")
    md.append("")
    (out_dir / "summary.md").write_text("\n".join(md))

    print("\n==================== SUMMARY ====================")
    print("\n".join(md))
    print(f"\nWrote {csv_path}, judge.jsonl, runs.jsonl, summary.md to {out_dir}")


if __name__ == "__main__":
    main()
