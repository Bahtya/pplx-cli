# pplx-cli

A command-line client for the [Perplexity AI](https://www.perplexity.ai) web API, built in Rust with **Chrome 136 TLS fingerprint emulation** via [`rquest`](https://github.com/0x676e67/rquest).

Inspired by [`perplexity-web-api-mcp`](https://github.com/Bahtya/perplexity-web-api-mcp) and the `pplx_bridge.py` design, but streamlined into an interactive CLI.

## Features

- 🔐 **Authentication via environment variable** — only `PERPLEXITY_SESSION_TOKEN` needed (CSRF auto-fetched)
- 🌐 **Three modes**: `search`, `ask`, `reason` (no research mode)
- 💬 **Multi-turn conversations** in `reason` mode (same thread, history preserved)
- 🧹 **Thread cleanup** — automatically deletes threads created by ask/search
- 📎 **File attachments** — upload PDFs, images, text, code for analysis
- 🔍 **TLS fingerprint** — Chrome 136 emulation via `rquest-util` (hard requirement)
- 🔀 **Proxy support** — HTTP, HTTPS, and SOCKS5
- 🤖 **Hardcoded LLM models** — Claude 4.8 Opus, GPT-5.5, Gemini 3.1 Pro, Kimi K2

## Modes

| Mode | Description | Model | Incognito | Multi-turn |
|------|-------------|-------|-----------|------------|
| `search` | Quick web search (titles, URLs, snippets only) | turbo | ✅ ON | ❌ |
| `ask` | AI question-answering with sources | claude-4.8-opus | ✅ ON | ❌ |
| `reason` | Deep reasoning, interactive multi-turn | claude-4.8-opus-thinking | ❌ OFF | ✅ |

- **ask** uses non-thinking models, incognito ON, single query
- **reason** uses thinking models, incognito OFF, preserves conversation history for follow-ups

## Installation

### Prerequisites

- Rust 1.85+ (edition 2024)
- **cmake** and a C/C++ compiler (for building BoringSSL via `boring-sys2`)
- **Android NDK** (only when building on Android/Termux — set `ANDROID_NDK_HOME`)

```bash
# On Termux (Android), install prerequisites:
pkg install cmake ndk-multilib
export ANDROID_NDK_HOME="$HOME/android-ndk"

# Create a minimal NDK toolchain pointing at Termux's own toolchain
mkdir -p ~/android-ndk/build/cmake
cat > ~/android-ndk/build/cmake/android.toolchain.cmake << 'EOF'
set(CMAKE_SYSTEM_NAME Android)
set(CMAKE_SYSTEM_VERSION 21)
set(CMAKE_SYSTEM_PROCESSOR aarch64)
set(CMAKE_C_COMPILER "$ENV{PREFIX}/bin/aarch64-linux-android-cc")
set(CMAKE_CXX_COMPILER "$ENV{PREFIX}/bin/aarch64-linux-android-c++")
set(CMAKE_ASM_COMPILER "$ENV{PREFIX}/bin/aarch64-linux-android-cc")
set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)
set(CMAKE_FIND_ROOT_PATH_MODE_LIBRARY ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_INCLUDE ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_PACKAGE ONLY)
EOF
```

### Build

```bash
git clone https://github.com/Bahtya/pplx-cli.git
cd pplx-cli
cargo build --release
# Binary at ./target/release/pplx-cli
```

> **Note:** Because `rquest` 5.x was yanked from crates.io, this project vendors `rquest` 5.1.0 and `rquest-util` 2.2.1 locally under `local/` (with a `[patch.crates-io]` redirect). No registry access to the yanked versions is needed.

## Configuration

Set your Perplexity session token as an environment variable:

```bash
export PERPLEXITY_SESSION_TOKEN="your-session-token-here"
```

**How to get the token:**
1. Log in to [perplexity.ai](https://www.perplexity.ai) in your browser
2. Open DevTools → Application → Cookies → `https://www.perplexity.ai`
3. Copy the value of `__Secure-next-auth.session-token`

Optional proxy:

```bash
export PERPLEXITY_PROXY="socks5://127.0.0.1:1080"
# or pass per-command: pplx-cli --proxy http://host:port search "..."
```

## Usage

### Search

```bash
pplx-cli search "rust async runtime comparison"
```
Returns ranked web results (titles, URLs, snippets). Always uses the turbo model.

### Ask

```bash
pplx-cli ask "explain tokio::select!"
pplx-cli ask "analyze this code" --file main.rs
pplx-cli ask "summarize" --file report.pdf --sources scholar
pplx-cli ask "quick question" --model gpt-5.5 --language zh-CN
```
Single-turn AI answer with sources. Incognito ON, thread auto-deleted after.

### Reason (interactive)

```bash
pplx-cli reason
pplx-cli reason --query "compare tokio and async-std" --file Cargo.toml
```
Enters an interactive multi-turn session. Type follow-up questions; the same thread (via `backend_uuid`) is reused. Type `/quit` or press `Ctrl-D` to exit (the thread is then deleted).

```
🤔 Model: claude48opusthinking | /quit to exit
> compare tokio and async-std
[streaming answer...]

> what about smol?
[streaming answer, same thread...]

> /quit
Cleaning up thread... done
```

## Available Models

Use `--model <name>` with `ask` or `reason`:

| Name | Thinking |
|------|----------|
| `claude-4.8-opus` | ❌ |
| `claude-4.8-opus-thinking` | ✅ |
| `gpt-5.5` | ❌ |
| `gpt-5.5-thinking` | ✅ |
| `gemini-3.1-pro` | ❌ |
| `gemini-3.1-pro-thinking` | ✅ |
| `kimi` | ❌ |
| `kimi-thinking` | ✅ |

## How It Works

- **TLS fingerprint**: `rquest` with `Emulation::Chrome136` mimics a real Chrome browser's TLS handshake, which Perplexity requires.
- **CSRF**: Fetched automatically from `GET /api/auth/csrf` at startup (also validates the session).
- **SSE streaming**: Responses stream from `/rest/sse/perplexity_ask` via Server-Sent Events; the parser handles `delta`, `answer`, `done`, `web_results`, `metadata`, and detects silent **model downgrade** (a sign the token expired).
- **Multi-turn**: The `backend_uuid` + `read_write_token` from each response are carried into the next request's `last_backend_uuid` field.
- **Thread cleanup**: On exit, calls `DELETE /rest/thread/delete_thread_by_entry_uuid` so no history is left behind.

## License

Same as the upstream `perplexity-web-api-mcp` project.
