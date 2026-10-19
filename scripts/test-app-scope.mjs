// Exercise the browser scope adapter without a browser or network dependency.
// Run with: node --test scripts/test-app-scope.mjs
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';

const source = readFileSync(new URL('../crates/hot_app/templates/scope_controls.html', import.meta.url), 'utf8')
  .replace(/^<script>\s*/, '').replace(/\s*<\/script>\s*$/, '')
  .replace(/const orgId = .*;/, "const orgId = 'org-a';")
  .replace(/const envId = .*;/, "const envId = 'env-a';");

function harness() {
  const handlers = new Map();
  const nodes = [];
  const requests = [];
  class Form {
    constructor(action, method = 'get') { this.action = action; this.method = method; this.tagName = 'FORM'; this.inputs = []; }
    querySelector(selector) { return this.inputs.find(input => selector === `[name="${input.name}"]`); }
    appendChild(input) { this.inputs.push(input); }
    getAttribute() { return this.action; }
    setAttribute(_, value) { this.action = value; }
  }
  const window = {
    location: { href: 'https://hot.example/stores', origin: 'https://hot.example' },
    fetch: (...args) => { requests.push(args); },
    EventSource: class { constructor(url) { this.url = url; } },
  };
  const document = {
    addEventListener(name, callback) { handlers.set(name, callback); },
    querySelectorAll() { return nodes; },
    createElement() { return {}; },
  };
  vm.runInNewContext(source, { window, document, URL, Headers, Request, HTMLFormElement: Form });
  return { window, handlers, nodes, requests, Form };
}

test('page scope persists in native GET forms and submit events', () => {
  const h = harness();
  const form = new h.Form('/runs?search=error');
  h.nodes.push(form);
  h.handlers.get('DOMContentLoaded')();
  assert.equal(new URL(form.action, h.window.location.href).searchParams.get('org_id'), 'org-a');
  assert.deepEqual(form.inputs.map(({ name, value }) => [name, value]), [['org_id', 'org-a'], ['env_id', 'env-a']]);
  h.handlers.get('submit')({ target: form });
  assert.equal(form.inputs.length, 2, 'repeated binding must not duplicate selectors');
  const explicit = new h.Form('/runs?org_id=org-b&env_id=env-b');
  h.handlers.get('submit')({ target: explicit });
  assert.deepEqual(explicit.inputs.map(({ name, value }) => [name, value]), [['org_id', 'org-b'], ['env_id', 'env-b']]);
});

test('fetch and HTMX use immutable page scope while preserving explicit targets', () => {
  const h = harness();
  h.window.fetch('/runs/retry', { method: 'POST', headers: { 'X-Custom': 'kept' } });
  let headers = h.requests[0][1].headers;
  assert.equal(headers.get('X-Hot-Org-Id'), 'org-a');
  assert.equal(headers.get('X-Hot-Env-Id'), 'env-a');
  assert.equal(headers.get('X-Custom'), 'kept');
  h.window.fetch(new Request('https://hot.example/runs?org_id=org-b&env_id=env-b', { headers: { 'X-Custom': 'request' } }));
  headers = h.requests[1][1].headers;
  assert.equal(headers.get('X-Hot-Org-Id'), 'org-b');
  assert.equal(headers.get('X-Hot-Env-Id'), 'env-b');
  assert.equal(headers.get('X-Custom'), 'request');
  const detail = { path: '/data/runs', headers: {} };
  h.handlers.get('htmx:configRequest')({ detail });
  assert.deepEqual(detail.headers, { 'X-Hot-Org-Id': 'org-a', 'X-Hot-Env-Id': 'env-a' });
});

test('streams bind scope; external requests and durable org routes stay untouched', () => {
  const h = harness();
  const stream = new h.window.EventSource('/envs/subscribe');
  assert.equal(stream.url, '/envs/subscribe?org_id=org-a&env_id=env-a');
  for (const target of ['https://other.example/upload', '/@customer/lifecycle', '/switch-org/org-b']) {
    h.window.fetch(target, { method: 'POST' });
    assert.equal(h.requests.at(-1)[1].headers, undefined);
  }
});
