//! Integration test for the persistent fork-server worker pool (`NsjailPool`).
//!
//! Like the flow tests, this is jailed and **skips** when the sandbox isn't provisioned
//! (`scripts/provision-sandbox.sh`) rather than falling back to an unsandboxed run.

use pylens::exec::{CallInput, NsjailPool, Sandbox, probe};
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
        .call("def f(xs):\n    xs.append(1)\n    return xs", "f", &[json!([0])], &[])
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
            &[],
        )
        .expect("call p");
    let clean = pool
        .call("def q(x):\n    return len(x)", "q", &[json!([1, 2, 3])], &[])
        .expect("call q");
    assert!(clean.ok);
    assert_eq!(clean.ret, json!(3), "builtins patch leaked across pool calls");
}

#[test]
fn call_batch_matches_the_same_calls_made_one_at_a_time() {
    if let Err(e) = probe() {
        eprintln!("SKIP call_batch_matches_the_same_calls_made_one_at_a_time: {e}");
        return;
    }

    let pool = NsjailPool::new(1).expect("spawn pool");
    let src = "def f(x):\n    if x < 0:\n        raise ValueError('neg')\n    return x * 2\n";

    let inputs = [json!(1), json!(-1), json!(3), json!(0)];
    let call_inputs: Vec<CallInput> =
        inputs.iter().map(|v| (std::slice::from_ref(v), &[][..])).collect();
    let batched = pool
        .call_batch(src, "f", &call_inputs, None, None)
        .expect("call_batch f");
    assert_eq!(batched.len(), inputs.len());

    for (v, batched_result) in inputs.iter().zip(&batched) {
        let single = pool.call(src, "f", std::slice::from_ref(v), &[]).expect("call f");
        assert_eq!(batched_result.ok, single.ok);
        assert_eq!(batched_result.ret, single.ret);
        assert_eq!(
            batched_result.exception.as_ref().map(|e| &e.ty),
            single.exception.as_ref().map(|e| &e.ty)
        );
    }
}

#[test]
fn call_batch_isolates_sibling_grandchildren_from_a_module_level_mutation() {
    if let Err(e) = probe() {
        eprintln!(
            "SKIP call_batch_isolates_sibling_grandchildren_from_a_module_level_mutation: {e}"
        );
        return;
    }

    // The primed-fork batch path execs the module once in an intermediate child, then forks
    // one grandchild per item from that primed state. If a grandchild's mutation to a
    // module-level global (`G`) leaked to its siblings — via the intermediate child, or via
    // fork not actually copying on write — every case after the first would observe a `G`
    // longer than one element. Each case must instead see the module exactly as freshly
    // loaded: `G` starts empty for every one of them.
    let pool = NsjailPool::new(1).expect("spawn pool");
    let src = "G = []\ndef f(x):\n    G.append(x)\n    return len(G)\n";

    let inputs = [json!(1), json!(2), json!(3), json!(4), json!(5)];
    let call_inputs: Vec<CallInput> =
        inputs.iter().map(|v| (std::slice::from_ref(v), &[][..])).collect();
    let batched = pool
        .call_batch(src, "f", &call_inputs, None, None)
        .expect("call_batch f");
    assert_eq!(batched.len(), inputs.len());

    for r in &batched {
        assert!(r.ok, "case should run: {:?}", r.error);
        assert_eq!(r.ret, json!(1), "a sibling grandchild's mutation to G leaked");
    }
}
