// Node-only contract checks for the shared browser uploader; no dependencies.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');
const code = fs.readFileSync(path.join(__dirname, '../static/distributed-upload.js'), 'utf8');

async function scenario({ failEdge = false, repair = false, resumed = false } = {}) {
  const calls = [];
  const reports = [];
  let active = 0, maxActive = 0, completes = 0;
  class Xhr {
    constructor() {
      this.events = {}; this.upload = { addEventListener: (event, cb) => { this.progress = cb; } }; this.headers = {};
    }
    open(_, url) { this.url = url; }
    setRequestHeader(key, value) { this.headers[key] = value; }
    addEventListener(event, cb) { this.events[event] = cb; }
    send(chunk) {
      this.running = true; active++; maxActive = Math.max(maxActive, active);
      const edge = this.url.startsWith('https://edge.example');
      calls.push({ url: this.url, edge, credentials: this.withCredentials, headers: this.headers, size: chunk.size });
      this.timer = setTimeout(() => {
        if (!this.running) return;
        this.running = false; active--;
        if (edge && failEdge) { this.events.error(); return; }
        this.progress?.({ lengthComputable: true, loaded: chunk.size });
        this.status = 200; this.responseText = '{"ok":true}'; this.events.load();
      }, edge ? 3 : 25);
    }
    abort() {
      if (!this.running) return; this.running = false; clearTimeout(this.timer); active--; this.events.abort();
    }
  }
  const context = vm.createContext({ window: {}, File, URL, FormData, XMLHttpRequest: Xhr, AbortController,
    performance, setTimeout, clearTimeout, setInterval, clearInterval,
    fetch: async (url, options) => {
      assert.equal(options.credentials, 'same-origin');
      if (url.endsWith('/init')) return { ok: true, status: 200, json: async () => ({ ok: true, uploadId: 'ab'.repeat(16),
        chunkSize: 1024 * 1024, totalChunks: 16, received: resumed ? [0, 1] : [],
        uploadTargets: [{ id: 'edge', baseUrl: 'https://edge.example/_edge/uploads/' + 'ab'.repeat(16), ticket: 'short-lived-capability' }] }) };
      assert.ok(url.endsWith('/complete')); completes++;
      return { ok: true, status: 200, json: async () => ({ ok: true, complete: !(repair && completes === 1), missing: repair && completes === 1 ? [2] : [] }) };
    },
  });
  vm.runInContext(code, context);
  const file = new File([new Uint8Array(16 * 1024 * 1024)], 'theme.sukisu');
  await context.window.SukisuDistributedUpload.upload(file, { api: '/api', onProgress: (_, bytes) => reports.push(bytes) });
  assert.ok(maxActive >= 2 && maxActive <= 4, 'bounded parallel upload');
  assert.equal(reports.at(-1), file.size);
  assert.ok(reports.every(n => n >= 0 && n <= file.size));
  for (const call of calls) {
    assert.equal(call.credentials, !call.edge, 'account cookies must stay on the primary');
    assert.equal(Boolean(call.headers['X-Sukisu-Upload-Ticket']), call.edge);
    assert.ok(!call.url.includes('short-lived-capability'), 'capabilities do not appear in URLs');
    if (resumed) assert.ok(!/\/(0|1)$/.test(call.url), 'acknowledged chunks are not reuploaded');
  }
  if (!failEdge && !repair) assert.ok(calls.filter(c => c.edge).length > calls.filter(c => !c.edge).length, 'faster edge claims more chunks');
  if (failEdge) assert.ok(calls.some(c => c.edge) && calls.some(c => !c.edge), 'edge failures fall back to primary');
  if (repair) {
    assert.equal(completes, 2);
    assert.ok(calls.at(-1).url.endsWith('/2') && !calls.at(-1).edge, 'only missing chunks repaired on primary');
  }
}

(async () => {
  await scenario(); await scenario({ failEdge: true }); await scenario({ repair: true }); await scenario({ resumed: true });
  for (const name of ['index.html', 'account.html']) {
    const html = fs.readFileSync(path.join(__dirname, '../static/', name), 'utf8');
    for (const match of html.matchAll(/<script\b[^>]*>([\s\S]*?)<\/script>/gi)) if (match[1].trim()) new vm.Script(match[1], { filename: name });
    const markup = html.replace(/<script\b[^>]*>[\s\S]*?<\/script>/gi, '');
    const ids = [...markup.matchAll(/\bid="([^"]+)"/g)].map(match => match[1]);
    assert.equal(new Set(ids).size, ids.length, 'unique HTML IDs: ' + name);
    assert.ok(html.includes('/distributed-upload.js'));
  }
  JSON.parse(fs.readFileSync(path.join(__dirname, '../static/locales/en.json'), 'utf8'));
  console.log('PASS: adaptive concurrency, failover, missing-chunk repair, resume, credential isolation, page scripts and IDs');
})().catch(error => { console.error(error); process.exitCode = 1; });
