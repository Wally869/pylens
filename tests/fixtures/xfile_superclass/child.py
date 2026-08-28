from base import Base


class SameModuleBase:
    def setup(self):
        self.ready = True


class LocalChild(SameModuleBase):
    def setup(self):
        SameModuleBase.setup(self)


class CrossFileChild(Base):
    def setup(self):
        Base.setup(self)
