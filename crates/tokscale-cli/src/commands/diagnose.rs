use anyhow::Result;
use chrono::{self, Timelike};
use colored::Colorize;
use std::collections::HashMap;
use tokscale_core::wiki::{WikiDb, WikiEntry};

pub struct DiagnoseOptions {
    pub json: bool,
    pub since: Option<String>,
    pub until: Option<String>,
    pub workspace: Option<String>,
    pub client: Option<String>,
    pub home_dir: Option<String>,
    pub scanner_settings: tokscale_core::scanner::ScannerSettings,
}

#[derive(serde::Serialize)]
struct AdvisorReport {
    current_period: PeriodSummary,
    previous_period: Option<PeriodSummary>,
    deltas: Option<PeriodDelta>,
    recommendations: Vec<Recommendation>,
}

#[derive(serde::Serialize, Clone)]
struct PeriodSummary {
    sessions: usize,
    total_cost: f64,
    total_tokens: i64,
    avg_cost_per_session: f64,
    cache_reuse_rate: f64,
    input_tokens_per_message: f64,
    top_model: Option<String>,
    top_model_share: f64,
}

#[derive(serde::Serialize)]
struct PeriodDelta {
    cost_change: f64,
    cost_change_pct: f64,
    sessions_change: i64,
    cache_rate_change_pp: f64,
    main_driver: String,
}

#[derive(serde::Serialize)]
struct Recommendation {
    id: String,
    severity: String,
    confidence: String,
    title: String,
    evidence: Vec<String>,
    action: String,
    estimated_savings: Option<f64>,
}

const MAX_VALID_DURATION_MINUTES: i64 = 1440;

pub fn run_diagnose(opts: DiagnoseOptions) -> Result<()> {
    let wiki_path = WikiDb::default_path();
    let db = WikiDb::open(&wiki_path)
        .map_err(|e| anyhow::anyhow!("Failed to open wiki DB: {}", e))?;

    super::report::populate_wiki_from_sessions_with_opts(
        &db,
        opts.home_dir.as_deref(),
        &opts.scanner_settings,
    )?;

    let (since_ts, until_ts) = resolve_trailing_7d_window(&opts);

    let current_entries: Vec<WikiEntry> = db
        .query_entries(
            since_ts,
            until_ts,
            opts.workspace.as_deref(),
            opts.client.as_deref(),
        )
        .map_err(|e| anyhow::anyhow!("{}", e))?
        .into_iter()
        .filter(|e| e.duration_minutes <= MAX_VALID_DURATION_MINUTES)
        .collect();

    if current_entries.is_empty() {
        println!("No sessions found for the given filters.");
        return Ok(());
    }

    let previous_entries = compute_previous_period(&db, &current_entries, &opts)?;

    let current_summary = compute_period_summary(&current_entries);
    let previous_summary = previous_entries
        .as_ref()
        .filter(|e| !e.is_empty())
        .map(|e| compute_period_summary(e));

    let deltas = previous_summary
        .as_ref()
        .map(|prev| compute_deltas(&current_summary, prev, &current_entries, previous_entries.as_deref().unwrap_or(&[])));

    let mut recommendations = Vec::new();
    rule_cache_miss(&current_entries, &mut recommendations);
    rule_premium_overuse(&current_entries, &current_summary, &mut recommendations);
    rule_spend_spike(&current_summary, &previous_summary, &mut recommendations);
    rule_runaway_sessions(&current_entries, &current_summary, &mut recommendations);
    rule_idle_context_bloat(&current_entries, &mut recommendations);
    rule_model_diversity_low(&current_entries, &current_summary, &mut recommendations);
    rule_short_session_churn(&current_entries, &mut recommendations);
    rule_night_owl_premium(&current_entries, &mut recommendations);
    rule_workspace_concentration(&current_entries, &current_summary, &mut recommendations);
    rule_output_waste(&current_entries, &mut recommendations);
    rule_token_efficiency_decline(&current_entries, previous_entries.as_deref(), &mut recommendations);
    rule_cost_per_task_regression(&current_entries, previous_entries.as_deref(), &mut recommendations);

    recommendations.sort_by(|a, b| {
        let sev = |s: &str| match s { "high" => 0, "medium" => 1, _ => 2 };
        sev(&a.severity).cmp(&sev(&b.severity))
            .then_with(|| {
                let sa = a.estimated_savings.unwrap_or(0.0);
                let sb = b.estimated_savings.unwrap_or(0.0);
                sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
            })
    });

    let report = AdvisorReport {
        current_period: current_summary,
        previous_period: previous_summary,
        deltas,
        recommendations,
    };

    if opts.json {
        let json = serde_json::to_string_pretty(&report)?;
        println!("{}", json);
    } else {
        print_advisor_report(&report);
    }

    Ok(())
}

fn resolve_trailing_7d_window(opts: &DiagnoseOptions) -> (Option<i64>, Option<i64>) {
    let (explicit_since, explicit_until) = super::report::parse_date_range(&opts.since, &opts.until);

    if explicit_since.is_some() || explicit_until.is_some() {
        return (explicit_since, explicit_until);
    }

    let now = chrono::Utc::now().timestamp();
    let seven_days_ago = now - 7 * 86400;
    (Some(seven_days_ago), Some(now))
}

