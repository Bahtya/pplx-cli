#![allow(dead_code)]

mod client;
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
        /// Model to use (default: claude-4.8-opus)
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
        /// Model to use (default: claude-4.8-opus-thinking)
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

    let result = match cli.command {
        Commands::Search {
            query,
            sources,
            language,
        } => cmd_search(&client, &query, &sources, &language).await,
        Commands::Ask {
            query,
            model,
            files,
            sources,
            language,
        } => cmd_ask(&client, &query, &model, &files, &sources, &language).await,
        Commands::Reason {
            query,
            model,
            files,
            sources,
            language,
        } => cmd_reason(&client, &query, &model, &files, &sources, &language).await,
    };

    if let Err(e) = result {
        eprintln!("❌ Error: {e}");
        process::exit(1);
    }
}

async fn cmd_search(
    client: &Client,
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

    print_web_results(&result.web_results);

    if let Some(ref uuid) = result.backend_uuid {
        if let Err(e) = client.delete_thread(uuid, result.read_write_token.as_deref()).await {
            eprintln!("⚠️  Thread cleanup failed: {e}");
        }
    }

    Ok(())
}

async fn cmd_ask(
    client: &Client,
    query: &str,
    model: &Option<String>,
    files: &[PathBuf],
    sources: &[String],
    language: &str,
) -> error::Result<()> {
    let (model_pref, mode) = resolve_model(model, defaults::ASK_MODEL, defaults::ASK_MODE);
    let file_refs: Vec<&std::path::Path> = files.iter().map(|p| p.as_path()).collect();
    let sources_ref: Vec<&str> = sources.iter().map(|s| s.as_str()).collect();

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

    eprintln!("🤔 Model: {model_pref} | /quit to exit");

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

        backend_uuid = result.backend_uuid;
        read_write_token = result.read_write_token;
    }

    loop {
        let input = match read_line("> ") {
            Some(s) if s.is_empty() => continue,
            Some(s) => s,
            None => break,
        };

        match input.as_str() {
            "/quit" | "/exit" | "/q" => break,
            "/help" => {
                eprintln!("Commands: /quit, /exit, /help");
                continue;
            }
            _ => {}
        }

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

        backend_uuid = result.backend_uuid;
        read_write_token = result.read_write_token;
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
