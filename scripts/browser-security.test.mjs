import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { readFileSync, readdirSync, statSync, mkdtempSync, rmSync, existsSync } from 'node:fs';
import { createServer } from 'node:http';
import { tmpdir } from 'node:os';
import { join, relative, resolve } from 'node:path';
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

function hotTokens(code) {
    const context = vm.createContext({});
    vm.runInContext(readFileSync(join(root, 'resources/app/assets/js/prism.js'), 'utf8'), context);
    vm.runInContext(grammar, context);
    const flatten = (tokens) => tokens.flatMap((token) => typeof token === 'string' ? []
        : Array.isArray(token.content) ? [[token.type, token.content.map((t) => typeof t === 'string' ? t : t.content).join('')], ...flatten(token.content)]
            : [[token.type, token.content]]);
    return flatten(context.Prism.tokenize(code, context.Prism.languages.hot));
}

test('qualified type and enum names highlight as types, not functions', () => {
    const tokens = hotTokens('x ::geo/Shape.Dot\ny ::hot::str/join(xs)');
    assert.ok(tokens.some(([type, text]) => type === 'class-name' && text === 'Shape'));
    assert.ok(tokens.some(([type, text]) => type === 'class-name' && text === 'Dot'));
    assert.ok(tokens.some(([type, text]) => type === 'function' && text === 'join'));
    assert.ok(!tokens.some(([type, text]) => type === 'function' && text === 'Shape'));
});

test('flow arrows, implements and enum open are tokens of their own', () => {
    const tokens = hotTokens('f fn (a: Int) -> Out { cond { eq(a, 1) => { 1 } } }\nAnimal enum open { Dog }');
    assert.ok(tokens.some(([type, text]) => type === 'operator' && text === '->'));
    assert.ok(tokens.some(([type, text]) => type === 'operator' && text === '=>'));
    assert.ok(tokens.some(([type, text]) => type === 'keyword' && text === 'open'));
});

test('nested meta maps and lists stay one meta annotation', () => {
    for (const meta of ['meta {on-event: "x", opts: {retry: {max: 3}}}', 'meta ["test", ["a", {b: "]"}]]']) {
        const annotation = hotTokens(meta + ' fn () { 1 }').find(([type]) => type === 'meta-annotation');
        assert.equal(annotation?.[1], meta);
    }
});

// Script-safety lint for the App templates.
// Every value a template puts into JavaScript must go through the `js` filter
// (crates/hot_app/src/templates/mod.rs), and `|safe` appears only where reviewed.
const JS_ATTRIBUTE = /^(on[a-z]+|x-[\w:.-]+|@[\w.:-]+|:[\w.-]+|hx-on[\w:.-]*|hx-vals|hx-vars|hx-headers)$/i;
const DATA_SCRIPT_TYPES = new Set(['application/json', 'text/plain']);
// Documentation HTML is sanitized with ammonia before it reaches the template.
const SAFE_MARKUP = new Set(['docs_namespace.html {{ content|safe }}']);

