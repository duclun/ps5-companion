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
    // Storefront source plans keep the exact original catalog evidence.
    let path = root.join("releases/PPSA00001.json");
    let mut records: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    records[0]["catalogProvenance"] = serde_json::json!({"store":"orbit","commit":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","release_id":"original"});
    records[0]["requiredFirmware"] = serde_json::json!("11.20");
    fs::write(path, serde_json::to_vec(&records).unwrap()).unwrap();
    let plan = plan_source(&source_config, "PPSA00001", "direct").unwrap();
    assert_eq!(plan.release.catalog_provenance.unwrap()["store"], "orbit");
    assert_eq!(plan.release.required_firmware.as_deref(), Some("11.20"));

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

#[test]
fn alternate_editions_keep_separate_sources_and_provenance() {
    let root = fixture_root("editions");
    fs::write(
        root.join("catalog.js"),
        r#"
var games = [{"id":"PPSA00001","title":"Example","platform":"ps5","work":7,"region":"USA"}];
var twins = [{"id":"PPSA00002","title":"Example","platform":"ps5","work":7,"region":"EUR"}];
throw new Error("must not execute");
"#,
    )
    .unwrap();
    for (id, release, artifact) in [
        ("PPSA00001", "us", "PPSA00003"),
        ("PPSA00002", "eu", "PPSA00002"),
    ] {
        fs::write(root.join("releases").join(format!("{id}.json")), format!(r#"[{{"id":"{release}","format":"zip","filename":"game.zip","titleId":"{artifact}","platform":"ps5","region":"ASIA","url":"https://example.com/game.zip"}}]"#)).unwrap();
    }
    let cfg = config(&root);
    let games = search_games(&cfg, "Example").unwrap();
    assert_eq!(games.len(), 2);
    assert!(games[1].alternate_edition);
    assert_eq!(games[1].work, Some(7));
    assert_eq!(
        search_games(&cfg, "Example EUR").unwrap()[0].id,
        "PPSA00002"
    );
    assert_eq!(list_sources(&cfg, "PPSA00002").unwrap()[0].id, "eu");
    assert!(plan_source(&cfg, "PPSA00002", "us").is_err());
    let plan = plan_source(&cfg, "PPSA00001", "us").unwrap();
    assert_eq!(plan.schema_version, 2);
    assert_eq!(plan.title_id, "PPSA00001");
    assert_eq!(plan.artifact_title_id, "PPSA00003");
    assert_eq!(plan.release.region.as_deref(), Some("ASIA"));
    assert!(plan.provenance.origin.contains("releases"));
    // Repeated IDs cannot silently shadow an edition or borrow its sources.
    let catalog = fs::read_to_string(root.join("catalog.js"))
        .unwrap()
        .replace("PPSA00002", "PPSA00001");
    fs::write(root.join("catalog.js"), catalog).unwrap();
    assert!(search_games(&cfg, "Example").is_err());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn release_identity_rejects_cross_platform_and_invalid_ids() {
    let root = fixture_root("identity");
    write_fixture(&root);
    let cfg = config(&root);
    for identity in [
        r#""titleId":"CUSA00001""#,
        r#""titleId":"../bad""#,
        r#""titleId":"PPSA00001","platform":"ps4""#,
        r#""titleId":"""#,
    ] {
        fs::write(
            root.join("releases/PPSA00001.json"),
            format!(r#"[{{"id":"bad","format":"zip","filename":"game.zip",{identity}}}]"#),
        )
        .unwrap();
        assert!(list_sources(&cfg, "PPSA00001").is_err());
    }
    fs::remove_dir_all(root).unwrap();
}
