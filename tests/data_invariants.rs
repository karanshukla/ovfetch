//! Rules every shipped data file must keep, whatever releases it records.
//! Tests here check shape only, so a discover PR that adds releases never
//! has to edit them.

use ovfetch::data::Data;
use ovfetch::version::Ver;

fn shipped() -> Data {
    Data::load(None).unwrap()
}

#[test]
fn every_ledger_entry_has_a_sha256_and_a_known_id() {
    let data = shipped();
    assert!(!data.ledger.list.is_empty());
    for e in &data.ledger.list {
        assert!(
            e.digest.len() == 64
                && e.digest
                    .bytes()
                    .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
            "{}: {}",
            e.id,
            e.digest
        );
        let parts: Vec<&str> = e.id.split('/').collect();
        let known = matches!(parts.as_slice(), ["pypi", p, f] if !p.is_empty() && !f.is_empty())
            || matches!(parts.as_slice(), ["github", o, r, t, a]
                if [o, r, t, a].iter().all(|s| !s.is_empty()));
        assert!(known, "{}", e.id);
    }
}

#[test]
fn no_ledger_entry_is_recorded_twice() {
    let data = shipped();
    let mut ids: Vec<&str> = data.ledger.list.iter().map(|e| e.id.as_str()).collect();
    ids.sort();
    let before = ids.len();
    ids.dedup();
    assert_eq!(before, ids.len());
}

#[test]
fn every_recipe_pins_a_ledger_input_and_a_full_commit() {
    let data = shipped();
    for b in &data.builds.list {
        assert!(
            data.ledger.get(&b.openvino_wheel).is_some(),
            "{}",
            b.openvino_wheel
        );
        assert_eq!(b.git_commit.len(), 40, "{}", b.git_commit);
        assert!(Ver::parse(&b.openvino).is_some(), "{}", b.openvino);
        assert!(Ver::parse(&b.onnxruntime).is_some(), "{}", b.onnxruntime);
        assert!(
            b.cmake_defines
                .iter()
                .any(|d| d == "CMAKE_INSTALL_RPATH=$ORIGIN")
        );
    }
}
