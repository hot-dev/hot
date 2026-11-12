#!/usr/bin/env python3
"""Tests for check-demo-doc-pins.py. Run: python3 scripts/test-check-demo-doc-pins.py"""

import importlib.util
import json
import pathlib
import tempfile
import unittest

SCRIPT = pathlib.Path(__file__).with_name("check-demo-doc-pins.py")
spec = importlib.util.spec_from_file_location("check_demo_doc_pins", SCRIPT)
pins = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pins)

PAGE = """# Hot Chat

Uses `hot.dev/hot-ai` and `hot.dev/anthropic`. Set `HOT_API_KEY`.
See [hot.hot](https://github.com/hot-dev/hot-demos/blob/main/hot-chat/hot.hot).

```bash
npm install
npm run dev
```

| Role | Command | What it does |
|------|---------|--------------|
| Write | `/remember <text>` | store a note |

The full agent adds `/forget`.

## Project Layout

```text
hot-chat/
  hot.hot                 # the project
  hot/
    src/
      agent.hot
```
"""

DEMO = {
    "hot-chat/hot.hot": 'hot.project.hot-chat.deps {\n  "hot.dev/hot-ai": "1.8.1",\n  "hot.dev/anthropic": "1.2.4",\n}\n',
    "hot-chat/.env.example": "HOT_API_KEY=\n",
    "hot-chat/package.json": json.dumps({"scripts": {"dev": "next dev"}}),
    "hot-chat/hot/src/agent.hot": 'on-remember fn (event) { starts-with(event.data.text, "/remember") }\n',
    "hot-chat/node_modules/x/index.js": 'const forget = "/forget"; // HOT_OTHER_KEY\n',
}


def checkout(files):
    root = pathlib.Path(tempfile.mkdtemp())
    for name, text in files.items():
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
    return root


def facts_of(page=PAGE, name="hot-chat.md"):
    facts, problems = pins.page_facts(name, page)
    return {f"demos/{name}": facts}, problems


class FactTests(unittest.TestCase):
    def test_a_page_states_paths_packages_commands_env_and_scripts(self):
        facts, problems = facts_of()
        self.assertEqual(problems, [])
        self.assertEqual(
            pins.as_json(facts)["demos/hot-chat.md"],
            {
                "paths": ["hot-chat", "hot-chat/hot", "hot-chat/hot.hot", "hot-chat/hot/src", "hot-chat/hot/src/agent.hot"],
                "packages": {"hot.dev/anthropic": None, "hot.dev/hot-ai": None},
                # `/forget` is prose about another project, not a table row.
                "commands": ["/remember"],
                "env": ["HOT_API_KEY"],
                "npm-scripts": ["dev"],
            },
        )

    def test_a_page_can_state_a_version(self):
        facts, _ = facts_of("`hot.dev/hot-ai 1.8.0`\n\n```hot\n\"hot.dev/anthropic\": \"1.2.4\",\n```\n")
        self.assertEqual(
            facts["demos/hot-chat.md"]["packages"],
            {"hot.dev/hot-ai": "1.8.0", "hot.dev/anthropic": "1.2.4"},
        )

    def test_a_link_to_another_branch_is_a_problem(self):
        _, problems = facts_of("[x](https://github.com/hot-dev/hot-demos/tree/hot-3.0/hot-chat)\n")
        self.assertEqual(problems, ["hot-chat.md:1: links to the `hot-3.0` branch, not `main`"])

    def test_the_index_page_states_links_only(self):
        page = "`hot.dev/hot-ai` `HOT_API_KEY` [x](https://github.com/hot-dev/hot-demos/tree/main/my-news)\n"
        facts, _ = facts_of(page, "index.md")
        self.assertEqual(pins.as_json(facts), {"demos/index.md": {"paths": ["my-news"]}})


