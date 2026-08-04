"""Complex free functions: in-place nested mutation, return-aliasing, and multi-path returns.

Exercises: nested-sequence subscript mutation, returning a mutated argument (aliasing),
return-kind unions over branches (str | int | none), explicit raise, default parameter.
"""


def normalize_rows(matrix):
    """Scale each row by its sum, in place. Returns the same (now mutated) matrix."""
    for row in matrix:
        s = sum(row)
        if s == 0:
            continue
        for i in range(len(row)):
            row[i] = row[i] / s          # mutates a nested element of `matrix`
    return matrix                        # return aliases the argument


def classify(score, threshold=0.5):
    if score is None:
        raise ValueError("score is required")
    if score >= threshold:
        return "pass"                    # -> str
    if score < 0:
        return -1                        # -> int
    return None                          # -> none


def clamp_all(values, lo, hi):
    """Clamp each value into [lo, hi], mutating `values` in place."""
    for i in range(len(values)):
        if values[i] < lo:
            values[i] = lo
        elif values[i] > hi:
            values[i] = hi
    return len(values)                   # -> int (does NOT return the list)
