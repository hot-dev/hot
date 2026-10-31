// Query OSV (https://osv.dev) for advisories against the App's vendored browser libraries.
// Run with: node scripts/check-vendored-js.mjs
// It needs the network, so it belongs in a scheduled or release job rather than per-PR CI.
// The per-PR browser-security test checks that the files still match the manifest.
import { readFileSync } from 'node:fs';

const manifest = JSON.parse(readFileSync(new URL('../resources/app/assets/js/vendor.json', import.meta.url), 'utf8'));
const queries = manifest.files.map(({ package: name, version }) => ({ package: { name, ecosystem: 'npm' }, version }));
const response = await fetch('https://api.osv.dev/v1/querybatch', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ queries }),
});
if (!response.ok) {
    console.error(`OSV query failed: HTTP ${response.status}`);
    process.exit(2);
}
const { results } = await response.json();
let advisories = 0;
results.forEach((result, index) => {
    const { file, package: name, version, ignored_advisories: ignored = [] } = manifest.files[index];
    // An ignored advisory names a reason it can't apply to how the App uses the library.
    const reviewed = new Set(ignored.map(({ id }) => id));
    const ids = (result.vulns ?? []).map((vuln) => vuln.id).filter((id) => !reviewed.has(id));
    advisories += ids.length;
    const note = ids.length ? ': ' + ids.join(', ') : reviewed.size ? ` (reviewed: ${[...reviewed].join(', ')})` : '';
    console.log(`${ids.length ? 'VULNERABLE' : 'ok'.padEnd(10)} ${name}@${version} (${file})${note}`);
});
process.exit(advisories ? 1 : 0);