function lintTemplate(name, source) {
    // Neutralize comments and raw blocks so their braces are not read as interpolation.
    let text = source.replace(/\{#[\s\S]*?#\}/g, (m) => m.replace(/[^\n]/g, ' '));
    text = text.replace(/(\{%-?\s*raw\s*-?%\})([\s\S]*?)(\{%-?\s*endraw\s*-?%\})/g,
        (_, open, body, close) => ' '.repeat(open.length) + body.replace(/[{}%]/g, ' ') + ' '.repeat(close.length));
    const problems = [];
    const report = (index, message) =>
        problems.push(`${name}:${source.slice(0, index).split('\n').length}: ${message}`);
    const expressions = (from, to) => {
        const found = [];
        const re = /\{\{-?([\s\S]*?)-?\}\}/g;
        re.lastIndex = from;
        for (let m = re.exec(text); m && m.index < to; m = re.exec(text)) {
            found.push({ index: m.index, expr: m[1].trim(), source: `{{ ${m[1].trim()} }}` });
        }
        return found;
    };
    const unsafe = (expr) => /\|\s*safe\b/.test(expr);
    let i = 0;
    while (i < text.length) {
        const lt = text.indexOf('<', i);
        const textEnd = lt < 0 ? text.length : lt;
        for (const e of expressions(i, textEnd)) {
            if (unsafe(e.expr) && !SAFE_MARKUP.has(`${name} ${e.source}`)) report(e.index, `unreviewed |safe: ${e.source}`);
        }
        if (lt < 0) break;
        const tag = /^<([a-zA-Z][\w-]*)/.exec(text.slice(lt, lt + 40));
        if (!tag) { i = lt + 1; continue; }
        // Find the end of the tag, collecting quoted attribute values and skipping template tags.
        const attrs = [];
        let j = lt + tag[0].length, quote = null, attr = null;
        while (j < text.length) {
            if (text[j] === '{' && (text[j + 1] === '{' || text[j + 1] === '%')) {
                const close = text.indexOf(text[j + 1] === '{' ? '}}' : '%}', j + 2);
                j = close < 0 ? text.length : close + 2;
                continue;
            }
            const c = text[j];
            if (quote) {
                if (c === quote) { attrs.push({ ...attr, to: j }); quote = null; }
            } else if (c === '>') {
                break;
            } else if (c === '"' || c === "'") {
                quote = c;
                attr = { name: /([@:a-zA-Z][\w:.@-]*)\s*=\s*$/.exec(text.slice(lt, j))?.[1] ?? '', from: j + 1 };
            }
            j++;
        }
        for (const a of attrs) {
            for (const e of expressions(a.from, a.to)) {
                if (unsafe(e.expr)) report(e.index, `|safe in ${a.name || 'attribute'}: ${e.source}`);
                else if (JS_ATTRIBUTE.test(a.name) && !/\|\s*js$/.test(e.expr)) {
                    report(e.index, `${a.name} interpolates ${e.source} without |js`);
                }
            }
        }
        i = j + 1;
        if (tag[1].toLowerCase() === 'script') {
            const end = text.indexOf('</script', i);
            const close = end < 0 ? text.length : end;
            const type = /\btype\s*=\s*["']([^"']+)["']/i.exec(text.slice(lt, j))?.[1]?.toLowerCase();
            const data = DATA_SCRIPT_TYPES.has(type);
            for (const e of expressions(i, close)) {
                const js = /\|\s*js\s*\|\s*safe$/.test(e.expr);
                if (data ? unsafe(e.expr) && !js : !js) {
                    report(e.index, data ? `|safe without |js in a data script: ${e.source}` : `script interpolates ${e.source} without |js|safe`);
                }
            }
            i = close + 1;
        }
    }
    return problems;
}

function templateFiles(dir) {
    return readdirSync(dir).flatMap((entry) => {
        const path = join(dir, entry);
        return statSync(path).isDirectory() ? templateFiles(path) : path.endsWith('.html') ? [path] : [];
    });
}

test('templates interpolate into JavaScript only through the js filter', () => {
    const dir = join(root, 'crates/hot_app/templates');
    const problems = templateFiles(dir).flatMap((file) =>
        lintTemplate(relative(dir, file), readFileSync(file, 'utf8')));
    assert.deepEqual(problems, []);
});

test('the template lint catches each script-embedding sink shape', () => {
    const bad = [
        `<div x-data="{ search: '{{ search_query }}' }"></div>`,
        `<button onclick="open('{{ event.data|escape }}')"></button>`,
        `<span :title="base + '{{ ep.path }}'"></span>`,
        `<button @click="copy({{ token }})"></button>`,
        `<script>const rules = {{ permissions_json|safe }};</script>`,
        `<script>var name = '{{ project_name }}';</script>`,
        `<script type="application/json">{{ graph_json|safe }}</script>`,
        `<a title="{{ title|safe }}"></a>`,
        `<div>{{ body|safe }}</div>`,
        `{% raw %}<b x-show="a">{% endraw %}<b x-show="p === '{{ id }}'"></b>`,
    ];
    for (const source of bad) assert.equal(lintTemplate('t.html', source).length, 1, source);
    const good = [
        `<div x-data="{ search: {{ search_query|js }} }"></div>`,
        `<button onclick="go({{ (page - 1)|js }})" data-modal-hot="{{ event.data }}"></button>`,
        `<script>const rules = {{ permissions|js|safe }};</script>`,
        `<script type="application/json">{{ data|escape }}</script>`,
        `<input value="{{ description }}" {% if checked %}checked{% endif %}>`,
        `{% raw %}<b x-show="{{ alpine }}"></b>{% endraw %}`,
        `{# <b onclick="{{ old }}"> #}`,
    ];
    for (const source of good) assert.deepEqual(lintTemplate('t.html', source), [], source);
});

// The `js` template filter: JSON with the characters that could end a script, an attribute or
// a string escaped. Mirrors crates/hot_app/src/templates/mod.rs.
const jsLiteral = (value) => JSON.stringify(value)
    .replace(/&/g, '\\u0026').replace(/</g, '\\u003c').replace(/>/g, '\\u003e')
    .replace(/'/g, '\\u0027').replace(/\u2028/g, '\\u2028').replace(/\u2029/g, '\\u2029');
const htmlAttribute = (value) => String(value)
    .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;').replace(/'/g, '&#x27;');

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
const assetsDir = join(root, 'resources/app/assets/js');

// Serve `html` and the App's scripts, open it in headless Chrome, and wait for the page to POST
// its result to /result. Waiting on the request (rather than dumping the DOM after a time
// budget) keeps a slow runner from reading the page before the scenario finishes.
async function runInChrome(t, html) {
    const temporary = mkdtempSync(join(tmpdir(), 'hot-browser-security-'));
    let deliver;
    const result = new Promise((done) => { deliver = done; });
    const server = createServer((request, response) => {
        const asset = /^\/(app|alpine\.min)\.js$/.exec(request.url ?? '');
        if (asset) {
            response.writeHead(200, { 'Content-Type': 'text/javascript' });
            response.end(readFileSync(join(assetsDir, asset[1] + '.js')));
        } else if (request.url === '/') {
            response.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8' });
            response.end(html);
        } else if (request.url === '/result' && request.method === 'POST') {
            let body = '';
            request.on('data', (chunk) => { body += chunk; });
            request.on('end', () => { response.writeHead(204); response.end(); deliver(JSON.parse(body)); });
        } else {
            response.writeHead(404); response.end();
        }
    });
    await new Promise((done) => server.listen(0, '127.0.0.1', done));
    const origin = 'http://127.0.0.1:' + server.address().port;
    const browser = spawn(chrome, [
        '--headless=new', '--disable-gpu', '--no-first-run', '--no-default-browser-check',
        '--disable-background-networking', '--disable-extensions',
        '--user-data-dir=' + join(temporary, 'profile'), origin,
    ], { stdio: ['ignore', 'ignore', 'pipe'] });
    let stderr = '';
    browser.stderr.on('data', (chunk) => { stderr += chunk; });
    // Stop the browser before the server: open keep-alive sockets would hold server.close().
    t.after(async () => {
        if (browser.exitCode === null) {
            browser.kill('SIGKILL');
            await new Promise((done) => browser.once('exit', done));
        }
        server.closeAllConnections();
        await new Promise((done) => server.close(done));
        rmSync(temporary, { recursive: true, force: true });
    });
    let timer;
    const timeout = new Promise((_, fail) => {
        timer = setTimeout(() => fail(new Error('The browser did not report a result in 20s. Chrome stderr:\n' + stderr)), 20000);
    });
    try {
        return { origin, result: await Promise.race([result, timeout]) };
    } finally {
        clearTimeout(timer);
    }
}

const report = `fetch('/result', { method: 'POST', body: JSON.stringify(result) });`;

test('entity decoding and documentation links keep untrusted input inert', {
    skip: chrome ? false : 'Set CHROME_BIN to run the real-browser regression test',
}, async (t) => {
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
    const html = [
        '<!doctype html><html><body><script src="/app.js"></script><script>',
        'window.__executed = 0;',
        linkify,
        'const decoded = ' + jsLiteral(inputs) + '.map(decodeHtmlEntities);',
        'const registry = ' + jsLiteral(registry) + ';',
        'const data = document.createElement("script");',
        'data.id = "type-registry-data"; data.type = "application/json";',
        'data.textContent = JSON.stringify(registry); document.body.appendChild(data);',
        'for (const name of Object.keys(registry)) {',
        '  const pre = document.createElement("pre"); pre.className = "fn-signature";',
        '  const span = document.createElement("span"); span.className = "token class-name";',
        '  span.textContent = name; pre.appendChild(span); document.body.appendChild(pre);',
        '}',
        'linkifyTypesInSignatures();',
        // Let any injected handler (an onerror on a broken image) run before reporting.
        'setTimeout(() => {',
        '  const links = Array.from(document.querySelectorAll("a.type-link"), a => ({name: a.textContent, href: a.getAttribute("href")}));',
        '  const result = {decoded, links, executed: window.__executed};',
        '  ' + report,
        '}, 100);',
        '</script></body></html>',
    ].join('\n');
    const { origin, result } = await runInChrome(t, html);
    assert.deepEqual(result.decoded, expected);
    assert.equal(result.executed, 0);
    assert.deepEqual(result.links, [
        { name: 'Relative', href: origin + registry.Relative },
        { name: 'External', href: registry.External },
    ]);
});

test('js-filtered values stay inert in Alpine expressions, inline handlers and scripts', {
    skip: chrome ? false : 'Set CHROME_BIN to run the real-browser regression test',
}, async (t) => {
    // The shapes from the v3.0.1 review: /streams search, onclick event data, webhook paths
    // and </script> in key permissions.
    const payloads = [
        "x',a:window.__executed++,b:'",
        '</script><script>window.__executed++</script>',
        '"><img src=missing onerror="window.__executed++">',
        "q');window.__executed++;('",
        '`;window.__executed++;`',
        '${window.__executed++}',
        "/y'+window.__executed+++'",
        'trailing backslash \\',
        'line\nbreak \u2028 \u2029 & &amp; &#x27;',
    ];
    const rows = payloads.map((payload, index) => {
        const literal = htmlAttribute(jsLiteral(payload));
        return [
            `<div class="alpine" x-data="{ value: ${literal}, base: '/hooks' }" :title="base + ${literal}" x-text="value"></div>`,
            `<button class="inline" onclick="window.__clicked.push(${literal})"></button>`,
            `<button class="modal" data-modal-title="Event Data" data-modal-hot="${htmlAttribute(payload)}" data-modal-json="${htmlAttribute(payload)}" onclick="event.stopPropagation(); openContentModalFromData(this); window.__modal.push(document.getElementById('content-modal-body').textContent)"></button>`,
            `<script>window.__scripted.push(${jsLiteral(payload)});</script>`,
        ].join('\n');
    });
    const html = [
        '<!doctype html><html><body>',
        '<script>window.__executed = 0; window.__clicked = []; window.__modal = []; window.__scripted = [];</script>',
        '<div id="content-modal" class="hidden"><h3 id="content-modal-title"></h3><div id="content-modal-format-toggle"></div><pre id="content-modal-body"></pre></div>',
        ...rows,
        '<script src="/app.js"></script>',
        '<script>',
        "document.addEventListener('alpine:initialized', () => setTimeout(() => {",
        "  for (const button of document.querySelectorAll('button')) button.click();",
        '  setTimeout(() => {',
        '    const result = {',
        "      alpine: Array.from(document.querySelectorAll('.alpine'), el => el.textContent),",
        "      titles: Array.from(document.querySelectorAll('.alpine'), el => el.getAttribute('title')),",
        '      clicked: window.__clicked, modal: window.__modal, scripted: window.__scripted,',
        '      executed: window.__executed,',
        '    };',
        '    ' + report,
        '  }, 100);',
        '}, 0));',
        '</script>',
        '<script defer src="/alpine.min.js"></script>',
        '</body></html>',
    ].join('\n');
    const { result } = await runInChrome(t, html);
    assert.equal(result.executed, 0);
    assert.deepEqual(result.alpine, payloads);
    assert.deepEqual(result.titles, payloads.map((payload) => '/hooks' + payload));
    assert.deepEqual(result.clicked, payloads);
    assert.deepEqual(result.modal, payloads);
    assert.deepEqual(result.scripted, payloads);
});
