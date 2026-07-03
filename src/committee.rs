//! Model committee (Council Mode): N heterogeneous experts answer the same
//! question in parallel, a distinct synthesizer model merges them into a
//! structured 4-part result (consensus / disagreement / unique findings /
//! comprehensive analysis). See Wu et al. 2026.
//!
//! Heterogeneity is the point — members span different providers, because
//! homogeneous stacking has near-zero effective independence (n_eff ≈ 2).
//! Structured synthesis beats majority vote (vote-replacement raises
//! hallucination). The synthesizer differs from the members to avoid self-bias.

use crate::client::Client;
use crate::config;
use crate::error::{Error, Result};
use crate::sse::WebResult;
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

/// One committee member's answer (or its captured error, for degraded runs).
#[derive(Serialize)]
pub struct MemberResult {
    pub model: String,
    pub answer: String,
    pub sources: Vec<WebResult>,
    pub latency_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The synthesizer's 4-part Council output. `raw` is the full text; the named
/// fields are a best-effort header parse (fall back to `raw` if it fails).
#[derive(Serialize)]
pub struct Synthesis {
    pub model: String,
    pub consensus: String,
    pub disagreement: String,
    pub unique_findings: String,
    pub comprehensive: String,
    pub raw: String,
    pub latency_ms: u128,
}

/// Full committee result.
#[derive(Serialize)]
pub struct CommitteeResult {
    pub query: String,
    pub members: Vec<MemberResult>,
    pub synthesis: Synthesis,
    pub latency_ms: u128,
}

/// Run a committee: parallel expert fan-out → structured synthesis.
///
/// `members` are display names (resolved via [`config::find_model`]); an
/// unknown member aborts early (config error). A member that *fails at runtime*
/// (timeout, server error) is recorded with `error` and excluded from the
/// synthesis — the committee degrades gracefully rather than aborting.
/// All threads (members + synthesizer) are cleaned up best-effort.
pub async fn run(
    client: &Client,
    query: &str,
    members: &[String],
    synthesizer: &str,
    files: &[&Path],
    sources: &[&str],
    language: &str,
) -> Result<CommitteeResult> {
    let start = Instant::now();

    // Resolve members up front — a bad name is a user error, so abort.
    let resolved: Vec<(String, String, String)> = members
        .iter()
        .map(|m| {
            let (pref, mode) = config::find_model(m).ok_or_else(|| Error::Server {
                status: 0,
                message: format!("unknown committee member model '{m}'"),
            })?;
            Ok((m.clone(), pref.to_string(), mode.to_string()))
        })
        .collect::<Result<Vec<_>>>()?;

    // One upload, shared across all members + the synthesizer.
    let attachments = client.upload_files(files).await?;

    // Parallel fan-out. Each future owns its inputs (no shared mutable borrows).
    let mut futs = Vec::new();
    for (name, pref, mode) in &resolved {
        let name = name.clone();
        let pref = pref.clone();
        let mode = mode.clone();
        let query = query.to_string();
        let language = language.to_string();
        let sources: Vec<String> = sources.iter().map(|s| s.to_string()).collect();
        let attachments = attachments.clone();
        futs.push(async move {
            let t = Instant::now();
            let sources_ref: Vec<&str> = sources.iter().map(|s| s.as_str()).collect();
            let res = client
                .query_with_attachments(&query, &mode, &pref, true, attachments, None, &language, &sources_ref)
                .await;
            (name, t.elapsed().as_millis(), res)
        });
    }
    let raw = futures_util::future::join_all(futs).await;

    // Collect member results; clean up each member's thread best-effort.
    let mut members_out: Vec<MemberResult> = Vec::with_capacity(raw.len());
    for (name, latency_ms, res) in raw {
        match res {
            Ok(r) => {
                if let Some(uuid) = &r.backend_uuid {
                    let _ = client.delete_thread(uuid, r.read_write_token.as_deref()).await;
                }
                members_out.push(MemberResult {
                    model: name,
                    answer: r.answer,
                    sources: r.web_results,
                    latency_ms: Some(latency_ms),
                    error: None,
                });
            }
            Err(e) => members_out.push(MemberResult {
                model: name,
                answer: String::new(),
                sources: Vec::new(),
                latency_ms: Some(latency_ms),
                error: Some(e.to_string()),
            }),
        }
    }

    // Synthesize over surviving members.
    let survivors: Vec<&MemberResult> = members_out.iter().filter(|m| m.error.is_none()).collect();
    if survivors.is_empty() {
        return Err(Error::Server {
            status: 0,
            message: "all committee members failed; cannot synthesize".into(),
        });
    }
    let (synth_pref, synth_mode) = config::find_model(synthesizer).ok_or_else(|| Error::Server {
        status: 0,
        message: format!("unknown synthesizer model '{synthesizer}'"),
    })?;
    let synth_prompt = build_synthesis_prompt(query, &survivors);
    let t = Instant::now();
    let synth_result = client
        .query_with_attachments(
            &synth_prompt,
            synth_mode,
            synth_pref,
            true,
            attachments,
            None,
            language,
            sources,
        )
        .await?;
    let synth_latency = t.elapsed().as_millis();
    if let Some(uuid) = &synth_result.backend_uuid {
        let _ = client.delete_thread(uuid, synth_result.read_write_token.as_deref()).await;
    }
    let (consensus, disagreement, unique, comprehensive) = parse_sections(&synth_result.answer);
    let synthesis = Synthesis {
        model: synthesizer.to_string(),
        consensus,
        disagreement,
        unique_findings: unique,
        comprehensive,
        raw: synth_result.answer,
        latency_ms: synth_latency,
    };

    Ok(CommitteeResult {
        query: query.to_string(),
        members: members_out,
        synthesis,
        latency_ms: start.elapsed().as_millis(),
    })
}

/// Build the Council synthesis prompt embedding the question + surviving
/// expert answers and prescribing the strict 4-part output format.
fn build_synthesis_prompt(query: &str, survivors: &[&MemberResult]) -> String {
    let mut s = String::new();
    s.push_str(
        "You are the synthesizer of a model committee. Several expert models \
         independently answered the SAME question. Produce one synthesis.\n\n",
    );
    s.push_str("Question: ");
    s.push_str(query);
    s.push_str("\n\nExpert answers:");
    for (i, m) in survivors.iter().enumerate() {
        s.push_str(&format!("\n\n[{}] ({}):\n{}", i + 1, m.model, m.answer));
    }
    s.push_str(
        "\n\nOutput EXACTLY these four Markdown sections, in this order, each \
         beginning with the exact header line:\n\
         ## Consensus\n(claims ALL experts agree on)\n\
         ## Disagreement\n(claims the experts conflict on — state each expert's stance and its reasoning)\n\
         ## Unique Findings\n(insights only ONE expert raised — attribute which)\n\
         ## Comprehensive Analysis\n(the best integrated final answer — do NOT introduce any claim none of the experts made)\n",
    );
    s
}

const SECTION_KEYS: [&str; 4] = ["consensus", "disagreement", "unique", "comprehensive"];

/// Classify a line as a section header (returns the canonical key) or None.
fn header_key(line: &str) -> Option<&'static str> {
    let stripped = line.trim_start_matches('#').trim().to_lowercase();
    for w in SECTION_KEYS {
        if stripped.starts_with(w) {
            return Some(w);
        }
    }
    None
}