fn compute_previous_period(
    db: &WikiDb,
    current_entries: &[WikiEntry],
    opts: &DiagnoseOptions,
) -> Result<Option<Vec<WikiEntry>>> {
    let min_ts = current_entries.iter().map(|e| e.created_at).min().unwrap_or(0);
    let max_ts = current_entries.iter().map(|e| e.created_at).max().unwrap_or(0);
    let span = max_ts - min_ts;

    if span <= 0 {
        return Ok(None);
    }

    let prev_since = min_ts - span;
    let prev_until = min_ts - 1;

    let entries: Vec<WikiEntry> = db
        .query_entries(
            Some(prev_since),
            Some(prev_until),
            opts.workspace.as_deref(),
            opts.client.as_deref(),
        )
        .map_err(|e| anyhow::anyhow!("{}", e))?
        .into_iter()
        .filter(|e| e.duration_minutes <= MAX_VALID_DURATION_MINUTES)
        .collect();

    Ok(Some(entries))
}

fn compute_period_summary(entries: &[WikiEntry]) -> PeriodSummary {
    let sessions = entries.len();
    let total_cost: f64 = entries.iter().map(|e| e.total_cost).sum();
    let total_tokens: i64 = entries
        .iter()
        .map(|e| e.total_input_tokens + e.total_output_tokens)
        .sum();
    let total_input: i64 = entries.iter().map(|e| e.total_input_tokens).sum();
    let total_cache_read: i64 = entries.iter().map(|e| e.total_cache_read).sum();
    let total_messages: i64 = entries.iter().map(|e| e.message_count as i64).sum();

    let cache_reuse_rate = if total_input + total_cache_read > 0 {
        total_cache_read as f64 / (total_input + total_cache_read) as f64
    } else {
        0.0
    };

    let input_tokens_per_message = if total_messages > 0 {
        total_input as f64 / total_messages as f64
    } else {
        0.0
    };

    let mut model_costs: HashMap<String, f64> = HashMap::new();
    for e in entries {
        let share = e.total_cost / e.models_used.len().max(1) as f64;
        for m in &e.models_used {
            *model_costs.entry(m.clone()).or_default() += share;
        }
    }
    let top_model = model_costs
        .iter()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
        .map(|(m, _)| m.clone());
    let top_model_share = top_model
        .as_ref()
        .and_then(|m| model_costs.get(m))
        .copied()
        .unwrap_or(0.0)
        / total_cost.max(0.001);

    PeriodSummary {
        sessions,
        total_cost,
        total_tokens,
        avg_cost_per_session: if sessions > 0 { total_cost / sessions as f64 } else { 0.0 },
        cache_reuse_rate,
        input_tokens_per_message,
        top_model,
        top_model_share,
    }
}

fn compute_deltas(
    current: &PeriodSummary,
    previous: &PeriodSummary,
    current_entries: &[WikiEntry],
    previous_entries: &[WikiEntry],
) -> PeriodDelta {
    let cost_change = current.total_cost - previous.total_cost;
    let cost_change_pct = if previous.total_cost > 0.0 {
        cost_change / previous.total_cost * 100.0
    } else {
        0.0
    };
    let sessions_change = current.sessions as i64 - previous.sessions as i64;
    let cache_rate_change_pp = (current.cache_reuse_rate - previous.cache_reuse_rate) * 100.0;

    let main_driver = find_main_cost_driver(current_entries, previous_entries);

    PeriodDelta {
        cost_change,
        cost_change_pct,
        sessions_change,
        cache_rate_change_pp,
        main_driver,
    }
}

fn find_main_cost_driver(current: &[WikiEntry], previous: &[WikiEntry]) -> String {
    let mut cur_model_cost: HashMap<&str, f64> = HashMap::new();
    let mut prev_model_cost: HashMap<&str, f64> = HashMap::new();

    for e in current {
        let share = e.total_cost / e.models_used.len().max(1) as f64;
        for m in &e.models_used {
            *cur_model_cost.entry(m.as_str()).or_default() += share;
        }
    }
    for e in previous {
        let share = e.total_cost / e.models_used.len().max(1) as f64;
        for m in &e.models_used {
            *prev_model_cost.entry(m.as_str()).or_default() += share;
        }
    }

    let mut max_delta = 0.0f64;
    let mut driver = String::from("mixed");

    for (model, cur_cost) in &cur_model_cost {
        let prev_cost = prev_model_cost.get(model).copied().unwrap_or(0.0);
        let delta = cur_cost - prev_cost;
        if delta.abs() > max_delta.abs() {
            max_delta = delta;
            driver = format!("{} ({:+.2})", model, delta);
        }
    }

    driver
}

/// Triggers when model/workspace has high input, low cache reuse, and significant spend
fn rule_cache_miss(entries: &[WikiEntry], recs: &mut Vec<Recommendation>) {
    let mut by_model: HashMap<&str, (i64, i64, f64)> = HashMap::new();

    for e in entries {
        for m in &e.models_used {
            let entry = by_model.entry(m.as_str()).or_default();
            let share = 1.0 / e.models_used.len().max(1) as f64;
            entry.0 += (e.total_input_tokens as f64 * share) as i64;
            entry.1 += (e.total_cache_read as f64 * share) as i64;
            entry.2 += e.total_cost * share;
        }
    }

    for (model, (input, cache_read, cost)) in &by_model {
        let total_context = *input + *cache_read;
        if total_context < 100_000 || *cost < 3.0 {
            continue;
        }

        let reuse_rate = *cache_read as f64 / total_context as f64;
        if reuse_rate < 0.10 {
            let estimated_savings = cost * 0.4;

            recs.push(Recommendation {
                id: "cache_miss_expensive".to_string(),
                severity: "high".to_string(),
                confidence: "high".to_string(),
                title: format!("Cache reuse near zero for {}", model),
                evidence: vec![
                    format!("{}k input tokens processed, cache reuse rate: {:.0}%", total_context / 1000, reuse_rate * 100.0),
                    format!("Spend on this model: ${:.2}", cost),
                    format!("With 50% cache reuse, input costs would drop ~40%"),
                ],
                action: "Enable prompt caching, use session continuation, or reduce repeated repository context. \
                         Check if your client supports cache_control headers."
                    .to_string(),
                estimated_savings: Some(estimated_savings),
            });
        }
    }
}

