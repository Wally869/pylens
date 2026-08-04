import os
from .util import f
from .sub.helper import g
from .missing import nope


def run(x):
    return g(f(x)) + os.getpid()
