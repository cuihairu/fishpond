//! F036: the committed OpenAPI fixture is the generator's input. It
//! must stay byte-identical to [`fishpond_server::http::openapi::document`],
//! so SDK generation always sees exactly what the server serves.

use std::fs;
use std::path::Path;

const FIXTURE: &str = "tests/fixtures/openapi.json";

fn rendered() -> String {
    let mut text = serde_json::to_string_pretty(&fishpond_server::http::openapi::document())
        .expect("document serializes");
    text.push('\n');
    text
}

#[test]
fn openapi_fixture_matches_the_served_document() {
    let path = Path::new(FIXTURE);
    let committed = fs::read_to_string(path).unwrap_or_else(|err| {
        panic!("fixture {FIXTURE} missing ({err}); run the ignored regenerate_fixture test")
    });
    assert_eq!(
        committed,
        rendered(),
        "fixture drifted from document(); regenerate with: cargo test -p fishpond-server --test f036_openapi_fixture regenerate_fixture -- --ignored"
    );
}

/// Regenerates the committed fixture after a deliberate contract
/// change. Run with:
/// `cargo test -p fishpond-server --test f036_openapi_fixture regenerate_fixture -- --ignored`
#[test]
#[ignore = "writes the fixture; run explicitly"]
fn regenerate_fixture() {
    fs::create_dir_all("tests/fixtures").expect("fixtures dir");
    fs::write(FIXTURE, rendered()).expect("fixture write");
    println!("wrote {FIXTURE}");
}