/// Triggers when >60% of spend is on expensive models for config/research/review/other tasks
fn rule_premium_overuse(entries: &[WikiEntry], summary: &PeriodSummary, recs: &mut Vec<Recommendation>) {
    let low_risk_categories = ["config", "research", "review", "other", "docs"];

    let mut model_token_cost: HashMap<&str, (f64, i64)> = HashMap::new();
    for e in entries {
        let share = 1.0 / e.models_used.len().max(1) as f64;
        for m in &e.models_used {
            let entry = model_token_cost.entry(m.as_str()).or_default();
            entry.0 += e.total_cost * share;
            entry.1 += ((e.total_input_tokens + e.total_output_tokens) as f64 * share) as i64;
        }
    }

    let mut models_by_efficiency: Vec<(&str, f64)> = model_token_cost
        .iter()
        .filter(|(_, (_, tokens))| *tokens > 10_000)
        .map(|(m, (cost, tokens))| (*m, *cost / (*tokens as f64 / 1_000_000.0)))
        .collect();
    models_by_efficiency.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

    if models_by_efficiency.is_empty() {
        return;
    }

    let premium_model = models_by_efficiency
        .iter()
        .find(|(m, _)| {
            model_token_cost.get(m).map(|(c, _)| *c).unwrap_or(0.0) / summary.total_cost.max(0.001) > 0.4
        });

    let cheap_model = models_by_efficiency.last();

    if let (Some((premium, premium_rate)), Some((cheap, cheap_rate))) = (premium_model, cheap_model) {
        if premium == cheap {
            return;
        }

        // Count low-risk sessions on premium model
        let low_risk_on_premium: Vec<&WikiEntry> = entries
            .iter()
            .filter(|e| {
                let cat = e.task_category.as_deref().unwrap_or("other");
                low_risk_categories.contains(&cat) && e.models_used.iter().any(|m| m.as_str() == *premium)
            })
            .collect();

        let low_risk_cost: f64 = low_risk_on_premium.iter().map(|e| e.total_cost).sum();
        let low_risk_share = low_risk_cost / summary.total_cost.max(0.001);

        if low_risk_cost >= 3.0 && (low_risk_share > 0.15 || low_risk_on_premium.len() >= 5) {
            // Estimate savings: difference in $/M tokens * tokens used
            let low_risk_tokens: i64 = low_risk_on_premium
                .iter()
                .map(|e| e.total_input_tokens + e.total_output_tokens)
                .sum();
            let savings_rate = (premium_rate - cheap_rate).max(0.0);
            let estimated_savings = savings_rate * (low_risk_tokens as f64 / 1_000_000.0) * 0.5; // conservative 50% migration

            let categories_used: Vec<&str> = low_risk_on_premium
                .iter()
                .filter_map(|e| e.task_category.as_deref())
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .collect();

            recs.push(Recommendation {
                id: "premium_overuse_low_risk".to_string(),
                severity: "medium".to_string(),
                confidence: "medium".to_string(),
                title: format!("Route {}/{} tasks away from {}", categories_used.join("/"), "", premium),
                evidence: vec![
                    format!("{} low-risk sessions used {} (${:.2} total)", low_risk_on_premium.len(), premium, low_risk_cost),
                    format!("{} costs ${:.1}/M tokens vs {} at ${:.1}/M tokens", premium, premium_rate, cheap, cheap_rate),
                    format!("Categories: {}", categories_used.join(", ")),
                ],
                action: format!(
                    "Use {} as first-pass model for {} tasks. Escalate to {} only when stuck or for complex architecture work.",
                    cheap, categories_used.join("/"), premium
                ),
                estimated_savings: Some(estimated_savings),
            });
        }
    }
}

/// Rule 3: Weekly spend spike
/// Triggers when current period spend is >50% above previous and absolute increase >$5
fn rule_spend_spike(
    current: &PeriodSummary,
    previous: &Option<PeriodSummary>,
    recs: &mut Vec<Recommendation>,
) {
    let prev = match previous {
        Some(p) if p.total_cost > 0.0 => p,
        _ => return,
    };

    let increase = current.total_cost - prev.total_cost;
    let increase_pct = increase / prev.total_cost;

    if increase > 5.0 && increase_pct > 0.5 {
        recs.push(Recommendation {
            id: "spend_spike".to_string(),
            severity: "high".to_string(),
            confidence: "high".to_string(),
            title: "Significant spend increase vs previous period".to_string(),
            evidence: vec![
                format!("Current: ${:.2} vs Previous: ${:.2} ({:+.0}%)", current.total_cost, prev.total_cost, increase_pct * 100.0),
                format!("Absolute increase: ${:.2}", increase),
                format!(
                    "Sessions: {} vs {} ({:+})",
                    current.sessions, prev.sessions, current.sessions as i64 - prev.sessions as i64
                ),
            ],
            action: "Review the 'What Changed' section below to identify the main cost driver. \
                     Consider whether the increase reflects productive work or inefficiency."
                .to_string(),
            estimated_savings: None, // Can't estimate without knowing if it's waste
        });
    }
}

