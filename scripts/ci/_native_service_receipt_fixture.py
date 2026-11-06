"""Receipt data for native-runner refusal tests; it never starts a service."""
import json
from _native_postgres_scopes import postgres_phases
from nextest_execution import junit, listing, required_command

FIXTURE_OWNER = "fixture-owner"


def write_receipt(path, document, cases, output=""):
    """One run_required_rust_tests evidence directory: receipt, nextest list JSON and JUnit."""
    path.mkdir(parents=True, exist_ok=True)
    (path/"results.json").write_text(json.dumps(document), encoding="utf-8")
    (path/"list.json").write_text(listing(*cases), encoding="utf-8")
    first = [f"{cases[0]}|<system-out>{output}</system-out>"] if cases else []
    (path/"junit.xml").write_text(junit(*first, *cases[1:]), encoding="utf-8")
    return path


def postgres_receipts(base, scope):
    paths=[]
    for index, phase in enumerate(postgres_phases(scope)):
        arguments=list(phase.arguments)
        _, floors=required_command(arguments)
        cases=sorted(set(floors))
        document={"schema_version":2,"kind":"required-nextest","status":"passed","exit_code":0,
                  "source_unchanged":True,"source_digest":"source","selected_cases":cases,
                  "required_cases":floors,
                  "arguments":arguments,"postgres_fixture_required":True,"postgres_fixture_configured":True,
                  "execution_inputs":{"fixture":{"kind":"postgres","owner":FIXTURE_OWNER},
                                      "crash_backend":dict(phase.environment).get("AWAKEN_CRASH_BACKEND")}}
        paths.append(write_receipt(base/"test-timings"/str(index), document, cases))
    return paths
