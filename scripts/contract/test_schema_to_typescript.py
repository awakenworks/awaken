"""Cause/effect coverage for flattened contract union reuse."""
import copy
import unittest
from schema_to_typescript import _reuse_named_unions


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


if __name__ == "__main__":
    unittest.main()