/// Best-effort split of the synthesis text into the four sections by header.
/// Lines before any header are dropped; the full text is preserved in `raw`.
fn parse_sections(raw: &str) -> (String, String, String, String) {
    let mut sections: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut current: Option<&str> = None;
    for line in raw.lines() {
        if let Some(key) = header_key(line) {
            current = Some(key);
            sections.entry(key).or_default();
        } else if let Some(c) = current {
            sections.get_mut(c).unwrap().push(line);
        }
    }
    let get = |k: &str| {
        sections
            .get(k)
            .map(|v| v.join("\n").trim().to_string())
            .unwrap_or_default()
    };
    (
        get("consensus"),
        get("disagreement"),
        get("unique"),
        get("comprehensive"),
    )
}

#[cfg(test)]
mod tests {
    use super::parse_sections;

    #[test]
    fn parses_four_sections() {
        let raw = "## Consensus\nThe sky is blue due to Rayleigh scattering.\n\n\
                   ## Disagreement\nGLM says X; GPT says Y.\n\n\
                   ## Unique Findings\nOnly Claude noted ozone.\n\n\
                   ## Comprehensive Analysis\nBlue light scatters most.\n";
        let (c, d, u, a) = parse_sections(raw);
        assert!(c.contains("Rayleigh"));
        assert!(d.contains("GLM"));
        assert!(u.contains("ozone"));
        assert!(a.contains("Blue light"));
    }

    #[test]
    fn missing_sections_yield_empty() {
        let raw = "## Comprehensive Analysis\nOnly one section here.\n";
        let (c, _d, _u, a) = parse_sections(raw);
        assert!(c.is_empty());
        assert!(a.contains("Only one section"));
    }
}
