//! Cross-repo contract conformance: replay the lockstep decision table through
//! Limen's real loader and merge code.
//!
//! `tests/lockstep/lockstep.contract.yaml` and `tests/lockstep/decisions.json`
//! are byte-identical twins of Pharos's copies. Pharos runs the same table
//! through its own engine, so any divergence in the shared vocabulary —
//! parsing, merge, or validation — fails in one repo or the other.

use std::collections::BTreeMap;
use std::path::PathBuf;

use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderName};
use limen::compare::diff::DiffLimits;
use limen::compare::{compare, Captured};
use limen::contract::load;
use limen::contract::merge::resolve;
use limen::contract::model::{BehavioralRules, ComparisonRules};
use serde::Deserialize;
use serde_json::{json, Value};
use url::Url;

fn lockstep_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/lockstep")
}

fn decision_table() -> Value {
    serde_json::from_str(
        &std::fs::read_to_string(lockstep_dir().join("decisions.json"))
            .expect("decision table must be readable"),
    )
    .expect("decision table must parse")
}

/// Render resolved rules as the engine-neutral JSON shape `decisions.json`
/// uses. The resolved model serializes to that shape directly — the one
/// divergence is the timestamp precision Limen historically spells `millis`,
/// which the neutral table records canonically as `milliseconds`. Canonicalizing
/// here is what makes "both spellings resolve alike" assertable.
fn rules_as_facts(rules: &ComparisonRules) -> Value {
    let mut facts = serde_json::to_value(rules).expect("resolved rules serialize");
    for entry in facts["json"]["normalize_timestamps"]
        .as_array_mut()
        .expect("normalize_timestamps is an array")
    {
        if entry["precision"] == json!("millis") {
            entry["precision"] = json!("milliseconds");
        }
    }
    facts
}

#[test]
fn lockstep_contract_loads_and_validates_clean() {
    let contract = load::load_file(&lockstep_dir().join("lockstep.contract.yaml"))
        .expect("lockstep fixture must parse");
    assert_eq!(contract.service, "example-service");
    let semantic_issues = load::validate_semantics(&contract);
    assert!(
        semantic_issues.is_empty(),
        "semantic issues: {semantic_issues:?}"
    );
    let path_issues = load::validate_paths(&contract);
    assert!(path_issues.is_empty(), "JSONPath issues: {path_issues:?}");
}

#[test]
fn lockstep_decision_table_matches_the_merge_engine() {
    let dir = lockstep_dir();
    let contract =
        load::load_file(&dir.join("lockstep.contract.yaml")).expect("lockstep fixture must parse");
    let decisions = decision_table();

    let cases = decisions["merge_cases"]
        .as_array()
        .expect("`merge_cases` must be an array");
    assert!(!cases.is_empty(), "the decision table must not be empty");

    let empty = BehavioralRules::default();
    for case in cases {
        let case_id = case["id"].as_str().expect("case id");
        let route_id = case["route_id"].as_str().expect("case route_id");
        let route = contract
            .route(route_id)
            .unwrap_or_else(|| panic!("case {case_id}: contract has no route {route_id:?}"));
        let resolved = resolve(
            &contract.defaults,
            route.comparison.as_ref().unwrap_or(&empty),
        );
        assert_eq!(
            rules_as_facts(&resolved),
            case["expected_rules"],
            "merge case {case_id} (route {route_id}) resolved differently"
        );
    }

    // Every fixture route must be pinned, or a vocabulary feature can drift
    // unnoticed between the two engines.
    for route in &contract.routes {
        assert!(
            cases
                .iter()
                .any(|c| c["route_id"].as_str() == Some(route.id.as_str())),
            "route {:?} has no merge case in decisions.json",
            route.id
        );
    }
}

/// One row of the shared verdict table: two responses, the rules they are
/// compared under, and the verdict both engines must reach.
#[derive(Deserialize)]
struct VerdictCase {
    id: String,
    rules: BehavioralRules,
    legacy: Side,
    new: Side,
    expected: Expected,
}

/// One side of a verdict case, in the table's engine-neutral response shape.
#[derive(Deserialize)]
struct Side {
    status: u16,
    /// The URL of the request that produced this response; a relative
    /// `Location` resolves against it.
    #[serde(default)]
    request_url: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, HeaderField>,
    /// Absent means "empty on both sides", so the body dimension never decides
    /// a verdict case.
    #[serde(default)]
    body: String,
}

