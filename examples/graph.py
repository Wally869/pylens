"""Aliasing, global mutation, and container-heavy traversal.

Exercises: a local alias of a parameter that is then mutated (alias tracking must blame the
parameter), a `global` write, set/list/dict usage, and a return that aliases an argument.
"""

_VISITS = 0


def walk(adj, start):
    """Iterative DFS. Records a global visit counter as a side effect."""
    global _VISITS
    seen = set()
    stack = [start]
    order = []
    while stack:
        node = stack.pop()
        if node in seen:
            continue
        seen.add(node)
        _VISITS += 1                    # global write
        order.append(node)
        for nxt in adj.get(node, []):
            stack.append(nxt)
    return order                        # -> sequence (fresh list)


def merge_into(dst, src):
    """Copy src into dst, in place, via an alias of dst."""
    alias = dst                         # alias = parameter
    for k, v in src.items():
        alias[k] = v                    # mutates `dst` through the alias
    return dst                          # return aliases `dst`
