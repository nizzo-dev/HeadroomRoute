//! Deterministic, explainable routing policy decisions.
//!
//! This module only evaluates already collected route facts. It never reads
//! request bodies, API keys, files, or network state.

use crate::model::{FailoverPolicy, Protocol, Route, RouteHealth};
use anyhow::{Result, bail};
use serde::{Deserialize, Deserializer, Serialize, de::Error};
use std::{collections::BTreeMap, fmt};

pub const ROUTING_POLICY_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RoutingStrategyConfig {
    pub version: u32,
    pub enabled: bool,
    pub observe_only: bool,
    pub weights: Weights,
    pub missing_metric: MissingMetricPolicy,
    pub min_success_samples: u64,
    #[serde(
        default,
        alias = "costs",
        deserialize_with = "deserialize_provider_costs"
    )]
    pub provider_costs: BTreeMap<String, f64>,
    #[serde(default)]
    pub model_rules: Vec<ModelRule>,
    pub references: NormalizationReferences,
}

impl Default for RoutingStrategyConfig {
    fn default() -> Self {
        Self {
            version: ROUTING_POLICY_SCHEMA_VERSION,
            enabled: false,
            observe_only: true,
            weights: Weights::default(),
            missing_metric: MissingMetricPolicy::default(),
            min_success_samples: 3,
            provider_costs: BTreeMap::new(),
            model_rules: Vec::new(),
            references: NormalizationReferences::default(),
        }
    }
}

impl RoutingStrategyConfig {
    pub fn validate(&self) -> Result<()> {
        if self.version == 0 || self.version > ROUTING_POLICY_SCHEMA_VERSION {
            bail!("unsupported routing policy version {}", self.version);
        }
        self.weights.validate()?;
        self.references.validate()?;
        if !self.missing_metric.weight.is_finite()
            || !(0.0..=1.0).contains(&self.missing_metric.weight)
        {
            bail!("missing metric weight must be finite and between 0 and 1");
        }
        for (provider, cost) in &self.provider_costs {
            validate_provider_cost(provider, *cost)?;
        }
        for rule in &self.model_rules {
            rule.validate()?;
        }
        Ok(())
    }

    pub fn provider_cost(&self, provider: &str) -> Option<f64> {
        self.provider_costs.get(provider).copied()
    }

    pub fn set_provider_cost(&mut self, provider: &str, cost: f64) -> Result<()> {
        validate_provider_cost(provider, cost)?;
        self.provider_costs.insert(provider.to_owned(), cost);
        Ok(())
    }
}

fn deserialize_provider_costs<'de, D>(deserializer: D) -> Result<BTreeMap<String, f64>, D::Error>
where
    D: Deserializer<'de>,
{
    let costs = BTreeMap::<String, f64>::deserialize(deserializer)?;
    for (provider, cost) in &costs {
        validate_provider_cost(provider, *cost).map_err(D::Error::custom)?;
    }
    Ok(costs)
}

