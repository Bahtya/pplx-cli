#!/usr/bin/env python3
"""
Find the best 3-member committee (+ synthesizer) among PRO-tier models.

Phase 1: fix synthesizer = gemini-3.1-pro; enumerate provider-diverse member
triples from the PRO pool; score each triple's committee. Report the best.
Phase 2: hold the best triple; try each non-member model as synthesizer; pick best.

Efficiency: a model's answer to a question is independent of which committee it
sits in, so each pool model's single answer is collected ONCE and reused as the
member answer for every triple containing it. The committee synthesis is then
just `ask --model <synth>` over the Council prompt (mirrors committee.rs), so a
triple costs only synth + judge per question — not 3 fresh member queries.

Judge: rotated non-member per triple (the judge is never one of the 3 members it
is ranking), avoiding the self-preference bias seen with member-judges. Caveat:
different triples are judged by different models, so absolute ranks are not
perfectly cross-comparable; the metric (committee rank among its own 4
candidates) is within-question and fairly robust to this.
"""

import argparse
import itertools
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
SYNTH_PHASE1 = "gemini-3.1-pro"

# PRO-tier pool (is_max_required=false), by provider. Heterogeneity is the point.
POOL = [
    ("claude-5.0-sonnet", "anthropic"),
    ("claude-5.0-sonnet-thinking", "anthropic"),
    ("gpt-5.4-thinking", "openai"),
    ("gemini-3.1-pro", "google"),
    ("glm-5.2", "zai"),
    ("kimi", "moonshot"),
    ("kimi-thinking", "moonshot"),
]
PER_CATEGORY = 2  # -> 8 questions (4 categories); kept small for tractable search


# --------------------------- CLI driver (mirrors run.py) ------------------- #
def run_pplx(pplx, args, proxy, tries=5, per_try_timeout=300):
    base = [str(pplx)]
    if proxy:
        base += ["--proxy", proxy]
    time.sleep(1.5)  # pace: Perplexity 429s on concurrent/bursty calls; sequential + gap stays under
    for attempt in range(1, tries + 1):
        try:
            p = subprocess.run(base + args, capture_output=True, text=True, timeout=per_try_timeout)
        except subprocess.TimeoutExpired:
            sys.stderr.write(f"    [timeout] {attempt}/{tries}\n"); time.sleep(5); continue
        out = (p.stdout or "").strip()
        if not out:
            err = (p.stderr or "").strip().splitlines()
            tail = (err[-1] if err else f"rc={p.returncode}")
            is_rate = "429" in tail or "Too Many Requests" in tail
            sys.stderr.write(f"    [no output{' / 429' if is_rate else ''}] {attempt}/{tries}: {tail[:120]}\n")
            time.sleep(20 if is_rate else 5)  # rate-limit needs a longer backoff
            continue
        try:
            return json.loads(out)
        except json.JSONDecodeError:
            sys.stderr.write(f"    [bad json] {attempt}/{tries}: {out[:160]}\n"); time.sleep(3)
    return None


def extract_answer(j):
    return (j.get("answer") or "").strip() if j else ""


def build_judge_prompt(question, labeled):
    parts = ["You are an impartial judge. Rank these candidate answers to ONE question "
             "from BEST (rank 1) to WORST. Lower = better. Ties allowed.",
             f"Question: {question}", ""]
    for label, ans in labeled:
        parts += [f"=== Candidate {label} ===", ans.strip() or "(empty)", ""]
    last = labeled[-1][0]
    parts.append(f'Output ONLY JSON mapping each label to its integer rank (1=best), all labels A..{last}. '
                 f'Example: {{"A": 1, "B": 3, "{last}": 2}}.')
    return "\n".join(parts)


def parse_ranks(text):
    if not text:
        return {}
    s, e = text.find("{"), text.rfind("}")
    if s < 0 or e <= s:
        return {}
    try:
        obj = json.loads(text[s:e + 1])
    except json.JSONDecodeError:
        return {}
    out = {}
    for k, v in obj.items():
        try:
            out[k.strip().upper()] = int(v)
        except (ValueError, TypeError):
            continue
    return out


