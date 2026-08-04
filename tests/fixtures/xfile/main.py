import os

from .util import touch


def caller(data):
    touch(data)


def uses_external():
    return os.getcwd()
