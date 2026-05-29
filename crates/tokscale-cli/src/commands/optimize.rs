use anyhow::Result;
use chrono;
use colored::Colorize;
use std::collections::HashMap;
use tokscale_core::wiki::{WikiDb, WikiEntry};

pub struct OptimizeOptions {
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

pub fn run_optimize(opts: OptimizeOptions) -> Result<()> {
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

fn resolve_trailing_7d_window(opts: &OptimizeOptions) -> (Option<i64>, Option<i64>) {
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
    opts: &OptimizeOptions,
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
