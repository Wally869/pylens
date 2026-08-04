"""A class whose methods use real libraries — class-method records alongside resolved deps."""

import hashlib
from datetime import datetime


class Ledger:
    def __init__(self):
        self.entries = []
        self.total = 0

    def add(self, label, amount):
        if amount < 0:
            raise ValueError("amount must be non-negative")
        self.total += amount                       # aug-assign on self attr
        self.entries.append((label, amount))       # mutate self.entries
        return self.total

    def digest(self):
        return hashlib.sha256(repr(self.entries).encode()).hexdigest()
