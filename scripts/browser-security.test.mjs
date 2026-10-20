import assert from 'node:assert/strict';
import { execFile, spawnSync } from 'node:child_process';
import { readFileSync, mkdtempSync, rmSync, existsSync } from 'node:fs';
import { createServer } from 'node:http';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { promisify } from 'node:util';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';
import test from 'node:test';

const root = process.env.HOT_BROWSER_TEST_ROOT
    ? resolve(process.env.HOT_BROWSER_TEST_ROOT)
    : fileURLToPath(new URL('../', import.meta.url));
const grammar = readFileSync(join(root, 'resources/app/assets/js/prism-hot.js'), 'utf8');

for (const [index, open, close] of [[0, '[', ']'], [1, '{', '}']]) {
    test(`meta ${open}${close} highlighting preserves quoted delimiters`, () => {
        const context = vm.createContext({ Prism: { languages: {} } });
        vm.runInContext(grammar, context);
        for (const content of [
            '"", "ordinary", true, 42',
            '"escaped \\\" quote", "closing ' + close + ' delimiter"',
            '"""raw ' + close + ' delimiter and "quotes"\nsecond line"""',
        ]) {
            context.input = 'meta ' + open + content + close;
            const match = vm.runInContext(
                `Prism.languages.hot['meta-annotation'][${index}].pattern.exec(input)`,
                context,
                { timeout: 2000 },
            );
            assert.equal(match?.[0], context.input);
        }
    });

    for (const repeated of ['!', '""""""']) {
        test(`unterminated meta ${open} handles repeated ${JSON.stringify(repeated)}`, () => {
            const context = vm.createContext({ Prism: { languages: {} } });
            vm.runInContext(grammar, context);
            context.input = 'meta ' + open + repeated.repeat(10000);
            const result = vm.runInContext(
                `Prism.languages.hot['meta-annotation'][${index}].pattern.test(input)`,
                context,
                { timeout: 2000 },
            );
            assert.equal(result, false);
        });
    }
}

function chromeExecutable() {
    if (process.env.CHROME_BIN) return process.env.CHROME_BIN;
    const mac = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
    if (existsSync(mac)) return mac;
    for (const name of ['google-chrome', 'chromium', 'chromium-browser']) {
        if (spawnSync(name, ['--version'], { stdio: 'ignore' }).status === 0) return name;
    }
    return null;
}

const chrome = chromeExecutable();
test('entity decoding and documentation links keep untrusted input inert', {
    skip: chrome ? false : 'Set CHROME_BIN to run the real-browser regression test',
}, async (t) => {
    const temporary = mkdtempSync(join(tmpdir(), 'hot-browser-security-'));
    t.after(() => rmSync(temporary, { recursive: true, force: true }));
    const app = readFileSync(join(root, 'resources/app/assets/js/app.js'));
    const template = readFileSync(join(root, 'crates/hot_app/templates/docs_namespace.html'), 'utf8');
    const linkify = template.match(/function linkifyTypesInSignatures\(\) \{[\s\S]*?\n\}/)?.[0];
    assert.ok(linkify, 'Load the actual documentation link renderer');
    const inputs = [
        '</textarea><img src=missing onerror="window.__executed++">',
        '<svg onload="window.__executed++"></svg>',
        '&lt;img src=missing onerror="window.__executed++"&gt;',
        '&amp;lt;script&amp;gt;',
        'quotes: &quot;&apos; &amp; &#x1f525;',
    ];
    const expected = [
        inputs[0], inputs[1], '<img src=missing onerror="window.__executed++">',
        '&lt;script&gt;', 'quotes: "\' & 🔥',
    ];
    const registry = {
        Javascript: 'javascript:window.__executed++',
        Obfuscated: '\n\tJaVaScRiPt:window.__executed++',
        Data: 'data:text/html,<script>parent.__executed++</script>',
        Relative: '/docs/example/type?name=value#Example',
        External: 'https://docs.example/types#Example',
    };
    const jsonForScript = (value) => JSON.stringify(value).replace(/</g, '\\u003c');
    const html = [
        '<!doctype html><html><body><script src="/app.js"></script><script>',
        'window.__executed = 0;',
        linkify,
        'const decoded = ' + jsonForScript(inputs) + '.map(decodeHtmlEntities);',
        'const registry = ' + jsonForScript(registry) + ';',
        'const data = document.createElement("script");',
        'data.id = "type-registry-data"; data.type = "application/json";',
        'data.textContent = JSON.stringify(registry); document.body.appendChild(data);',
        'for (const name of Object.keys(registry)) {',
        '  const pre = document.createElement("pre"); pre.className = "fn-signature";',
        '  const span = document.createElement("span"); span.className = "token class-name";',
        '  span.textContent = name; pre.appendChild(span); document.body.appendChild(pre);',
        '}',
        'linkifyTypesInSignatures();',
        'setTimeout(() => {',
        '  const links = Array.from(document.querySelectorAll("a.type-link"), a => ({name: a.textContent, href: a.getAttribute("href")}));',
        '  const result = JSON.stringify({decoded, links, executed: window.__executed});',
        '  const bytes = Array.from(new TextEncoder().encode(result), b => String.fromCharCode(b)).join("");',
        '  document.body.setAttribute("data-test-result", btoa(bytes));',
        '}, 50);',
        '</script></body></html>',
    ].join('\n');
    const server = createServer((request, response) => {
        if (request.url === '/app.js') {
            response.writeHead(200, { 'Content-Type': 'text/javascript' });
            response.end(app);
        } else if (request.url === '/') {
            response.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8' });
            response.end(html);
        } else {
            response.writeHead(404); response.end();
        }
    });
    await new Promise((done) => server.listen(0, '127.0.0.1', done));
    t.after(() => new Promise((done) => server.close(done)));
    const origin = 'http://127.0.0.1:' + server.address().port;
    const { stdout } = await promisify(execFile)(chrome, [
        '--headless=new', '--disable-gpu', '--no-first-run', '--no-default-browser-check',
        '--disable-background-networking', '--disable-extensions',
        '--user-data-dir=' + join(temporary, 'profile'),
        '--virtual-time-budget=1000', '--dump-dom', origin,
    ], { timeout: 20000, maxBuffer: 4 * 1024 * 1024 });
    const encoded = stdout.match(/data-test-result="([^"]+)"/)?.[1];
    assert.ok(encoded, 'The browser must complete the regression scenario');
    const result = JSON.parse(Buffer.from(encoded, 'base64').toString('utf8'));
    assert.deepEqual(result.decoded, expected);
    assert.equal(result.executed, 0);
    assert.deepEqual(result.links, [
        { name: 'Relative', href: origin + registry.Relative },
        { name: 'External', href: registry.External },
    ]);
});