def council_prompt(query, member_answers):
    """Mirrors committee.rs build_synthesis_prompt exactly."""
    s = ("You are the synthesizer of a model committee. Several expert models "
         "independently answered the SAME question. Produce one synthesis.\n\n"
         f"Question: {query}\n\nExpert answers:")
    for i, (m, a) in enumerate(member_answers):
        s += f"\n\n[{i + 1}] ({m}):\n{a}"
    s += ("\n\nOutput EXACTLY these four Markdown sections, in this order, each "
          "beginning with the exact header line:\n"
          "## Consensus\n(claims ALL experts agree on)\n"
          "## Disagreement\n(claims the experts conflict on — state each expert's stance and its reasoning)\n"
          "## Unique Findings\n(insights only ONE expert raised — attribute which)\n"
          "## Comprehensive Analysis\n(the best integrated final answer — do NOT introduce any claim none of the experts made)\n")
    return s


def parse_comprehensive(text):
    """Extract the '## Comprehensive Analysis' section body (mirror committee.rs)."""
    keys = ["consensus", "disagreement", "unique", "comprehensive"]
    sections, current = {}, None
    for line in (text or "").splitlines():
        stripped = line.lstrip("#").strip().lower()
        for w in keys:
            if stripped.startswith(w):
                current = w
                sections.setdefault(w, [])
                break
        else:
            if current:
                sections.setdefault(current, []).append(line)
    body = "\n".join(sections.get("comprehensive", [])).strip()
    return body or (text or "").strip()


def load_jsonl(path):
    return [json.loads(l) for l in Path(path).read_text().splitlines() if l.strip()] if Path(path).exists() else []


def append_jsonl(path, obj, lock=None):
    def _w():
        with open(path, "a") as f:
            f.write(json.dumps(obj, ensure_ascii=False) + "\n")
    if lock:
        with lock:
            _w()
    else:
        _w()


# --------------------------- pipeline -------------------------------------- #
def pick_subset(questions):
    by_cat = {}
    for q in questions:
        by_cat.setdefault(q["category"], []).append(q)
    subset = []
    for cat in sorted(by_cat):
        subset.extend(by_cat[cat][:PER_CATEGORY])
    return subset


def provider_diverse_triples():
    idx = range(len(POOL))
    out = []
    for combo in itertools.combinations(idx, 3):
        if len({POOL[i][1] for i in combo}) == 3:
            out.append(combo)
    return out


# Curated, tractable subset — the full 25-triple grid is infeasible under
# Perplexity's rate limit (429 at concurrency >= 4). Covers the baseline plus the
# meaningful provider / thinking-variant swaps across the 5 providers.
CURATED = [
    ("claude-5.0-sonnet", "glm-5.2", "gpt-5.4-thinking"),           # baseline: Anthropic/Z.ai/OpenAI
    ("claude-5.0-sonnet-thinking", "glm-5.2", "gpt-5.4-thinking"),  # thinking claude
    ("claude-5.0-sonnet", "gemini-3.1-pro", "gpt-5.4-thinking"),    # Google for Z.ai
    ("claude-5.0-sonnet", "glm-5.2", "kimi"),                       # Moonshot for OpenAI
    ("claude-5.0-sonnet", "glm-5.2", "gemini-3.1-pro"),             # Google for OpenAI
    ("gpt-5.4-thinking", "gemini-3.1-pro", "glm-5.2"),              # no Anthropic
]


def curated_triples():
    name2idx = {POOL[i][0]: i for i in range(len(POOL))}
    out = []
    for names in CURATED:
        idxs = tuple(name2idx[n] for n in names)
        if len({POOL[i][1] for i in idxs}) == 3:
            out.append(idxs)
    return out


def collect_singles(pplx, proxy, pool_models, subset, singles_path, lock):
    """Collect each pool model's answer on the subset (resumable). Return {(model,qid): answer}."""
    done = {(r["model"], r["qid"]): r["answer"] for r in load_jsonl(singles_path)}
    singles = dict(done)
    jobs = [(m, q) for m in pool_models for q in subset if (m, q["id"]) not in done]
    print(f"Singles: {len(done)} cached, {len(jobs)} to collect")

    def work(job):
        m, q = job
        j = run_pplx(pplx, ["ask", "--json", "--model", m, q["question"]], proxy, per_try_timeout=300)
        ans = extract_answer(j) if j else ""
        append_jsonl(singles_path, {"model": m, "qid": q["id"], "answer": ans}, lock)
        return (m, q["id"], ans)

    with ThreadPoolExecutor(max_workers=8) as ex:
        for m, qid, ans in ex.map(work, jobs):
            singles[(m, qid)] = ans
    return singles


