"""Interprocedural TypeError propagation: a callee's parameter-rooted may-raise must survive a
call through a caller whose own parameter is untyped.

Regression guard for the parameter-shape-suppression bug: `helper`'s `x` gets pinned to `int` by
its own `x < 0` guard, but that pin is only a hypothesis about how `helper` happens to be used —
it constrains no caller. `caller`'s `v` stays `Shape::Any`, so passing a non-int through `caller`
into `helper` must still surface as an implicit `TypeError` on `caller`, propagated from the
callee.
"""


def helper(x):
    if x < 0:
        raise ValueError("negative")
    return x


def caller(v):
    return helper(v)
