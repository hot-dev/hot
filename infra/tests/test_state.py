"""The state and teardown guide names real resources, and the plan workflow
guards every durable one."""
from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = (ROOT.parent / ".github/workflows/selfhost-plan.yml").read_text()
SECTION = (ROOT / "README.md").read_text().split("## State, drift, and teardown", 1)[1].split("\n## ", 1)[0]


def resources(cloud):
    text = "\n".join(p.read_text() for p in (ROOT / f"opentofu/stacks/{cloud}/single-machine").glob("*.tf"))
    return {f"{kind}.{name}" for kind, name in re.findall(r'^resource "(\w+)" "(\w+)"', text, re.M)}


class StateGuide(unittest.TestCase):
    def test_named_resources_exist(self):
        named = {re.sub(r"\[\d+\]$", "", address) for address in re.findall(r"`'?((?:aws|google)_\w+\.\w+(?:\[\d+\])?)'?`", SECTION)}
        named |= {re.sub(r"\[\d+\]$", "", a) for a in re.findall(r"'((?:aws|google)_\w+\.\w+(?:\[\d+\])?)'", SECTION)}
        self.assertGreater(len(named), 15)
        known = resources("aws") | resources("gcp")
        self.assertEqual(sorted(named - known), [])

    def test_the_plan_workflow_guards_every_durable_resource(self):
        durable = re.compile(re.search(r"DURABLE: '([^']+)'", WORKFLOW).group(1).replace("\\\\", "\\"))
        table = SECTION.split("**Durable resources.**", 1)[1].split("\n\n", 2)[1]
        addresses = re.findall(r"`((?:aws|google)_\w+\.\w+)(?:\[\d+\])?`", table)
        self.assertGreaterEqual(len(addresses), 9)
        for address in addresses:
            self.assertRegex(address + ".", durable)
        self.assertIn('if [[ -s durable.txt && "$ALLOW_DATA_DELETION" != "true" ]]', WORKFLOW)
        self.assertIn("if: inputs.apply && !inputs.drift_check", WORKFLOW)


if __name__ == "__main__":
    unittest.main()
