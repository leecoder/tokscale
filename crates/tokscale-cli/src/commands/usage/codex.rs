use anyhow::Result;
use chrono::{TimeZone, Utc};
use serde::Deserialize;

use super::helpers::capitalize;
use super::{UsageMetric, UsageOutput};

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

#[derive(Debug, Deserialize)]
struct Auth {
    tokens: Option<Tokens>,
}

#[derive(Debug, Deserialize)]
struct Tokens {
    access_token: Option<String>,
    refresh_token: Option<String>,
    account_id: Option<String>,
    id_token: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct Usage {
    email: Option<String>,
    plan_type: Option<String>,
    rate_limit: Option<RateLimit>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct RateLimit {
    primary_window: Option<Window>,
    secondary_window: Option<Window>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct Window {
    used_percent: Option<i64>,
    reset_at: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct Refresh {
    access_token: Option<String>,
    refresh_token: Option<String>,
    #[allow(dead_code)]
    expires_in: Option<i64>,
}

#[derive(Debug, Clone)]
enum CredentialSource {
    File(std::path::PathBuf),
    Keychain,
}

fn read_credentials() -> Result<(Auth, CredentialSource)> {
    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    let mut paths: Vec<std::path::PathBuf> = Vec::new();

    // CODEX_HOME takes precedence
    if let Ok(codex_home) = std::env::var("CODEX_HOME") {
        paths.push(std::path::PathBuf::from(codex_home).join("auth.json"));
    }
    paths.push(home.join(".config").join("codex").join("auth.json"));
    paths.push(home.join(".codex").join("auth.json"));

    for p in &paths {
        if p.exists() {
            let content = std::fs::read_to_string(p)?;
            if let Ok(auth) = serde_json::from_str::<Auth>(&content) {
                // Only accept if tokens contains a usable access_token
                if auth
                    .tokens
                    .as_ref()
                    .and_then(|t| t.access_token.as_ref())
                    .is_some()
                {
                    return Ok((auth, CredentialSource::File(p.clone())));
                }
            }
        }
    }

    // macOS keychain fallback
    if let Ok(raw) = super::helpers::read_keychain("Codex Auth") {
        if let Ok(auth) = serde_json::from_str::<Auth>(&raw) {
            if auth
                .tokens
                .as_ref()
                .and_then(|t| t.access_token.as_ref())
                .is_some()
            {
                return Ok((auth, CredentialSource::Keychain));
            }
        }
    }

    anyhow::bail!("No Codex credentials found. Run 'codex' to log in.")
}

fn save_credentials(
    path: &std::path::Path,
    access_token: &str,
    refresh_token: &str,
    account_id: Option<&str>,
    id_token: Option<&str>,
) {
    let mut tokens = serde_json::json!({
        "access_token": access_token,
        "refresh_token": refresh_token,
    });
    if let Some(aid) = account_id {
        tokens["account_id"] = serde_json::Value::String(aid.to_string());
    }
    if let Some(it) = id_token {
        tokens["id_token"] = serde_json::Value::String(it.to_string());
    }
    let json = serde_json::json!({
        "tokens": tokens,
        "last_refresh": chrono::Utc::now().to_rfc3339(),
    });
    let content = match serde_json::to_string_pretty(&json) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("warning: failed to serialize Codex credentials: {e}");
            return;
        }
    };
    if let Err(e) = super::helpers::atomic_write_secret(path, content.as_bytes()) {
        eprintln!("warning: failed to save Codex credentials: {e}");
    }
}

pub fn has_credentials() -> bool {
    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    if let Ok(codex_home) = std::env::var("CODEX_HOME") {
        if std::path::PathBuf::from(codex_home)
            .join("auth.json")
            .exists()
        {
            return true;
        }
    }
    if home
        .join(".config")
        .join("codex")
        .join("auth.json")
        .exists()
    {
        return true;
    }
    if home.join(".codex").join("auth.json").exists() {
        return true;
    }
    super::helpers::read_keychain("Codex Auth").is_ok()
}

async fn refresh_token(client: &reqwest::Client, rt: &str) -> Result<Refresh> {
    let resp = client
        .post("https://auth.openai.com/oauth/token")
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("refresh_token", rt),
        ])
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("Codex token refresh failed (HTTP {})", resp.status());
    }
    Ok(resp.json().await?)
}