def eval_triple(triple_models, synth, judge, subset, singles, pplx, proxy):
    """Return {n, committee_avg_rank, committee_win1, committee_beats_best_member, per_q}."""
    ranks_committee, beats_best = [], 0
    per_q = []
    for q in subset:
        qid = q["id"]
        member_ans = [(m, singles.get((m, qid), "")) for m in triple_models]
        if not all(a for _, a in member_ans):
            continue
        # synthesis (committee) over the 3 cached member answers
        sj = run_pplx(pplx, ["ask", "--json", "--model", synth, council_prompt(q["question"], member_ans)],
                      proxy, per_try_timeout=300)
        committee_ans = parse_comprehensive(sj.get("answer", "")) if sj else ""
        candidates = [(m, a) for m, a in member_ans] + [("committee", committee_ans)]
        random.shuffle(candidates)
        labels = [chr(ord("A") + i) for i in range(len(candidates))]
        lab2sys = {labels[i]: candidates[i][0] for i in range(len(candidates))}
        jj = run_pplx(pplx, ["ask", "--json", "--model", judge,
                             build_judge_prompt(q["question"], [(labels[i], candidates[i][1]) for i in range(len(candidates))])],
                      proxy, per_try_timeout=240)
        ranks = parse_ranks(jj.get("answer", "")) if jj else {}
        sys_rank = {lab2sys[l]: r for l, r in ranks.items() if l in lab2sys}
        per_q.append(sys_rank)
        if "committee" in sys_rank:
            ranks_committee.append(sys_rank["committee"])
            best_member = min(sys_rank[s] for s in triple_models if s in sys_rank) if any(s in sys_rank for s in triple_models) else 99
            if sys_rank["committee"] < best_member or (sys_rank["committee"] == min(sys_rank.values())):
                beats_best += 1
    n = len(ranks_committee)
    avg = sum(ranks_committee) / n if n else 99
    win1 = sum(1 for r in ranks_committee if r == 1)
    return {"n": n, "committee_avg_rank": round(avg, 3), "committee_win1": win1,
            "committee_beats_best_member": beats_best, "per_q": per_q}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--pplx", default=str(DEFAULT_PPLX))
    ap.add_argument("--questions", default=str(DEFAULT_QUESTIONS))
    ap.add_argument("--proxy", default=os.environ.get("PERPLEXITY_PROXY", ""))
    ap.add_argument("--out-dir", default="")
    ap.add_argument("--skip-phase2", action="store_true")
    ap.add_argument("--curated", action="store_true",
                    help="use a curated 6-triple subset (feasible under rate limits)")
    args = ap.parse_args()

    pplx = Path(args.pplx)
    if not pplx.exists():
        sys.exit(f"pplx-cli not found at {pplx}")
    out = Path(args.out_dir) if args.out_dir else \
        Path(__file__).resolve().parent / "results" / ("search-" + time.strftime("%Y%m%d-%H%M%S"))
    out.mkdir(parents=True, exist_ok=True)
    singles_path = out / "singles.jsonl"
    triples_path = out / "triples.jsonl"
    lock = threading.Lock()

    questions = load_jsonl(args.questions)
    subset = pick_subset(questions)
    pool_models = [m for m, _ in POOL]
    print(f"Subset: {len(subset)} questions | Pool: {len(pool_models)} PRO models | out={out}")

    singles = collect_singles(pplx, args.proxy, pool_models, subset, singles_path, lock)

    triples = curated_triples() if args.curated else provider_diverse_triples()
    print(f"Triples: {len(triples)} ({'curated' if args.curated else 'all provider-diverse'})")
    done_keys = {(r["triple"], r["synth"]) for r in load_jsonl(triples_path)}
    results = {(r["triple"], r["synth"]): r for r in load_jsonl(triples_path)}

    # ---- Phase 1: fix synth, score each triple (rotated non-member judge) ----
    def phase1(idx_combo):
        ti, combo = idx_combo
        triple = tuple(POOL[i][0] for i in combo)
        key = (",".join(triple), SYNTH_PHASE1)
        if key in done_keys:
            return f"  cached {triple}"
        non_members = [POOL[i][0] for i in range(len(POOL)) if i not in combo]
        judge = non_members[ti % len(non_members)]  # rotate
        print(f"  [{ti+1}/{len(triples)}] {triple} synth={SYNTH_PHASE1} judge={judge}", flush=True)
        sc = eval_triple(list(triple), SYNTH_PHASE1, judge, subset, singles, pplx, args.proxy)
        rec = {"triple": ",".join(triple), "synth": SYNTH_PHASE1, "judge": judge, **{k: v for k, v in sc.items() if k != "per_q"}}
        append_jsonl(triples_path, rec, lock)
        done_keys.add(key)
        results[key] = rec
        return f"  {triple} -> avg_rank={sc['committee_avg_rank']} win1={sc['committee_win1']}/{sc['n']}"

    with ThreadPoolExecutor(max_workers=1) as ex:
        for line in ex.map(phase1, list(enumerate(triples))):
            print(line)

    # ---- report phase 1 ----
    ph1 = sorted([r for r in results.values() if r["synth"] == SYNTH_PHASE1],
                 key=lambda r: r["committee_avg_rank"])
    report = ["# committee config search (phase 1: members, synth=gemini-3.1-pro)\n",
              "Lower committee_avg_rank = better committee. win1 = #questions committee ranked #1.\n",
              "| rank | triple | judge | n | committee_avg_rank | win1 | beats_best_member |",
              "|---|---|---|---|---|---|---|"]
    for i, r in enumerate(ph1, 1):
        report.append(f"| {i} | {r['triple']} | {r['judge']} | {r['n']} | {r['committee_avg_rank']} | {r['committee_win1']} | {r['committee_beats_best_member']} |")
    best = ph1[0]["triple"].split(",") if ph1 else None
    report.append(f"\n**Best triple:** {best}\n")
    (out / "search_phase1.md").write_text("\n".join(report))
    print("\n" + "\n".join(report))

    if args.skip_phase2 or not best:
        print(f"\nDone. Results in {out}"); return

    # ---- Phase 2: hold best triple, vary synthesizer among non-members ----
    print(f"\n=== Phase 2: best triple {best}, trying synthesizers ===")
    best_idx = {POOL[i][0]: i for i in range(len(POOL))}
    combo = tuple(best_idx[m] for m in best)
    non_members = [POOL[i][0] for i in range(len(POOL)) if i not in combo]
    synths_to_try = [m for m in non_members if m != SYNTH_PHASE1] + [SYNTH_PHASE1]
    p2 = []
    for si, synth in enumerate(synths_to_try):
        key = (",".join(best), synth)
        if key in done_keys:
            sc = results[key]
        else:
            judge = non_members[si % len(non_members)]
            print(f"  synth={synth} judge={judge}", flush=True)
            sc = eval_triple(best, synth, judge, subset, singles, pplx, args.proxy)
            rec = {"triple": ",".join(best), "synth": synth, "judge": judge, **{k: v for k, v in sc.items() if k != "per_q"}}
            append_jsonl(triples_path, rec, lock)
            results[key] = rec
        p2.append((synth, sc))
    p2.sort(key=lambda x: x[1]["committee_avg_rank"])
    rep2 = ["\n# Phase 2: synthesizer search (best triple held)\n",
            "| synth | judge | n | committee_avg_rank | win1 | beats_best_member |",
            "|---|---|---|---|---|---|"]
    for synth, sc in p2:
        rep2.append(f"| {synth} | {sc.get('judge','?')} | {sc['n']} | {sc['committee_avg_rank']} | {sc['committee_win1']} | {sc['committee_beats_best_member']} |")
    best_synth = p2[0][0]
    rep2.append(f"\n**Best synthesizer:** {best_synth}\n**RECOMMENDED CONFIG: members={best}  synthesizer={best_synth}**\n")
    (out / "search_phase2.md").write_text("\n".join(rep2))
    print("\n" + "\n".join(rep2))
    print(f"\nDone. Results in {out}")


if __name__ == "__main__":
    main()