class VerifyTests(unittest.TestCase):
    def test_true_facts_pass(self):
        facts, _ = facts_of()
        self.assertEqual(pins.verify(facts, checkout(DEMO)), [])

    def test_each_kind_of_fact_is_checked(self):
        page = PAGE.replace("agent.hot", "gone.hot").replace("npm run dev", "npm run build")
        page = page.replace("`hot.dev/anthropic`", "`hot.dev/openai`, `hot.dev/hot-ai 1.8.0`, `HOT_OTHER_KEY`")
        page = page.replace("| Write |", "| Forget | `/forget` | forgets |\n| Write |")
        facts, _ = facts_of(page)
        self.assertEqual(
            pins.verify(facts, checkout(DEMO)),
            [
                "demos/hot-chat.md: hot-demos has no `hot-chat/hot/src/gone.hot`",
                "demos/hot-chat.md: says `hot.dev/hot-ai` 1.8.0, but `hot-chat/hot.hot` pins 1.8.1",
                "demos/hot-chat.md: `hot-chat/hot.hot` does not depend on `hot.dev/openai`",
                # node_modules is not the demo's own source.
                "demos/hot-chat.md: no source file of `hot-chat` handles `/forget`",
                "demos/hot-chat.md: nothing in `hot-chat` reads or sets `HOT_OTHER_KEY`",
                "demos/hot-chat.md: `hot-chat/package.json` has no `build` script",
            ],
        )

    def test_a_page_needs_its_demo_directory(self):
        facts, _ = facts_of("# Gone\n", "gone.md")
        self.assertEqual(
            pins.verify(facts, checkout(DEMO)),
            ["demos/gone.md: hot-demos has no `gone` directory for this page"],
        )


class ManifestTests(unittest.TestCase):
    VERSIONS = {"hot.dev/hot-ai": "1.8.1", "hot.dev/anthropic": "1.2.5"}

    def manifest(self, page=PAGE):
        facts, _ = facts_of(page)
        return {"facts": pins.as_json(facts)}

    def test_pages_that_match_the_manifest_pass(self):
        facts, _ = facts_of()
        self.assertEqual(pins.compare(facts, self.manifest(), self.VERSIONS), [])

    def test_a_new_or_dropped_fact_fails(self):
        facts, _ = facts_of(PAGE.replace("`HOT_API_KEY`", "`HOT_API_URL`"))
        self.assertEqual(
            pins.compare(facts, self.manifest(), self.VERSIONS),
            [
                "demos/hot-chat.md: states env `HOT_API_URL`, which the manifest does not record",
                "demos/hot-chat.md: no longer states env `HOT_API_KEY`, which the manifest records",
            ],
        )

    def test_a_changed_version_fails(self):
        facts, _ = facts_of(PAGE.replace("`hot.dev/hot-ai`", "`hot.dev/hot-ai 1.8.1`"))
        self.assertEqual(
            pins.compare(facts, self.manifest(), self.VERSIONS),
            ["demos/hot-chat.md: states `hot.dev/hot-ai` at 1.8.1, and the manifest records no version"],
        )

    def test_recorded_packages_must_be_in_this_checkout(self):
        page = PAGE.replace("`hot.dev/hot-ai`", "`hot.dev/hot-ai 1.9.0`, `hot.dev/gone`")
        facts, _ = facts_of(page)
        self.assertEqual(
            pins.compare(facts, self.manifest(page), self.VERSIONS),
            [
                "demos/hot-chat.md: `hot.dev/gone` is not a package in hot/pkg",
                "demos/hot-chat.md: the demo pins `hot.dev/hot-ai` 1.9.0, newer than hot/pkg's 1.8.1",
            ],
        )

    def test_the_checked_in_manifest_is_in_the_shape_the_check_writes(self):
        manifest = json.loads(pins.MANIFEST.read_text(encoding="utf-8"))
        self.assertEqual(manifest["repository"], pins.REPOSITORY)
        self.assertRegex(manifest["commit"], r"^[0-9a-f]{40}$")
        facts, problems = pins.read_pages()
        self.assertEqual(problems, [])
        self.assertEqual(manifest["facts"], pins.as_json(facts))


if __name__ == "__main__":
    unittest.main()
