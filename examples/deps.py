"""Every import style, mixing resolvable stdlib modules with missing/fake packages.

A missing *module-level* import makes the whole module fail to load, so `summarize` below
can't execute here — that failure is recorded honestly. The point of this file is the
`dependencies` section of `pylens record`: it catalogs each import and reports which resolve.
"""

import os                                          # plain — resolves
import numpy as np                                 # alias — MISSING
import xml.etree.ElementTree as ET                 # dotted path + alias — resolves
from collections import OrderedDict, defaultdict   # several names from one lib — resolves
from json import dumps                             # single name — resolves
from nonexistent_pkg import widget                 # MISSING
from fakelib.sub.deep import Thing                 # dotted path from a MISSING lib
from os.path import *                              # star import — resolves
from . import sibling                              # relative — not resolvable standalone


def summarize(rows):
    return np.array(rows).mean()
