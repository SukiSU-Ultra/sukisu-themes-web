/*
 * Client-side localization for the SukiSU theme mirror pages.
 *
 * Chinese (zh-CN) is the source language embedded in the markup. Other
 * locales live in /locales/<code>.json and translate the original Chinese
 * strings, so adding a language is a matter of dropping in a new file and
 * registering it in LANGUAGES below.
 */
(function () {
  'use strict';

  const STORAGE_KEY = 'sukisu-lang';
  const DEFAULT_LANGUAGE = 'zh-CN';
  const LANGUAGES = [
    { code: 'zh-CN', label: '中文' },
    { code: 'en', label: 'English' },
  ];

  const ATTRS = ['placeholder', 'title', 'aria-label', 'alt'];
  const SKIP_TAGS = new Set(['SCRIPT', 'STYLE', 'CODE', 'PRE', 'TEXTAREA', 'NOSCRIPT']);
  const SKIP_SELECTOR = '[data-i18n-ignore], .sha-box, .author-fp';

  const locales = {};
  const localePromises = {};
  const textStates = new WeakMap();
  const attrStates = new WeakMap();
  let current = pickInitialLanguage();
  let applying = false;
  let observer = null;

  function pickInitialLanguage() {
    try {
      const saved = localStorage.getItem(STORAGE_KEY);
      if (saved && LANGUAGES.some(item => item.code === saved)) return saved;
    } catch (_) {}
    const preferred = (navigator.languages && navigator.languages[0]) || navigator.language || DEFAULT_LANGUAGE;
    return /^zh\b/i.test(preferred) ? 'zh-CN' : 'en';
  }

  function hasLocale(code) {
    return code === 'zh-CN' || !!locales[code];
  }

  function translateValue(raw) {
    return translate(raw, 0);
  }

  function translate(raw, depth) {
    if (current === 'zh-CN' || !hasLocale(current) || typeof raw !== 'string') return raw;
    const lead = raw.match(/^\s*/)[0];
    const tail = raw.match(/\s*$/)[0];
    let core = raw.slice(lead.length, raw.length - tail.length);
    if (!core) return raw;
    core = core.replace(/\s+/g, ' ');
    if (current === 'en' && /^[。，、；：！？]+$/.test(core)) {
      return lead + core.replace(/[。，、；：！？]/g, ch => ({
        '。': '.', '，': ',', '、': ',', '；': ';', '：': ':', '！': '!', '？': '?',
      })[ch]) + tail;
    }
    if (!/[\u3400-\u9fff]/.test(core)) return raw;
    const locale = locales[current];
    const exact = locale.messages[core];
    if (exact != null) return lead + exact + tail;
    for (const [regex, template] of locale.patterns) {
      const match = core.match(regex);
      if (!match) continue;
      let out = template.replace(/\{\{p:\$(\d)\|([^|]*)\|([^}]*)\}\}/g, (_, index, one, many) => {
        return Number(match[Number(index)]) === 1 ? one : many;
      });
      out = out.replace(/\{\{t:\$(\d)\}\}/g, (_, index) => {
        const value = match[Number(index)] ?? '';
        return depth < 3 ? translate(value, depth + 1) : value;
      });
      out = out.replace(/\$(\d)/g, (_, index) => match[Number(index)] ?? '');
      return lead + out + tail;
    }
    return raw;
  }

  function escapeRegExp(value) {
    return value.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  }

  function compileLocale(data) {
    const messages = data.messages || {};
    const patterns = (data.patterns || []).map(([source, template]) => [new RegExp(source), template]);
    // Backend messages use named or empty placeholders ({}, {filename}, ...).
    // Turn every translated template into a matcher so runtime messages with
    // filled-in values still translate, without maintaining a regex per string.
    const templates = [];
    for (const [source, target] of Object.entries(messages)) {
      if (!/\{[A-Za-z0-9_]*\}/.test(source)) continue;
      const parts = source.split(/\{[A-Za-z0-9_]*\}/);
      let sourcePattern = '^';
      parts.forEach((part, index) => {
        sourcePattern += escapeRegExp(part);
        if (index < parts.length - 1) sourcePattern += '(.+?)';
      });
      sourcePattern += '$';
      let group = 0;
      const replacement = target.replace(/\{[A-Za-z0-9_]*\}/g, () => '$' + (++group));
      try {
        templates.push([new RegExp(sourcePattern), replacement, source.length]);
      } catch (_) {}
    }
    templates.sort((a, b) => b[2] - a[2]);
    return { messages, patterns: patterns.concat(templates) };
  }

  function loadLocale(code) {
    if (code === DEFAULT_LANGUAGE || hasLocale(code)) return Promise.resolve();
    if (localePromises[code]) return localePromises[code];
    localePromises[code] = fetch('/locales/' + encodeURIComponent(code) + '.json', { cache: 'no-store' })
      .then(response => {
        if (!response.ok) throw new Error('locale request failed: HTTP ' + response.status);
        return response.json();
      })
      .then(data => {
        locales[code] = compileLocale(data);
      })
      .catch(error => {
        delete localePromises[code];
        throw error;
      });
    return localePromises[code];
  }

  function shouldSkip(element) {
    if (!element || element.nodeType !== 1) return false;
    if (SKIP_TAGS.has(element.tagName)) return true;
    if (element.isContentEditable) return true;
    try {
      return !!element.closest(SKIP_SELECTOR);
    } catch (_) {
      return false;
    }
  }

  function applyTextNode(node) {
    if (!node.parentElement || shouldSkip(node.parentElement)) return;
    const raw = node.nodeValue;
    if (raw == null) return;
    let state = textStates.get(node);
    if (!state) {
      state = { source: raw, applied: raw };
      textStates.set(node, state);
    } else if (raw !== state.applied) {
      state.source = raw;
      state.applied = raw;
    }
    const target = translateValue(state.source);
    if (target !== raw) {
      applying = true;
      try {
        node.nodeValue = target;
      } finally {
        applying = false;
      }
      state.applied = target;
    }
  }

  function shouldSkipAttribute(element) {
    if (!element || element.nodeType !== 1) return false;
    if (element.isContentEditable) return true;
    if (element.tagName === 'SCRIPT' || element.tagName === 'STYLE') return true;
    try {
      if (element.closest(SKIP_SELECTOR)) return true;
    } catch (_) {
      return true;
    }
    // A textarea's placeholder is UI copy even though its content must not be
    // touched, so skip only skipped ancestors for attribute translation.
    let parent = element.parentElement;
    while (parent) {
      if (SKIP_TAGS.has(parent.tagName)) return true;
      parent = parent.parentElement;
    }
    return false;
  }

  function applyAttribute(element, name) {
    if (!ATTRS.includes(name) || shouldSkipAttribute(element)) return;
    const raw = element.getAttribute(name);
    if (raw == null) return;
    let byName = attrStates.get(element);
    if (!byName) {
      byName = {};
      attrStates.set(element, byName);
    }
    let state = byName[name];
    if (!state) {
      state = { source: raw, applied: raw };
      byName[name] = state;
    } else if (raw !== state.applied) {
      state.source = raw;
      state.applied = raw;
    }
    const target = translateValue(state.source);
    if (target !== raw) {
      applying = true;
      try {
        element.setAttribute(name, target);
      } finally {
        applying = false;
      }
      state.applied = target;
    }
  }

  function applyAttributes(element) {
    ATTRS.forEach(name => {
      if (element.hasAttribute(name)) applyAttribute(element, name);
    });
  }

  function applyTree(root) {
    if (!root) return;
    if (root.nodeType === 3) {
      applyTextNode(root);
      return;
    }
    if (root.nodeType !== 1 && root.nodeType !== 9 && root.nodeType !== 11) return;
    // Attributes are handled separately from text so that elements whose text
    // must stay untouched (textarea, pre, code) still get translated
    // placeholders, titles, and labels.
    if (root.nodeType === 1) applyAttributes(root);
    root.querySelectorAll('*').forEach(applyAttributes);
    const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT, {
      acceptNode(node) {
        return node.parentElement && shouldSkip(node.parentElement)
          ? NodeFilter.FILTER_REJECT
          : NodeFilter.FILTER_ACCEPT;
      },
    });
    let node = walker.nextNode();
    while (node) {
      applyTextNode(node);
      node = walker.nextNode();
    }
  }

  function startObserver() {
    if (observer || typeof MutationObserver !== 'function') return;
    observer = new MutationObserver(records => {
      if (applying) return;
      for (const record of records) {
        if (record.type === 'childList') {
          record.addedNodes.forEach(node => applyTree(node));
        } else if (record.type === 'characterData') {
          applyTextNode(record.target);
        } else if (record.type === 'attributes' && record.attributeName) {
          applyAttribute(record.target, record.attributeName);
        }
      }
    });
    observer.observe(document.documentElement, {
      subtree: true,
      childList: true,
      characterData: true,
      attributes: true,
      attributeFilter: ATTRS,
    });
  }

  function updateSwitchers() {
    document.querySelectorAll('.i18n-select').forEach(select => {
      select.value = current;
      const label = translateValue('切换语言');
      select.setAttribute('aria-label', label);
      select.title = label;
    });
    document.documentElement.lang = current;
  }

  function mountSwitchers() {
    document.querySelectorAll('[data-lang-switcher]').forEach(host => {
      if (host.querySelector('.i18n-select')) return;
      const select = document.createElement('select');
      select.className = 'i18n-select';
      LANGUAGES.forEach(language => {
        const option = document.createElement('option');
        option.value = language.code;
        option.textContent = language.label;
        select.appendChild(option);
      });
      select.value = current;
      select.addEventListener('change', () => {
        void setLanguage(select.value);
      });
      host.appendChild(select);
    });
  }

  function injectStyles() {
    if (document.getElementById('i18n-style')) return;
    const style = document.createElement('style');
    style.id = 'i18n-style';
    style.textContent = [
      'html[data-i18n-loading] body{visibility:hidden!important}',
      '[data-lang-switcher]{display:inline-flex;align-items:center}',
      '.i18n-select{height:40px;padding:0 28px 0 11px;border:1px solid var(--line,rgba(148,163,184,.28));',
      'border-radius:12px;background-color:var(--panel,rgba(15,23,42,.62));color:var(--muted,#a6b4d4);',
      'font:inherit;font-size:12px;font-weight:650;cursor:pointer;-webkit-appearance:none;appearance:none;',
      'background-image:url("data:image/svg+xml,%3Csvg xmlns=\'http://www.w3.org/2000/svg\' width=\'10\' height=\'6\' viewBox=\'0 0 10 6\'%3E%3Cpath d=\'M1 1l4 4 4-4\' fill=\'none\' stroke=\'%2394a3b8\' stroke-width=\'1.6\' stroke-linecap=\'round\' stroke-linejoin=\'round\'/%3E%3C/svg%3E");',
      'background-repeat:no-repeat;background-position:right 10px center;transition:.2s ease}',
      '.i18n-select:hover{color:var(--text,#edf3ff);border-color:var(--line-strong,rgba(148,163,184,.5))}',
      '.i18n-select:focus-visible{outline:2px solid var(--brand,#2563eb);outline-offset:2px}',
      '.lang-switch-fixed{position:fixed;top:22px;right:76px;z-index:6}',
      '@media(max-width:620px){.lang-switch-fixed{top:14px;right:66px}.i18n-select{height:36px;font-size:11.5px}}',
      '.lang-switch-compact .i18n-select{height:32px;border-radius:11px;font-size:11.5px}',
    ].join('');
    (document.head || document.documentElement).appendChild(style);
  }

  async function setLanguage(code, options) {
    const settings = options || {};
    if (!LANGUAGES.some(item => item.code === code)) return;
    if (code !== DEFAULT_LANGUAGE) {
      try {
        await loadLocale(code);
      } catch (error) {
        if (window.console) console.warn('[i18n] falling back to Chinese:', error);
        code = DEFAULT_LANGUAGE;
      }
    }
    current = code;
    if (settings.persist !== false) {
      try { localStorage.setItem(STORAGE_KEY, code); } catch (_) {}
    }
    updateSwitchers();
    applyTree(document.documentElement);
    document.dispatchEvent(new CustomEvent('sukisu:languagechange', { detail: { language: code } }));
  }

  function t(source) {
    return translateValue(source);
  }

  async function init() {
    injectStyles();
    mountSwitchers();
    try {
      if (current !== DEFAULT_LANGUAGE) await loadLocale(current);
    } catch (error) {
      if (window.console) console.warn('[i18n] falling back to Chinese:', error);
      current = DEFAULT_LANGUAGE;
    }
    updateSwitchers();
    applyTree(document.documentElement);
    startObserver();
    document.documentElement.removeAttribute('data-i18n-loading');
    if (current === DEFAULT_LANGUAGE) {
      // Warm the English dictionary so switching feels instant.
      setTimeout(() => { loadLocale('en').catch(() => {}); }, 1200);
    }
  }

  if (current !== DEFAULT_LANGUAGE) {
    document.documentElement.setAttribute('data-i18n-loading', '');
    injectStyles();
    loadLocale(current).catch(() => {});
  }

  window.SukisuI18n = {
    get language() { return current; },
    get locale() { return current === 'zh-CN' ? 'zh-CN' : (current === 'en' ? 'en' : current); },
    get languages() { return LANGUAGES.slice(); },
    t,
    apply: applyTree,
    setLanguage,
  };

  window.addEventListener('storage', event => {
    if (event.key !== STORAGE_KEY || !event.newValue) return;
    if (event.newValue === current) return;
    void setLanguage(event.newValue, { persist: false });
  });

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', () => { void init(); }, { once: true });
  } else {
    void init();
  }
})();
