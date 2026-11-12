#!/usr/bin/env python3
"""Tests for check-doc-links.py. Run: python3 scripts/test-check-doc-links.py"""

import importlib.util
import pathlib
import tempfile
import unittest

SCRIPT = pathlib.Path(__file__).with_name("check-doc-links.py")
spec = importlib.util.spec_from_file_location("check_doc_links", SCRIPT)
links = importlib.util.module_from_spec(spec)
spec.loader.exec_module(links)

PACKAGES = {"hot.dev/hot-std": {"::hot::store"}, "hot-std": {"::hot::store"}}


def check(files):
    """Problems for a docs tree given as {relative path: text}."""
    with tempfile.TemporaryDirectory() as tmp:
        docs = pathlib.Path(tmp)
        for name, text in files.items():
            path = docs / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text, encoding="utf-8")
        pages = links.load_pages(docs)
        problems, _ = links.check_links(docs, pages, PACKAGES)
        return problems + links.check_nav(docs, pages)


NAV = "- [Guide](guide)\n  - [Errors](guide/errors)\n"
TREE = {
    "nav.md": NAV,
    "index.md": "# Home\n\n[Guide](/docs/guide)\n",
    "guide/index.md": "# Guide\n\n## The `retry` field\n\n## Limits {#limits}\n\n## Twice\n\n## Twice\n",
    "guide/errors.md": "# Errors\n\n## Handling errors\n",
}


def tree(**changes):
    return {**TREE, **changes}


class SlugTests(unittest.TestCase):
    def test_runs_of_other_characters_become_one_dash(self):
        self.assertEqual(links.slugify("What's new -- in 3.0?", True), "what-s-new-in-3-0")

    def test_the_two_rules_differ_on_code_spans_and_letters(self):
        text = "The `retry` field"
        self.assertEqual(links.slugify(links.heading_text(text, False), True), "the-field")
        self.assertEqual(links.slugify(links.heading_text(text, True), False), "the-retry-field")
        self.assertEqual(links.slugify("Café", True), "caf")
        self.assertEqual(links.slugify("Café", False), "café")

    def test_a_link_in_a_heading_counts_as_its_text(self):
        self.assertEqual(links.heading_text("See [the guide](/docs/guide)", True), "See the guide")


class LinkTests(unittest.TestCase):
    def test_a_sound_tree_passes(self):
        page = "# Errors\n\n## Handling errors\n\n[a](/docs/guide#limits) [b](#handling-errors) [c](https://example.com/x#y)\n"
        self.assertEqual(check(tree(**{"guide/errors.md": page})), [])

    def test_a_missing_page_fails(self):
        problems = check(tree(**{"guide/errors.md": "# Errors\n\n[x](/docs/guide/nope)\n"}))
        self.assertEqual(problems, ["guide/errors.md:3: /docs/guide/nope: no such page"])

    def test_a_missing_fragment_fails(self):
        problems = check(tree(**{"guide/errors.md": "# Errors\n\n[x](/docs/guide#limit)\n"}))
        self.assertEqual(len(problems), 1)
        self.assertIn("#limit names no heading (close: limits)", problems[0])

    def test_a_fragment_that_depends_on_a_code_span_fails(self):
        for fragment in ("the-field", "the-retry-field"):
            problems = check(tree(**{"guide/errors.md": f"# Errors\n\n[x](/docs/guide#{fragment})\n"}))
            self.assertEqual(len(problems), 1, fragment)
            self.assertIn("depends on a code span", problems[0])

    def test_a_fragment_two_headings_share_fails(self):
        problems = check(tree(**{"guide/errors.md": "# Errors\n\n[x](/docs/guide#twice)\n"}))
        self.assertEqual(len(problems), 1)
        self.assertIn("more than one heading", problems[0])

    def test_the_sites_own_absolute_links_are_checked(self):
        problems = check(tree(**{"guide/errors.md": "# Errors\n\n[x](https://hot.dev/docs/nope)\n"}))
        self.assertEqual(problems, ["guide/errors.md:3: https://hot.dev/docs/nope: no such page"])

    def test_a_relative_link_fails(self):
        problems = check(tree(**{"guide/index.md": "# Guide\n\n[x](errors)\n"}))
        self.assertEqual(len(problems), 1)
        self.assertIn("relative link", problems[0])

    def test_links_in_code_are_left_alone(self):
        page = "# Errors\n\n`[x](/docs/nope)`\n\n```md\n[x](/docs/nope)\n## Not a heading\n```\n"
        self.assertEqual(check(tree(**{"guide/errors.md": page})), [])

    def test_package_links_name_real_packages_and_namespaces(self):
        page = "# Errors\n\n[a](/pkg/hot-std) [b](/pkg/hot.dev/hot-std/hot/store#put)\n"
        self.assertEqual(check(tree(**{"guide/errors.md": page})), [])
        page = "# Errors\n\n[a](/pkg/hot.dev/hot-std/hot/stor) [b](/pkg/hot.dev/nope)\n"
        problems = check(tree(**{"guide/errors.md": page}))
        self.assertEqual(len(problems), 2)
        self.assertIn("declares no namespace ::hot::stor", problems[0])
        self.assertIn("no package hot.dev/nope", problems[1])

    def test_an_unknown_site_path_fails(self):
        page = "# Errors\n\n[a](/pricing) [b](/prices)\n"
        problems = check(tree(**{"guide/errors.md": page}))
        self.assertEqual(len(problems), 1)
        self.assertIn("/prices", problems[0])


class NavTests(unittest.TestCase):
    def test_nav_must_list_each_page_once(self):
        self.assertEqual(
            check(tree(**{"nav.md": "- [Guide](guide)\n"})),
            ["nav.md: guide/errors.md is not listed"],
        )
        self.assertEqual(
            check(tree(**{"nav.md": NAV + "- [Gone](gone)\n- [Guide](guide)\n"})),
            ["nav.md: gone names no page", "nav.md: guide is listed 2 times"],
        )


if __name__ == "__main__":
    unittest.main()
