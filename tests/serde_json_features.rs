//! protoledger turns on no global serde_json feature, so it cannot change
//! how a dependent's serde_json behaves. Checked by behaviour, not by reading
//! Cargo.toml: each test fails exactly when the feature it names is on.

use serde_json::{Map, Value, json};

/// `preserve_order` keeps insertion order; without it a map is sorted.
#[test]
fn serde_json_maps_are_sorted_so_preserve_order_is_off() {
    let mut m = Map::new();
    m.insert("b".into(), json!(1));
    m.insert("a".into(), json!(2));
    assert_eq!(serde_json::to_string(&m).unwrap(), r#"{"a":2,"b":1}"#, "a map kept insertion order: serde_json/preserve_order is on");
}

/// `arbitrary_precision` keeps a number's source spelling; without it a number
/// is written in its shortest round-trip spelling.
#[test]
fn serde_json_numbers_are_respelled_so_arbitrary_precision_is_off() {
    let v: Value = serde_json::from_str("[1.50,1e3,0.000005]").unwrap();
    assert_eq!(serde_json::to_string(&v).unwrap(), "[1.5,1000.0,5e-6]", "a number kept its source spelling: serde_json/arbitrary_precision is on");
}
