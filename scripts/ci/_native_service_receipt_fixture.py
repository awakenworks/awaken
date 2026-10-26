"""Receipt data for native-runner refusal tests; it never starts a service."""
import json
from _native_postgres_scopes import postgres_phases
from check_rust_test_execution import selection_commands

FIXTURE_OWNER = "fixture-owner"


def postgres_receipts(base, scope):
    paths=[]
    for index, phase in enumerate(postgres_phases(scope)):
        path=base/"test-timings"/str(index);path.mkdir(parents=True)
        arguments=list(phase.arguments)
        _, floors=selection_commands(arguments)
        cases=sorted(set(floors))
        document={"schema_version":1,"kind":"required-libtest","status":"passed","exit_code":0,
                  "source_unchanged":True,"source_digest":"source","selected_cases":cases,
                  "executed_cases":[[case,"ok"] for case in cases],"required_cases":floors,
                  "arguments":arguments,"postgres_fixture_required":True,"postgres_fixture_configured":True,
                  "execution_inputs":{"fixture":{"kind":"postgres","owner":FIXTURE_OWNER},
                                      "crash_backend":dict(phase.environment).get("AWAKEN_CRASH_BACKEND")}}
        (path/"results.json").write_text(json.dumps(document),encoding="utf-8")
        (path/"list.log").write_text("".join(case+": test\n" for case in cases),encoding="utf-8")
        (path/"results.log").write_text("".join("test "+case+" ... ok\n" for case in cases)
            +f"test result: ok. {len(cases)} passed; 0 failed; 0 ignored;\n",encoding="utf-8")
        paths.append(path)
    return paths