/// Rule 4: Runaway session anomaly
/// Triggers for sessions costing >3x the median
fn rule_runaway_sessions(entries: &[WikiEntry], summary: &PeriodSummary, recs: &mut Vec<Recommendation>) {
    if entries.len() < 5 {
        return;
    }

    let mut costs: Vec<f64> = entries.iter().map(|e| e.total_cost).collect();
    costs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = costs[costs.len() / 2];
    let threshold = (median * 3.0).max(1.0);

    let runaways: Vec<&WikiEntry> = entries
        .iter()
        .filter(|e| e.total_cost > threshold)
        .collect();

    if runaways.is_empty() {
        return;
    }

    let runaway_cost: f64 = runaways.iter().map(|e| e.total_cost).sum();
    let runaway_share = runaway_cost / summary.total_cost.max(0.001);

    // Only recommend if runaways are a meaningful share
    if runaway_share < 0.1 && runaways.len() < 3 {
        return;
    }

    let top_runaways: Vec<String> = runaways
        .iter()
        .take(3)
        .map(|e| {
            format!(
                "${:.2} / {}msg / {} ({})",
                e.total_cost,
                e.message_count,
                e.models_used.first().map(|s| s.as_str()).unwrap_or("?"),
                e.task_category.as_deref().unwrap_or("?"),
            )
        })
        .collect();

    recs.push(Recommendation {
        id: "runaway_sessions".to_string(),
        severity: "medium".to_string(),
        confidence: "high".to_string(),
        title: format!("{} session(s) cost >3x the median (${:.2})", runaways.len(), median),
        evidence: vec![
            format!("Median session cost: ${:.2}, threshold: ${:.2}", median, threshold),
            format!("Runaway sessions total: ${:.2} ({:.0}% of period spend)", runaway_cost, runaway_share * 100.0),
            format!("Top sessions: {}", top_runaways.join(" | ")),
        ],
        action: "For similar work, consider splitting into smaller sessions (plan → implement → review). \
                 Long sessions accumulate context that inflates input costs."
            .to_string(),
        estimated_savings: Some(runaway_cost * 0.2), // Conservative: 20% reducible
    });
}

/// Rule 5: Idle context bloat
/// Triggers when input tokens/message is very high but output/input ratio is low,
/// suggesting the model is receiving large repeated context without producing proportional output.
fn rule_idle_context_bloat(entries: &[WikiEntry], recs: &mut Vec<Recommendation>) {
    if entries.len() < 3 {
        return;
    }

    let total_input: i64 = entries.iter().map(|e| e.total_input_tokens).sum();
    let total_output: i64 = entries.iter().map(|e| e.total_output_tokens).sum();
    let total_messages: i64 = entries.iter().map(|e| e.message_count as i64).sum();

    if total_messages == 0 || total_input == 0 {
        return;
    }

    let input_per_msg = total_input as f64 / total_messages as f64;
    let output_input_ratio = total_output as f64 / total_input as f64;

    // High input per message (>50k) and low output ratio (<0.15)
    if input_per_msg > 50_000.0 && output_input_ratio < 0.15 {
        let total_cost: f64 = entries.iter().map(|e| e.total_cost).sum();
        // If context were halved, input costs drop ~40%
        let estimated_savings = total_cost * 0.3;

        recs.push(Recommendation {
            id: "idle_context_bloat".to_string(),
            severity: "medium".to_string(),
            confidence: "medium".to_string(),
            title: "Large context sent per message with low output ratio".to_string(),
            evidence: vec![
                format!("Avg input/message: {:.0}k tokens", input_per_msg / 1000.0),
                format!("Output/input ratio: {:.1}% (healthy: >20%)", output_input_ratio * 100.0),
                format!("Total messages: {}, total input: {:.1}M tokens", total_messages, total_input as f64 / 1_000_000.0),
            ],
            action: "Reduce repeated context by using session continuation, smaller file includes, \
                     or summarized context instead of full file contents. Consider .kiroignore or \
                     context pruning strategies."
                .to_string(),
            estimated_savings: Some(estimated_savings),
        });
    }
}

