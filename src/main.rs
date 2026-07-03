#![allow(dead_code)]

mod client;
mod committee;
mod config;
mod error;
mod sse;
mod upload;

use clap::{Parser, Subcommand};
use client::Client;
use config::defaults;
use std::path::PathBuf;
use std::process;

#[derive(Parser)]
#[command(name = "pplx-cli", version, about = "CLI client for Perplexity AI")]
struct Cli {
    /// Proxy URL (http://, https://, socks5://)
    #[arg(long, global = true, env = "PERPLEXITY_PROXY")]
    proxy: Option<String>,

    /// Emit machine-readable JSON (committee / ask / search)
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Quick web search returning titles, URLs, and snippets
    Search {
        /// Search query
        query: String,
        /// Information sources: web, scholar, social
        #[arg(long, value_delimiter = ',', default_value = "web")]
        sources: Vec<String>,
        /// Language code (ISO 639), e.g. "en-US", "zh-CN"
        #[arg(long, default_value = "en-US")]
        language: String,
    },
    /// Ask Perplexity AI a question (single turn, incognito, no thinking model)
    Ask {
        /// Question to ask
        query: String,
        /// Model to use (default: claude-5.0-sonnet)
        #[arg(long)]
        model: Option<String>,
        /// Files to attach
        #[arg(long = "file")]
        files: Vec<PathBuf>,
        /// Information sources: web, scholar, social
        #[arg(long, value_delimiter = ',', default_value = "web")]
        sources: Vec<String>,
        /// Language code (ISO 639)
        #[arg(long, default_value = "en-US")]
        language: String,
    },
    /// Interactive reasoning mode (multi-turn, thinking model, history preserved)
    Reason {
        /// Optional initial question (skips prompt for first turn)
        #[arg(long)]
        query: Option<String>,
        /// Model to use (default: claude-5.0-sonnet-thinking)
        #[arg(long)]
        model: Option<String>,
        /// Files to attach (applied to first turn only)
        #[arg(long = "file")]
        files: Vec<PathBuf>,
        /// Information sources: web, scholar, social
        #[arg(long, value_delimiter = ',', default_value = "web")]
        sources: Vec<String>,
        /// Language code (ISO 639)
        #[arg(long, default_value = "en-US")]
        language: String,
    },
    /// Show current rate-limit status (queries /rest/rate-limit/all)
    Ratelimit,
    /// Model committee: 3 models answer in parallel, a synthesizer merges them
    Committee {
        /// Question for the committee
        query: String,
        /// Committee member models (3 different providers recommended)
        #[arg(long, value_delimiter = ',', default_value = "claude-5.0-sonnet,glm-5.2,gpt-5.4-thinking")]
        models: Vec<String>,
        /// Synthesizer model (distinct from members)
        #[arg(long, default_value = "gemini-3.1-pro")]
        synthesizer: String,
        /// Files to attach (shared across all members + synthesizer)
        #[arg(long = "file")]
        files: Vec<PathBuf>,
        /// Information sources: web, scholar, social
        #[arg(long, value_delimiter = ',', default_value = "web")]
        sources: Vec<String>,
        /// Language code (ISO 639)
        #[arg(long, default_value = "en-US")]
        language: String,
        /// Also print each member's raw answer
        #[arg(long)]
        raw: bool,
    },
}

fn resolve_model(
    name: &Option<String>,
    default_pref: &str,
    default_mode: &str,
) -> (String, String) {
    match name {
        Some(n) => match config::find_model(n) {
            Some((pref, mode)) => (pref.to_string(), mode.to_string()),
            None => {
                eprintln!("❌ Unknown model '{n}'. Available models:");
                for (name, _, _) in config::MODELS {
                    eprintln!("   • {name}");
                }
                process::exit(1);
            }
        },
        None => (default_pref.to_string(), default_mode.to_string()),
    }
}

fn print_web_results(results: &[sse::WebResult]) {
    if results.is_empty() {
        return;
    }
    println!("\nSources:");
    for (i, r) in results.iter().enumerate() {
        println!("[{}] {} — {}", i + 1, r.name, r.url);
        if !r.snippet.is_empty() {
            println!("    {}", r.snippet.replace('\n', " "));
        }
    }
}

fn get_session_token() -> String {
    std::env::var("PERPLEXITY_SESSION_TOKEN").unwrap_or_else(|_| {
        eprintln!("❌ PERPLEXITY_SESSION_TOKEN environment variable not set");
        eprintln!("   Get it from your browser cookies on perplexity.ai");
        process::exit(1);
    })
}

fn read_line(prompt: &str) -> Option<String> {
    eprint!("{prompt}");
    let mut input = String::new();
    match std::io::stdin().read_line(&mut input) {
        Ok(0) => None,
        Ok(_) => Some(input.trim().to_string()),
        Err(_) => None,
    }
}

/// One turn read from stdin in `reason` mode.
enum TurnInput {
    Quit,
    Query(String),
}

