/*
 * Shared image viewer for theme previews and screenshots.
 *
 * The console (index.html) and the account centre (account.html) both render
 * theme details, so the zoomable viewer and the gallery markup live here to keep
 * one behaviour across every theme page:
 *
 *   SukiSuImageViewer.gallery(images[, options])  -> gallery HTML string
 *   SukiSuImageViewer.attach(root, images)        -> bind the gallery triggers
 *   SukiSuImageViewer.open(images[, index])       -> open the full screen viewer
 *   SukiSuImageViewer.close()
 *
 * `images` is an array of { src, label, downloadName }. The viewer always shows
 * the stored file at its real pixel size (100% is one image pixel per CSS pixel),
 * fits it to the window on open and supports wheel/pinch zoom plus drag panning.
 */
(function () {
  'use strict';

  var MIN_SCALE = 0.05;
  var MAX_SCALE = 8;
  var ZOOM_STEP = 1.25;

  var dialog = null;
  var els = null;
  var galleries = new WeakMap();
  var pointers = new Map();
  var drag = null;
  var pinch = null;

  var state = {
    images: [],
    index: 0,
    scale: 1,
    fitScale: 1,
    x: 0,
    y: 0,
    width: 0,
    height: 0,
    dragged: false,
  };

  function escapeHtml(value) {
    return String(value == null ? '' : value)
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;')
      .replace(/'/g, '&#39;');
  }

  function clamp(value, min, max) {
    return Math.min(max, Math.max(min, value));
  }

  function normalize(images) {
    return (Array.isArray(images) ? images : [])
      .map(function (item) {
        if (!item) return null;
        if (typeof item === 'string') item = { src: item };
        var src = String(item.src || '').trim();
        if (!src) return null;
        return {
          src: src,
          label: String(item.label || ''),
          downloadName: String(item.downloadName || ''),
        };
      })
      .filter(Boolean);
  }

  function fileExtension(path) {
    var match = String(path || '').match(/\.(png|jpe?g|webp|gif|avif)$/i);
    return match ? match[0].toLowerCase() : '.png';
  }

  // ── Markup ────────────────────────────────────────────────────────────────

  function galleryHtml(images, options) {
    var list = normalize(images);
    if (!list.length) return '';
    options = options || {};
    var badges = options.badges ? String(options.badges) : '';
    var hero = list[0];
    var rest = list.slice(1);
    var strip = rest.length
      ? '<div class="gallery-strip">' + rest.map(function (item, index) {
        return '<button type="button" class="gallery-thumb" data-gallery-index="' + (index + 1) +
          '" title="查看原图"><img src="' + escapeHtml(item.src) + '" alt="' +
          escapeHtml(item.label) + '" loading="lazy" decoding="async">' +
          '<span class="gallery-badge">' + escapeHtml(item.label) + '</span></button>';
      }).join('') + '</div>'
      : '';
    return '<div class="theme-gallery">' +
      '<div class="gallery-head"><span class="gallery-title">主题图片</span>' +
      '<span class="gallery-meta">共 ' + list.length + ' 张 · 点击查看原图</span></div>' +
      '<button type="button" class="gallery-hero" data-gallery-index="0" title="查看原图">' +
      '<img class="gallery-hero-img" src="' + escapeHtml(hero.src) + '" alt="' +
      escapeHtml(hero.label) + '" decoding="async" fetchpriority="high">' +
      '<span class="gallery-badge">' + escapeHtml(hero.label) + '</span>' +
      '<span class="gallery-zoom">点击查看原图</span>' +
      badges +
      '</button>' +
      strip +
      '</div>';
  }

  function viewerHtml() {
    return '' +
      '<div class="iv-bar">' +
      '<div class="iv-caption"><b id="ivName">预览图</b><span class="iv-counter" id="ivCounter"></span></div>' +
      '<div class="iv-tools">' +
      '<button class="iv-btn" type="button" data-iv="zoom-out" title="缩小" aria-label="缩小">−</button>' +
      '<button class="iv-btn iv-scale-btn" type="button" data-iv="fit" title="适应窗口" aria-label="适应窗口"><span id="ivScale">100%</span></button>' +
      '<button class="iv-btn" type="button" data-iv="zoom-in" title="放大" aria-label="放大">+</button>' +
      '<button class="iv-btn" type="button" data-iv="actual" title="原始尺寸" aria-label="原始尺寸">1:1</button>' +
      '<a class="iv-btn iv-download" id="ivDownload" download title="下载原图">下载原图</a>' +
      '<button class="iv-btn iv-close" type="button" data-iv="close" title="关闭" aria-label="关闭">关闭</button>' +
      '</div>' +
      '</div>' +
      '<div class="iv-stage" id="ivStage" tabindex="-1">' +
      '<img class="iv-image" id="ivImage" alt="" draggable="false" decoding="async">' +
      '<button class="iv-nav iv-prev" type="button" data-iv="prev" aria-label="上一张" title="上一张">‹</button>' +
      '<button class="iv-nav iv-next" type="button" data-iv="next" aria-label="下一张" title="下一张">›</button>' +
      '<div class="iv-status" id="ivStatus">加载中…</div>' +
      '</div>' +
      '<div class="iv-thumbs" id="ivThumbs" role="tablist" aria-label="主题图片"></div>' +
      '<div class="iv-hint">滚轮或双指缩放，拖动平移；←/→ 切换图片，Esc 关闭</div>';
  }

  function ensure() {
    if (dialog) return dialog;
    dialog = document.createElement('dialog');
    dialog.className = 'image-viewer-dialog';
    dialog.id = 'imageViewerDialog';
    dialog.setAttribute('aria-label', '图片查看器');
    dialog.innerHTML = viewerHtml();
    els = {
      name: dialog.querySelector('#ivName'),
      counter: dialog.querySelector('#ivCounter'),
      scale: dialog.querySelector('#ivScale'),
      download: dialog.querySelector('#ivDownload'),
      stage: dialog.querySelector('#ivStage'),
      image: dialog.querySelector('#ivImage'),
      status: dialog.querySelector('#ivStatus'),
      thumbs: dialog.querySelector('#ivThumbs'),
      prev: dialog.querySelector('.iv-prev'),
      next: dialog.querySelector('.iv-next'),
    };
    bindViewer();
    document.body.appendChild(dialog);
    return dialog;
  }

  function close() {
    if (dialog && dialog.open) dialog.close();
  }

  // Chrome delivers Escape to every stacked modal dialog, which would also
  // close the theme detail underneath. Guard it while the viewer is open and
  // close only this layer; the next Escape goes through to the page again.
  function onEscape(event) {
    if (event.key !== 'Escape') return;
    if (!dialog || !dialog.open) return;
    event.preventDefault();
    event.stopPropagation();
    close();
  }

  // ── Viewer behaviour ──────────────────────────────────────────────────────

  function computeFit() {
    var stage = els.stage;
    var width = stage.clientWidth;
    var height = stage.clientHeight;
    if (!state.width || !state.height || !width || !height) return 1;
    return Math.min(1, width / state.width, height / state.height);
  }

  function clampPosition() {
    var stage = els.stage;
    var width = state.width * state.scale;
    var height = state.height * state.scale;
    if (width <= stage.clientWidth) {
      state.x = (stage.clientWidth - width) / 2;
    } else {
      state.x = clamp(state.x, stage.clientWidth - width, 0);
    }
    if (height <= stage.clientHeight) {
      state.y = (stage.clientHeight - height) / 2;
    } else {
      state.y = clamp(state.y, stage.clientHeight - height, 0);
    }
  }

  function applyTransform() {
    clampPosition();
    els.image.style.transform = 'translate(' + state.x + 'px, ' + state.y + 'px) scale(' + state.scale + ')';
    els.scale.textContent = Math.round(state.scale * 100) + '%';
    els.stage.classList.toggle('is-zoomed', state.scale > state.fitScale + 0.001);
  }

  function fit() {
    state.fitScale = computeFit();
    state.scale = state.fitScale;
    state.x = 0;
    state.y = 0;
    applyTransform();
  }

  function actualSize() {
    var wasFit = Math.abs(state.scale - state.fitScale) < 0.001;
    state.scale = 1;
    if (wasFit) {
      var stage = els.stage;
      state.x = (stage.clientWidth - state.width) / 2;
      state.y = (stage.clientHeight - state.height) / 2;
    }
    applyTransform();
  }

  function zoomAt(px, py, factor) {
    var next = clamp(state.scale * factor, MIN_SCALE, MAX_SCALE);
    if (Math.abs(next - state.scale) < 0.0001) return;
    var ratio = next / state.scale;
    state.x = px - (px - state.x) * ratio;
    state.y = py - (py - state.y) * ratio;
    state.scale = next;
    applyTransform();
  }

  function zoomFromCenter(factor) {
    zoomAt(els.stage.clientWidth / 2, els.stage.clientHeight / 2, factor);
  }

  function updateChrome() {
    var item = state.images[state.index];
    if (!item) return;
    els.name.textContent = item.label || '预览图';
    els.counter.textContent = (state.index + 1) + ' / ' + state.images.length;
    els.download.href = item.src;
    if (item.downloadName) els.download.setAttribute('download', item.downloadName);
    else els.download.removeAttribute('download');
    els.prev.disabled = state.index <= 0;
    els.next.disabled = state.index >= state.images.length - 1;
    var single = state.images.length < 2;
    els.prev.classList.toggle('hidden', single);
    els.next.classList.toggle('hidden', single);
  }

  function highlightThumb() {
    Array.prototype.forEach.call(els.thumbs.children, function (button, index) {
      var active = index === state.index;
      button.classList.toggle('active', active);
      button.setAttribute('aria-selected', active ? 'true' : 'false');
      if (active) {
        try {
          button.scrollIntoView({ block: 'nearest', inline: 'center' });
        } catch (_) {}
      }
    });
  }

  function renderThumbs() {
    if (state.images.length < 2) {
      els.thumbs.classList.add('hidden');
      els.thumbs.innerHTML = '';
      return;
    }
    els.thumbs.classList.remove('hidden');
    els.thumbs.innerHTML = state.images.map(function (item, index) {
      return '<button type="button" role="tab" data-index="' + index + '" title="' +
        escapeHtml(item.label) + '"><img src="' + escapeHtml(item.src) + '" alt="" loading="lazy" decoding="async"></button>';
    }).join('');
    highlightThumb();
  }

  function show(index, reset) {
    if (index < 0 || index >= state.images.length) return;
    state.index = index;
    state.width = 0;
    state.height = 0;
    state.scale = 1;
    state.fitScale = 1;
    state.x = 0;
    state.y = 0;
    els.image.style.visibility = 'hidden';
    els.image.style.width = '0px';
    els.image.style.height = '0px';
    els.image.style.transform = '';
    els.image.classList.remove('is-error');
    els.stage.classList.remove('is-zoomed');
    els.status.textContent = '加载中…';
    els.image.src = state.images[index].src;
    updateChrome();
    highlightThumb();
    if (reset) state.dragged = false;
    // A cached image completes before the load listener runs, so handle it here.
    if (els.image.complete && els.image.naturalWidth) handleLoad();
  }

  function handleLoad() {
    var image = els.image;
    if (!image.naturalWidth) return;
    state.width = image.naturalWidth;
    state.height = image.naturalHeight;
    image.style.width = state.width + 'px';
    image.style.height = state.height + 'px';
    image.style.visibility = 'visible';
    els.status.textContent = state.width + ' × ' + state.height;
    fit();
  }

  function handleError() {
    if (!els.image.getAttribute('src')) return;
    els.image.style.visibility = 'hidden';
    els.status.textContent = '图片加载失败';
  }

  function go(step) {
    var next = state.index + step;
    if (next < 0 || next >= state.images.length) return;
    show(next, true);
  }

  function pointerDistance() {
    var items = Array.from(pointers.values());
    if (items.length < 2) return 0;
    return Math.hypot(items[0].x - items[1].x, items[0].y - items[1].y);
  }

  function pointerMidpoint() {
    var items = Array.from(pointers.values());
    if (items.length < 2) return { x: 0, y: 0 };
    return {
      x: (items[0].x + items[1].x) / 2,
      y: (items[0].y + items[1].y) / 2,
    };
  }

  function cancelGesture() {
    pointers.clear();
    drag = null;
    pinch = null;
    els.stage.classList.remove('is-dragging');
  }

  function onPointerDown(event) {
    if (event.button !== undefined && event.button !== 0) return;
    if (event.target.closest('.iv-nav')) return;
    try {
      els.stage.setPointerCapture(event.pointerId);
    } catch (_) {}
    pointers.set(event.pointerId, { x: event.clientX, y: event.clientY });
    if (pointers.size === 1) {
      drag = { x: event.clientX, y: event.clientY, startX: state.x, startY: state.y };
      state.dragged = false;
      els.stage.classList.add('is-dragging');
    } else if (pointers.size === 2) {
      drag = null;
      pinch = { last: pointerDistance() };
      els.stage.classList.add('is-dragging');
    }
  }

  function onPointerMove(event) {
    if (!pointers.has(event.pointerId)) return;
    pointers.set(event.pointerId, { x: event.clientX, y: event.clientY });
    if (pointers.size >= 2) {
      var distance = pointerDistance();
      var midpoint = pointerMidpoint();
      if (pinch && pinch.last > 0 && distance > 0) {
        var rect = els.stage.getBoundingClientRect();
        zoomAt(midpoint.x - rect.left, midpoint.y - rect.top, distance / pinch.last);
        pinch.last = distance;
      }
      state.dragged = true;
      return;
    }
    if (!drag) return;
    var dx = event.clientX - drag.x;
    var dy = event.clientY - drag.y;
    if (Math.abs(dx) + Math.abs(dy) > 3) state.dragged = true;
    state.x = drag.startX + dx;
    state.y = drag.startY + dy;
    applyTransform();
  }

  function onPointerUp(event) {
    if (!pointers.has(event.pointerId)) return;
    pointers.delete(event.pointerId);
    if (pointers.size < 2) pinch = null;
    if (pointers.size === 0) {
      drag = null;
      els.stage.classList.remove('is-dragging');
      // A plain click on the dark stage background closes the viewer; dragging
      // never does, so panning around never dismisses the image.
      if (!state.dragged && event.target === els.stage) close();
    }
  }

  function onWheel(event) {
    if (!event.deltaY && !event.deltaX) return;
    event.preventDefault();
    var rect = els.stage.getBoundingClientRect();
    var factor = Math.exp(-(event.deltaY || event.deltaX) * 0.0016);
    zoomAt(event.clientX - rect.left, event.clientY - rect.top, factor);
  }

  function onDialogClick(event) {
    var button = event.target.closest('[data-iv]');
    if (!button) return;
    switch (button.getAttribute('data-iv')) {
      case 'zoom-in':
        zoomFromCenter(ZOOM_STEP);
        break;
      case 'zoom-out':
        zoomFromCenter(1 / ZOOM_STEP);
        break;
      case 'fit':
        fit();
        break;
      case 'actual':
        actualSize();
        break;
      case 'prev':
        go(-1);
        break;
      case 'next':
        go(1);
        break;
      case 'close':
        close();
        break;
      default:
        break;
    }
  }

  function onKey(event) {
    var key = event.key;
    if (key === 'ArrowLeft') {
      event.preventDefault();
      go(-1);
    } else if (key === 'ArrowRight') {
      event.preventDefault();
      go(1);
    } else if (key === '+' || key === '=') {
      event.preventDefault();
      zoomFromCenter(ZOOM_STEP);
    } else if (key === '-' || key === '_') {
      event.preventDefault();
      zoomFromCenter(1 / ZOOM_STEP);
    } else if (key === '0') {
      event.preventDefault();
      fit();
    } else if (key === '1') {
      event.preventDefault();
      actualSize();
    }
  }

  function onThumbsClick(event) {
    var button = event.target.closest('[data-index]');
    if (!button) return;
    show(Number(button.getAttribute('data-index')) || 0, true);
  }

  function onStageDoubleClick(event) {
    if (event.target.closest('.iv-nav')) return;
    if (state.scale > 1.001) fit();
    else actualSize();
  }

  function bindViewer() {
    els.image.addEventListener('load', handleLoad);
    els.image.addEventListener('error', handleError);
    els.stage.addEventListener('wheel', onWheel, { passive: false });
    els.stage.addEventListener('pointerdown', onPointerDown);
    els.stage.addEventListener('pointermove', onPointerMove);
    els.stage.addEventListener('pointerup', onPointerUp);
    els.stage.addEventListener('pointercancel', onPointerUp);
    els.stage.addEventListener('dblclick', onStageDoubleClick);
    els.thumbs.addEventListener('click', onThumbsClick);
    dialog.addEventListener('click', onDialogClick);
    dialog.addEventListener('keydown', onKey);
    dialog.addEventListener('close', function () {
      document.removeEventListener('keydown', onEscape, true);
      cancelGesture();
      state.images = [];
      state.index = 0;
      els.thumbs.innerHTML = '';
      els.image.removeAttribute('src');
      els.image.style.transform = '';
    });
    window.addEventListener('resize', function () {
      if (!dialog || !dialog.open) return;
      var wasFit = Math.abs(state.scale - state.fitScale) < 0.001;
      state.fitScale = computeFit();
      if (wasFit) fit();
      else applyTransform();
    });
  }

  // ── Public API ────────────────────────────────────────────────────────────

  function open(images, index, options) {
    var list = normalize(images);
    if (!list.length) return;
    ensure();
    state.images = list;
    state.index = clamp(Number(index) || 0, 0, list.length - 1);
    renderThumbs();
    if (!dialog.open) {
      document.addEventListener('keydown', onEscape, true);
      dialog.showModal();
    }
    show(state.index, true);
    options = options || {};
    if (options.focus !== false) {
      try {
        els.stage.focus({ preventScroll: true });
      } catch (_) {}
    }
  }

  function attach(root, images) {
    if (!root) return;
    galleries.set(root, normalize(images));
    if (root.dataset.viewerBound === '1') return;
    root.dataset.viewerBound = '1';
    root.addEventListener('click', function (event) {
      var trigger = event.target.closest('[data-gallery-index]');
      if (!trigger) return;
      event.preventDefault();
      var list = galleries.get(root) || [];
      if (!list.length) return;
      open(list, Number(trigger.getAttribute('data-gallery-index')) || 0);
    });
  }

  window.SukiSuImageViewer = {
    open: open,
    close: close,
    gallery: galleryHtml,
    attach: attach,
    fileExtension: fileExtension,
  };
})();
