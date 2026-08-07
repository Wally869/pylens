use serde_json::Value;

pub fn purity_counts(functions: &[Value]) -> (u64, u64, u64) {
    let mut pure = 0;
    let mut impure = 0;
    let mut unknown = 0;
    for f in functions {
        match f.get("purity").and_then(Value::as_str) {
            Some("pure") => pure += 1,
            Some("impure") => impure += 1,
            Some("unknown") => unknown += 1,
            _ => {}
        }
    }
    (pure, impure, unknown)
}
