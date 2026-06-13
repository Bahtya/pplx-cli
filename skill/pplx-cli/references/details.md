# pplx-cli — reference details

Loaded on demand from SKILL.md. Covers the model list, troubleshooting, and the
optional true-multi-turn recipe.

## Modes recap (defaults)

| Mode | Command | Default model | Search mode | Incognito | Multi-turn |
|------|---------|---------------|-------------|-----------|------------|
| search | `pplx-cli search "<q>"` | turbo | concise | ON | no |
| ask | `pplx-cli ask "<q>" [flags]` | claude-4.6-sonnet (non-thinking) | concise | ON | no (single turn) |
| reason | `pplx-cli reason [--query "<q>"]` | claude-4.6-sonnet-thinking | copilot | OFF | yes (interactive) |

- `search` is always `turbo` (fast, free tier) — it's for links/snippets, not synthesis.
- `ask` is single-turn and incognito; the temp thread is auto-deleted after the answer.
- `reason` keeps a thread (incognito OFF, history preserved) so follow-ups carry context — *within one running session*.

## `--model` values (ask / reason)

Hardcoded list — pass the display name to `--model`:

| `--model` | Thinking | Notes |
|-----------|----------|-------|
| `claude-4.6-sonnet` | no | ask default |
| `claude-4.6-sonnet-thinking` | yes | reason default |
| `claude-4.8-opus` | no | stronger, slower |
| `claude-4.8-opus-thinking` | yes | strongest/slowest |
| `gpt-5.5` / `gpt-5.5-thinking` | no/yes | |
| `gemini-3.1-pro` / `-thinking` | no/yes | |
| `kimi` / `kimi-thinking` | no/yes | |

An unknown name prints the valid list and exits. `search` ignores `--model` (always turbo).

## Troubleshooting

### Token expired
Symptoms: `❌ Session validation failed`, or persistent 403 / empty answers, or
a `ℹ️ Server used turbo` notice on *non-trivial* queries (a downgrade on a real
question is a soft sign the session is degrading). Fix: the user refreshes
`PERPLEXITY_SESSION_TOKEN` in `~/.bashrc` from the
`__Secure-next-auth.session-token` cookie on perplexity.ai, then `source ~/.bashrc`.

### Timeouts / hangs / answers cut mid-sentence
The streaming request timeout is generous (360s) to tolerate the thinking
model's ~20–40s silent "thinking" period. If a call still hangs or the network
is flaky, retry with the local proxy:
```bash
pplx-cli --proxy http://127.0.0.1:7890 ask "..."
```
(`--proxy` is a global flag; put it before the subcommand.) `PERPLEXITY_PROXY`
works too.

### Silent downgrade to turbo
On trivial inputs Perplexity routes to the free `turbo` model even if you asked
for Sonnet/Opus; you'll see `ℹ️ Server used turbo …` on stderr. This is
*legitimate* for trivial questions and does NOT mean the token is bad. Only
treat repeated downgrades on substantive questions as a possible token issue.
Lower-quality answer → re-ask more specifically or switch model.

### "thread cleanup failed"
Non-fatal — the answer succeeded but the temp thread couldn't be deleted (often
a transient network blip). No action needed.

## Optional: true server-side multi-turn (one continuous `reason` thread)

The default (SKILL.md) is **sequential independent calls** — robust, and the
agent carries context in each prompt. That's recommended.

If you genuinely need Perplexity to remember across turns (one continuous
thread, incognito-OFF history), `reason` supports it — but an agent can't hold
an interactive prompt open across tool calls, so you must drive it via a FIFO
with a "holder" keeping stdin open, sending one turn at a time:

```bash
F=~/pplx_req; rm -f "$F"; mkfifo "$F"
: > ~/pplx_out.log; : > ~/pplx_err.log
( sleep 1800 > "$F" ) &                       # holder: keeps write-end open (no EOF between turns)
pplx-cli reason --query "turn 1 question" < "$F" > ~/pplx_out.log 2> ~/pplx_err.log &
# send turn N: question lines, then a BLANK line to submit
printf 'turn 2 line one\nturn 2 line two\n\n' > "$F"
# when done:
printf '/quit\n' > "$F"
```

Caveats (why this is optional, not the default):
- After sending a turn, wait for the answer to finish before sending the next.
  Poll `~/pplx_out.log`: wait until it grows past the previous size (answer
  started), then until it's byte-stable for ~10s (answer done). The thinking
  silence means it may not grow for 20–40s — that's fine, just wait.
- `reason` deletes the thread on `/quit`/exit, so this only persists *during*
  the session.
- Don't put `pkill -f 'pplx-cli reason'` in a script whose own command line
  contains that string — it will match and kill its own shell. Use `TaskStop`
  or kill by PID.

Prefer sequential calls unless you have a concrete reason to need the shared
thread.
