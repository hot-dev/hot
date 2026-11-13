#!/usr/bin/env python3
"""Check the internal links, anchors and navigation of the docs.

Usage:
    python3 scripts/check-doc-links.py [DOCS_DIR]

DOCS_DIR defaults to resources/docs. The check reads files only: it needs no
build and no network, and it leaves links to other sites alone.

It fails when:
  - a `/docs/...` link (or `https://hot.dev/docs/...`) names no page;
  - a `#fragment` names no heading of the page it points at;
  - a link is relative (`[x](errors)`). A page such as `language/index.md`
    is served at `/docs/language`, where a browser resolves a relative link
    against `/docs/`, not `/docs/language/`. Write `/docs/...` in full;
  - a `/pkg/...` link names a package that is not in hot/pkg, or a namespace
    that package does not declare;
  - any other site-absolute link is not in SITE_PATHS below;
  - nav.md lists a page that does not exist, lists a page twice, or leaves a
    page out.

Fragments. A heading gets its anchor from its text, and the two renderers of
these pages disagree about code spans: `hot_docs` drops their text
(`markdown_to_html_with_toc` collects text events only), while the app's docs
links keep it (`extract_anchors` in crates/hot_app/src/templates/mod.rs).
A heading can also carry an explicit id, `## Title {#my-id}`, which both
honor. So a fragment must be an explicit id, or the heading's slug under both
rules. One that is right under only one rule fails, and so does one that two
headings of the page share, since it reaches only the first.
"""

import html
import pathlib
import re
import sys

REPO_ROOT = pathlib.Path(__file__).resolve().parent.parent

# Pages of the site outside /docs and /pkg that the docs may link to.
SITE_PATHS = {"/pricing"}

FENCE = re.compile(r"^ {0,3}(`{3,}|~{3,})")
HEADING = re.compile(r"^ {0,3}(#{1,6})\s+(.*?)\s*#*\s*$")
HEADING_ID = re.compile(r"\s*\{#([^}\s]+)\}\s*$")
HTML_ID = re.compile(r"""\bid=["']([^"']+)["']""")
CODE_SPAN = re.compile(r"(`+)(.+?)\1")
INLINE_LINK = re.compile(r"!?\[((?:[^\]\[]|\[[^\]]*\])*)\]\(\s*<?([^)\s>]*)>?(?:\s+\"[^\"]*\")?\s*\)")
REFERENCE_DEF = re.compile(r"^ {0,3}\[[^\]]+\]:\s*<?(\S+?)>?(?:\s|$)")
HREF = re.compile(r"""\b(?:href|src)=["']([^"']+)["']""")
SCHEME = re.compile(r"^[A-Za-z][A-Za-z0-9+.-]*:")
OWN_SITE = re.compile(r"^https?://(?:www\.)?hot\.dev(/(?:docs|pkg)(?:[/#?].*)?)$")
NS_DECL = re.compile(r"^\s*(::[A-Za-z0-9_:-]+)\s+ns\b")
PACKAGE_NAME = re.compile(r'^\s*name:\s*"([^"]+)"', re.MULTILINE)


def slugify(text, ascii_only):
    """Lowercase; each run of other characters becomes one `-`; trimmed.

    `ascii_only` is hot_docs' rule. The app's rule keeps any Unicode letter
    or digit."""
    out, dashed = [], False
    for ch in text.lower():
        keep = (ch.isascii() and ch.isalnum()) if ascii_only else ch.isalnum()
        if keep:
            out.append(ch)
            dashed = False
        elif not dashed:
            out.append("-")
            dashed = True
    return "".join(out).strip("-")


def heading_text(text, keep_code):
    """A heading's text as a renderer sees it, without its markup."""
    text = INLINE_LINK.sub(lambda m: m.group(1), text)
    text = CODE_SPAN.sub((lambda m: m.group(2)) if keep_code else "", text)
    return html.unescape(text)


