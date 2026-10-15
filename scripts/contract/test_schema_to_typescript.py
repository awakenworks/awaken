"""Cause/effect coverage for flattened contract union reuse."""
import copy
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from schema_to_typescript import (
    ContractCodegenError,
    _reuse_named_unions,
    build_codegen_schema,
    finalize_stdin_typescript,
    normalize_schema,
)


class NamedUnionTests(unittest.TestCase):
    def test_generation_outputs_have_platform_independent_checkout_bytes(self):
        # Each generated-output family is byte-compared by the real pipeline.
        # Attribute regression must fail on Linux too, before a Windows clone
        # introduces CRLF. This command only reads this source repository.
        root = Path(__file__).resolve().parents[2]
        outputs = [
            "contracts/model-config.d.ts",
            "contracts/model-schemas.generated.json",
            "contracts/anthropic-managed/upstream-oracle.generated.json",
            "crates/server/awaken-protocol-managed/contracts/sdk-support.generated.json",
            "packages/managed-sdk-oracle/fixtures/generated/oldest.ts",
            "packages/managed-sdk-oracle/src/current-wire.generated.ts",
        ]
        checked = subprocess.run(
            ["git", "-C", str(root), "check-attr", "eol", "--", *outputs],
            env={key: value for key, value in os.environ.items() if not key.startswith("GIT_")},
            check=True, capture_output=True, text=True, encoding="utf-8",
        )
        self.assertEqual(checked.stdout.splitlines(), [f"{path}: eol: lf" for path in outputs])

    def test_generated_artifacts_keep_lf_bytes_across_native_platforms(self):
        # Checked-in artifacts and Bash headers use LF. Every Python writer
        # must preserve that byte format, even when native Windows defaults to
        # CRLF and its input came from a CRLF checkout. Values remain unchanged.
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            schema = {"type": "string", "description": "第一行\nsecond line"}
            source, output = root / "schema.json", root / "normalized.json"
            source.write_bytes(json.dumps(schema, indent=2, ensure_ascii=False).replace("\n", "\r\n").encode("utf-8"))
            normalize_schema(source, output)
            self.assertNotIn(b"\r", output.read_bytes())
            self.assertEqual(json.loads(output.read_bytes()), schema)

            bundle = root / "bundle.json"
            bundle.write_text(json.dumps({"schemas": {
                name: {"title": name, **schema}
                for name in ("ApiDialect", "ApiError", "ProfileCandidate")
            }}), encoding="utf-8")
            build_codegen_schema(bundle, output)
            self.assertNotIn(b"\r", output.read_bytes())
            self.assertEqual(json.loads(output.read_bytes())["$defs"]["ApiDialect"]["description"], schema["description"])

            typescript = root / "generated.d.ts"
            typescript.write_bytes(b"// by `undefined`'s JSON-Schema\r\nexport type Value = string;\r\n")
            finalize_stdin_typescript(typescript)
            self.assertEqual(typescript.read_bytes(), b"// by the generated JSON Schema\nexport type Value = string;\n")

    def test_decision_table(self):
        # Graph: properties + union + exactly one identical named union => ref.
        # R1 all true: retain field/required assertions; R2 absent properties,
        # R3 no matching owner, R4 ambiguous owners: preserve original schema.
        branches = [{"$ref": "#/$defs/Http"}, {"$ref": "#/$defs/Stdio"}]
        for keyword in ("anyOf", "oneOf"):
            for rule in range(1, 5):
                with self.subTest(keyword=keyword, rule=rule):
                    node = {keyword: branches, "properties": {"name": {"type": "string"}}, "required": ["name"]}
                    definitions = {"Binding": node, "Transport": {keyword: branches, "description": "transport"}}
                    if rule == 2:
                        del node["properties"]
                    if rule == 3:
                        del definitions["Transport"]
                    if rule == 4:
                        definitions["Other"] = {keyword: branches}
                    before = copy.deepcopy(definitions)
                    _reuse_named_unions(definitions)
                    if rule == 1:
                        self.assertNotIn(keyword, node)
                        self.assertEqual(node["allOf"], [{"$ref": "#/$defs/Transport"}])
                        self.assertEqual(node["properties"], before["Binding"]["properties"])
                        self.assertEqual(node["required"], ["name"])
                    else:
                        self.assertEqual(definitions, before)


class CodegenNewlineTests(unittest.TestCase):
    def test_native_codegen_emits_identical_utf8_lf_for_each_input_newline(self):
        # LF, CRLF and mixed inputs must produce the same complete bytes. This
        # also preserves Unicode and schema content: line normalization cannot
        # compensate for an altered field or TypeScript body.
        schema = {"type": "object", "description": "配置", "properties": {"name": {"type": "string"}}}
        typescript = "/* by `undefined`'s JSON-Schema: 配置 */\nexport type Name = string;\n"
        expected = typescript.replace("by `undefined`'s JSON-Schema", "by the generated JSON Schema").encode()
        for newline in ("lf", "crlf", "mixed"):
            with self.subTest(newline=newline), tempfile.TemporaryDirectory() as directory:
                source = Path(directory) / "source.json"
                output = Path(directory) / "output.json"
                declaration = Path(directory) / "output.d.ts"
                def input_bytes(text):
                    if newline == "crlf":
                        text = text.replace("\n", "\r\n")
                    elif newline == "mixed":
                        text = text.replace("\n", "\r\n", 1)
                    return text.encode()
                source.write_bytes(input_bytes(json.dumps(schema, indent=2, ensure_ascii=False) + "\n"))
                declaration.write_bytes(input_bytes(typescript))
                normalize_schema(source, output)
                finalize_stdin_typescript(declaration)
                self.assertEqual(json.loads(output.read_bytes()), schema)
                self.assertEqual(output.read_bytes(), (json.dumps(schema, indent=2, ensure_ascii=False) + "\n").encode())
                self.assertEqual(declaration.read_bytes(), expected)

    def test_missing_stdin_marker_refuses_without_changing_bytes(self):
        with tempfile.TemporaryDirectory() as directory:
            declaration = Path(directory) / "output.d.ts"
            original = b"export type Name = string;\r\n"
            declaration.write_bytes(original)
            with self.assertRaises(ContractCodegenError):
                finalize_stdin_typescript(declaration)
            self.assertEqual(declaration.read_bytes(), original)


if __name__ == "__main__":
    unittest.main()