/// Read a (possibly multi-line) question for `reason`. Lines accumulate until a
/// blank line (or EOF) submits them as one question. `/quit`/`/exit`/`/q` on any
/// line quits; `/help` prints help and keeps reading.
fn read_turn() -> TurnInput {
    let mut buf = String::new();
    loop {
        let prompt = if buf.is_empty() { "> " } else { "… " };
        let Some(line) = read_line(prompt) else {
            // EOF: submit any pending text, else signal quit (avoids looping forever).
            return if buf.trim().is_empty() {
                TurnInput::Quit
            } else {
                TurnInput::Query(buf.trim().to_string())
            };
        };
        match line.as_str() {
            "/quit" | "/exit" | "/q" => return TurnInput::Quit,
            "/help" => {
                eprintln!("Commands: /quit, /exit, /help  (a blank line submits the question)");
                continue;
            }
            "" => {
                // Blank line submits when there is pending text; leading blanks are ignored.
                if buf.trim().is_empty() {
                    continue;
                }
                return TurnInput::Query(buf.trim().to_string());
            }
            _ => {
                if !buf.is_empty() {
                    buf.push('\n');
                }
                buf.push_str(&line);
            }
        }
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let session_token = get_session_token();
    let proxy = cli.proxy.as_deref();

    let client = match Client::new(&session_token, proxy).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("❌ Failed to initialize client: {e}");
            process::exit(1);
        }
    };

    if let Err(e) = client.fetch_csrf().await {
        eprintln!("❌ Session validation failed: {e}");
        eprintln!("   Your PERPLEXITY_SESSION_TOKEN may have expired");
        process::exit(1);
    }

    let json = cli.json;
    let result = match cli.command {
        Commands::Search { query, sources, language } => {
            cmd_search(&client, json, &query, &sources, &language).await
        }
        Commands::Ask { query, model, files, sources, language } => {
            cmd_ask(&client, json, &query, &model, &files, &sources, &language).await
        }
        Commands::Reason { query, model, files, sources, language } => {
            cmd_reason(&client, &query, &model, &files, &sources, &language).await
        }
        Commands::Committee { query, models, synthesizer, files, sources, language, raw } => {
            cmd_committee(&client, json, &query, &models, &synthesizer, &files, &sources, &language, raw).await
        }
        Commands::Ratelimit => cmd_ratelimit(&client).await,
    };

    if let Err(e) = result {
        eprintln!("❌ Error: {e}");
        process::exit(1);
    }
}

async fn cmd_search(
    client: &Client,
    json: bool,
    query: &str,
    sources: &[String],
    language: &str,
) -> error::Result<()> {
    let file_paths: Vec<&std::path::Path> = Vec::new();
    let sources_ref: Vec<&str> = sources.iter().map(|s| s.as_str()).collect();

    let result = client
        .query(
            query,
            defaults::SEARCH_MODE,
            defaults::SEARCH_MODEL,
            true,
            &file_paths,
            None,
            language,
            &sources_ref,
        )
        .await?;

    if json {
        let out = serde_json::json!({ "model": defaults::SEARCH_MODEL, "results": result.web_results });
        println!("{out}");
    } else {
        print_web_results(&result.web_results);
    }

    if let Some(ref uuid) = result.backend_uuid {
        if let Err(e) = client.delete_thread(uuid, result.read_write_token.as_deref()).await {
            eprintln!("⚠️  Thread cleanup failed: {e}");
        }
    }

    Ok(())
}

async fn cmd_ask(
    client: &Client,
    json: bool,
    query: &str,
    model: &Option<String>,
    files: &[PathBuf],
    sources: &[String],
    language: &str,
) -> error::Result<()> {
    let (model_pref, mode) = resolve_model(model, defaults::ASK_MODEL, defaults::ASK_MODE);
    let file_refs: Vec<&std::path::Path> = files.iter().map(|p| p.as_path()).collect();
    let sources_ref: Vec<&str> = sources.iter().map(|s| s.as_str()).collect();

    if json {
        // Machine mode: collect (no streaming) and emit JSON.
        let result = client
            .query(query, &mode, &model_pref, true, &file_refs, None, language, &sources_ref)
            .await?;
        if let Some(ref uuid) = result.backend_uuid {
            if let Err(e) = client.delete_thread(uuid, result.read_write_token.as_deref()).await {
                eprintln!("⚠️  Thread cleanup failed: {e}");
            }
        }
        let out = serde_json::json!({
            "model": model_pref,
            "answer": result.answer,
            "sources": result.web_results,
        });
        println!("{out}");
        return Ok(());
    }

    let result = client
        .query_live(
            query,
            &mode,
            &model_pref,
            true,
            &file_refs,
            None,
            language,
            &sources_ref,
        )
        .await?;

    print_web_results(&result.web_results);

    if let Some(ref uuid) = result.backend_uuid {
        if let Err(e) = client.delete_thread(uuid, result.read_write_token.as_deref()).await {
            eprintln!("⚠️  Thread cleanup failed: {e}");
        }
    }

    Ok(())
}

