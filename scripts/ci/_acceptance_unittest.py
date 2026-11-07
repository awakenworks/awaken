"""Run a unittest entry point with a report separate from its console output."""

import json
from pathlib import Path
import runpy
import sys
import unittest

import _strict_unittest


def main() -> None:
    report, *arguments = sys.argv[1:]

    class ReportResult(_strict_unittest.ObservedResult):
        def __init__(self, *args):
            super().__init__(*args)
            self.cases = {}
            self._case_outcome = "passed"

        def startTest(self, test):
            super().startTest(test)
            self.note(test, "failed")

        def note(self, test, verdict):
            name = test.id().removeprefix("__main__.")
            self.cases[name] = verdict

        def addSuccess(self, test):
            super().addSuccess(test)
            if self.cases[test.id().removeprefix("__main__.")] != "skipped":
                self.note(test, "passed")

        def addSkip(self, test, reason):
            super().addSkip(test, reason)
            self.note(getattr(test, "test_case", test), "skipped")

        def stopTestRun(self):
            super().stopTestRun()
            with Path(report).open("x", encoding="utf-8") as output:
                json.dump({"total": self.testsRun, "cases": self.cases}, output)

    unittest.TextTestRunner.resultclass = ReportResult
    _strict_unittest.ObservedResult = ReportResult
    if arguments[:2] == ["-m", "unittest"]:
        sys.argv = ["unittest", *arguments[2:]]
        runpy.run_module("unittest", run_name="__main__", alter_sys=True)
    else:
        script, *_ = arguments
        sys.path.insert(0, str(Path(script).resolve().parent))
        sys.argv = arguments
        runpy.run_path(script, run_name="__main__")


if __name__ == "__main__":
    main()
