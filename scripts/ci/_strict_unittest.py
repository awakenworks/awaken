"""Keep unittest's CLI while refusing abbreviated gate arguments."""
import unittest


class Program(unittest.TestProgram):
    def _getParentArgParser(self):
        parser = super()._getParentArgParser()
        parser.allow_abbrev = False
        return parser

    def _getMainArgParser(self, parent):
        parser = super()._getMainArgParser(parent)
        parser.allow_abbrev = False
        return parser

    def _getDiscoveryArgParser(self, parent):
        parser = super()._getDiscoveryArgParser(parent)
        parser.allow_abbrev = False
        return parser


def main(*args, **kwargs) -> None:
    Program(*args, **kwargs)
