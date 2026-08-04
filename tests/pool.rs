//! Integration test for the persistent fork-server worker pool (`NsjailPool`).
//!
//! Like the flow tests, this is jailed and **skips** when the sandbox isn't provisioned
//! (`scripts/provision-sandbox.sh`) rather than falling back to an unsandboxed run.

use pylens::exec::{NsjailPool, Sandbox, probe};
use serde_json::json;

#[test]
fn pool_executes_and_isolates_across_calls() {
    if let Err(e) = probe() {
        eprintln!("SKIP pool_executes_and_isolates_across_calls: {e}");
        return;
    }

    let pool = NsjailPool::new(2).expect("spawn pool");

    // A mutation + return-aliasing call.
    let r = pool
        .call("def f(xs):\n    xs.append(1)\n    return xs", "f", &[json!([0])])
        .expect("call f");
    assert!(r.ok, "f should run: {:?}", r.error);
    assert_eq!(r.ret, json!([0, 1]));
    assert_eq!(r.return_aliases_arg, Some(0));

    // A second call through the pool: a source that monkeypatches builtins must NOT affect a
    // later call (each request runs in its own forked child inside the jail).
    let _ = pool
        .call(
            "import builtins\ndef p(x):\n    builtins.len = lambda z: 999\n    return len(x)",
            "p",
            &[json!([1, 2, 3])],
        )
        .expect("call p");
    let clean = pool
        .call("def q(x):\n    return len(x)", "q", &[json!([1, 2, 3])])
        .expect("call q");
    assert!(clean.ok);
    assert_eq!(clean.ret, json!(3), "builtins patch leaked across pool calls");
}
