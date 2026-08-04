"""Generators, external-library calls, unknown callees, and I/O.

Exercises: `yield` (generator), a qualified stdlib call (`re.finditer` — the kind of external
dep the static layer can't model), an unknown free callee passed an argument (-> unresolved),
a method call on a parameter, and stdout I/O.
"""

import re


def tokenize(text):
    """Generator: lowercased word tokens. Runs `re` at call time."""
    for m in re.finditer(r"\w+", text):
        yield m.group(0).lower()


def histogram(text):
    counts = {}
    for tok in tokenize(text):
        counts[tok] = counts.get(tok, 0) + 1
    return counts                         # -> mapping


def report(rows, sink):
    """Writes formatted rows to an unknown sink; calls an unresolved helper."""
    written = 0
    for r in rows:
        sink.write(format_row(r))         # `format_row` is unknown -> unresolved_effect
        written += 1
    print("wrote", written, "rows")       # stdout I/O
    return written                        # -> int
