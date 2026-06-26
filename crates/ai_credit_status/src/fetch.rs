use std::sync::Arc;

use anyhow::{Context as _, Result};
use chrono::{Datelike, Utc};
use client::{Client, UserStore, zed_urls};
use copilot_chat::CopilotChat;
use futures::AsyncReadExt as _;
use gpui::{App, AsyncApp, Entity};
use http_client::{AsyncBody, HttpClient, Method, Request};
use language_model::{
    ANTHROPIC_PROVIDER_ID, LanguageModelProviderId, LanguageModelRegistry, OPEN_AI_PROVIDER_ID,
    ZED_CLOUD_PROVIDER_ID,
};

const COPILOT_CHAT_PROVIDER_ID: LanguageModelProviderId =
    LanguageModelProviderId::new("copilot_chat");
const OPENROUTER_PROVIDER_ID: LanguageModelProviderId = LanguageModelProviderId::new("openrouter");
const MISTRAL_PROVIDER_ID: LanguageModelProviderId = LanguageModelProviderId::new("mistral");
use project::DisableAiSettings;
use serde::Deserialize;
use serde_json::Value;
use settings::Settings;
use theme::ActiveTheme;

#[derive(Debug, Clone, PartialEq)]
pub struct CreditSnapshot {
    pub provider_label: String,
    pub used_ratio: f32,
    pub label: String,
    pub tooltip: String,
    pub uses_estimated_budget: bool,
    pub account_url: Option<String>,
}

pub fn active_provider_id(cx: &App) -> Option<LanguageModelProviderId> {
    if DisableAiSettings::get_global(cx).disable_ai {
        return None;
    }

    LanguageModelRegistry::read_global(cx)
        .default_model()
        .map(|model| model.model.provider_id())
}

fn active_provider_api_key(provider_id: &LanguageModelProviderId, cx: &App) -> Option<String> {
    let configured_model = LanguageModelRegistry::read_global(cx).default_model()?;
    if configured_model.model.provider_id() == *provider_id {
        configured_model.model.api_key(cx)
    } else {
        None
    }
}

pub async fn fetch_credit_snapshot(
    provider_id: LanguageModelProviderId,
    user_store: Entity<UserStore>,
    client: Arc<Client>,
    monthly_budget_usd: Option<f32>,
    cx: &AsyncApp,
) -> Result<CreditSnapshot> {
    if provider_id == ZED_CLOUD_PROVIDER_ID {
        return fetch_zed_hosted(user_store, client, cx).await;
    }

    if provider_id == COPILOT_CHAT_PROVIDER_ID {
        return fetch_copilot(client.http_client(), cx).await;
    }

    let api_key = cx.update(|cx| active_provider_api_key(&provider_id, cx));

    if provider_id == OPENROUTER_PROVIDER_ID {
        return fetch_openrouter(client.http_client(), api_key).await;
    }

    if provider_id == OPEN_AI_PROVIDER_ID {
        return fetch_openai(client.http_client(), monthly_budget_usd, api_key).await;
    }

    if provider_id == ANTHROPIC_PROVIDER_ID {
        return fetch_anthropic(client.http_client(), monthly_budget_usd, api_key).await;
    }

    if provider_id == MISTRAL_PROVIDER_ID {
        return fetch_mistral(client.http_client(), monthly_budget_usd, api_key).await;
    }

    anyhow::bail!("Unsupported provider: {}", provider_id)
}

fn zed_hosted_snapshot(usage: cloud_llm_client::TokenSpendUsage, cx: &App) -> CreditSnapshot {
    let limit = usage.limit_cents.max(1) as f32;
    let spent = usage.spent_cents as f32;
    CreditSnapshot {
        provider_label: "Zed Pro".to_string(),
        used_ratio: (spent / limit).clamp(0.0, 1.0),
        label: format!("${:.2} used", spent / 100.0),
        tooltip: format!(
            "Zed Pro token spend: ${:.2} of ${:.2} monthly limit",
            spent / 100.0,
            usage.limit_cents as f32 / 100.0
        ),
        uses_estimated_budget: false,
        account_url: Some(zed_urls::account_url(cx)),
    }
}

