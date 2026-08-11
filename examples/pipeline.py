"""Local instance construction and method resolution: same-module classes, tracked locals."""


class Account:
    def __init__(self):
        self.balance = 0
        self.history = []

    def deposit(self, amount):
        if amount < 0:
            raise ValueError("amount must be non-negative")
        self.balance += amount
        self.history.append(amount)
        return self.balance

    def label(self, tag):
        self.history.append(tag)
        return self.history


class Basket:
    def __init__(self):
        self.items = []

    def label(self, tag):
        self.items.append(tag)
        return self.items


def open_and_fund(amount):
    account = Account()
    if amount != 0:
        account.deposit(amount)
    return account.balance


def either_account_or_basket(use_account, tag):
    if use_account:
        holder = Account()
    else:
        holder = Basket()
    holder.label(tag)
    return holder
