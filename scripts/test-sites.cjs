// Isolated full-site handover regression. No production hosts or credentials.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const net = require('node:net');
const crypto = require('node:crypto');
const { spawn, execFileSync } = require('node:child_process');
const repo = path.resolve(__dirname, '..');
const binary = path.join(repo, 'target/debug/sukisu-theme-web' + (process.platform === 'win32' ? '.exe' : ''));
const root = fs.mkdtempSync(path.join(repo, 'target/sites-e2e-'));
const children = new Set();
const csrf = { 'X-Requested-With': 'SukiSUThemeAdmin', 'Content-Type': 'application/json' };
const cookies = new Map();
const delay = ms => new Promise(r => setTimeout(r, ms));
const password = 'Test!' + crypto.randomBytes(16).toString('hex');
async function port() {
  const server = net.createServer(); await new Promise(r => server.listen(0, '127.0.0.1', r));
  const p = server.address().port; await new Promise(r => server.close(r)); return p;
}
function start(args, env, label) {
  const child = spawn(binary, args, { cwd: repo, env, windowsHide: true, detached: process.platform !== 'win32',
    stdio: ['ignore', fs.openSync(path.join(root, label + '.out'), 'a'), fs.openSync(path.join(root, label + '.err'), 'a')] });
  children.add(child); return child;
}
async function stop(child) {
  if (!child || child.exitCode !== null || child.signalCode !== null) return;
  const ended = new Promise(r => child.once('exit', r));
  if (child.site) {
    try { const r=await request(child.site.base,'/_site/shutdown',{}, {'X-Sukisu-Site-Key':child.site.secret}); if(r.ok){await r.arrayBuffer();await ended;return;} } catch {}
  }
  if (process.platform === 'win32') {
    try { execFileSync(path.join(process.env.SystemRoot || 'C:/Windows','System32/taskkill.exe'), ['/PID', String(child.pid), '/T', '/F'], { windowsHide: true, stdio: 'pipe' }); }
    catch (error) { child.kill(); }
  }
  else process.kill(-child.pid, 'SIGTERM');
  await ended;
}
async function request(base, route, body, headers = {}, signedIn = false) {
  const response = await fetch(base + route, { method: body === undefined ? 'GET' : 'POST',
    headers: { ...csrf, ...headers, ...(signedIn ? { Cookie: [...cookies].map(([k,v]) => k+'='+v).join('; ') } : {}) },
    body: body === undefined ? undefined : JSON.stringify(body), redirect: 'manual', signal: AbortSignal.timeout(120000) });
  if (signedIn) for (const value of response.headers.getSetCookie()) { const pair=value.split(';')[0], at=pair.indexOf('='); cookies.set(pair.slice(0,at),pair.slice(at+1)); }
  return response;
}
async function api(base, route, body) {
  const response = await request(base, '/api' + route, body, {}, true);
  const value = await response.json(); assert.ok(response.ok && value.ok !== false, route + ': ' + response.status + ' ' + (value.error || ''));
  return value;
}
async function until(check, name) {
  for (let i=0;i<100;i++) { try { if(await check()) return; } catch {} await delay(200); }
  throw Error('Timed out: '+name);
}
(async () => {
  const aPort=await port(), bPort=await port();
  const a='http://127.0.0.1:'+aPort, b='http://127.0.0.1:'+bPort;
  const env={...process.env,THEME_STATIC_DIR:path.join(repo,'static'),THEME_DEV_MODE:'1',THEME_DISABLE_SYSTEMCTL:'1'};
  for(const [key,file] of Object.entries({THEME_STORAGE_ENV:'storage.env',THEME_ADMIN_CONFIG:'config.env',THEME_DB_PATH:'legacy.db',THEME_ADMIN_ENV:'admin.env',THEME_ADMIN_PASSWORD_FILE:'password',THEME_TOKEN_ENV:'store.env',THEME_ACCOUNT_ENV:'account.env',THEME_NETWORK_ENV:'network.env',THEME_SOURCES_FILE:'sources.json',THEME_AUTHORS_FILE:'authors.json',THEME_DISTRIBUTION_FILE:'distribution.json',THEME_MIRRORS_FILE:'mirrors.json'}))env[key]=path.join(root,file);
  const data=path.join(root,'store');for(const name of ['meta','themes','previews'])fs.mkdirSync(path.join(data,name),{recursive:true});
  const fixture=crypto.randomBytes(256*1024);
  fs.writeFileSync(path.join(data,'themes/11010-01-v1.sukisu'),fixture);
  fs.writeFileSync(path.join(data,'previews/11010-01-v1.png'),Buffer.from('image'));
  fs.writeFileSync(path.join(data,'meta/11010-01.json'),JSON.stringify({id:'11010-01',version:1,name:'handover fixture',author:'test',package:'themes/11010-01-v1.sukisu',preview:'previews/11010-01-v1.png',formatVersion:1}));
  fs.writeFileSync(env.THEME_STORAGE_ENV,'THEME_STORAGE_ROOT='+data.replace(/\\/g,'/')+'\nTHEME_STORAGE_LIMIT_BYTES=1073741824\n');
  fs.writeFileSync(env.THEME_NETWORK_ENV,'ADMIN_BIND=127.0.0.1\nADMIN_PORT='+aPort+'\n');
  const oldMaster=crypto.randomBytes(32).toString('hex');fs.writeFileSync(env.THEME_TOKEN_ENV,'THEME_STORE_TOKEN='+oldMaster+'\n');
  execFileSync(binary,['migrate-db'],{cwd:repo,env,windowsHide:true,stdio:'pipe'});
  const initial=start([],env,'initial');await until(async()=> (await request(a,'/healthz')).ok,'initial');
  await api(a,'/setup',{username:'site-admin',password,limitMb:1024});
  const bootstrap=await api(a,'/sites/bootstrap',{nodes:[{name:'Site A',baseUrl:a,dataDir:path.join(root,'a')},{name:'Site B',baseUrl:b,dataDir:path.join(root,'b')}]});
  const configs=bootstrap.configs.map((x,i)=>({...x.config,bind:'127.0.0.1:'+[aPort,bPort][i],syncIntervalSecs:10,quotaBytes:1073741824}));
  for(let i=0;i<2;i++)fs.writeFileSync(path.join(root,'site-'+i+'.json'),JSON.stringify(configs[i]));
  await stop(initial);
  let primary=start(['site',path.join(root,'site-0.json')],env,'site-a');
  let follower=start(['site',path.join(root,'site-1.json')],env,'site-b');
  primary.site={base:a,secret:configs[0].secret};follower.site={base:b,secret:configs[0].secret};
  await until(async()=> (await request(a,'/api/session',undefined,{},true)).ok,'site A');
  await until(async()=> (await request(b,'/api/session',undefined,{},true)).ok,'site B');
  assert.equal((await request(b,'/account')).status,200);
  assert.equal((await request(b,'/api/sites')).status,401);
  assert.equal((await request(a,'/_site/latest')).status,401);
  assert.equal((await request(a,'/_site/drain',{})).status,404);
  assert.equal((await request(a,'/catalog.json',undefined,{'X-Sukisu-Theme-Token':oldMaster})).status,401);
  await api(b,'/login',{username:'site-admin',password});
  const issuedResponse=await request(b,'/account/api/access',{name:'Personal phone',expiresDays:7},{},true);
  assert.equal(issuedResponse.status,200);assert.equal(issuedResponse.headers.get('cache-control'),'no-store');
  const issued=await issuedResponse.json();assert.equal(issued.endpoints.length,2);
  const tokenHeader=t=>({'X-Sukisu-Theme-Token':t});
  assert.equal((await request(b,'/catalog.json',undefined,tokenHeader(issued.accessToken))).status,200);
  assert.equal((await fetch(a+'/submit',{method:'POST',headers:tokenHeader(issued.accessToken),body:new FormData()})).status,401);
  let rotated=await (await request(b,'/client/refresh',{refreshToken:issued.refreshToken})).json();
  assert.equal((await request(a,'/catalog.json',undefined,tokenHeader(issued.accessToken))).status,401);
  assert.equal((await request(b,'/catalog.json',undefined,tokenHeader(rotated.accessToken))).status,200);
  const headers={'X-Sukisu-Site-Key':configs[0].secret};
  await api(a,'/sites/sync',{});await api(b,'/sites/sync',{});
  const sites=await api(b,'/sites');assert.ok(sites.replica);assert.equal(sites.writerId,configs[0].nodeId);
  assert.ok(!JSON.stringify(sites).includes(configs[0].secret));
  await api(a,'/sites/transfer',{targetId:configs[1].nodeId});
  let state=await (await request(b,'/_site/state',undefined,headers)).json();assert.equal(state.mode,'writer');assert.equal(state.epoch,2);
  const oldState=await (await request(a,'/_site/state',undefined,headers)).json();assert.equal(oldState.mode,'follower');assert.equal(oldState.ready,false);
  assert.equal((await request(a,'/catalog.json',undefined,tokenHeader(rotated.accessToken))).status,200);
  const downloaded=await request(b,'/themes/11010-01-v1.sukisu',undefined,{...tokenHeader(rotated.accessToken),Range:'bytes=0-'+(fixture.length-1)});
  assert.equal(downloaded.status,206);assert.deepEqual(Buffer.from(await downloaded.arrayBuffer()),fixture);
  const duplicate={sourceId:configs[0].nodeId,epoch:2,snapshotId:state.finalSnapshot,transferId:state.transferId};
  assert.equal((await request(b,'/_site/activate',duplicate,headers)).status,200);
  assert.equal((await request(b,'/_site/activate',{...duplicate,epoch:1},headers)).status,503);
  assert.equal((await request(b,'/client/refresh',{refreshToken:issued.refreshToken})).status,401);
  assert.equal((await request(a,'/catalog.json',undefined,tokenHeader(rotated.accessToken))).status,401);
  const personal=await (await request(b,'/account/api/access',{name:'Revocable phone'},{},true)).json();
  assert.equal((await request(a,'/catalog.json',undefined,tokenHeader(personal.accessToken))).status,200);
  assert.equal((await request(a,'/account/api/access/revoke',{id:personal.id},{},true)).status,200);
  assert.equal((await request(b,'/catalog.json',undefined,tokenHeader(personal.accessToken))).status,401);
  await api(b,'/sites/transfer',{targetId:configs[0].nodeId});
  state=await (await request(a,'/_site/state',undefined,headers)).json();assert.equal(state.mode,'writer');assert.equal(state.epoch,3);
  assert.equal((await request(b,'/_site/activate',duplicate,headers)).status,503);
  await stop(follower);follower=start(['site',path.join(root,'site-1.json')],env,'site-b-restart');follower.site={base:b,secret:configs[0].secret};
  await until(async()=> (await request(b,'/api/session',undefined,{},true)).ok,'follower restart');
  await stop(primary);
  await delay(12000);
  state=await (await request(b,'/_site/state',undefined,headers)).json();assert.equal(state.mode,'follower');assert.equal(state.ready,false);
  assert.equal((await request(b,'/account/api/access',{name:'no-writer'},{},true)).status,503);
  primary=start(['site',path.join(root,'site-0.json')],env,'site-a-restart');primary.site={base:a,secret:configs[0].secret};
  await until(async()=> (await request(b,'/api/session',undefined,{},true)).ok,'writer restart');
  assert.equal((await api(a,'/sites')).epoch,3);
  console.log('PASS: full-site access, private checkpoints, personal credentials, refresh/replay/revoke, A→B→A, restart, stale epoch and no split-brain promotion');
})().catch(error=>{console.error(error.stack);console.error('Isolated logs: '+root);process.exitCode=1;}).finally(async()=>{for(const child of children)await stop(child).catch(()=>{});});
