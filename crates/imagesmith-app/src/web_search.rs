//! Web search for the assistant, run on the Mac so it works the same with
//! OpenRouter and local models: LangSearch's API, or OpenRouter's web search
//! server tool driven by a small side request.

use imagesmith_core::settings::{Settings, WebSearch, DEFAULT_AI_MODEL};
use serde_json::{json, Value};
use std::io::Write;
use std::process::{Command, Stdio};

const LANGSEARCH_URL: &str = "https://api.langsearch.com/v1/web-search";
const OPENROUTER_URL: &str = "https://openrouter.ai/api/v1/chat/completions";
/// Characters of page text kept per result.
const TEXT_CAP: usize = 1500;

#[derive(Clone)]
pub enum Backend {
    LangSearch { key: String },
    OpenRouter { key: String, model: String },
}

impl Backend {
    pub fn label(&self) -> &'static str {
        match self {
            Backend::LangSearch { .. } => "LangSearch",
            Backend::OpenRouter { .. } => "OpenRouter",
        }
    }
}

/// The configured search backend, if it's enabled and has a key.
pub fn backend(s: &Settings) -> Option<Backend> {
    let key = |k: &Option<String>| k.as_deref().map(str::trim).filter(|k| !k.is_empty()).map(String::from);
    match s.web_search {
        WebSearch::Off => None,
        WebSearch::LangSearch => Some(Backend::LangSearch { key: key(&s.langsearch_api_key)? }),
        WebSearch::OpenRouter => Some(Backend::OpenRouter {
            key: key(&s.openrouter_api_key)?,
            model: if s.ai_model.trim().is_empty() { DEFAULT_AI_MODEL.into() } else { s.ai_model.trim().to_string() },
        }),
    }
}

/// Searches and returns results formatted for the model, plus how many there were.
/// `domains` limits results to those sites when non-empty.
pub fn search(b: &Backend, query: &str, count: u64, domains: &[String]) -> Result<(String, usize), String> {
    let query = query.trim();
    if query.is_empty() {
        return Err("empty query".into());
    }
    let count = count.clamp(1, 10);
    match b {
        Backend::LangSearch { key } => {
            let mut body = json!({ "query": query, "count": count, "contents": { "text": { "max_characters": TEXT_CAP } } });
            if !domains.is_empty() {
                body["includeDomains"] = json!(domains);
            }
            let v = post_json(LANGSEARCH_URL, key, &body, 30).map_err(|e| format!("LangSearch: {e}"))?;
            Ok(format_langsearch(&v))
        }
        Backend::OpenRouter { key, model } => {
            let mut params = json!({ "engine": "exa", "max_results": count });
            if !domains.is_empty() {
                params["allowed_domains"] = json!(domains);
            }
            let body = json!({
                "model": model,
                "messages": [{ "role": "user", "content": format!(
                    "Search the web for: {query}\n\nReply with the most relevant findings. Quote exact URLs, commands, repository lines and key URLs verbatim. Do not guess."
                ) }],
                "tools": [{ "type": "openrouter:web_search", "parameters": params }],
            });
            let v = post_json(OPENROUTER_URL, key, &body, 120).map_err(|e| format!("OpenRouter: {e}"))?;
            Ok(format_openrouter(&v))
        }
    }
}

/// POSTs JSON with a bearer key passed on stdin, so it stays out of the process list.
fn post_json(url: &str, key: &str, body: &Value, timeout: u32) -> Result<Value, String> {
    if key.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err("the API key is not valid".into());
    }
    let mut child = Command::new("/usr/bin/curl")
        .args(["-sS", "--max-time", &timeout.to_string(), "-X", "POST", url, "-H", "Content-Type: application/json", "-H", "X-Title: ImageSmith", "-H", "@-"])
        .args(["--data-binary", &body.to_string(), "-w", "\n%{http_code}"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run curl: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = writeln!(stdin, "Authorization: Bearer {key}");
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().trim_start_matches("curl: ").to_string());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let (body, code) = text.rsplit_once('\n').unwrap_or(("", &text));
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let code = code.trim();
    if code == "401" || code == "403" {
        return Err("the API key was rejected. Check it in Settings.".into());
    }
    let msg = v.pointer("/error/message").or_else(|| v.get("msg")).or_else(|| v.get("message")).and_then(Value::as_str);
    if code != "200" {
        return Err(msg.map(String::from).unwrap_or_else(|| format!("HTTP {code}")));
    }
    if let Some(m) = v.pointer("/error/message").and_then(Value::as_str) {
        return Err(m.to_string());
    }
    Ok(v)
}

fn clip(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() > TEXT_CAP {
        format!("{}…", s.chars().take(TEXT_CAP).collect::<String>())
    } else {
        s.to_string()
    }
}

