"""Dynamic attribute writes — the constructs the static analyzer must flag, not silently pass.

Exercises: `setattr` on `self` and on a parameter (-> dynamic_setattr unresolved_effect),
self-attribute dict mutation, and a return.
"""


class Config:
    def __init__(self):
        self.values = {}

    def update(self, **kwargs):
        for k, v in kwargs.items():
            setattr(self, k, v)         # dynamic write to self -> unresolved
            self.values[k] = v          # concrete self-attr dict mutation
        return self.values              # -> mapping (aliased)


def apply_patch(obj, patch):
    """Apply key/value patch onto an arbitrary object by dynamic attribute set."""
    for key, val in patch.items():
        setattr(obj, key, val)          # dynamic_setattr on a parameter
    return obj                          # -> aliases `obj`
