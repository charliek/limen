//! The shipped example config + contract must always load, validate, and pass
//! `check-contract`. These guard against drift between the docs/examples and
//! the loaders.

use std::path::PathBuf;

use limen::config::{self, ConfigOverrides};
use limen::contract::load as contract_load;

fn repo_path(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel)
}

#[test]
fn example_config_loads_and_validates() {
    let path = repo_path("config/limen.example.yaml");
    let loaded =
        config::load::load(&path, &ConfigOverrides::default()).expect("example config should load");
    config::validate(&loaded.config, &loaded.base_dir)
        .expect("example config should pass semantic validation");
    assert!(
        !loaded.config.routes.is_empty(),
        "example config should define routes"
    );
}

#[test]
fn example_contract_check_passes() {
    let path = repo_path("config/contracts/example-service.contract.yaml");
    let contract = contract_load::load_file(&path).expect("example contract should load");
    let issues = contract_load::validate_paths(&contract);
    assert!(
        issues.is_empty(),
        "example contract should have no JSONPath issues, found: {issues:?}"
    );
}