async fn fetch_zed_hosted(
    user_store: Entity<UserStore>,
    _client: Arc<Client>,
    cx: &AsyncApp,
) -> Result<CreditSnapshot> {
    if let Some(snapshot) = cx.update(|cx| {
        user_store
            .read(cx)
            .token_spend_usage()
            .map(|usage| zed_hosted_snapshot(usage, cx))
    }) {
        return Ok(snapshot);
    }

    let refresh =
        cx.update(|cx| user_store.update(cx, |store, cx| store.refresh_authenticated_user(cx)));
    refresh
        .await
        .context("failed to refresh Zed account usage")?;

    cx.update(|cx| {
        user_store
            .read(cx)
            .token_spend_usage()
            .map(|usage| zed_hosted_snapshot(usage, cx))
    })
    .ok_or_else(|| anyhow::anyhow!("Zed Pro token spend is not available from your account yet"))
}

async fn fetch_copilot(http: Arc<dyn HttpClient>, cx: &AsyncApp) -> Result<CreditSnapshot> {
    let oauth_token = cx
        .update(|cx| {
            CopilotChat::global(cx)
                .and_then(|chat| chat.read(cx).oauth_token().map(str::to_string))
                .or_else(resolve_github_token)
        })
        .ok_or_else(|| anyhow::anyhow!("Sign in to GitHub Copilot to view usage"))?;

    #[derive(Debug, Deserialize)]
    struct CopilotUserResponse {
        #[allow(dead_code)]
        copilot_plan: Option<String>,
        quota_reset_date: Option<String>,
        quota_snapshots: Option<QuotaSnapshots>,
    }

    #[derive(Debug, Deserialize)]
    struct QuotaSnapshots {
        premium_interactions: Option<QuotaSnapshot>,
    }

    #[derive(Debug, Deserialize)]
    struct QuotaSnapshot {
        percent_remaining: Option<f64>,
        quota_remaining: Option<f64>,
        entitlement: Option<f64>,
        unlimited: Option<bool>,
    }

    let request = Request::builder()
        .method(Method::GET)
        .uri("https://api.github.com/copilot_internal/user")
        .header("Authorization", format!("Bearer {oauth_token}"))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "zed-ai-credit-status")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .body(AsyncBody::default())?;

    let mut response = http.send(request).await?;
    let mut body = String::new();
    response.body_mut().read_to_string(&mut body).await?;

    if !response.status().is_success() {
        anyhow::bail!("GitHub Copilot usage request failed: {}", body);
    }

    let parsed: CopilotUserResponse = serde_json::from_str(&body)?;
    let premium = parsed
        .quota_snapshots
        .and_then(|snapshots| snapshots.premium_interactions)
        .context("No Copilot premium quota data available")?;

    if premium.unlimited.unwrap_or(false) {
        return Ok(CreditSnapshot {
            provider_label: "Copilot".to_string(),
            used_ratio: 0.0,
            label: "Premium included".to_string(),
            tooltip: "GitHub Copilot premium requests are included with your plan".to_string(),
            uses_estimated_budget: false,
            account_url: Some("https://github.com/settings/copilot".into()),
        });
    }

    let percent_remaining = premium.percent_remaining.unwrap_or(100.0) as f32;
    let used_ratio = ((100.0 - percent_remaining) / 100.0).clamp(0.0, 1.0);
    let usage_detail = match (premium.quota_remaining, premium.entitlement) {
        (Some(remaining), Some(total)) if total > 0.0 => {
            Some(format!(" ({:.0}/{:.0})", total - remaining, total))
        }
        _ => None,
    };
    let label = format!("{:.0}%", used_ratio * 100.0);

    let reset = parsed
        .quota_reset_date
        .map(|date| format!("\nResets {date}"))
        .unwrap_or_default();
    let usage_detail = usage_detail.unwrap_or_default();

    Ok(CreditSnapshot {
        provider_label: "Copilot".to_string(),
        used_ratio,
        label,
        tooltip: format!(
            "GitHub Copilot premium requests: {:.0}% used{usage_detail}{reset}",
            used_ratio * 100.0
        ),
        uses_estimated_budget: false,
        account_url: Some("https://github.com/settings/copilot".into()),
    })
}

