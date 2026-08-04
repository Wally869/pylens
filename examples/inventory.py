"""A stateful class: methods that mutate `self`, raise on bad input, and return values.

Exercises: self-attribute mutation (dict subscript, `del`, augmented assign, list append),
explicit raises of several types, return-kind unions, and a method that returns aliased state.
"""


class Inventory:
    def __init__(self, capacity=100):
        self.items = {}        # name -> quantity
        self.capacity = capacity
        self.log = []

    def add(self, name, qty):
        if qty <= 0:
            raise ValueError("qty must be positive")
        if sum(self.items.values()) + qty > self.capacity:
            raise OverflowError("capacity exceeded")
        self.items[name] = self.items.get(name, 0) + qty   # aug assign on self dict
        self.log.append(("add", name, qty))                # mutate self.log
        return self.items[name]                            # -> int

    def remove(self, name, qty):
        if name not in self.items:
            raise KeyError(name)
        if qty > self.items[name]:
            raise ValueError("not enough stock")
        self.items[name] -= qty
        if self.items[name] == 0:
            del self.items[name]                           # del on self dict
        self.log.append(("remove", name, qty))
        return self.items.get(name, 0)                     # -> int

    def view(self):
        return self.items                                  # returns aliased self state
