// Check that the SDK version the docs tell users to install is published in
// every registry. The SDKs release in lockstep, so the version pinned in
// resources/docs/api/sdks.md must exist on npm, PyPI, crates.io, Maven Central
// and the Go module proxy.
//
// Run with: node scripts/check-sdk-doc-pins.mjs [path/to/sdks.md]
// It needs the network, so it belongs in a scheduled or release job rather than
// per-PR CI. Exit codes: 0 all published, 1 a pinned version is missing or the
// page's pins disagree, 2 a registry query failed.
import { readFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';

// Each pattern captures a version the page tells users to install.
const PIN_PATTERNS = [
  { sdk: 'Go', pattern: /go get github\.com\/hot-dev\/hot-go@v(\d+\.\d+\.\d+)/g },
  { sdk: 'Java (Gradle)', pattern: /dev\.hot:hot-sdk:(\d+\.\d+\.\d+)/g },
  { sdk: 'Java (Maven)', pattern: /<artifactId>hot-sdk<\/artifactId>\s*<version>(\d+\.\d+\.\d+)<\/version>/g },
  { sdk: 'JavaScript', pattern: /@hot-dev\/sdk@(\d+\.\d+\.\d+)/g },
  { sdk: 'Python', pattern: /hot-dev==(\d+\.\d+\.\d+)/g },
  { sdk: 'Rust', pattern: /hot-dev@(\d+\.\d+\.\d+)|hot-dev = "(\d+\.\d+\.\d+)"/g },
  { sdk: 'Rust', pattern: /After upgrading to (\d+\.\d+\.\d+)/g },
];

export const REGISTRIES = [
  { name: 'npm @hot-dev/sdk', url: (v) => `https://registry.npmjs.org/@hot-dev%2Fsdk/${v}` },
  { name: 'PyPI hot-dev', url: (v) => `https://pypi.org/pypi/hot-dev/${v}/json` },
  { name: 'crates.io hot-dev', url: (v) => `https://crates.io/api/v1/crates/hot-dev/${v}` },
  {
    name: 'Maven Central dev.hot:hot-sdk',
    url: (v) => `https://repo1.maven.org/maven2/dev/hot/hot-sdk/${v}/hot-sdk-${v}.pom`,
  },
  { name: 'Go proxy github.com/hot-dev/hot-go', url: (v) => `https://proxy.golang.org/github.com/hot-dev/hot-go/@v/v${v}.info` },
];

// Return every pinned version on the page as [{ sdk, version, line }].
export function findPins(markdown) {
  const pins = [];
  for (const { sdk, pattern } of PIN_PATTERNS) {
    for (const match of markdown.matchAll(pattern)) {
      const version = match.slice(1).find(Boolean);
      const line = markdown.slice(0, match.index).split('\n').length;
      pins.push({ sdk, version, line });
    }
  }
  return pins.sort((a, b) => a.line - b.line);
}

// The single lockstep version, or an error naming the disagreeing pins.
export function lockstepVersion(pins) {
  const versions = [...new Set(pins.map((pin) => pin.version))];
  if (versions.length === 0) return { error: 'no SDK version is pinned on the page' };
  if (versions.length > 1) {
    const where = pins.map((pin) => `${pin.sdk} ${pin.version} (line ${pin.line})`).join(', ');
    return { error: `the SDKs release in lockstep, but the page pins several versions: ${where}` };
  }
  return { version: versions[0] };
}

async function published(registry, version) {
  const response = await fetch(registry.url(version), {
    headers: { 'User-Agent': 'hot-docs-sdk-pin-check (https://github.com/hot-dev/hot)' },
  });
  if (response.status === 404) return false;
  if (!response.ok) throw new Error(`${registry.name}: HTTP ${response.status}`);
  return true;
}

async function main() {
  const path = process.argv[2] ?? new URL('../resources/docs/api/sdks.md', import.meta.url);
  const pins = findPins(readFileSync(path, 'utf8'));
  const { version, error } = lockstepVersion(pins);
  if (error) {
    console.error(error);
    process.exit(1);
  }
  let missing = 0;
  for (const registry of REGISTRIES) {
    let found;
    try {
      found = await published(registry, version);
    } catch (failure) {
      console.error(`Registry query failed: ${failure.message}`);
      process.exit(2);
    }
    if (!found) missing += 1;
    console.log(`${found ? 'ok        ' : 'MISSING   '} ${registry.name} ${version}`);
  }
  if (missing) {
    console.error(`The docs pin SDK ${version}, which ${missing} registr${missing === 1 ? 'y does' : 'ies do'} not have yet.`);
    console.error('Publish the SDKs before deploying these docs, or pin the published version.');
  }
  process.exit(missing ? 1 : 0);
}

// Run only as a script: `node --test scripts/*.mjs` imports this file too.
if (import.meta.url === pathToFileURL(process.argv[1]).href && !process.env.NODE_TEST_CONTEXT) {
  await main();
}