/// Rule 6: Model diversity low
/// Triggers when a single model accounts for >90% of total spend.
fn rule_model_diversity_low(entries: &[WikiEntry], summary: &PeriodSummary, recs: &mut Vec<Recommendation>) {
    if entries.len() < 5 || summary.total_cost < 5.0 {
        return;
    }

    let mut model_costs: HashMap<&str, f64> = HashMap::new();
    for e in entries {
        let share = e.total_cost / e.models_used.len().max(1) as f64;
        for m in &e.models_used {
            *model_costs.entry(m.as_str()).or_default() += share;
        }
    }

    if model_costs.len() <= 1 {
        // Only one model available — can't diversify
        if let Some((model, cost)) = model_costs.iter().next() {
            let share = *cost / summary.total_cost.max(0.001);
            if share > 0.90 {
                recs.push(Recommendation {
                    id: "model_diversity_low".to_string(),
                    severity: "low".to_string(),
                    confidence: "medium".to_string(),
                    title: format!("100% reliance on {}", model),
                    evidence: vec![
                        format!("All ${:.2} spent on a single model", cost),
                        "Single-model dependency creates risk if the provider has outages or price changes".to_string(),
                    ],
                    action: "Consider configuring a fallback model for resilience. \
                             Even occasional use of an alternative reduces vendor lock-in risk."
                        .to_string(),
                    estimated_savings: None,
                });
            }
        }
        return;
    }

    let top = model_costs
        .iter()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
        .map(|(m, c)| (*m, *c));

    if let Some((model, cost)) = top {
        let share = cost / summary.total_cost.max(0.001);
        if share > 0.90 {
            let other_models: Vec<&str> = model_costs
                .keys()
                .filter(|m| **m != model)
                .copied()
                .collect();

            recs.push(Recommendation {
                id: "model_diversity_low".to_string(),
                severity: "low".to_string(),
                confidence: "medium".to_string(),
                title: format!("{} accounts for {:.0}% of spend", model, share * 100.0),
                evidence: vec![
                    format!("${:.2} of ${:.2} total on {}", cost, summary.total_cost, model),
                    format!("Other available models: {}", other_models.join(", ")),
                    "High concentration creates outage/pricing risk".to_string(),
                ],
                action: format!(
                    "Route simpler tasks to {} for cost savings and resilience. \
                     Reserve {} for complex architecture and debugging.",
                    other_models.first().unwrap_or(&"a cheaper model"),
                    model
                ),
                estimated_savings: None,
            });
        }
    }
}

/// Rule 7: Short session churn
/// Triggers when >30% of sessions are under 5 minutes, suggesting frequent restarts
/// that invalidate prompt caches and waste warm-up tokens.
fn rule_short_session_churn(entries: &[WikiEntry], recs: &mut Vec<Recommendation>) {
    if entries.len() < 10 {
        return;
    }

    let short_sessions: Vec<&WikiEntry> = entries
        .iter()
        .filter(|e| e.duration_minutes < 5 && e.message_count >= 2)
        .collect();

    let short_ratio = short_sessions.len() as f64 / entries.len() as f64;

    if short_ratio > 0.30 {
        let short_cost: f64 = short_sessions.iter().map(|e| e.total_cost).sum();
        let total_cost: f64 = entries.iter().map(|e| e.total_cost).sum();
        let avg_short_msgs: f64 = short_sessions.iter().map(|e| e.message_count as f64).sum::<f64>()
            / short_sessions.len().max(1) as f64;

        recs.push(Recommendation {
            id: "short_session_churn".to_string(),
            severity: "medium".to_string(),
            confidence: "medium".to_string(),
            title: format!("{:.0}% of sessions are under 5 minutes", short_ratio * 100.0),
            evidence: vec![
                format!("{} of {} sessions lasted <5min", short_sessions.len(), entries.len()),
                format!("Short session spend: ${:.2} ({:.0}% of total)", short_cost, short_cost / total_cost.max(0.001) * 100.0),
                format!("Avg messages in short sessions: {:.1}", avg_short_msgs),
            ],
            action: "Frequent short sessions invalidate prompt caches and repeat system prompts. \
                     Try continuing existing sessions instead of starting new ones. \
                     Check if your client auto-creates sessions on each command."
                .to_string(),
            estimated_savings: Some(short_cost * 0.3), // 30% of short session cost is cache warm-up waste
        });
    }
}

/// Rule 8: Night owl premium
/// Triggers when sessions during 00:00–06:00 local time use expensive models disproportionately,
/// suggesting automated/batch work that could use cheaper models.
fn rule_night_owl_premium(entries: &[WikiEntry], recs: &mut Vec<Recommendation>) {
    if entries.len() < 10 {
        return;
    }

    let night_entries: Vec<&WikiEntry> = entries
        .iter()
        .filter(|e| {
            let hour = chrono::DateTime::from_timestamp(e.created_at, 0)
                .map(|dt| dt.with_timezone(&chrono::Local).hour())
                .unwrap_or(12);
            hour < 6
        })
        .collect();

    if night_entries.len() < 3 {
        return;
    }

    let day_entries: Vec<&WikiEntry> = entries
        .iter()
        .filter(|e| {
            let hour = chrono::DateTime::from_timestamp(e.created_at, 0)
                .map(|dt| dt.with_timezone(&chrono::Local).hour())
                .unwrap_or(12);
            hour >= 6
        })
        .collect();

    if day_entries.is_empty() {
        return;
    }

    let night_avg_cost = night_entries.iter().map(|e| e.total_cost).sum::<f64>() / night_entries.len() as f64;
    let day_avg_cost = day_entries.iter().map(|e| e.total_cost).sum::<f64>() / day_entries.len() as f64;
    let night_total_cost: f64 = night_entries.iter().map(|e| e.total_cost).sum();

    // Night sessions cost >50% more per session than day sessions
    if night_avg_cost > day_avg_cost * 1.5 && night_total_cost > 3.0 {
        recs.push(Recommendation {
            id: "night_owl_premium".to_string(),
            severity: "low".to_string(),
            confidence: "low".to_string(),
            title: "Late-night sessions use more expensive models".to_string(),
            evidence: vec![
                format!("{} sessions between 00:00–06:00 (avg ${:.2}/session)", night_entries.len(), night_avg_cost),
                format!("Daytime avg: ${:.2}/session ({:.0}% cheaper)", day_avg_cost, (1.0 - day_avg_cost / night_avg_cost.max(0.001)) * 100.0),
                format!("Night spend total: ${:.2}", night_total_cost),
            ],
            action: "If late-night work is automated (CI, batch jobs, scheduled tasks), \
                     configure a cheaper model for those runs. If it's manual work, \
                     consider whether fatigue-driven sessions are cost-efficient."
                .to_string(),
            estimated_savings: Some(night_total_cost * 0.3),
        });
    }
}