async fn fetch_openrouter(
    http: Arc<dyn HttpClient>,
    api_key: Option<String>,
) -> Result<CreditSnapshot> {
    let api_key = api_key
        .or_else(|| std::env::var("OPENROUTER_API_KEY").ok())
        .filter(|key| !key.is_empty())
        .context("Configure OpenRouter API key in settings (or set OPENROUTER_API_KEY) to view OpenRouter credits")?;

    #[derive(Debug, Deserialize)]
    struct KeyResponse {
        data: KeyData,
    }

    #[derive(Debug, Deserialize)]
    struct KeyData {
        limit: Option<f64>,
        limit_remaining: Option<f64>,
        usage: f64,
    }

    let request = Request::builder()
        .method(Method::GET)
        .uri("https://openrouter.ai/api/v1/key")
        .header("Authorization", format!("Bearer {api_key}"))
        .body(AsyncBody::default())?;

    let mut response = http.send(request).await?;
    let mut body = String::new();
    response.body_mut().read_to_string(&mut body).await?;

    if !response.status().is_success() {
        anyhow::bail!("OpenRouter usage request failed: {}", body);
    }

    let parsed: KeyResponse = serde_json::from_str(&body)?;
    let data = parsed.data;

    if let (Some(limit), Some(remaining)) = (data.limit, data.limit_remaining) {
        if limit > 0.0 {
            let used = (limit - remaining).max(0.0);
            let used_ratio = (used / limit).clamp(0.0, 1.0) as f32;
            return Ok(CreditSnapshot {
                provider_label: "OpenRouter".to_string(),
                used_ratio,
                label: format!("${:.2} used", used),
                tooltip: format!(
                    "OpenRouter credits: ${:.2} used of ${:.2} limit (${:.2} total usage)",
                    used, limit, data.usage
                ),
                uses_estimated_budget: false,
                account_url: Some("https://openrouter.ai/settings/credits".into()),
            });
        }
    }

    Ok(CreditSnapshot {
        provider_label: "OpenRouter".to_string(),
        used_ratio: 0.0,
        label: format!("${:.2} used", data.usage),
        tooltip: format!("OpenRouter total usage: ${:.2}", data.usage),
        uses_estimated_budget: false,
        account_url: Some("https://openrouter.ai/settings/credits".into()),
    })
}

