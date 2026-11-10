//! Client programs read Hub replies leniently, and every place client code turns JSON into a typed
//! value is accounted for.
use std::collections::BTreeMap;
use std::fmt::Debug;
use std::path::Path;

use agentscm::hub::reply::{self, ReplyError};
use agentscm::hub::work_http::actionable_status::ActionableStatus;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

const NEWER: &str = "added_by_a_newer_hub";

/// Checks that a client reads `sample`, a reply of type `T`, when a newer Hub adds to it.
///
/// A new field is put on each object in turn, and each text value in turn is replaced with one
/// no enum has. Where the Hub's own strict reading of `T` refuses the result as an unknown field,
/// the client's reading must give the same value as before. Where it refuses an unknown variant,
/// the client's reading must name that value. Returns how many of each were found.
pub fn reads_newer<T: DeserializeOwned + PartialEq + Debug>(sample: &Value) -> (usize, usize) {
    let strict: T = serde_json::from_value(sample.clone()).expect("the sample is a reply of T");
    assert_eq!(reply::decode::<T>(sample).as_ref(), Ok(&strict));
    let mut places = Vec::new();
    find_places(sample, String::new(), &mut places);
    let (mut fields, mut values) = (0, 0);
    for place in places {
        let mut newer = sample.clone();
        match newer.pointer_mut(&place).expect("a place in the sample") {
            Value::Object(map) => {
                map.insert(NEWER.into(), json!({"id": [1]}));
            }
            text => *text = json!(NEWER),
        }
        let Err(refusal) = serde_json::from_value::<T>(newer.clone()) else {
            continue;
        };
        let refusal = refusal.to_string();
        if refusal.starts_with(&format!("unknown field `{NEWER}`")) {
            assert_eq!(reply::decode::<T>(&newer).as_ref(), Ok(&strict), "{place}");
            fields += 1;
        } else if refusal.starts_with(&format!("unknown variant `{NEWER}`")) {
            match reply::decode::<T>(&newer) {
                Err(ReplyError::UnknownValue { value, .. }) => assert_eq!(value, NEWER, "{place}"),
                other => panic!("{place}: {other:?}"),
            }
            assert_eq!(reply::check::<T>(&newer), Ok(()), "{place}");
            values += 1;
        }
    }
    (fields, values)
}

/// The JSON pointer of every object and every text value.
fn find_places(value: &Value, at: String, places: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, inner) in map {
                let key = key.replace('~', "~0").replace('/', "~1");
                find_places(inner, format!("{at}/{key}"), places);
            }
            places.push(at);
        }
        Value::Array(items) => {
            for (index, inner) in items.iter().enumerate() {
                find_places(inner, format!("{at}/{index}"), places);
            }
        }
        Value::String(_) => places.push(at),
        _ => {}
    }
}

#[test]
fn the_shared_status_reads_with_a_newer_hubs_fields_and_values() {
    for fixture in ["actionable-status.json", "needs-you-surfaces.json"] {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/hub/fixtures")
            .join(fixture);
        let sample: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let (fields, values) = reads_newer::<ActionableStatus>(&sample);
        assert!(fields > 5 && values > 5, "{fixture}: {fields} {values}");
    }
}

/// Every place a client source file turns JSON into a typed value with `serde_json`.
///
/// A client must read a Hub reply with `agentscm::hub::reply`, because `serde_json` applies the
/// strictness the Hub's types carry for requests, and one new field then breaks every installed
/// client. `tests/fixtures/client-decodes.txt` lists each call to `serde_json::from_*` in a file
/// that can send an HTTP request, one line per call, under a comment saying what the calls read.
/// A new call fails this test until it reads through `agentscm::hub::reply` or is listed as
/// reviewed; a listed call that is gone fails it too. Calls are keyed by their own source line,
/// not counted per file, so changes that add or remove different calls do not collide.
#[test]
fn client_code_reads_hub_replies_only_through_the_lenient_reader() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut found: BTreeMap<(String, String), usize> = BTreeMap::new();
    let mut folders = vec![root.join("src")];
    while let Some(folder) = folders.pop() {
        for entry in std::fs::read_dir(folder).unwrap() {
            let path = entry.unwrap().path();
            let name = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            // The Hub and the other servers read requests, which stay strict.
            if name.starts_with("src/hub/") || name.starts_with("src/server/") {
                continue;
            }
            if path.is_dir() {
                folders.push(path);
            } else if name.ends_with(".rs") && !name.ends_with("/tests.rs") {
                let text = std::fs::read_to_string(&path).unwrap();
                let code = text.split("\n#[cfg(test)]\nmod ").next().unwrap();
                if !(code.contains("reqwest") || code.contains("forge_http")) {
                    continue;
                }
                for line in code.lines() {
                    for _ in line.matches("serde_json::from_") {
                        let call = (name.clone(), line.trim().to_owned());
                        *found.entry(call).or_default() += 1;
                    }
                }
            }
        }
    }
    let mut listed: BTreeMap<(String, String), usize> = BTreeMap::new();
    let ledger = std::fs::read_to_string(root.join("tests/fixtures/client-decodes.txt")).unwrap();
    for line in ledger.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let (path, call) = line
            .split_once(' ')
            .expect("a path, then the call's source line");
        *listed
            .entry((path.to_owned(), call.trim().to_owned()))
            .or_default() += 1;
    }
    assert_eq!(
        found, listed,
        "a client's `serde_json::from_*` calls changed; read a Hub reply with \
         agentscm::hub::reply, and list any other call in tests/fixtures/client-decodes.txt \
         (a reformatted listed line needs its ledger line updated too)"
    );
}
