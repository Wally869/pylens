//! Order-preserving deduplication of an [`EffectSignature`](crate::model::EffectSignature)'s
//! may-set fields, applied once per function after its body walk completes.

use crate::model::Mutation;

pub(super) fn dedup<T: Clone + PartialEq>(v: &mut Vec<T>) {
    let mut seen: Vec<T> = Vec::new();
    v.retain(|x| {
        if seen.contains(x) {
            false
        } else {
            seen.push(x.clone());
            true
        }
    });
}

pub(super) fn dedup_mutations(v: &mut Vec<Mutation>) {
    let mut seen: Vec<Mutation> = Vec::new();
    v.retain(|x| {
        if seen.contains(x) {
            false
        } else {
            seen.push(x.clone());
            true
        }
    });
}
