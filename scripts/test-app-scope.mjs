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
    history: { state: { tab: 'kept' }, replaceState(state, _, url) { this.state = state; this.url = url; } },
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


test('filter navigation and dynamically inserted links preserve the rendered workspace', () => {
  const h = harness();
  assert.equal(h.window.hotScopeUrl('/stores?search=customer'), '/stores?search=customer&org_id=org-a&env_id=env-a');
  assert.equal(h.window.hotScopeUrl('/@customer/children'), '/@customer/children');
  h.handlers.get('DOMContentLoaded')();
  assert.equal(h.window.history.url, '/stores?org_id=org-a&env_id=env-a');
  assert.deepEqual(h.window.history.state, { tab: 'kept' });
  // The inspector inserts this anchor after the page's initial binding.
  const anchor = { href: '/tasks/task-id', getAttribute() { return this.href; }, setAttribute(_, value) { this.href = value; } };
  h.handlers.get('click')({ target: { closest() { return anchor; } } });
  assert.equal(anchor.href, '/tasks/task-id?org_id=org-a&env_id=env-a');
  anchor.href = '/tasks/task-id?org_id=org-b&env_id=env-b';
  h.handlers.get('click')({ target: { closest() { return anchor; } } });
  assert.equal(anchor.href, '/tasks/task-id?org_id=org-b&env_id=env-b');
});

test('the store filter keeps the old tab scope when rebuilding its URL', () => {
  const h = harness();
  const template = readFileSync(new URL('../crates/hot_app/templates/stores_list.html', import.meta.url), 'utf8');
  const filterScript = template.match(/<script>\s*(function storesListFilters\(\)[\s\S]*?)<\/script>/)[1];
  const context = { window: h.window, URLSearchParams };
  vm.createContext(context);
  vm.runInContext(filterScript, context);
  const filter = context.storesListFilters();
  filter.searchQuery = 'customer records';
  filter.applyFilters();
  assert.equal(h.window.location.href, '/stores?search=customer+records&org_id=org-a&env_id=env-a');
});

test('an environment switch from an old tab keeps its org but not its environment', () => {
  const h = harness();
  assert.equal(h.window.hotScopeUrl('/switch-env/env-b'), '/switch-env/env-b?org_id=org-a');
  // The environment menus submit these forms.
  const menu = new h.Form('/envs/env-b/switch', 'post');
  h.handlers.get('submit')({ target: menu });
  assert.equal(menu.action, '/envs/env-b/switch?org_id=org-a');
  assert.deepEqual(menu.inputs, []);
  const form = new h.Form('/switch-env/env-b');
  h.handlers.get('submit')({ target: form });
  assert.deepEqual(form.inputs.map(({ name, value }) => [name, value]), [['org_id', 'org-a']]);
  h.window.fetch('/envs/env-b/switch', { method: 'POST' });
  const headers = h.requests[0][1].headers;
  assert.equal(headers.get('X-Hot-Org-Id'), 'org-a');
  assert.equal(headers.get('X-Hot-Env-Id'), null);
  const detail = { path: '/envs/env-b/switch', headers: {} };
  h.handlers.get('htmx:configRequest')({ detail });
  assert.deepEqual(detail.headers, { 'X-Hot-Org-Id': 'org-a' });
});