fn validate_provider_cost(provider: &str, cost: f64) -> Result<()> {
    if provider.trim().is_empty() {
        bail!("provider cost mapping contains an empty Provider ID");
    }
    if contains_secret_marker(provider) {
        bail!("provider cost mapping contains a sensitive Provider ID");
    }
    if !cost.is_finite() || cost < 0.0 {
        bail!("cost for Provider {provider} must be finite and non-negative");
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Weights {
    pub health: f64,
    pub success_rate: f64,
    pub latency: f64,
    pub cost: f64,
    pub model_match: f64,
}

impl Default for Weights {
    fn default() -> Self {
        Self {
            health: 0.40,
            success_rate: 0.30,
            latency: 0.15,
            cost: 0.10,
            model_match: 0.05,
        }
    }
}

impl Weights {
    fn validate(&self) -> Result<()> {
        let values = [
            self.health,
            self.success_rate,
            self.latency,
            self.cost,
            self.model_match,
        ];
        if values
            .iter()
            .any(|value| !value.is_finite() || *value < 0.0)
        {
            bail!("routing policy weights must be finite and non-negative");
        }
        Ok(())
    }

    fn total(&self) -> f64 {
        self.health + self.success_rate + self.latency + self.cost + self.model_match
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MissingMetricPolicy {
    pub enabled: bool,
    pub weight: f64,
    pub ignore_model: bool,
}

impl Default for MissingMetricPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            weight: 0.5,
            ignore_model: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CostUnit {
    pub provider: String,
    pub cost: f64,
}

impl Default for CostUnit {
    fn default() -> Self {
        Self {
            provider: String::new(),
            cost: 0.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NormalizationReferences {
    pub success_rate: f64,
    pub success_weight: f64,
    pub fail_rate: f64,
    pub fail_weight: f64,
    pub timeout_weight: f64,
    pub latency_ms: f64,
    pub cost: f64,
}

impl Default for NormalizationReferences {
    fn default() -> Self {
        Self {
            success_rate: 1.0,
            success_weight: 1.0,
            fail_rate: 1.0,
            fail_weight: 1.0,
            timeout_weight: 1.0,
            latency_ms: 1000.0,
            cost: 1.0,
        }
    }
}

impl NormalizationReferences {
    fn validate(&self) -> Result<()> {
        let values = [
            self.success_rate,
            self.success_weight,
            self.fail_rate,
            self.fail_weight,
            self.timeout_weight,
            self.latency_ms,
            self.cost,
        ];
        if values
            .iter()
            .any(|value| !value.is_finite() || *value <= 0.0)
        {
            bail!("routing normalization references must be finite and positive");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelRule {
    pub pattern: String,
    pub weight: f64,
}

impl Default for ModelRule {
    fn default() -> Self {
        Self {
            pattern: "*".into(),
            weight: 1.0,
        }
    }
}

impl ModelRule {
    fn validate(&self) -> Result<()> {
        if self.pattern.trim().is_empty() || !self.weight.is_finite() || self.weight < 0.0 {
            bail!("model rule must have a non-empty pattern and finite non-negative weight");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelPattern {
    pub prefix: String,
    pub weight: f64,
}

impl Default for ModelPattern {
    fn default() -> Self {
        Self {
            prefix: String::new(),
            weight: 1.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingStrategy {
    Default,
    OpenAI,
    Anthropic,
    Codex,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DecisionContext {
    pub model: String,
    pub protocol: Protocol,
    pub allowed_targets: Vec<String>,
    pub provider: Option<String>,
    pub provider_cost: Option<f64>,
}

impl Default for DecisionContext {
    fn default() -> Self {
        Self {
            model: String::new(),
            protocol: Protocol::OpenAi,
            allowed_targets: Vec::new(),
            provider: None,
            provider_cost: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionMode {
    Observe,
    Apply,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RoutingCandidate {
    pub provider: String,
    pub score: f64,
    pub rationale: String,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct CandidateFacts {
    pub model: String,
    pub protocol: Protocol,
    pub route: Route,
    pub provider: Option<String>,
    pub provider_cost: Option<f64>,
    pub weights: Weights,
    pub reason: String,
}

impl fmt::Debug for CandidateFacts {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut safe_route = self.route.clone();
        safe_route.api_key = None;
        formatter
            .debug_struct("CandidateFacts")
            .field("model", &self.model)
            .field("protocol", &self.protocol)
            .field("route", &safe_route)
            .field("provider", &self.provider)
            .field("provider_cost", &self.provider_cost)
            .field("weights", &self.weights)
            .field("reason", &self.reason)
            .finish()
    }
}

impl CandidateFacts {
    pub fn from_route(model: impl Into<String>, route: &Route) -> Self {
        let mut safe_route = route.clone();
        safe_route.api_key = None;
        Self {
            model: model.into(),
            protocol: safe_route.protocol,
            route: safe_route,
            provider: None,
            provider_cost: None,
            weights: Weights::default(),
            reason: "route facts".into(),
        }
    }

    pub fn with_provider_cost(mut self, cost: Option<f64>) -> Self {
        self.provider_cost = cost;
        self
    }

    pub fn provider_id(&self) -> &str {
        self.provider
            .as_deref()
            .unwrap_or(self.route.provider.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RoutingDecision {
    pub model: String,
    pub protocol: Protocol,
    pub decision: DecisionMode,
    pub selected_provider: Option<String>,
    pub score: f64,
    pub rationale: String,
    pub ranked: Vec<RoutingCandidate>,
    pub candidates: Vec<CandidateFacts>,
    pub strategy: RoutingStrategyConfig,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingDecisionRecord {
    pub model: String,
    pub protocol: Protocol,
    pub mode: DecisionMode,
    pub selected_provider: Option<String>,
    pub score_basis: String,
    pub rationale: String,
}

impl RoutingDecision {
    pub fn as_record(&self) -> RoutingDecisionRecord {
        RoutingDecisionRecord {
            model: self.model.clone(),
            protocol: self.protocol,
            mode: self.decision.clone(),
            selected_provider: self.selected_provider.clone(),
            score_basis: format_score(self.score),
            rationale: self.rationale.clone(),
        }
    }
}

pub fn score(
    model: &str,
    protocol: Protocol,
    candidates: &[CandidateFacts],
    strategy: &RoutingStrategyConfig,
) -> (f64, Vec<CandidateFacts>) {
    let ranked = evaluate(model, protocol, None, &[], candidates, strategy).unwrap_or_default();
    let best_score = ranked.first().map_or(0.0, |candidate| candidate.score);
    let facts = ranked
        .into_iter()
        .map(|candidate| candidate.facts)
        .collect();
    (best_score, facts)
}

pub fn decide(
    model: &str,
    protocol: Protocol,
    candidates: &[CandidateFacts],
    strategy: &RoutingStrategyConfig,
) -> Result<RoutingDecision> {
    decide_with_context(
        &DecisionContext {
            model: model.to_owned(),
            protocol,
            allowed_targets: Vec::new(),
            provider: None,
            provider_cost: None,
        },
        candidates,
        strategy,
    )
}

pub fn decide_with_context(
    context: &DecisionContext,
    candidates: &[CandidateFacts],
    strategy: &RoutingStrategyConfig,
) -> Result<RoutingDecision> {
    strategy.validate()?;
    if let Some(cost) = context.provider_cost {
        validate_provider_cost(context.provider.as_deref().unwrap_or("context"), cost)?;
    }
    let ranked = evaluate(
        &context.model,
        context.protocol,
        context.provider.as_deref(),
        &context.allowed_targets,
        candidates,
        strategy,
    )?;
    let selected = ranked.first();
    let selected_provider = selected.map(|candidate| candidate.facts.provider_id().to_owned());
    let score = selected.map_or(0.0, |candidate| candidate.score);
    let rationale = selected.map_or_else(
        || "no eligible healthy Provider".into(),
        |candidate| candidate.rationale.clone(),
    );
    let mode = if strategy.enabled && !strategy.observe_only && selected.is_some() {
        DecisionMode::Apply
    } else {
        DecisionMode::Observe
    };
    Ok(RoutingDecision {
        model: context.model.clone(),
        protocol: context.protocol,
        decision: mode,
        selected_provider,
        score,
        rationale,
        ranked: ranked
            .iter()
            .map(|candidate| RoutingCandidate {
                provider: candidate.facts.provider_id().to_owned(),
                score: candidate.score,
                rationale: candidate.rationale.clone(),
            })
            .collect(),
        candidates: ranked
            .into_iter()
            .map(|candidate| candidate.facts)
            .collect(),
        strategy: strategy.clone(),
    })
}

struct ScoredFacts {
    facts: CandidateFacts,
    score: f64,
    rationale: String,
    input_index: usize,
}

fn evaluate(
    model: &str,
    protocol: Protocol,
    current_provider: Option<&str>,
    allowed_targets: &[String],
    candidates: &[CandidateFacts],
    strategy: &RoutingStrategyConfig,
) -> Result<Vec<ScoredFacts>> {
    strategy.validate()?;
    let mut scored = Vec::new();
    for (input_index, candidate) in candidates.iter().enumerate() {
        if candidate.protocol != protocol || candidate.route.protocol != protocol {
            continue;
        }
        if candidate.route.state != RouteHealth::Healthy {
            continue;
        }
        let provider = candidate.provider_id();
        if !allowed_targets.is_empty() && !allowed_targets.iter().any(|target| target == provider) {
            continue;
        }
        let model_weight = match model_rule_weight(model, &strategy.model_rules) {
            Some(weight) => weight,
            None if strategy.missing_metric.ignore_model => strategy.missing_metric.weight,
            None => continue,
        };
        let provider_cost = candidate
            .provider_cost
            .or_else(|| strategy.provider_cost(provider));
        if let Some(cost) = provider_cost {
            validate_provider_cost(provider, cost)?;
        }
        let successes = candidate.route.consecutive_successes as f64;
        let failures = candidate.route.consecutive_failures as f64;
        let samples = successes + failures;
        let success_metric = if samples >= strategy.min_success_samples as f64 {
            successes / samples.max(1.0)
        } else if strategy.missing_metric.enabled {
            strategy.missing_metric.weight.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let latency_metric = candidate.route.latency_ms.map_or_else(
            || strategy.missing_metric.weight.clamp(0.0, 1.0),
            |latency| (1.0 - latency as f64 / strategy.references.latency_ms).clamp(0.0, 1.0),
        );
        let cost_metric = provider_cost.map_or_else(
            || strategy.missing_metric.weight.clamp(0.0, 1.0),
            |cost| (1.0 - cost / strategy.references.cost).clamp(0.0, 1.0),
        );
        let weights = &strategy.weights;
        let total_weight = weights.total();
        let raw_score = if total_weight == 0.0 {
            0.0
        } else {
            (weights.health
                + weights.success_rate * success_metric
                + weights.latency * latency_metric
                + weights.cost * cost_metric
                + weights.model_match * model_weight)
                / total_weight
        };
        let score = normalize_score(raw_score);
        let model_reason = model_rule_reason(model, &strategy.model_rules);
        let rationale = format!(
            "healthy; {model_reason}; success={:.2}; latency={:.2}; cost={:.2}",
            success_metric, latency_metric, cost_metric
        );
        let mut facts = candidate.clone();
        facts.provider = Some(provider.to_owned());
        facts.provider_cost = provider_cost;
        facts.reason = rationale.clone();
        scored.push(ScoredFacts {
            facts,
            score,
            rationale,
            input_index,
        });
    }
    scored.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| {
                let left_current = current_provider == Some(left.facts.provider_id());
                let right_current = current_provider == Some(right.facts.provider_id());
                right_current.cmp(&left_current)
            })
            .then_with(|| left.facts.provider_id().cmp(right.facts.provider_id()))
            .then_with(|| left.input_index.cmp(&right.input_index))
    });
    Ok(scored)
}

fn model_rule_weight(model: &str, rules: &[ModelRule]) -> Option<f64> {
    if rules.is_empty() {
        return Some(1.0);
    }
    rules
        .iter()
        .enumerate()
        .filter_map(|(index, rule)| {
            let specificity = rule_specificity(model, &rule.pattern)?;
            Some((specificity, index, rule.weight.clamp(0.0, 1.0)))
        })
        .max_by(|left, right| left.0.cmp(&right.0).then_with(|| right.1.cmp(&left.1)))
        .map(|(_, _, weight)| weight)
}

fn model_rule_reason(model: &str, rules: &[ModelRule]) -> String {
    let Some((pattern, specificity)) = rules
        .iter()
        .filter_map(|rule| Some((&rule.pattern, rule_specificity(model, &rule.pattern)?)))
        .max_by_key(|(_, specificity)| *specificity)
    else {
        return "model rule=default".into();
    };
    let kind = if specificity >= 1_000_000 {
        "exact"
    } else if pattern.ends_with('*') {
        "prefix"
    } else {
        "wildcard"
    };
    format!("model rule={kind}:{pattern}")
}

fn rule_specificity(model: &str, pattern: &str) -> Option<u32> {
    if pattern == model {
        return Some(1_000_000 + pattern.len() as u32);
    }
    if pattern == "*" {
        return Some(0);
    }
    let prefix = pattern.strip_suffix('*')?;
    model
        .starts_with(prefix)
        .then_some(100 + prefix.len() as u32)
}

pub fn normalize_score(value: f64) -> f64 {
    value.clamp(0.0, 1.0)
}

pub fn classify_by_protocol(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::OpenAi => "openai",
        Protocol::Anthropic => "anthropic",
    }
}

pub fn format_score(value: f64) -> String {
    format!("{value:.2}")
}

pub fn allowed_targets(policy: &FailoverPolicy, protocol: Protocol, provider: &str) -> Vec<String> {
    policy
        .targets(protocol, provider)
        .map_or_else(Vec::new, ToOwned::to_owned)
}

pub fn route_strategy_for_provider(provider_id: &str) -> RoutingStrategyConfig {
    let mut strategy = RoutingStrategyConfig::default();
    if !provider_id.trim().is_empty() {
        strategy.provider_costs.insert(provider_id.to_owned(), 0.0);
    }
    strategy
}

pub fn default_routing_strategy() -> RoutingStrategyConfig {
    RoutingStrategyConfig::default()
}

pub fn route_strategy(config: &RoutingStrategyConfig) -> Result<RoutingStrategyConfig> {
    config.validate()?;
    Ok(config.clone())
}

fn contains_secret_marker(provider: &str) -> bool {
    let lower = provider.to_ascii_lowercase();
    (lower.contains("sk-") || lower.contains("sk_")) && provider.len() >= 12
        || lower.starts_with("eyj") && provider.split('.').count() == 3
        || lower.contains("api_key")
        || lower.contains("apikey")
        || lower.contains("auth_token")
        || lower.contains("access_token")
        || lower.contains("secret")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::AuthStyle;

    fn route(provider: &str, score: i32, latency: u64, successes: u32, failures: u32) -> Route {
        let mut route = Route::new(
            Protocol::OpenAi,
            provider.into(),
            provider.into(),
            "https://example.invalid/v1".into(),
            Some("secret".into()),
            AuthStyle::Bearer,
            "test",
        );
        route.state = RouteHealth::Healthy;
        route.score = score;
        route.latency_ms = Some(latency);
        route.consecutive_successes = successes;
        route.consecutive_failures = failures;
        route
    }

    #[test]
    fn defaults_disable_behavior_changes_and_use_observe_only() {
        let config = RoutingStrategyConfig::default();
        assert!(!config.enabled);
        assert!(config.observe_only);
    }

    #[test]
    fn invalid_provider_costs_are_rejected() {
        let mut config = RoutingStrategyConfig::default();
        assert!(config.set_provider_cost("provider", -1.0).is_err());
        assert!(config.set_provider_cost("provider", f64::NAN).is_err());
        assert!(config.set_provider_cost("provider", f64::INFINITY).is_err());
        assert!(
            serde_json::from_str::<RoutingStrategyConfig>(
                r#"{"provider_costs":{"provider":-1.0}}"#
            )
            .is_err()
        );
    }

    #[test]
    fn scoring_is_deterministic_and_prefers_current_provider_on_tie() {
        let left = CandidateFacts::from_route("gpt-4", &route("a", 10, 500, 10, 0));
        let right = CandidateFacts::from_route("gpt-4", &route("b", 10, 500, 10, 0));
        let config = RoutingStrategyConfig {
            enabled: true,
            observe_only: false,
            weights: Weights {
                health: 1.0,
                success_rate: 0.0,
                latency: 0.0,
                cost: 0.0,
                model_match: 0.0,
            },
            ..RoutingStrategyConfig::default()
        };
        let decision = decide_with_context(
            &DecisionContext {
                model: "gpt-4".into(),
                protocol: Protocol::OpenAi,
                allowed_targets: Vec::new(),
                provider: Some("b".into()),
                provider_cost: None,
            },
            &[left, right],
            &config,
        )
        .unwrap();
        assert_eq!(decision.decision, DecisionMode::Apply);
        assert_eq!(decision.selected_provider.as_deref(), Some("b"));
    }

    #[test]
    fn unhealthy_and_disallowed_candidates_are_never_selected() {
        let bad = Route {
            state: RouteHealth::Degraded,
            ..route("bad", 100, 1, 100, 0)
        };
        let good = route("good", 1, 900, 1, 0);
        let config = RoutingStrategyConfig {
            model_rules: vec![ModelRule {
                pattern: "gpt-*".into(),
                weight: 1.0,
            }],
            ..RoutingStrategyConfig::default()
        };
        let decision = decide_with_context(
            &DecisionContext {
                model: "gpt-4".into(),
                protocol: Protocol::OpenAi,
                allowed_targets: vec!["good".into()],
                provider: None,
                provider_cost: None,
            },
            &[
                CandidateFacts::from_route("gpt-4", &bad),
                CandidateFacts::from_route("gpt-4", &good),
            ],
            &config,
        )
        .unwrap();
        assert_eq!(decision.selected_provider.as_deref(), Some("good"));
        assert_eq!(decision.candidates.len(), 1);
    }

    #[test]
    fn candidate_facts_debug_does_not_expose_route_api_key() {
        let facts = CandidateFacts::from_route("gpt-4", &route("safe", 1, 10, 1, 0));
        assert!(!format!("{facts:?}").contains("secret"));
        assert!(!serde_json::to_string(&facts).unwrap().contains("secret"));
    }
}
