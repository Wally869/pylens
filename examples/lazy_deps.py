"""Imports inside function bodies. The module loads, so per-function dependency resolution
shows up in the cases: a missing import raises `ModuleNotFoundError` at call time, a real one
works. The `dependencies` section still catalogs all of them (imports nested in functions are
collected too).
"""

import json                                  # module-level — resolves


def to_json(obj):
    return json.dumps(obj)                   # works


def render(points):
    import matplotlib                        # MISSING — raises at call time
    return matplotlib.plot(points)


def load_config(text):
    import yaml                              # usually MISSING (PyYAML)
    return yaml.safe_load(text)