/// Rule 9: Workspace concentration
/// Triggers when a single workspace accounts for >70% of total spend.
fn rule_workspace_concentration(entries: &[WikiEntry], summary: &PeriodSummary, recs: &mut Vec<Recommendation>) {
    if entries.len() < 5 || summary.total_cost < 5.0 {
        return;
    }

    let mut ws_costs: HashMap<&str, (f64, usize)> = HashMap::new();
    for e in entries {
        let ws = e.workspace.as_deref().unwrap_or("(none)");
        let entry = ws_costs.entry(ws).or_default();
        entry.0 += e.total_cost;
        entry.1 += 1;
    }

    if ws_costs.len() <= 1 {
        return; // Only one workspace — nothing to compare
    }

    let top = ws_costs
        .iter()
        .max_by(|a, b| a.1 .0.partial_cmp(&b.1 .0).unwrap());

    if let Some((ws, (cost, sessions))) = top {
        let share = *cost / summary.total_cost.max(0.001);
        if share > 0.70 {
            recs.push(Recommendation {
                id: "workspace_concentration".to_string(),
                severity: "low".to_string(),
                confidence: "high".to_string(),
                title: format!("\"{}\" accounts for {:.0}% of spend", ws, share * 100.0),
                evidence: vec![
                    format!("${:.2} across {} sessions in this workspace", cost, sessions),
                    format!("Total spend: ${:.2} across {} workspaces", summary.total_cost, ws_costs.len()),
                    "High concentration means a single project drives most of your bill".to_string(),
                ],
                action: format!(
                    "Review whether \"{}\" has specific inefficiencies (large context, \
                     expensive model choice, or runaway sessions). Consider workspace-specific \
                     model routing rules.",
                    ws
                ),
                estimated_savings: None,
            });
        }
    }
}

/// Rule 10: Output waste
/// Triggers when output tokens are high relative to input but message count is low,
/// suggesting the model generates large outputs that may not be fully utilized.
fn rule_output_waste(entries: &[WikiEntry], recs: &mut Vec<Recommendation>) {
    if entries.len() < 5 {
        return;
    }

    let total_output: i64 = entries.iter().map(|e| e.total_output_tokens).sum();
    let total_messages: i64 = entries.iter().map(|e| e.message_count as i64).sum();
    let total_cost: f64 = entries.iter().map(|e| e.total_cost).sum();

    if total_messages == 0 || total_cost < 3.0 {
        return;
    }

    let output_per_msg = total_output as f64 / total_messages as f64;

    // High output per message (>8k tokens) — model is generating a lot
    if output_per_msg > 8_000.0 {
        // Check if sessions have low message counts (suggesting single-shot large generations)
        let low_msg_sessions: Vec<&WikiEntry> = entries
            .iter()
            .filter(|e| e.message_count <= 3 && e.total_output_tokens > 10_000)
            .collect();

        let low_msg_ratio = low_msg_sessions.len() as f64 / entries.len() as f64;

        if low_msg_ratio > 0.20 {
            let waste_cost: f64 = low_msg_sessions.iter().map(|e| e.total_cost).sum();

            recs.push(Recommendation {
                id: "output_waste".to_string(),
                severity: "low".to_string(),
                confidence: "low".to_string(),
                title: format!("High output generation ({:.0}k tokens/msg avg)", output_per_msg / 1000.0),
                evidence: vec![
                    format!("Avg output per message: {:.0}k tokens", output_per_msg / 1000.0),
                    format!("{} sessions ({:.0}%) have ≤3 messages but >10k output tokens",
                        low_msg_sessions.len(), low_msg_ratio * 100.0),
                    format!("Cost of low-interaction high-output sessions: ${:.2}", waste_cost),
                ],
                action: "Large single-shot generations often produce code that needs heavy editing. \
                         Consider iterative prompting (plan → implement → refine) to reduce wasted output. \
                         Use max_tokens limits if your client supports them."
                    .to_string(),
                estimated_savings: Some(waste_cost * 0.25),
            });
        }
    }
}