async fn cmd_reason(
    client: &Client,
    initial_query: &Option<String>,
    model: &Option<String>,
    initial_files: &[PathBuf],
    sources: &[String],
    language: &str,
) -> error::Result<()> {
    let (model_pref, mode) = resolve_model(model, defaults::REASON_MODEL, defaults::REASON_MODE);
    let sources_ref: Vec<&str> = sources.iter().map(|s| s.as_str()).collect();

    eprintln!("🤔 Model: {model_pref} | blank line to send, /quit to exit");

    let mut backend_uuid: Option<String> = None;
    let mut read_write_token: Option<String> = None;

    if let Some(query) = initial_query {
        let file_refs: Vec<&std::path::Path> = initial_files.iter().map(|p| p.as_path()).collect();
        eprintln!("> {query}");

        let result = client
            .query_live(
                query,
                &mode,
                &model_pref,
                false,
                &file_refs,
                backend_uuid.as_deref(),
                language,
                &sources_ref,
            )
            .await?;

        backend_uuid = result.backend_uuid.clone();
        read_write_token = result.read_write_token.clone();
    }

    loop {
        let input = match read_turn() {
            TurnInput::Quit => break,
            TurnInput::Query(q) => q,
        };

        let file_refs: Vec<&std::path::Path> = Vec::new();

        let result = match client
            .query_live(
                &input,
                &mode,
                &model_pref,
                false,
                &file_refs,
                backend_uuid.as_deref(),
                language,
                &sources_ref,
            )
            .await
        {
            Ok(r) => r,
            Err(e) => {
                eprintln!("❌ {e}");
                continue;
            }
        };

        backend_uuid = result.backend_uuid.clone();
        read_write_token = result.read_write_token.clone();
    }

    if let Some(ref uuid) = backend_uuid {
        eprint!("Cleaning up thread... ");
        match client.delete_thread(uuid, read_write_token.as_deref()).await {
            Ok(()) => eprintln!("done"),
            Err(e) => eprintln!("failed: {e}"),
        }
    }

    Ok(())
}

async fn cmd_committee(
    client: &Client,
    json: bool,
    query: &str,
    members: &[String],
    synthesizer: &str,
    files: &[PathBuf],
    sources: &[String],
    language: &str,
    raw: bool,
) -> error::Result<()> {
    let file_refs: Vec<&std::path::Path> = files.iter().map(|p| p.as_path()).collect();
    let sources_ref: Vec<&str> = sources.iter().map(|s| s.as_str()).collect();

    let result =
        committee::run(client, query, members, synthesizer, &file_refs, &sources_ref, language)
            .await?;

    if json {
        println!("{}", serde_json::to_string(&result).unwrap());
        return Ok(());
    }

    print_committee_human(&result, raw);
    Ok(())
}

async fn cmd_ratelimit(client: &Client) -> error::Result<()> {
    // /rest/ endpoints require the version+source query params (cf. delete_thread).
    let path = format!("/rest/rate-limit/all?version={}&source=default", config::API_VERSION);
    let body = client.get_raw(&path).await?;
    // Pretty-print if it's JSON, else print raw.
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
        println!("{}", serde_json::to_string_pretty(&v).unwrap_or(body));
    } else {
        println!("{body}");
    }
    Ok(())
}

fn print_committee_human(result: &committee::CommitteeResult, raw: bool) {
    let s = &result.synthesis;
    eprintln!(
        "🏛️  Committee [{}] → synth {} | {:.1}s",
        result
            .members
            .iter()
            .map(|m| m.model.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        s.model,
        result.latency_ms as f64 / 1000.0
    );

    if raw {
        for m in &result.members {
            println!("\n──────── {} ────────", m.model);
            if let Some(e) = &m.error {
                println!("❌ {e}");
            } else {
                println!("{}", m.answer);
            }
        }
    }

    println!("\n==================== SYNTHESIS ({}) ====================", s.model);
    let print_section = |title: &str, body: &str| {
        if !body.is_empty() {
            println!("\n■ {title}\n{}", body);
        }
    };
    if s.consensus.is_empty()
        && s.disagreement.is_empty()
        && s.unique_findings.is_empty()
        && s.comprehensive.is_empty()
    {
        // Header parse failed — fall back to the raw synthesis text.
        println!("{}", s.raw);
    } else {
        print_section("Consensus", &s.consensus);
        print_section("Disagreement", &s.disagreement);
        print_section("Unique Findings", &s.unique_findings);
        print_section("Comprehensive Analysis", &s.comprehensive);
    }

    // Merged, URL-deduped sources across all members.
    let mut seen = std::collections::HashSet::new();
    let merged: Vec<&sse::WebResult> = result
        .members
        .iter()
        .flat_map(|m| m.sources.iter())
        .filter(|r| seen.insert(r.url.as_str()))
        .collect();
    if !merged.is_empty() {
        println!("\nSources:");
        for (i, r) in merged.iter().enumerate() {
            println!("[{}] {} — {}", i + 1, r.name, r.url);
        }
    }
}
