"""Cause/effect coverage for flattened contract union reuse."""
import copy
import json
from pathlib import Path
import tempfile
import unittest
from schema_to_typescript import ContractCodegenError, _reuse_named_unions, finalize_stdin_typescript, normalize_schema


class NamedUnionTests(unittest.TestCase):
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