async fn fetch_usage(
    client: &reqwest::Client,
    token: &str,
    account_id: Option<&str>,
) -> Result<Usage> {
    let mut req = client
        .get("https://chatgpt.com/backend-api/wham/usage")
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/json")
        .header(
            "User-Agent",
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)",
        );
    if let Some(id) = account_id {
        req = req.header("ChatGPT-Account-Id", id);
    }
    let resp = req.send().await?;
    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        anyhow::bail!("NEEDS_AUTH");
    }
    if !status.is_success() {
        anyhow::bail!("Codex usage request failed (HTTP {status})");
    }
    let body = resp.text().await?;
    if body.trim().starts_with('<') {
        anyhow::bail!("NEEDS_AUTH");
    }
    Ok(serde_json::from_str(&body)?)
}

pub fn fetch() -> Result<UsageOutput> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let (auth, source) = read_credentials()?;
        let tokens = auth
            .tokens
            .ok_or_else(|| anyhow::anyhow!("No Codex tokens."))?;
        let access_token = tokens
            .access_token
            .clone()
            .ok_or_else(|| anyhow::anyhow!("No Codex access token."))?;
        let account_id = tokens.account_id.as_deref();

        let client = reqwest::Client::new();
        let resp = match fetch_usage(&client, &access_token, account_id).await {
            Ok(r) => r,
            Err(e) if e.to_string().contains("NEEDS_AUTH") => {
                let rt_str = tokens
                    .refresh_token
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("No refresh token."))?;
                let refreshed = refresh_token(&client, rt_str).await?;
                let new = refreshed
                    .access_token
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("Refresh returned no token."))?;
                if let CredentialSource::File(ref path) = source {
                    let new_rt = refreshed
                        .refresh_token
                        .as_deref()
                        .unwrap_or_else(|| tokens.refresh_token.as_deref().unwrap_or(""));
                    save_credentials(
                        path,
                        &new,
                        new_rt,
                        tokens.account_id.as_deref(),
                        tokens.id_token.as_deref(),
                    );
                }
                fetch_usage(&client, &new, account_id).await?
            }
            Err(e) => return Err(e),
        };

        let plan = resp.plan_type.as_deref().map(capitalize);
        let mut metrics = Vec::new();
        if let Some(ref rl) = resp.rate_limit {
            if let Some(ref w) = rl.primary_window {
                let pct = w.used_percent.unwrap_or(0).clamp(0, 100) as f64;
                metrics.push(UsageMetric {
                    label: "Session".into(),
                    used_percent: pct,
                    remaining_percent: 100.0 - pct,
                    remaining_label: None,
                    resets_at: w
                        .reset_at
                        .and_then(|ts| Utc.timestamp_opt(ts, 0).single())
                        .map(|dt| dt.to_rfc3339()),
                });
            }
            if let Some(ref w) = rl.secondary_window {
                let pct = w.used_percent.unwrap_or(0).clamp(0, 100) as f64;
                metrics.push(UsageMetric {
                    label: "Weekly".into(),
                    used_percent: pct,
                    remaining_percent: 100.0 - pct,
                    remaining_label: None,
                    resets_at: w
                        .reset_at
                        .and_then(|ts| Utc.timestamp_opt(ts, 0).single())
                        .map(|dt| dt.to_rfc3339()),
                });
            }
        }

        Ok(UsageOutput {
            provider: "Codex".into(),
            plan,
            email: resp.email,
            metrics,
        })
    })
}
