/* Shared by the admin console and account center. No account cookies go to edges. */
(() => {
  'use strict';
  let roundRobin = 0;
  const csrf = { 'X-Requested-With': 'SukiSUThemeAdmin' };
  async function apiJson(api, path, body, onUnauthorized) {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), 10 * 60 * 1000);
    try {
      const response = await fetch(api + path, { method: 'POST', credentials: 'same-origin', cache: 'no-store',
        signal: controller.signal, headers: { ...csrf, 'Content-Type': 'application/json' }, body: JSON.stringify(body || {}) });
      const value = await response.json().catch(() => ({}));
      if (response.status === 401) {
        onUnauthorized?.();
        const error = new Error('请重新登录'); error.unauthorized = true; throw error;
      }
      if (!response.ok || value.ok === false) throw new Error(value.error || ('上传失败 HTTP ' + response.status));
      return value;
    } finally { clearTimeout(timer); }
  }

  async function upload(file, { api, onProgress = () => {}, onUnauthorized = () => {} }) {
    const session = await apiJson(api, '/uploads/init', {
      filename: file.name, size: file.size, fingerprint: file.name + ':' + file.size + ':' + (file.lastModified || 0),
    }, onUnauthorized);
    const id = session.uploadId;
    const chunkSize = Number(session.chunkSize) || 4 * 1024 * 1024;
    const totalChunks = Number(session.totalChunks) || Math.ceil(file.size / chunkSize);
    const primary = { id: 'primary', baseUrl: api + '/uploads/' + encodeURIComponent(id), active: 0, speed: 0, cooldown: 0 };
    const targets = [primary];
    for (const value of session.uploadTargets || []) {
      const url = new URL(value.baseUrl);
      if (url.protocol !== 'https:' && !(url.protocol === 'http:' && ['127.0.0.1', 'localhost', '[::1]'].includes(url.hostname))) continue;
      targets.push({ ...value, active: 0, speed: 0, cooldown: 0 });
    }
    const received = new Set((session.received || []).map(Number));
    const length = index => Math.min(chunkSize, file.size - index * chunkSize);
    let uploaded = [...received].reduce((sum, index) => sum + length(index), 0);
    const inFlight = new Map();
    const requests = new Set();
    let failure = null;
    const report = () => {
      const bytes = Math.min(file.size, uploaded + [...inFlight.values()].reduce((sum, n) => sum + n, 0));
      onProgress(file.size ? bytes / file.size : 0, bytes, file.size);
    };
    report();
    const choose = attempted => {
      const now = Date.now();
      let candidates = targets.filter(t => !attempted.has(t.id) && t.cooldown <= now);
      if (!candidates.length) candidates = targets.filter(t => !attempted.has(t.id));
      if (!candidates.length) return primary;
      const start = roundRobin++ % candidates.length;
      candidates = candidates.slice(start).concat(candidates.slice(0, start));
      const bestSpeed = Math.max(...targets.map(t => t.speed), 256 * 1024);
      return candidates.reduce((best, t) => {
        const score = candidate => (candidate.active + 1) / (candidate.speed || bestSpeed);
        return score(t) < score(best) ? t : best;
      });
    };
    const send = (target, index, chunk) => new Promise((resolve, reject) => {
      const xhr = new XMLHttpRequest();
      const started = performance.now();
      let lastProgress = started;
      xhr.open('POST', target.baseUrl.replace(/\/$/, '') + '/' + index);
      xhr.withCredentials = target === primary;
      xhr.setRequestHeader('Content-Type', 'application/octet-stream');
      xhr.setRequestHeader('X-Requested-With', 'SukiSUThemeAdmin');
      if (target.ticket) xhr.setRequestHeader('X-Sukisu-Upload-Ticket', target.ticket);
      xhr.upload.addEventListener('progress', event => {
        lastProgress = performance.now();
        if (event.lengthComputable) { inFlight.set(index, Math.min(event.loaded, chunk.size)); report(); }
      });
      const timer = setInterval(() => {
        const now = performance.now();
        const fastest = Math.max(...targets.filter(t => t !== target && t.cooldown <= Date.now()).map(t => t.speed), 0);
        // Retry a stalled or demonstrably slower route. The immutable chunk ID
        // makes retries safe even if the original response was lost.
        if (now - lastProgress > 30000 || now - started > 10 * 60 * 1000 ||
            (fastest > 0 && now - started > Math.max(15000, chunk.size / fastest * 4000))) xhr.abort();
      }, 1000);
      const finish = error => {
        clearInterval(timer); requests.delete(xhr); target.active--;
        if (error) reject(error); else {
          const speed = chunk.size / Math.max(0.001, (performance.now() - started) / 1000);
          target.speed = target.speed ? target.speed * 0.6 + speed * 0.4 : speed;
          resolve();
        }
      };
      xhr.addEventListener('load', () => {
        let data = {}; try { data = JSON.parse(xhr.responseText || '{}'); } catch (_) {}
        if (target === primary && xhr.status === 401) {
          onUnauthorized(); const error = new Error('请重新登录'); error.unauthorized = true; finish(error); return;
        }
        finish(xhr.status >= 200 && xhr.status < 300 && data.ok === true ? null : new Error(data.error || ('分片上传失败 HTTP ' + xhr.status)));
      });
      xhr.addEventListener('error', () => finish(new Error('上传线路不可达')));
      xhr.addEventListener('abort', () => finish(new Error('上传线路超时，正在切换入口')));
      target.active++; requests.add(xhr); xhr.send(chunk);
    });
    async function transfer(indices, onlyPrimary = false) {
      let cursor = 0;
      const worker = async () => {
        while (!failure && cursor < indices.length) {
          const index = indices[cursor++];
          const chunk = file.slice(index * chunkSize, index * chunkSize + length(index));
          const attempted = new Set();
          for (let attempt = 0; !failure; attempt++) {
            const target = onlyPrimary ? primary : choose(attempted);
            try {
              await send(target, index, chunk);
              inFlight.delete(index); uploaded += chunk.size; report(); break;
            } catch (error) {
              inFlight.delete(index); report();
              target.cooldown = Date.now() + 30000; attempted.add(target.id);
              if (error.unauthorized || attempt >= Math.max(2, targets.length)) {
                failure = error; for (const request of requests) request.abort(); throw error;
              }
            }
          }
        }
      };
      await Promise.allSettled(Array.from({ length: Math.min(4, indices.length) }, worker));
      if (failure) throw failure;
    }
    await transfer(Array.from({ length: totalChunks }, (_, i) => i).filter(i => !received.has(i)));
    let result = await apiJson(api, '/uploads/' + encodeURIComponent(id) + '/complete', {}, onUnauthorized);
    if (!result.complete) {
      // A previously acknowledged edge may have gone offline. Repair only the
      // missing chunks directly, without resending the whole file or publishing twice.
      const missing = (result.missing || []).map(Number);
      uploaded -= missing.reduce((sum, index) => sum + length(index), 0); report();
      await transfer(missing, true);
      result = await apiJson(api, '/uploads/' + encodeURIComponent(id) + '/complete', {}, onUnauthorized);
    }
    if (!result.complete) throw new Error('分片尚未合并完成，请重试上传');
    onProgress(1, file.size, file.size);
    return id;
  }

  async function attachments(form, options) {
    const files = [...form.entries()].filter(([name, value]) => ['preview', 'avatar', 'screenshots'].includes(name) && value instanceof File && value.size > 0);
    let completed = 0;
    const refs = [];
    for (const [name, file] of files) {
      const id = await upload(file, { ...options, onProgress: (_, bytes) => options.onProgress?.(completed + bytes) });
      refs.push([name === 'screenshots' ? 'screenshotUploadId' : name + 'UploadId', id]);
      completed += file.size;
    }
    for (const name of ['preview', 'avatar', 'screenshots']) form.delete(name);
    for (const [key, id] of refs) form.append(key, id);
    return completed;
  }
  window.SukisuDistributedUpload = { upload, attachments };
})();