def prose_lines(path):
    """Yield (line number, line) for the lines outside fenced code blocks
    and the front matter."""
    fence = None
    lines = path.read_text(encoding="utf-8").split("\n")
    in_front_matter = bool(lines) and lines[0].strip() == "---"
    for number, line in enumerate(lines, start=1):
        if in_front_matter:
            if number > 1 and line.strip() == "---":
                in_front_matter = False
            continue
        if m := FENCE.match(line):
            marker = m.group(1)
            if fence is None:
                fence = marker
            elif marker[0] == fence[0] and len(marker) >= len(fence) and not line.strip(marker[0] + " "):
                fence = None
            continue
        if fence is None:
            yield number, line


class Anchors:
    """The fragments a page answers to."""

    def __init__(self):
        self.explicit = set()
        self.without_code = []  # hot_docs' slugs, one per heading
        self.with_code = []  # the app's slugs, one per heading

    def problem(self, fragment):
        """Why `fragment` is not a sound link into this page, or None."""
        if fragment in self.explicit:
            return None
        in_docs, in_app = fragment in self.without_code, fragment in self.with_code
        if in_docs and in_app:
            if self.without_code.count(fragment) > 1 or self.with_code.count(fragment) > 1:
                return "is the anchor of more than one heading, so it reaches only the first"
            return None
        if in_docs or in_app:
            return (
                "depends on a code span in the heading: the docs server and the app"
                " give that heading different anchors. Give the heading an explicit {#id}"
            )
        near = sorted(self.explicit | set(self.without_code) | set(self.with_code))
        close = [a for a in near if a and (a in fragment or fragment in a)][:3]
        return "names no heading" + (f" (close: {', '.join(close)})" if close else "")


def page_key(relative):
    """`language/index.md` -> `language`; `index.md` -> ``; `retries.md` -> `retries`."""
    key = relative.as_posix()[: -len(".md")]
    if key == "index":
        return ""
    return key[: -len("/index")] if key.endswith("/index") else key


def load_pages(docs):
    pages = {}
    for path in sorted(docs.rglob("*.md")):
        relative = path.relative_to(docs)
        if relative.name == "nav.md" or any(part.startswith(".") for part in relative.parts):
            continue
        pages[page_key(relative)] = path
    return pages


def page_anchors(path):
    anchors = Anchors()
    for _, line in prose_lines(path):
        anchors.explicit.update(HTML_ID.findall(line))
        if not (m := HEADING.match(line)):
            continue
        text = m.group(2)
        if explicit := HEADING_ID.search(text):
            anchors.explicit.add(explicit.group(1))
            text = text[: explicit.start()]
        anchors.without_code.append(slugify(heading_text(text, keep_code=False), ascii_only=True))
        anchors.with_code.append(slugify(heading_text(text, keep_code=True), ascii_only=False))
    return anchors


def link_targets(line):
    """The link targets on a line, ignoring code spans."""
    line = CODE_SPAN.sub(lambda m: " " * len(m.group(0)), line)
    targets = [m.group(2) for m in INLINE_LINK.finditer(line)]
    targets += HREF.findall(line)
    if m := REFERENCE_DEF.match(line):
        targets.append(m.group(1))
    return targets


def package_namespaces(repo_root):
    """Route name -> declared namespaces, for each package in hot/pkg.

    A package answers to its full name (`hot.dev/hot-std`) and to the part
    after the slash (`hot-std`)."""
    found = {}
    for manifest in sorted((repo_root / "hot/pkg").glob("*/pkg.hot")):
        name = PACKAGE_NAME.search(manifest.read_text(encoding="utf-8"))
        namespaces = set()
        for source in (manifest.parent / "src").rglob("*.hot"):
            first = next((l for l in source.read_text(encoding="utf-8").splitlines() if l.strip()), "")
            if m := NS_DECL.match(first):
                namespaces.add(m.group(1))
        for route in {manifest.parent.name, name.group(1) if name else manifest.parent.name}:
            found[route] = namespaces
            found[route.split("/")[-1]] = namespaces
    return found


