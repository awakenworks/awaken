"""Canonical Worker fixture laws, discovered source owners and actual TAP evidence.

The served PostgreSQL runner and its independently hooked controller tests share
this pure owner. Native service startup remains in the native gate; discovery
does not introduce another service entry point or a pre-commit service run.
"""
from pathlib import Path
import re

SKIP=re.compile(r"^[ \t]*(?:skip(?:ped|ping)?\b|\[skip\])",re.M|re.I)

WORKER_FIXTURE_CASES = (
    'the external Worker fixture explicitly supports the current Rust execution protocol',
    'the Provider capability fixture follows the exact Rust credential codec',
    'the external Worker fixture names the frozen child-toolset format exactly',
    'the external Worker fixture names the frozen tool-result journal format exactly',
    'the generic Worker fixture never infers or upgrades journal capability evidence',
    'the Provider placement fixture replaces only the superseded Host model capability',
    'the Provider placement fixture retains caller inputs and exact credential demands',
    'the Provider placement fixture never invents protocol capability evidence',
    'the shared ThreadCommit fixture preserves the contract-owned hash decisions',
    'the terminal ThreadCommit fixture includes its exact committed lifecycle boundary',
    'the claimed commit encoder preserves caller-owned lifecycle evidence',
)

def discover_fixture_tests(root: Path) -> tuple[str, ...]:
    """Select the owners of mandatory fixture laws from their actual sources.

    File discovery may overselect; actual unique TAP outcomes remain the pass
    authority. Missing laws reject before Node's empty argv could autodiscover
    unrelated tests or a native execution bundle could be built.
    """
    required=set(WORKER_FIXTURE_CASES)
    found=set()
    paths=[]
    for path in sorted((root/"e2e").glob("*_fixture.test.mjs")):
        source=path.read_text(encoding="utf-8")
        declared={name for name in required if name in source}
        if declared:
            paths.append(path.relative_to(root).as_posix())
            found.update(declared)
    if required-found:
        raise ValueError("native Worker fixture laws have no discovered source: "+", ".join(sorted(required-found)))
    return tuple(paths)

def validate_worker_fixture_probe(text: str, code: int) -> list[str]:
    rows=re.findall(r"^ok ([1-9][0-9]*) - ([^\r\n]+)$",text,re.M)
    names=[name for _,name in rows]
    if (code or SKIP.search(text) or re.search(r"^not ok ",text,re.M)
            or not set(WORKER_FIXTURE_CASES)<=set(names) or len(names)!=len(set(names))
            or [int(index) for index,_ in rows]!=list(range(1,len(rows)+1))
            or re.findall(r"^1\.\.([0-9]+)$",text,re.M)!=[str(len(rows))]):
        raise ValueError("required native Worker fixture codecs did not execute completely")
    for name,count in (("tests",len(rows)),("pass",len(rows)),("fail",0),("cancelled",0),("skipped",0),("todo",0)):
        if re.findall(r"^# "+name+r" ([0-9]+)$",text,re.M)!=[str(count)]:
            raise ValueError("required native Worker fixture codec totals are incomplete")
    return names
