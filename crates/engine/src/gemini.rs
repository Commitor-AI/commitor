use anyhow::{Context, Result};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};

const MODEL: &str = "gemini-3.7-flash";
const API_URL_FORMAT: &str =
    "https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent";

#[derive(Serialize)]
struct ContentPart {
    text: String,
}

#[derive(Serialize)]
struct Content {
    parts: Vec<ContentPart>,
}

#[derive(Serialize)]
struct GenerateRequest {
    contents: Vec<Content>,
}

#[derive(Deserialize)]
struct CandidatePart {
    text: String,
}

#[derive(Deserialize)]
struct CandidateContent {
    parts: Vec<CandidatePart>,
}

#[derive(Deserialize)]
struct Candidate {
    content: CandidateContent,
}

#[derive(Deserialize)]
struct GenerateResponse {
    candidates: Vec<Candidate>,
}

/// Send a prompt to the Gemini API and return the raw text response.
///
/// Authentication is via the `x-goog-api-key` header (current Google format).
pub fn call_gemini(api_key: &str, prompt: &str) -> Result<String> {
    let url = API_URL_FORMAT.replace("{model}", MODEL);

    let body = GenerateRequest {
        contents: vec![Content {
            parts: vec![ContentPart {
                text: prompt.to_string(),
            }],
        }],
    };

    let client = Client::new();
    let response = client
        .post(&url)
        .header("x-goog-api-key", api_key)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .context("failed to send request to Gemini API")?;

    let status = response.status();
    if !status.is_success() {
        let body_text = response
            .text()
            .unwrap_or_else(|_| "<could not read response body>".to_string());
        anyhow::bail!(
            "Gemini API returned HTTP {}: {}",
            status.as_u16(),
            body_text
        );
    }

    let parsed: GenerateResponse = response
        .json()
        .context("failed to parse Gemini API response")?;

    parsed
        .candidates
        .into_iter()
        .next()
        .and_then(|c| c.content.parts.into_iter().next())
        .map(|p| p.text)
        .context("Gemini API response contained no text")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_response_body() {
        let json = r#"{"candidates": []}"#;
        let parsed: GenerateResponse = serde_json::from_str(json).unwrap();
        let result: Result<String> = parsed
            .candidates
            .into_iter()
            .next()
            .and_then(|c| c.content.parts.into_iter().next())
            .map(|p| p.text)
            .context("no text");
        assert!(result.is_err());
    }
}