def package_link_problem(path, packages):
    """Why `/pkg/...` names nothing, or None. A namespace is `/hot/store`."""
    parts = [p for p in path[len("/pkg") :].split("/") if p]
    if not parts:
        return None
    for size in (2, 1):
        route = "/".join(parts[:size])
        if len(parts) >= size and route in packages:
            rest = parts[size:]
            if rest and "::" + "::".join(rest) not in packages[route]:
                return f"package {route} declares no namespace ::{'::'.join(rest)}"
            return None
    return f"no package {'/'.join(parts[:2])} in hot/pkg"


def check_links(docs, pages, packages):
    anchors = {key: page_anchors(path) for key, path in pages.items()}
    problems, checked = [], 0
    for key, path in pages.items():
        relative = path.relative_to(docs)
        for number, line in prose_lines(path):
            for target in link_targets(line):
                where = f"{relative}:{number}: {target}"
                if m := OWN_SITE.match(target):
                    target = m.group(1)
                elif SCHEME.match(target) or target.startswith("//"):
                    continue
                checked += 1
                url, _, fragment = target.partition("#")
                url = url.split("?", 1)[0]
                if url == "":
                    target_key = key
                elif url == "/docs" or url.startswith("/docs/"):
                    target_key = url[len("/docs") :].strip("/")
                    target_key = re.sub(r"\.md$", "", target_key)
                    if target_key == "index":
                        target_key = ""
                    elif target_key.endswith("/index"):
                        target_key = target_key[: -len("/index")]
                elif url == "/pkg" or url.startswith("/pkg/"):
                    if problem := package_link_problem(url, packages):
                        problems.append(f"{where}: {problem}")
                    continue
                elif url.startswith("/"):
                    if url.rstrip("/") not in SITE_PATHS:
                        problems.append(
                            f"{where}: not a docs or package page, and not in SITE_PATHS"
                        )
                    continue
                else:
                    problems.append(
                        f"{where}: relative link; write it in full, as /docs/..."
                    )
                    continue
                if target_key not in pages:
                    problems.append(f"{where}: no such page")
                elif fragment and (problem := anchors[target_key].problem(fragment)):
                    problems.append(f"{where}: #{fragment} {problem}")
    return problems, checked


def check_nav(docs, pages):
    nav = docs / "nav.md"
    if not nav.exists():
        return [f"{nav.name}: missing"]
    text = re.sub(r"<!--.*?-->", "", nav.read_text(encoding="utf-8"), flags=re.S)
    entries = [m.group(2).strip("/") for m in INLINE_LINK.finditer(text)]
    entries = ["" if entry == "index" else entry for entry in entries]
    problems = [f"nav.md: {entry or 'index'} names no page" for entry in entries if entry not in pages]
    problems += [
        f"nav.md: {entry or 'index'} is listed {entries.count(entry)} times"
        for entry in sorted(set(entries))
        if entries.count(entry) > 1
    ]
    # The home page is reached from the header, not the navigation.
    problems += [
        f"nav.md: {pages[key].relative_to(docs)} is not listed"
        for key in sorted(pages)
        if key and key not in entries
    ]
    return problems


def main(argv):
    docs = pathlib.Path(argv[1]).resolve() if len(argv) > 1 else REPO_ROOT / "resources/docs"
    pages = load_pages(docs)
    problems, checked = check_links(docs, pages, package_namespaces(REPO_ROOT))
    problems += check_nav(docs, pages)
    if problems:
        print(f"{len(problems)} problem(s) in {len(pages)} page(s), {checked} internal link(s):")
        for problem in problems:
            print(f"  {problem}")
        return 1
    print(f"All {checked} internal link(s) in {len(pages)} page(s) resolve, and nav.md lists every page")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