/// A header in the table: one value, or several for a repeated header
/// (`set-cookie`).
#[derive(Deserialize)]
#[serde(untagged)]
enum HeaderField {
    One(String),
    Many(Vec<String>),
}

#[derive(Deserialize)]
struct Expected {
    is_match: bool,
    mismatch_kinds: Vec<String>,
}

impl HeaderField {
    fn values(&self) -> &[String] {
        match self {
            HeaderField::One(value) => std::slice::from_ref(value),
            HeaderField::Many(values) => values,
        }
    }
}

impl Side {
    fn captured(&self) -> Captured {
        let mut headers = HeaderMap::new();
        for (name, field) in &self.headers {
            let name = HeaderName::from_bytes(name.as_bytes()).expect("valid header name");
            for value in field.values() {
                headers.append(&name, value.parse().expect("valid header value"));
            }
        }
        Captured {
            status: self.status,
            headers,
            body: Bytes::from(self.body.clone()),
            request_url: self
                .request_url
                .as_deref()
                .map(|url| Url::parse(url).expect("valid request_url")),
        }
    }

    /// The cookie values this side sets, used to prove none of them is ever
    /// rendered. Short values (`us`, `one`) occur as substrings of the result's
    /// own field names, so only distinctive ones are checked here; the
    /// exhaustive proof is the dedicated unit test in `compare`.
    fn cookie_values(&self) -> impl Iterator<Item = &str> {
        self.headers
            .get("set-cookie")
            .into_iter()
            .flat_map(HeaderField::values)
            .filter_map(|entry| {
                let (_, value) = entry.split(';').next()?.split_once('=')?;
                Some(value.trim())
            })
            .filter(|value| value.len() >= 5)
    }

    /// The values this side's `Location` carries under a secret-bearing query
    /// parameter name (an OAuth `code`, an `access_token`), which the rendered
    /// result must mask just as it masks cookie values.
    fn sensitive_query_values(&self) -> impl Iterator<Item = &str> {
        const SENSITIVE: &[&str] = &["access_token", "token", "api_key", "apikey", "code"];
        self.headers
            .get("location")
            .into_iter()
            .flat_map(HeaderField::values)
            .filter_map(|value| value.split_once('?'))
            .flat_map(|(_, query)| query.split('&'))
            .filter_map(|pair| pair.split_once('='))
            .filter(|(name, _)| SENSITIVE.contains(&name.to_ascii_lowercase().as_str()))
            .map(|(_, value)| value)
            .filter(|value| value.len() >= 5)
    }
}

/// Replay the shared verdict table through the real comparison engine. Pharos
/// replays the same cases through its own, so a divergence in `set_cookie` /
/// `location` semantics fails in one repo or the other.
#[test]
fn lockstep_verdict_table_matches_the_comparison_engine() {
    let cases: Vec<VerdictCase> = serde_json::from_value(decision_table()["verdict_cases"].clone())
        .expect("`verdict_cases` must be an array of verdict cases");
    assert!(!cases.is_empty(), "the verdict table must not be empty");

    for case in &cases {
        let id = &case.id;
        // Verdict cases carry their rules inline, so they resolve over empty
        // service defaults — the same `resolve` the merge cases exercise.
        let rules = resolve(&BehavioralRules::default(), &case.rules);
        let result = compare(
            &rules,
            &DiffLimits::default(),
            &case.legacy.captured(),
            &case.new.captured(),
        );

        assert_eq!(
            result.is_match(),
            case.expected.is_match,
            "verdict case {id}: unexpected verdict ({result:?})"
        );
        assert_eq!(
            result.mismatch_kinds(),
            case.expected.mismatch_kinds,
            "verdict case {id}: unexpected mismatch kinds ({result:?})"
        );
        // No cookie value, and no secret-bearing Location query value, may reach
        // an output surface (limen invariant 5).
        let rendered = serde_json::to_string(&result).expect("result serializes");
        for secret in case
            .legacy
            .cookie_values()
            .chain(case.new.cookie_values())
            .chain(case.legacy.sensitive_query_values())
            .chain(case.new.sensitive_query_values())
        {
            assert!(
                !rendered.contains(secret),
                "verdict case {id}: rendered result leaked {secret:?}"
            );
        }
    }
}
