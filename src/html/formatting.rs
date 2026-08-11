use serde_json::Value;

/// Render a `Shape` value (as it appears in JSON: a scalar tag string, or a tagged
/// `{"seq"|"set"|"map": ...}` object) compactly — mirrors `report::shape_to_string`.
pub fn shape_value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Object(map) => {
            if let Some(elem) = map.get("seq") {
                format!("seq<{}>", shape_value_to_string(elem))
            } else if let Some(elem) = map.get("set") {
                format!("set<{}>", shape_value_to_string(elem))
            } else if let Some(kv) = map.get("map") {
                let key = kv.get("key").map(shape_value_to_string).unwrap_or_else(|| "any".to_string());
                let value = kv.get("value").map(shape_value_to_string).unwrap_or_else(|| "any".to_string());
                format!("map<{key},{value}>")
            } else if let Some(name) = map.get("instance").and_then(Value::as_str) {
                name.to_string()
            } else {
                "any".to_string()
            }
        }
        _ => "any".to_string(),
    }
}

fn mutation_target_string(t: &Value) -> String {
    let root = t.get("root").and_then(Value::as_str).unwrap_or("?");
    match t.get("name").and_then(Value::as_str) {
        Some(n) => format!("{root}:{n}"),
        None => root.to_string(),
    }
}

pub fn mutation_to_string(m: &Value) -> String {
    let target = m.get("target").map(mutation_target_string).unwrap_or_else(|| "?".to_string());
    let via = m.get("via").and_then(Value::as_str).unwrap_or("?");
    match m.get("name").and_then(Value::as_str) {
        Some(n) => format!("{target} via {via}({n})"),
        None => format!("{target} via {via}"),
    }
}

pub fn values_to_strings(arr: &[Value]) -> Vec<String> {
    arr.iter().filter_map(Value::as_str).map(str::to_string).collect()
}
