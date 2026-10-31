// Run with: node --test scripts/check-sdk-doc-pins.test.mjs
// Parses the SDK page's pins without touching the network.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

import { findPins, lockstepVersion } from './check-sdk-doc-pins.mjs';

test('the SDK page pins one lockstep version', () => {
  const page = readFileSync(new URL('../resources/docs/api/sdks.md', import.meta.url), 'utf8');
  const pins = findPins(page);
  const sdks = new Set(pins.map((pin) => pin.sdk));
  for (const sdk of ['Go', 'Java (Gradle)', 'Java (Maven)']) {
    assert.ok(sdks.has(sdk), `the page pins a ${sdk} version`);
  }
  assert.equal(lockstepVersion(pins).error, undefined);
});

test('pins that disagree are reported with their lines', () => {
  const pins = findPins([
    'go get github.com/hot-dev/hot-go@v1.3.0',
    'implementation("dev.hot:hot-sdk:1.2.0")',
  ].join('\n'));
  assert.deepEqual(pins.map((pin) => [pin.sdk, pin.version, pin.line]), [
    ['Go', '1.3.0', 1],
    ['Java (Gradle)', '1.2.0', 2],
  ]);
  assert.match(lockstepVersion(pins).error, /Go 1\.3\.0 \(line 1\), Java \(Gradle\) 1\.2\.0 \(line 2\)/);
});

test('a page with no pins is an error', () => {
  assert.match(lockstepVersion(findPins('npm install @hot-dev/sdk')).error, /no SDK version/);
});

test('a Maven pin spans lines', () => {
  const pins = findPins('<groupId>dev.hot</groupId>\n  <artifactId>hot-sdk</artifactId>\n  <version>1.3.0</version>');
  assert.deepEqual(pins.map((pin) => pin.version), ['1.3.0']);
});