fn format_langsearch(v: &Value) -> (String, usize) {
    let results = v.pointer("/data/webPages/value").and_then(Value::as_array).cloned().unwrap_or_default();
    if results.is_empty() {
        return ("No results.".into(), 0);
    }
    let mut out = String::new();
    for (i, r) in results.iter().enumerate() {
        let text = r["text"].as_str().or_else(|| r["snippet"].as_str()).unwrap_or("");
        out.push_str(&format!("{}. {}\n   {}\n", i + 1, r["name"].as_str().unwrap_or(""), r["url"].as_str().unwrap_or("")));
        if let Some(d) = r["datePublished"].as_str() {
            out.push_str(&format!("   Published {d}\n"));
        }
        out.push_str(&format!("   {}\n\n", clip(text).replace('\n', "\n   ")));
    }
    (out, results.len())
}

fn format_openrouter(v: &Value) -> (String, usize) {
    let msg = &v["choices"][0]["message"];
    let mut out = msg["content"].as_str().unwrap_or("").trim().to_string();
    let sources: Vec<&Value> = msg["annotations"].as_array().into_iter().flatten().filter(|a| a["type"] == "url_citation").map(|a| &a["url_citation"]).collect();
    if !sources.is_empty() {
        out.push_str("\n\nSources:\n");
        for (i, s) in sources.iter().enumerate() {
            out.push_str(&format!("{}. {}\n   {}\n", i + 1, s["title"].as_str().unwrap_or(""), s["url"].as_str().unwrap_or("")));
            if let Some(c) = s["content"].as_str().filter(|c| !c.trim().is_empty()) {
                out.push_str(&format!("   {}\n", clip(c).replace('\n', "\n   ")));
            }
        }
    }
    if out.is_empty() {
        out = "No results.".into();
    }
    (out, sources.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_results() {
        let v = json!({ "code": 200, "data": { "webPages": { "value": [
            { "name": "NodeSource distributions", "url": "https://github.com/nodesource/distributions", "text": "curl -fsSL https://deb.nodesource.com/gpgkey/nodesource-repo.gpg.key", "datePublished": "2026-01-02" },
            { "name": "No text", "url": "https://x", "snippet": "short" },
        ]}}});
        let (s, n) = format_langsearch(&v);
        assert_eq!(n, 2);
        assert!(s.contains("1. NodeSource distributions\n   https://github.com/nodesource/distributions\n   Published 2026-01-02"));
        assert!(s.contains("nodesource-repo.gpg.key") && s.contains("short"));
        assert_eq!(format_langsearch(&json!({ "data": {} })).1, 0);

        let v = json!({ "choices": [{ "message": { "content": "Use the deb822 source.", "annotations": [
            { "type": "url_citation", "url_citation": { "url": "https://deb.nodesource.com", "title": "NodeSource", "content": "Key at /gpgkey/nodesource-repo.gpg.key" } },
        ]}}]});
        let (s, n) = format_openrouter(&v);
        assert_eq!(n, 1);
        assert!(s.starts_with("Use the deb822 source.\n\nSources:\n1. NodeSource\n   https://deb.nodesource.com\n   Key at"));
    }

    #[test]
    #[ignore = "needs IMAGESMITH_LANGSEARCH_KEY and/or IMAGESMITH_OPENROUTER_KEY"]
    fn live_search() {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let mut backends = Vec::new();
        if let Some(key) = env("IMAGESMITH_LANGSEARCH_KEY") {
            backends.push(Backend::LangSearch { key });
        }
        if let Some(key) = env("IMAGESMITH_OPENROUTER_KEY") {
            backends.push(Backend::OpenRouter { key, model: "openai/gpt-4.1-mini".into() });
        }
        assert!(!backends.is_empty());
        let mut failures = Vec::new();
        for b in backends {
            match search(&b, "nodesource distributions deb repository", 5, &["github.com".into(), "nodesource.com".into()]) {
                Ok((text, n)) if n > 0 && text.to_lowercase().contains("nodesource") => eprintln!("{} ok: {n} results", b.label()),
                Ok((text, n)) => failures.push(format!("{}: {n} results without nodesource:\n{text}", b.label())),
                Err(e) => failures.push(format!("{}: {e}", b.label())),
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    }

    #[test]
    fn backend_needs_a_key() {
        let mut s = Settings { web_search: WebSearch::LangSearch, ..Settings::default() };
        assert!(backend(&s).is_none());
        s.langsearch_api_key = Some(" sk-x ".into());
        assert!(matches!(backend(&s), Some(Backend::LangSearch { key }) if key == "sk-x"));
        s.web_search = WebSearch::OpenRouter;
        assert!(backend(&s).is_none());
        s.openrouter_api_key = Some("or".into());
        assert!(matches!(backend(&s), Some(Backend::OpenRouter { .. })));
    }
}
