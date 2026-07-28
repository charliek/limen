//! Cross-repo contract conformance: replay the lockstep decision table through
//! Limen's real loader and merge code.
//!
//! `tests/lockstep/lockstep.contract.yaml` and `tests/lockstep/decisions.json`
//! are byte-identical twins of Pharos's copies. Pharos runs the same table
//! through its own engine, so any divergence in the shared vocabulary —
//! parsing, merge, or validation — fails in one repo or the other.

use std::path::PathBuf;

use limen::contract::load;
use limen::contract::merge::resolve;
use limen::contract::model::{BehavioralRules, ComparisonRules};
use serde_json::{json, Value};

fn lockstep_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/lockstep")
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
    let decisions: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("decisions.json")).unwrap())
            .expect("decision table must parse");

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
