// Local-only end-to-end test. Starts isolated primary/edge processes and databases.
// Run cargo build first, then node scripts/test-distribution.cjs.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const net = require('node:net');
const crypto = require('node:crypto');
const zlib = require('node:zlib');
const { spawn, execFileSync } = require('node:child_process');
const repo = path.resolve(__dirname, '..');
const binary = path.join(repo, 'target/debug/sukisu-theme-web' + (process.platform === 'win32' ? '.exe' : ''));
const root = fs.mkdtempSync(path.join(repo, 'target/distribution-e2e-'));
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
const sha256 = bytes => crypto.createHash('sha256').update(bytes).digest('hex');
const cookies = new Map();
const csrf = { 'X-Requested-With': 'SukiSUThemeAdmin' };
let main, edge, base, edgeBase;

async function freePort() {
  const server = net.createServer();
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  const port = server.address().port;
  await new Promise(resolve => server.close(resolve)); return port;
}
async function request(url, { method = 'GET', body, headers = {}, signedIn = false } = {}) {
  if (signedIn) headers = { ...headers, Cookie: [...cookies].map(([k, v]) => k + '=' + v).join('; ') };
  const response = await fetch(url, { method, body, headers, redirect: 'manual', signal: AbortSignal.timeout(30000) });
  if (signedIn) for (const line of response.headers.getSetCookie()) {
    const pair = line.split(';')[0], equal = pair.indexOf('='); cookies.set(pair.slice(0, equal), pair.slice(equal + 1));
  }
  return response;
}
async function api(route, body, method = body === undefined ? 'GET' : 'POST') {
  const response = await request(base + '/api' + route, { method, signedIn: true, headers: { ...csrf, 'Content-Type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body) });
  const value = await response.json();
  assert.ok(response.ok && value.ok !== false, route + ': ' + response.status + ' ' + (value.error || ''));
  return value;
}
async function until(check, label) {
  for (let i = 0; i < 80; i++) { try { if (await check()) return; } catch {} await delay(150); }
  throw new Error('Timed out: ' + label);
}
function start(args, env, name) {
  return spawn(binary, args, { cwd: repo, env, windowsHide: true,
    stdio: ['ignore', fs.openSync(path.join(root, name + '.out'), 'w'), fs.openSync(path.join(root, name + '.err'), 'w')] });
}
async function stop(child) {
  if (!child || child.exitCode !== null || child.signalCode !== null) return;
  child.kill(); await new Promise(resolve => child.once('exit', resolve));
}
function crc32(bytes) {
  let crc = 0xffffffff;
  for (const byte of bytes) { crc ^= byte; for (let i = 0; i < 8; i++) crc = (crc >>> 1) ^ (0xedb88320 & -(crc & 1)); }
  return (crc ^ 0xffffffff) >>> 0;
}
function fixtures() {
  const entries = [['theme.json', Buffer.from('{"version":1}')], ['padding.bin', crypto.randomBytes(3 * 1024 * 1024)]];
  const locals = [], central = []; let offset = 0;
  for (const [name, bytes] of entries) {
    const filename = Buffer.from(name), local = Buffer.alloc(30), directory = Buffer.alloc(46), crc = crc32(bytes);
    local.writeUInt32LE(0x04034b50); local.writeUInt16LE(20, 4); local.writeUInt32LE(crc, 14);
    local.writeUInt32LE(bytes.length, 18); local.writeUInt32LE(bytes.length, 22); local.writeUInt16LE(filename.length, 26);
    directory.writeUInt32LE(0x02014b50); directory.writeUInt16LE(20, 4); directory.writeUInt16LE(20, 6);
    directory.writeUInt32LE(crc, 16); directory.writeUInt32LE(bytes.length, 20); directory.writeUInt32LE(bytes.length, 24);
    directory.writeUInt16LE(filename.length, 28); directory.writeUInt32LE(offset, 42);
    locals.push(local, filename, bytes); central.push(directory, filename); offset += local.length + filename.length + bytes.length;
  }
  const directory = Buffer.concat(central), end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50); end.writeUInt16LE(entries.length, 8); end.writeUInt16LE(entries.length, 10);
  end.writeUInt32LE(directory.length, 12); end.writeUInt32LE(offset, 16);
  const keySource = fs.readFileSync(path.join(repo, 'src/config.rs'), 'utf8').match(/PACKAGE_KEY: \[u8; 32\] = \[([\s\S]*?)\];/)[1];
  const key = Buffer.from([...keySource.matchAll(/0x([0-9a-f]+)/gi)].map(m => parseInt(m[1], 16)));
  const nonce = crypto.randomBytes(12), cipher = crypto.createCipheriv('aes-256-gcm', key, nonce);
  const encrypted = Buffer.concat([cipher.update(Buffer.concat([...locals, directory, end])), cipher.final()]);
  const packageBytes = Buffer.concat([Buffer.from('SKTH'), nonce, encrypted, cipher.getAuthTag()]);
  const ihdr = Buffer.alloc(13); ihdr.writeUInt32BE(2); ihdr.writeUInt32BE(2, 4); ihdr[8] = 8; ihdr[9] = 2;
  function pngChunk(name, bytes) {
    const size = Buffer.alloc(4), type = Buffer.from(name), crc = Buffer.alloc(4); size.writeUInt32BE(bytes.length);
    crc.writeUInt32BE(crc32(Buffer.concat([type, bytes]))); return Buffer.concat([size, type, bytes, crc]);
  }
  const image = Buffer.concat([Buffer.from('89504e470d0a1a0a', 'hex'), pngChunk('IHDR', ihdr), pngChunk('IDAT', zlib.deflateSync(Buffer.from([0,255,0,0,255,0,0,0,255,0,0,255,0,0]))), pngChunk('IEND', Buffer.alloc(0))]);
  return { packageBytes, image };
}

(async () => {
  const mainPort = await freePort(), edgePort = await freePort();
  base = 'http://127.0.0.1:' + mainPort; edgeBase = 'http://127.0.0.1:' + edgePort;
  const data = path.join(root, 'data'); for (const name of ['meta', 'themes', 'previews']) fs.mkdirSync(path.join(data, name), { recursive: true });
  const { packageBytes, image } = fixtures();
  fs.writeFileSync(path.join(data, 'themes/11010-01-v1.sukisu'), packageBytes);
  fs.writeFileSync(path.join(data, 'previews/11010-01-v1.png'), image);
  fs.writeFileSync(path.join(data, 'meta/11010-01.json'), JSON.stringify({ id: '11010-01', version: 1, name: 'Local fixture', author: 'test', package: 'themes/11010-01-v1.sukisu', preview: 'previews/11010-01-v1.png', formatVersion: 1 }));
  const env = { ...process.env, THEME_STATIC_DIR: path.join(repo, 'static'), THEME_DISABLE_SYSTEMCTL: '1', THEME_DEV_MODE: '1' };
  for (const [key, file] of Object.entries({ THEME_STORAGE_ENV: 'storage.env', THEME_ADMIN_CONFIG: 'config.env', THEME_DB_PATH: 'legacy.db',
    THEME_ADMIN_ENV: 'admin.env', THEME_ADMIN_PASSWORD_FILE: 'admin-password', THEME_TOKEN_ENV: 'store.env', THEME_ACCOUNT_ENV: 'account.env',
    THEME_NETWORK_ENV: 'network.env', THEME_SOURCES_FILE: 'sources.json', THEME_AUTHORS_FILE: 'authors.json', THEME_MIRRORS_FILE: 'mirrors.json', THEME_DISTRIBUTION_FILE: 'distribution.json' })) env[key] = path.join(root, file);
  fs.writeFileSync(env.THEME_STORAGE_ENV, 'THEME_STORAGE_ROOT=' + data.replace(/\\/g, '/') + '\nTHEME_STORAGE_LIMIT_BYTES=1073741824\n');
  fs.writeFileSync(env.THEME_NETWORK_ENV, 'ADMIN_BIND=127.0.0.1\nADMIN_PORT=' + mainPort + '\nOUTBOUND_IP_FAMILY=auto\n');
  execFileSync(binary, ['migrate-db'], { cwd: repo, env, windowsHide: true, stdio: 'pipe' });
  main = start([], env, 'primary');
  await until(async () => (await fetch(base + '/healthz')).ok, 'primary start');
  assert.equal((await request(base + '/api/distribution')).status, 401);
  await api('/setup', { username: 'local-test', password: 'Local!' + crypto.randomBytes(12).toString('hex'), limitMb: 1024 });
  const credentialResponse = await request(base + '/client/access', {
    method: 'POST', signedIn: true, headers: { ...csrf, 'Content-Type': 'application/json' }, body: JSON.stringify({ name: 'Distribution regression' }),
  });
  assert.equal(credentialResponse.status, 200);
  const token = (await credentialResponse.json()).accessToken;
  const settings = { enabled: true, uploadsEnabled: true, originUrl: base, strategy: 'latency', syncIntervalSecs: 2, cacheTtlSecs: 60 };
  await api('/distribution', settings);
  const created = await api('/distribution/nodes', { name: 'Local edge', baseUrl: edgeBase, enabled: true, uploadEnabled: true, priority: 100 });
  const edgeConfig = { ...created.edgeConfig, cacheDir: path.join(root, 'cache'), bind: '127.0.0.1:' + edgePort, quotaBytes: 1073741824 };
  const configFile = path.join(root, 'edge.json'); fs.writeFileSync(configFile, JSON.stringify(edgeConfig));
  edge = start(['edge', configFile], env, 'edge');
  await until(async () => (await api('/distribution/probe', {})).nodes[0].status.uploadReady, 'edge upload lease');
  assert.ok(!JSON.stringify(await api('/distribution')).includes(edgeConfig.secret));
  const script = await request(base + '/distributed-upload.js'); assert.equal(script.status, 200);
  assert.ok((await script.text()).includes('SukisuDistributedUpload'));
  const nodeHeaders = { 'X-Sukisu-Node': edgeConfig.nodeId, 'X-Sukisu-Node-Key': edgeConfig.secret };
  const manifest = await request(base + '/distribution/manifest', { headers: nodeHeaders });
  assert.equal((await request(base + '/distribution/manifest', { headers: { ...nodeHeaders, 'If-None-Match': manifest.headers.get('ETag') } })).status, 304);

  async function init(name, bytes) {
    return api('/uploads/init', { filename: name, size: bytes.length, chunkSize: 1024 * 1024, fingerprint: crypto.randomUUID() });
  }
  async function chunk(session, bytes, index, useEdge = false) {
    const target = session.uploadTargets[0];
    return request(useEdge ? target.baseUrl + '/' + index : base + '/api/uploads/' + session.uploadId + '/' + index,
      { method: 'POST', body: bytes, signedIn: !useEdge, headers: useEdge ? { Origin: base, 'X-Sukisu-Upload-Ticket': target.ticket } : csrf });
  }
  async function upload(name, bytes) {
    const session = await init(name, bytes); assert.equal(session.uploadTargets.length, 1);
    const chunks = [];
    for (let index = 0; index < session.totalChunks; index++) chunks.push(chunk(session, bytes.subarray(index * session.chunkSize, (index + 1) * session.chunkSize), index, index % 2 === 0));
    for (const response of await Promise.all(chunks)) assert.equal(response.status, 200);
    const status = await api('/uploads/' + session.uploadId); assert.equal(status.received.length, session.totalChunks);
    assert.equal((await api('/uploads/' + session.uploadId + '/complete', {})).complete, true);
    const parts = Array.from({ length: session.totalChunks }, (_, i) => fs.readFileSync(path.join(data, 'uploads', session.uploadId, i + '.part')));
    assert.equal(sha256(Buffer.concat(parts)), sha256(bytes)); return session;
  }
  const packageSession = await upload('parallel.sukisu', packageBytes);
  const previewSession = await upload('preview.png', image);
  const avatarSession = await upload('avatar.png', image);
  const screenshotSession = await upload('screenshot.png', image);
  const duplicate = await chunk(packageSession, packageBytes.subarray(0, packageSession.chunkSize), 0, true); assert.equal(duplicate.status, 200);
  const changed = Buffer.from(packageBytes.subarray(0, packageSession.chunkSize)); changed[0] ^= 1;
  assert.equal((await chunk(packageSession, changed, 0, true)).status, 409);
  const target = packageSession.uploadTargets[0];
  const preflight = await request(target.baseUrl + '/0', { method: 'OPTIONS', headers: { Origin: base, 'Access-Control-Request-Method': 'POST' } });
  assert.equal(preflight.headers.get('Access-Control-Allow-Origin'), base); assert.equal(preflight.headers.get('Access-Control-Allow-Credentials'), null);
  assert.equal((await request(target.baseUrl + '/0', { method: 'POST', body: packageBytes.subarray(0, packageSession.chunkSize) })).status, 403);
  assert.equal((await request(target.baseUrl + '/0', { method: 'POST', body: packageBytes.subarray(0, packageSession.chunkSize), headers: { Origin: 'https://untrusted.example', 'X-Sukisu-Upload-Ticket': target.ticket } })).status, 403);
  assert.equal((await request(target.baseUrl + '/0')).status, 401);
  assert.equal((await request(base + '/api/uploads/' + packageSession.uploadId, { signedIn: true })).headers.get('Cache-Control'), 'no-store');
  const form = new FormData();
  for (const [key, value] of Object.entries({ id: 'cdn-upload-test', name: 'Distributed upload test', author: 'Local test', version: '1', minManagerVersion: '0',
    uploadId: packageSession.uploadId, previewUploadId: previewSession.uploadId, avatarUploadId: avatarSession.uploadId, screenshotUploadId: screenshotSession.uploadId })) form.append(key, value);
  const published = await request(base + '/api/upload-theme', { method: 'POST', body: form, signedIn: true, headers: csrf });
  assert.equal(published.status, 200, 'publish failed: ' + await published.clone().text());
  for (const session of [packageSession, previewSession, avatarSession, screenshotSession]) {
    assert.ok(!fs.existsSync(path.join(edgeConfig.cacheDir, 'upload-staging', session.uploadId)), 'staging cleaned after publish');
  }
  await until(async () => {
    const state = (await api('/distribution/probe', {})).nodes[0].status;
    return state.ready && state.filesTotal >= 6;
  }, 'published resources replicated');
  const catalogResponse = await request(base + '/catalog.json', { headers: { 'X-Sukisu-Theme-Token': token } });
  const theme = (await catalogResponse.json()).themes.find(t => t.id === 'cdn-upload-test'); assert.ok(theme);
  const redirect = await request(base + '/' + theme.path, { headers: { 'X-Sukisu-Theme-Token': token } });
  assert.equal(redirect.status, 302); assert.equal(sha256(Buffer.from(await (await request(redirect.headers.get('Location'))).arrayBuffer())), sha256(packageBytes));
  const corrupted = await init('corruption.sukisu', Buffer.from('hello'));
  assert.equal((await chunk(corrupted, Buffer.from('hello'), 0, true)).status, 200);
  fs.writeFileSync(path.join(edgeConfig.cacheDir, 'upload-staging', corrupted.uploadId, '0.part'), 'other');
  assert.deepEqual((await api('/uploads/' + corrupted.uploadId + '/complete', {})).missing, [0]);
  assert.equal((await chunk(corrupted, Buffer.from('hello'), 0)).status, 200);
  assert.equal((await api('/uploads/' + corrupted.uploadId + '/complete', {})).complete, true);
  const offline = await init('offline.sukisu', Buffer.from('hello'));
  assert.equal((await chunk(offline, Buffer.from('hello'), 0, true)).status, 200);
  await stop(edge);
  assert.deepEqual((await api('/uploads/' + offline.uploadId + '/complete', {})).missing, [0]);
  assert.equal((await chunk(offline, Buffer.from('hello'), 0)).status, 200);
  assert.equal((await api('/uploads/' + offline.uploadId + '/complete', {})).complete, true);
  await api('/distribution', { ...settings, uploadsEnabled: false });
  assert.equal((await init('primary-only.sukisu', Buffer.from('hello'))).uploadTargets.length, 0);
  assert.equal((await request(base + '/' + theme.path, { headers: { 'X-Sukisu-Theme-Token': token } })).status, 200);
  console.log('PASS: parallel primary/edge ingress, SHA-256 merge, all image references, publication and replication, idempotence, CORS, auth, checksum rejection, node-offline repair, upload disable and download fallback.');
  console.log('Isolated test data: ' + root);
})().catch(error => { console.error(error); process.exitCode = 1; }).finally(async () => { await stop(edge); await stop(main); });