async fn fetch_openai(
    http: Arc<dyn HttpClient>,
    monthly_budget_usd: Option<f32>,
    api_key: Option<String>,
) -> Result<CreditSnapshot> {
    let provider_api_key = api_key
        .or_else(|| std::env::var("OPENAI_API_KEY").ok())
        .filter(|key| !key.is_empty())
        .context(
            "Configure OpenAI API key in settings (or set OPENAI_API_KEY) to view OpenAI usage",
        )?;
    let usage_api_key = std::env::var("OPENAI_ADMIN_API_KEY")
        .ok()
        .filter(|key| !key.is_empty())
        .unwrap_or_else(|| provider_api_key.clone());
    let budget = monthly_budget_usd.filter(|budget| *budget > 0.0);

    let now = Utc::now();
    let start = format!("{}-{:02}-01", now.year(), now.month());
    let end = format!("{}-{:02}-{:02}", now.year(), now.month(), now.day());
    let start_of_month = now
        .date_naive()
        .with_day(1)
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .map(|date_time| date_time.and_utc())
        .ok_or_else(|| {
            anyhow::anyhow!("failed to compute OpenAI usage start-of-month timestamp")
        })?;
    let start_time = start_of_month.timestamp();
    let end_time = now.timestamp();

    let candidate_uris = [
        format!(
            "https://api.openai.com/v1/organization/costs?start_time={start_time}&end_time={end_time}&bucket_width=1d"
        ),
        format!(
            "https://api.openai.com/v1/dashboard/billing/usage?start_date={start}&end_date={end}"
        ),
        format!("https://api.openai.com/v1/usage?start_date={start}&end_date={end}"),
        format!("https://api.openai.com/v1/usage?date={end}"),
    ];

    let mut errors = Vec::new();
    let mut spent_usd = None;
    for uri in candidate_uris {
        let request = Request::builder()
            .method(Method::GET)
            .uri(uri.clone())
            .header("Authorization", format!("Bearer {usage_api_key}"))
            .body(AsyncBody::default())?;

        let mut response = http.send(request).await?;
        let mut body = String::new();
        response.body_mut().read_to_string(&mut body).await?;

        if !response.status().is_success() {
            errors.push(format!("{uri}: {body}"));
            continue;
        }

        let parsed: Value = serde_json::from_str(&body)?;
        if let Some(parsed_spent_usd) = parse_openai_spent_usd(&parsed) {
            spent_usd = Some(parsed_spent_usd);
            break;
        }

        errors.push(format!("{uri}: unexpected response format"));
    }

    if let Some(spent_usd) = spent_usd {
        let used_ratio = budget
            .map(|budget| (spent_usd / budget as f64).clamp(0.0, 1.0) as f32)
            .unwrap_or(0.0);
        let tooltip = if let Some(budget) = budget {
            format!(
                "OpenAI usage this month: ${:.2} of ${:.2} configured budget",
                spent_usd, budget
            )
        } else {
            format!(
                "OpenAI usage this month: ${:.2}. Configure ai_credit_status.monthly_budget_usd to track percentage.",
                spent_usd
            )
        };

        return Ok(CreditSnapshot {
            provider_label: "OpenAI".to_string(),
            used_ratio,
            label: format!("${:.2} used this month", spent_usd),
            tooltip,
            uses_estimated_budget: false,
            account_url: Some("https://platform.openai.com/usage".into()),
        });
    }

    let usage_requires_session_key = errors
        .iter()
        .any(|error| error.to_ascii_lowercase().contains("session key"));

    let (label, tooltip, uses_estimated_budget) = if budget.is_some() {
        (
            "".to_string(),
            if usage_requires_session_key {
                "OpenAI usage endpoint rejected API-key access (requires browser session key). \
                 There is usually no account toggle to enable this endpoint for standard API keys. \
                 Tip: use an Organization Admin API key with OpenAI organization usage/cost APIs for programmatic access. \
                 Check https://platform.openai.com/usage for actual spend."
                    .to_string()
            } else {
                "OpenAI usage endpoint did not return parsable data. \
                 Check https://platform.openai.com/usage for actual spend."
                    .to_string()
            },
            true,
        )
    } else {
        (
            "Usage unavailable".to_string(),
            if usage_requires_session_key {
                "OpenAI usage endpoint rejected API-key access (requires browser session key). There is usually no account toggle to enable this endpoint for standard API keys. Tip: use an Organization Admin API key with OpenAI organization usage/cost APIs for programmatic access. Open https://platform.openai.com/usage for actual spend, or configure ai_credit_status.monthly_budget_usd for an estimated monthly label.".to_string()
            } else {
                format!(
                    "OpenAI usage endpoint did not return parsable data. Latest errors: {}",
                    errors.join(" | ")
                )
            },
            false,
        )
    };

    Ok(CreditSnapshot {
        provider_label: "OpenAI".to_string(),
        used_ratio: 0.0,
        label,
        tooltip,
        uses_estimated_budget,
        account_url: Some("https://platform.openai.com/usage".into()),
    })
}

fn parse_openai_spent_usd(parsed: &Value) -> Option<f64> {
    if let Some(total_usage_cents) = parsed.get("total_usage").and_then(Value::as_f64) {
        return Some(total_usage_cents / 100.0);
    }

    let mut total_cost = 0.0;
    let mut found_cost = false;
    if let Some(data) = parsed.get("data").and_then(Value::as_array) {
        for bucket in data {
            if let Some(results) = bucket.get("results").and_then(Value::as_array) {
                for result in results {
                    if let Some(cost_value) = result
                        .get("amount")
                        .and_then(|amount| amount.get("value"))
                        .and_then(Value::as_f64)
                    {
                        total_cost += cost_value;
                        found_cost = true;
                    }
                }
            }
        }
    }

    if found_cost { Some(total_cost) } else { None }
}

