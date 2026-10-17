//! Every place that states AgentSCM's own license says the same thing.
use std::fs;

const ID: &str = "FSL-1.1-ALv2";

fn read(path: &str) -> String {
    fs::read_to_string(format!("{}/{path}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

#[test]
fn the_license_text_and_every_metadata_field_agree() {
    let license = read("LICENSE");
    assert!(license.starts_with("# Functional Source License, Version 1.1, ALv2 Future License"));
    assert!(license.contains("Copyright 2026 DUVATL, Inc."));
    assert_eq!(license, read("tools/portable-verifier/LICENSE"));
    for (path, needle) in [
        ("Cargo.toml", format!("license = \"{ID}\"")),
        (
            "tools/portable-verifier/Cargo.toml",
            format!("license = \"{ID}\""),
        ),
        (
            "packages/npm/package.json",
            format!("\"license\": \"{ID}\""),
        ),
        ("Dockerfile", format!("image.licenses=\"{ID}\"")),
        (
            ".github/workflows/release.yml",
            format!("image.licenses={ID}"),
        ),
        ("NOTICE", ID.to_owned()),
        (
            "tools/portable-verifier/sbom.cdx.json",
            format!("\"id\": \"{ID}\""),
        ),
    ] {
        assert!(read(path).contains(&needle), "{path} does not say {needle}");
    }
}