/// Rule 11: Token efficiency decline
/// Triggers when input tokens per message is trending upward compared to previous period,
/// suggesting prompts are getting bloated over time.
fn rule_token_efficiency_decline(
    current_entries: &[WikiEntry],
    previous_entries: Option<&[WikiEntry]>,
    recs: &mut Vec<Recommendation>,
) {
    let prev = match previous_entries {
        Some(p) if p.len() >= 5 => p,
        _ => return,
    };

    if current_entries.len() < 5 {
        return;
    }

    let cur_input: i64 = current_entries.iter().map(|e| e.total_input_tokens).sum();
    let cur_msgs: i64 = current_entries.iter().map(|e| e.message_count as i64).sum();
    let prev_input: i64 = prev.iter().map(|e| e.total_input_tokens).sum();
    let prev_msgs: i64 = prev.iter().map(|e| e.message_count as i64).sum();

    if cur_msgs == 0 || prev_msgs == 0 {
        return;
    }

    let cur_ratio = cur_input as f64 / cur_msgs as f64;
    let prev_ratio = prev_input as f64 / prev_msgs as f64;

    if prev_ratio <= 0.0 {
        return;
    }

    let increase_pct = (cur_ratio - prev_ratio) / prev_ratio;

    // >30% increase in tokens/message
    if increase_pct > 0.30 && cur_ratio > 20_000.0 {
        let cur_cost: f64 = current_entries.iter().map(|e| e.total_cost).sum();
        let excess_ratio = 1.0 - (prev_ratio / cur_ratio);
        let estimated_savings = cur_cost * excess_ratio * 0.5; // Conservative: half the excess is reducible

        recs.push(Recommendation {
            id: "token_efficiency_decline".to_string(),
            severity: "medium".to_string(),
            confidence: "medium".to_string(),
            title: format!("Input tokens/message up {:.0}% vs previous period", increase_pct * 100.0),
            evidence: vec![
                format!("Current: {:.0}k tokens/msg, Previous: {:.0}k tokens/msg", cur_ratio / 1000.0, prev_ratio / 1000.0),
                format!("Increase: {:+.0}% ({:+.0}k tokens/msg)", increase_pct * 100.0, (cur_ratio - prev_ratio) / 1000.0),
                format!("This often indicates growing system prompts, larger file includes, or context accumulation"),
            ],
            action: "Review what changed in your prompts or workflow. Common causes: \
                     new tools/plugins adding context, larger codebases being included, \
                     or session history growing without pruning. Consider .kiroignore rules \
                     or explicit context selection."
                .to_string(),
            estimated_savings: Some(estimated_savings),
        });
    }
}

/// Rule 12: Cost per task regression
/// Triggers when the same task category costs significantly more than in the previous period.
fn rule_cost_per_task_regression(
    current_entries: &[WikiEntry],
    previous_entries: Option<&[WikiEntry]>,
    recs: &mut Vec<Recommendation>,
) {
    let prev = match previous_entries {
        Some(p) if p.len() >= 5 => p,
        _ => return,
    };

    if current_entries.len() < 5 {
        return;
    }

    let mut cur_by_cat: HashMap<&str, (f64, usize)> = HashMap::new();
    for e in current_entries {
        let cat = e.task_category.as_deref().unwrap_or("other");
        let entry = cur_by_cat.entry(cat).or_default();
        entry.0 += e.total_cost;
        entry.1 += 1;
    }

    let mut prev_by_cat: HashMap<&str, (f64, usize)> = HashMap::new();
    for e in prev {
        let cat = e.task_category.as_deref().unwrap_or("other");
        let entry = prev_by_cat.entry(cat).or_default();
        entry.0 += e.total_cost;
        entry.1 += 1;
    }

    let mut regressions: Vec<(String, f64, f64, f64)> = Vec::new();

    for (cat, (cur_cost, cur_count)) in &cur_by_cat {
        if *cur_count < 3 {
            continue;
        }
        let cur_avg = *cur_cost / *cur_count as f64;

        if let Some((prev_cost, prev_count)) = prev_by_cat.get(cat) {
            if *prev_count < 2 {
                continue;
            }
            let prev_avg = *prev_cost / *prev_count as f64;

            if prev_avg <= 0.0 {
                continue;
            }

            let increase_pct = (cur_avg - prev_avg) / prev_avg;
            let abs_increase = cur_avg - prev_avg;

            // >50% increase and at least $0.50 more per session
            if increase_pct > 0.50 && abs_increase > 0.50 {
                regressions.push((cat.to_string(), cur_avg, prev_avg, increase_pct));
            }
        }
    }

    if regressions.is_empty() {
        return;
    }

    // Sort by absolute cost impact
    regressions.sort_by(|a, b| {
        let impact_a = (a.1 - a.2) * cur_by_cat.get(a.0.as_str()).map(|c| c.1 as f64).unwrap_or(1.0);
        let impact_b = (b.1 - b.2) * cur_by_cat.get(b.0.as_str()).map(|c| c.1 as f64).unwrap_or(1.0);
        impact_b.partial_cmp(&impact_a).unwrap_or(std::cmp::Ordering::Equal)
    });

    let top = &regressions[0];
    let cur_count = cur_by_cat.get(top.0.as_str()).map(|c| c.1).unwrap_or(0);
    let excess_per_session = top.1 - top.2;
    let total_excess = excess_per_session * cur_count as f64;

    let mut evidence = vec![
        format!("\"{}\" tasks: ${:.2}/session → ${:.2}/session ({:+.0}%)", top.0, top.2, top.1, top.3 * 100.0),
        format!("{} sessions this period, excess cost: ${:.2}", cur_count, total_excess),
    ];

    if regressions.len() > 1 {
        let others: Vec<String> = regressions[1..].iter().take(2)
            .map(|r| format!("{} ({:+.0}%)", r.0, r.3 * 100.0))
            .collect();
        evidence.push(format!("Also regressed: {}", others.join(", ")));
    }

    recs.push(Recommendation {
        id: "cost_per_task_regression".to_string(),
        severity: "medium".to_string(),
        confidence: "high".to_string(),
        title: format!("\"{}\" tasks cost {:.0}% more per session than last period", top.0, top.3 * 100.0),
        evidence,
        action: format!(
            "Investigate why \"{}\" tasks became more expensive. Common causes: \
             model upgrade, larger codebase context, more complex requirements, \
             or switching from a cheaper model. Compare session lengths and models used.",
            top.0
        ),
        estimated_savings: Some(total_excess * 0.4), // 40% of excess is likely reducible
    });
}

