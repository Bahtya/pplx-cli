# Collect web sources before asking

Use [Parallel Search MCP](https://docs.parallel.ai/integrations/mcp/search-mcp)
to save search excerpts and page text to a local Markdown file, inspect it, then
attach it to a `pplx-cli` question. This is useful when you want the answer to
consider specific pages as well as search results.

The collector uses free anonymous search in Fast mode, with no Parallel account
or API key. Free access has rate limits. It is a standalone Python example;
`pplx-cli` still needs its usual installation and `PERPLEXITY_SESSION_TOKEN`
for the subsequent question and file upload.

## Run

Install [uv](https://docs.astral.sh/uv/getting-started/installation/) and use
Python 3.11 or newer. From the repository root, `uv run` installs the example's
declared dependencies in an isolated environment:

```bash
uv run examples/parallel_sources.py \
  "Find Tokio guidance on cancellation safety" \
  --query "tokio select cancellation safety" \
  --url "https://docs.rs/tokio/latest/tokio/macro.select.html" \
  --output sources.md
```

Repeat `--query` for related search angles. Omit `--url` to collect only search
results, or repeat it to fetch up to 20 specific public HTTP(S) pages. Fetches
return page excerpts, not guaranteed complete documents. The example does not
automatically fetch the search result URLs.

Review `sources.md`, including its source URLs, before uploading it. Web content
is untrusted reference material and may be incorrect or contain instructions;
verify claims against the original sources. Queries and requested page URLs go
to Parallel; attaching the resulting file sends it to Perplexity. Avoid secrets
and private URLs.

```bash
pplx-cli ask \
  "Using the attached sources as reference material, explain cancellation safety in tokio::select!. Cite source URLs and distinguish evidence from inference." \
  --file sources.md
```

Collection has a 60-second total deadline; use `--timeout 120` for a longer one.
A failed tool call, per-page extraction error, empty result set, or timeout exits
unsuccessfully without replacing the output file. Even partial fetch failures
leave the previous file intact. Run the `pplx-cli` command only after collection succeeds and you have
reviewed the file. To run the collector's offline tests:

```bash
uv run --with mcp==1.30.0 python -m unittest discover -s examples -p 'test_*.py'
```
