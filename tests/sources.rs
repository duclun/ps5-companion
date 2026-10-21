use ps5_companion::{
    list_sources, plan_source, search_games, write_source_plan, SourceAccess, SourceConfig,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

fn fixture_root(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("ps5-companion-sources-{name}-{nonce}"));
    fs::create_dir_all(root.join("releases")).expect("fixture directory");
    root
}

fn config(root: &Path) -> SourceConfig {
    SourceConfig {
        catalog_dir: Some(root.to_path_buf()),
        repository: "example/metadata".to_owned(),
        reference: "fixture".to_owned(),
    }
}

fn write_fixture(root: &Path) {
    fs::write(
        root.join("catalog.js"),
        r#"// JavaScript wrapper intentionally contains more code after the JSON.
var games = [
  {"id":"PPSA00001","title":"Outer Wilds","platform":"ps5","providers":["Example"],"sizeGb":10,"extra":{"ignored":true}},
  {"id":"PPSA00002","title":"Onimusha","platform":"ps5","providers":[],"extraField":"ignored"},
  {"id":"PPSA00003","title":"Astro Bot","platform":"ps5","providers":["Example"]},
  {"id":"PPSA00004","title":"Assassin's Creed\xAE Mirage","platform":"ps5","providers":[]},
  {"id":"PPSA00005","title":"Literal \\xAE text [brackets]","platform":"ps5","providers":[]},
];
const unrelated = {"not":"catalog"};
"#,
    )
    .expect("catalog fixture");
    fs::write(
        root.join("releases").join("PPSA00001.json"),
        r#"[
  {"id":"direct","provider":"Example","format":"zip","filename":"outer.zip","url":"https://example.com/outer.zip","sizeBytes":42,"version":"1","label":"Direct","extra":true},
  {"id":"browser","provider":"MediaFire","format":"zip","filename":"outer-browser.zip","url":"https://example.com/landing","browserUrl":"https://example.com/landing","requiresBrowser":false},
  {"id":"manifest","provider":"Example","format":"json","filename":"manifest.json","manifestUrl":"https://example.com/manifest.json","category":"game","source":"fixture","flow":"manifest","record":"record-1","volume":"part1"},
  {"id":"gone","provider":"Example","format":"zip","filename":"gone.zip","url":"https://example.com/gone","unavailableReason":"removed upstream"}
]"#,
    )
    .expect("release fixture");
}

#[test]
fn parses_catalog_searches_all_terms_and_classifies_sources() {
    let root = fixture_root("lookup");
    write_fixture(&root);
    let source_config = config(&root);

    let outer = search_games(&source_config, "outer WILDS").expect("search");
    assert_eq!(outer.len(), 1);
    assert_eq!(outer[0].id, "PPSA00001");
    assert_eq!(
        search_games(&source_config, "Astro Bot").unwrap()[0].id,
        "PPSA00003"
    );
    assert_eq!(
        search_games(&source_config, "ASSASSIN CREED®").unwrap()[0].id,
        "PPSA00004"
    );
    assert_eq!(
        search_games(&source_config, r"literal \xAE text").unwrap()[0].id,
        "PPSA00005"
    );

    let sources = list_sources(&source_config, "PPSA00001").expect("sources");
    assert_eq!(sources.len(), 4);
    assert_eq!(sources[0].access(), SourceAccess::Direct);
    assert_eq!(sources[1].access(), SourceAccess::BrowserRequired);
    assert_eq!(sources[2].access(), SourceAccess::Manifest);
    assert_eq!(sources[2].volume.as_deref(), Some("part1"));
    assert_eq!(sources[3].access(), SourceAccess::Unavailable);

    let plan = plan_source(&source_config, "PPSA00001", "direct").expect("plan");
    assert_eq!(plan.access, SourceAccess::Direct);
    assert_eq!(plan.release.size_bytes, Some(42));
    assert!(
        plan.provenance.origin.starts_with("local catalog:")
            || plan.provenance.origin.starts_with("local releases:")
    );
    assert!(plan_source(&source_config, "PPSA00001", "gone").is_err());

    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn plans_are_immutable_and_unsafe_urls_are_rejected() {
    let root = fixture_root("immutable");
    write_fixture(&root);
    let source_config = config(&root);
    let plan = plan_source(&source_config, "PPSA00001", "direct").expect("plan");
    let plan_path = root.join("plan.json");
    write_source_plan(&plan_path, &plan).expect("first write");
    assert!(write_source_plan(&plan_path, &plan).is_err());

    fs::write(
        root.join("releases").join("PPSA00001.json"),
        r#"[{"id":"bad","provider":"Example","format":"zip","filename":"bad.zip","url":"http://example.com/bad.zip"}]"#,
    )
    .expect("unsafe release fixture");
    assert!(list_sources(&source_config, "PPSA00001").is_err());

    fs::write(
        root.join("catalog.js"),
        r#"var games = [{"id":"../escape","title":"Bad","platform":"ps5"}];"#,
    )
    .expect("unsafe catalog fixture");
    assert!(search_games(&source_config, "Bad").is_err());
    fs::write(root.join("catalog.js"), "x").expect("short catalog fixture");
    assert!(search_games(&source_config, "anything").is_err());
    fs::remove_dir_all(root).expect("cleanup");
}