async fn fetch_anthropic(
    http: Arc<dyn HttpClient>,
    monthly_budget_usd: Option<f32>,
    api_key: Option<String>,
) -> Result<CreditSnapshot> {
    let api_key = api_key
        .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())
        .filter(|key| !key.is_empty())
        .context("Configure Anthropic API key in settings (or set ANTHROPIC_API_KEY) to view Anthropic usage")?;
    let budget = monthly_budget_usd.filter(|budget| *budget > 0.0);

    let request = Request::builder()
        .method(Method::GET)
        .uri("https://api.anthropic.com/v1/models")
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .body(AsyncBody::default())?;

    let mut response = http.send(request).await?;
    if !response.status().is_success() {
        let mut body = String::new();
        response.body_mut().read_to_string(&mut body).await?;
        anyhow::bail!("Anthropic request failed: {}", body);
    }

    let (label, tooltip, uses_estimated_budget) = if budget.is_some() {
        (
            "".to_string(),
            "Anthropic does not expose remaining credits via API. \
             Check https://console.anthropic.com/settings/billing for actual spend."
                .to_string(),
            true,
        )
    } else {
        (
            "Usage unavailable".to_string(),
            "Anthropic does not expose remaining credits via API. Configure ai_credit_status.monthly_budget_usd to show an estimated monthly spend label, or check https://console.anthropic.com/settings/billing for actual spend.".to_string(),
            false,
        )
    };

    Ok(CreditSnapshot {
        provider_label: "Anthropic".to_string(),
        used_ratio: 0.0,
        label,
        tooltip,
        uses_estimated_budget,
        account_url: Some("https://console.anthropic.com/settings/billing".into()),
    })
}

async fn fetch_mistral(
    http: Arc<dyn HttpClient>,
    monthly_budget_usd: Option<f32>,
    api_key: Option<String>,
) -> Result<CreditSnapshot> {
    let api_key = api_key
        .or_else(|| std::env::var("MISTRAL_API_KEY").ok())
        .filter(|key| !key.is_empty())
        .context(
            "Configure Mistral API key in settings (or set MISTRAL_API_KEY) to view Mistral usage",
        )?;
    let budget = monthly_budget_usd.filter(|budget| *budget > 0.0);

    let request = Request::builder()
        .method(Method::GET)
        .uri("https://api.mistral.ai/v1/models")
        .header("Authorization", format!("Bearer {api_key}"))
        .body(AsyncBody::default())?;

    let mut response = http.send(request).await?;
    if !response.status().is_success() {
        let mut body = String::new();
        response.body_mut().read_to_string(&mut body).await?;
        anyhow::bail!("Mistral request failed: {}", body);
    }

    let (label, tooltip, uses_estimated_budget) = if budget.is_some() {
        (
            "".to_string(),
            "Mistral does not expose remaining credits via API. \
             Check https://console.mistral.ai/billing for actual spend."
                .to_string(),
            true,
        )
    } else {
        (
            "Usage unavailable".to_string(),
            "Mistral does not expose remaining credits via API. Configure ai_credit_status.monthly_budget_usd to show an estimated monthly spend label, or check https://console.mistral.ai/billing for actual spend.".to_string(),
            false,
        )
    };

    Ok(CreditSnapshot {
        provider_label: "Mistral".to_string(),
        used_ratio: 0.0,
        label,
        tooltip,
        uses_estimated_budget,
        account_url: Some("https://console.mistral.ai/billing".into()),
    })
}

fn resolve_github_token() -> Option<String> {
    for key in [
        "GITHUB_TOKEN",
        "COPILOT_USAGE_TOKEN",
        copilot_chat::COPILOT_OAUTH_ENV_VAR,
        copilot_chat::GITHUB_COPILOT_OAUTH_ENV_VAR,
        "GH_TOKEN",
    ] {
        if let Ok(token) = std::env::var(key) {
            let token = token.trim();
            if !token.is_empty() {
                return Some(token.to_string());
            }
        }
    }
    None
}

pub fn usage_color(used_ratio: f32, cx: &App) -> gpui::Hsla {
    let ratio = used_ratio.clamp(0.0, 1.0);
    let status = cx.theme().status();
    if ratio < 0.25 {
        status.success
    } else if ratio < 0.50 {
        status.warning
    } else if ratio < 0.75 {
        status.modified
    } else {
        status.error
    }
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;

    use super::*;

    #[gpui::test]
    fn usage_color_escalates_with_ratio(cx: &mut TestAppContext) {
        cx.update(|cx| {
            assert_eq!(usage_color(0.1, cx), cx.theme().status().success);
            assert_eq!(usage_color(0.4, cx), cx.theme().status().warning);
            assert_eq!(usage_color(0.6, cx), cx.theme().status().modified);
            assert_eq!(usage_color(0.9, cx), cx.theme().status().error);
        });
    }
}