// ─── Rendering ───────────────────────────────────────────────────────────────

fn print_advisor_report(report: &AdvisorReport) {
    println!();
    println!("  {}", "═══ Advisor ═══".bold());
    println!();

    // Summary line
    let cur = &report.current_period;
    print!(
        "  Spend: {}  |  Sessions: {}  |  Cache reuse: {:.0}%",
        format!("${:.2}", cur.total_cost).yellow(),
        cur.sessions.to_string().cyan(),
        cur.cache_reuse_rate * 100.0,
    );

    if let Some(delta) = &report.deltas {
        println!();
        let cost_arrow = if delta.cost_change >= 0.0 { "↑" } else { "↓" };
        let cost_color = if delta.cost_change > 5.0 {
            format!("{} ${:.2} ({:+.0}%)", cost_arrow, delta.cost_change.abs(), delta.cost_change_pct).red().to_string()
        } else if delta.cost_change < -1.0 {
            format!("{} ${:.2} ({:+.0}%)", cost_arrow, delta.cost_change.abs(), delta.cost_change_pct).green().to_string()
        } else {
            format!("{} ${:.2} ({:+.0}%)", cost_arrow, delta.cost_change.abs(), delta.cost_change_pct).dimmed().to_string()
        };
        print!("  vs previous: {}  |  Driver: {}", cost_color, delta.main_driver);

        if delta.cache_rate_change_pp.abs() > 5.0 {
            let cache_dir = if delta.cache_rate_change_pp > 0.0 { "↑" } else { "↓" };
            print!("  |  Cache: {}{:.0}pp", cache_dir, delta.cache_rate_change_pp.abs());
        }
    }
    println!();

    // Estimated avoidable spend
    let total_savings: f64 = report
        .recommendations
        .iter()
        .filter_map(|r| r.estimated_savings)
        .sum();
    if total_savings > 0.5 {
        println!(
            "  Estimated avoidable spend: {}",
            format!("~${:.2}/period", total_savings).green()
        );
    }
    println!();

    // Recommendations
    if report.recommendations.is_empty() {
        println!("  {} No issues detected. Usage looks efficient.", "✓".green());
        println!();
        return;
    }

    println!("  {}", "Recommendations".bold().underline());
    println!();

    for (i, rec) in report.recommendations.iter().take(5).enumerate() {
        let sev_badge = match rec.severity.as_str() {
            "high" => format!("[{}]", "HIGH".red().bold()),
            "medium" => format!("[{}]", "MED".yellow().bold()),
            _ => format!("[{}]", "LOW".dimmed().bold()),
        };
        let conf = match rec.confidence.as_str() {
            "high" => "●●●",
            "medium" => "●●○",
            _ => "●○○",
        };

        println!("  {}. {} {}  {}", i + 1, sev_badge, rec.title.bold(), conf.dimmed());
        println!();
        for ev in &rec.evidence {
            println!("     • {}", ev);
        }
        println!();
        println!("     {} {}", "→".green(), rec.action);
        if let Some(savings) = rec.estimated_savings {
            println!("       Estimated savings: {}", format!("~${:.2}", savings).green());
        }
        println!();
    }

    // What Changed section (if we have deltas)
    if let (Some(delta), Some(prev)) = (&report.deltas, &report.previous_period) {
        println!("  {}", "What Changed".bold().underline());
        println!(
            "  {:>20} {:>10} {:>10} {:>10}",
            "", "Current", "Previous", "Δ"
        );
        println!("  {}", "─".repeat(52));
        println!(
            "  {:>20} {:>10} {:>10} {:>10}",
            "Cost",
            format!("${:.2}", report.current_period.total_cost),
            format!("${:.2}", prev.total_cost),
            format!("{:+.2}", delta.cost_change),
        );
        println!(
            "  {:>20} {:>10} {:>10} {:>10}",
            "Sessions",
            report.current_period.sessions,
            prev.sessions,
            format!("{:+}", delta.sessions_change),
        );
        println!(
            "  {:>20} {:>10} {:>10} {:>10}",
            "Cache reuse",
            format!("{:.0}%", report.current_period.cache_reuse_rate * 100.0),
            format!("{:.0}%", prev.cache_reuse_rate * 100.0),
            format!("{:+.0}pp", delta.cache_rate_change_pp),
        );
        println!(
            "  {:>20} {:>10} {:>10} {:>10}",
            "Avg cost/session",
            format!("${:.2}", report.current_period.avg_cost_per_session),
            format!("${:.2}", prev.avg_cost_per_session),
            format!(
                "{:+.2}",
                report.current_period.avg_cost_per_session - prev.avg_cost_per_session
            ),
        );
        if let Some(ref model) = report.current_period.top_model {
            println!(
                "  {:>20} {:>10}",
                "Top model",
                format!("{} ({:.0}%)", model, report.current_period.top_model_share * 100.0),
            );
        }
        println!();
    }
}
