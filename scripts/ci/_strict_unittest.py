"""Keep unittest's CLI while refusing abbreviated gate arguments."""
import unittest
import time


class ObservedResult(unittest.TextTestResult):
    """Keep unittest outcomes while recording cases in the enclosing build."""
    def startTest(self, test):
        self._case_started = time.monotonic()
        self._case_outcome = 'passed'
        super().startTest(test)

    def addFailure(self, test, err):
        self._case_outcome = 'failed'
        super().addFailure(test, err)

    def addError(self, test, err):
        self._case_outcome = 'failed'
        super().addError(test, err)

    def addSubTest(self, test, subtest, err):
        if err is not None:
            self._case_outcome = 'failed'
        super().addSubTest(test, subtest, err)

    def addSkip(self, test, reason):
        if self._case_outcome != 'failed':
            self._case_outcome = 'skipped'
        super().addSkip(test, reason)

    def addUnexpectedSuccess(self, test):
        self._case_outcome = 'failed'
        super().addUnexpectedSuccess(test)

    def addExpectedFailure(self, test, err):
        self._case_outcome = 'expected-failure'
        super().addExpectedFailure(test, err)

    def stopTest(self, test):
        super().stopTest(test)
        from ci_performance import record_stage
        record_stage('unittest:' + test.id(), time.monotonic() - self._case_started,
                     int(self._case_outcome == 'failed'), semantic_result=self._case_outcome)


def observed_runner(**kwargs):
    return unittest.TextTestRunner(resultclass=ObservedResult, **kwargs)


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
