let currentTab = 'balls';
let currentSnapshot = null;
let currentSettings = { version: 1, order: [], balls: {} };
let currentProxyDetail = null;
let proxyPollTimer = null;
let saveDebounceTimer = null;
let activeRowBalls = {}; // planId -> GrokBall instance
let currentCredentials = {}; // planId -> bool，手动凭据槽的占用图

const $ = s => document.querySelector(s);

// 行内「或手动粘凭据 →」锚点：跳到凭据区并直接展开对应的粘贴表单。
// 和 popover 开关一样走 document 级委托——行是每次 render 重建的。
document.addEventListener('click', (e) => {
  const jump = e.target.closest('.jump-cred');
  if(!jump) return;
  const panel = $('#credPanel');
  if(!panel) return;
  panel.scrollIntoView({ behavior: 'smooth', block: 'nearest' });
  panel.style.transition = 'box-shadow .3s ease';
  panel.style.boxShadow = '0 0 0 2px var(--sage-deep)';
  setTimeout(() => { panel.style.boxShadow = ''; }, 1000);
  panel.querySelector(`.cred-paste[data-plan="${jump.dataset.plan}"]`)?.click();
});

// 点击外部关闭所有 popover（颜色 / 形态）。注册一次：以前在 renderBallsTab
// 里注册，每次快照刷新（60s 一跳）都会再叠一个，窗口开得越久点一下要跑的
// 监听器越多。
document.addEventListener('click', (e) => {
  if(!e.target.closest('.color-picker-wrap')){
    document.querySelectorAll('.color-picker-wrap.open').forEach(w => {
      w.classList.remove('open');
      w.querySelector('.color-popover')?.classList.remove('open');
    });
  }
  if(!e.target.closest('.shape-picker-wrap')){
    document.querySelectorAll('.shape-picker-wrap.open').forEach(w => {
      w.classList.remove('open');
      w.querySelector('.shape-popover')?.classList.remove('open');
    });
  }
});

function showToast(msg){
  const t = $('#toast');
  t.textContent = msg;
  t.classList.add('show');
  clearTimeout(t._timer);
  t._timer = setTimeout(() => t.classList.remove('show'), 1600);
}

// popover 贴底时向上翻：先量出自然高度，超出内容区可视范围就加 .flip-up
function flipPopoverIfClipped(pop){
  pop.classList.remove('flip-up');
  const rect = pop.getBoundingClientRect();
  const paneRect = $('#mainPane').getBoundingClientRect();
  if(rect.bottom > paneRect.bottom - 8){
    pop.classList.add('flip-up');
  }
}

// 主题：settings.theme（system/dark/light）→ <html data-theme>，CSS 变量接管
function applyTheme(theme){
  const t = ['dark', 'light'].includes(theme) ? theme : 'system';
  document.documentElement.dataset.theme = t;
}

function updateHeaderTime(ts){
  const el = $('#updateTime');
  if(el){
    el.textContent = ts ? ('更新于 ' + formatTime(ts)) : '刚刚更新';
  }
  updateAppMemory();
}

async function updateAppMemory(){
  const el = $('#appMemory');
  if(!el) return;
  try{
    const res = await DangoBridge.appMemory();
    if(res && res.formatted){
      el.textContent = '内存 ' + res.formatted;
    }
  }catch(_){
    // backend starting or unavailable
  }
}

function showSaveStatus(text, type = ''){
  const el = $('#saveStatus');
  if(!el) return;
  el.textContent = text;
  el.className = 'save-status ' + type;
}

function triggerSaveSettings(immediate = false){
  if(saveDebounceTimer){
    clearTimeout(saveDebounceTimer);
    saveDebounceTimer = null;
  }
  if(immediate){
    doSaveSettings();
  }else{
    showSaveStatus('修改中...');
    saveDebounceTimer = setTimeout(doSaveSettings, 300);
  }
}

async function doSaveSettings(){
  showSaveStatus('保存中...');
  try{
    currentSettings = await DangoBridge.saveSettings(currentSettings);
    showSaveStatus('已保存', 'ok');
    setTimeout(() => {
      const el = $('#saveStatus');
      if(el && el.textContent === '已保存') el.textContent = '';
    }, 2000);
  }catch(err){
    console.error('save_settings failed:', err);
    showSaveStatus('保存失败: ' + err, 'err');
  }
}

/* ============================================================
   页签 1: 小球设置
   ============================================================ */
/* ============================================================
   凭据区：手动粘 token（dango 自己的钥匙串专区，vendor 只读）
   ============================================================ */
const CRED_PLANS = [
  { id:'haze',    name:'Haze',    hint:'session token（Haze App 登录态）' },
  { id:'devin',   name:'Devin',   hint:'CLI token' },
  { id:'factory', name:'Factory', hint:'access token' },
];
// 「凭据」面板支持手动凭据的套餐 id（其它套餐不是单 token 形态）
const CRED_SUPPORTED = new Set(CRED_PLANS.map(p => p.id));

// 每家「读不到登录态时怎么办」的一句话指引；支持手动槽的再给一个跳转锚点。
const CONNECT_GUIDE = {
  claude:      '打开 Claude 桌面端就会重新采样额度',
  haze:        '在 Haze App 里「退出登录 → 重新登录」（只关窗重开不会换新凭据）',
  devin:       '在 Devin CLI 里重新登录（写回 credentials.toml）',
  factory:     '在 Factory App 里重新登录',
  cursor:      '在 Cursor App 里重新登录',
  antigravity: '账号池归反代桥管——去「Gemini 反代」页看账号健康',
};

async function renderCredsPanel(){
  const list = $('#credList');
  if(!list) return;
  try{
    currentCredentials = await DangoBridge.credentials();
  }catch(_){ currentCredentials = {}; }

  list.innerHTML = CRED_PLANS.map(({id, name, hint}) => {
    const set = !!currentCredentials[id];
    return `
      <div class="cred-row" data-cred="${id}">
        <span class="cred-name">${name}</span>
        <span class="cred-status ${set ? 'is-manual' : ''}">${set ? '● 手动凭据生效中' : '自动读取中'}</span>
        <button class="glass-btn btn-xs cred-paste" data-plan="${id}">${set ? '更换凭据' : '粘贴凭据'}</button>
        ${set ? `<button class="glass-btn btn-xs cred-clear" data-plan="${id}">清除</button>` : ''}
      </div>
      <div class="cred-form" data-form="${id}" style="display:none">
        <input type="password" placeholder="粘贴 ${hint}，只进钥匙串不回显" autocomplete="off" spellcheck="false">
        <button class="glass-btn btn-xs cred-save" data-plan="${id}">保存</button>
        <button class="glass-btn btn-xs cred-cancel">取消</button>
      </div>`;
  }).join('');

  list.querySelectorAll('.cred-paste').forEach(btn => btn.addEventListener('click', () => {
    const form = list.querySelector(`.cred-form[data-form="${btn.dataset.plan}"]`);
    const show = form.style.display === 'none';
    list.querySelectorAll('.cred-form').forEach(f => { f.style.display = 'none'; });
    if(show){
      form.style.display = 'flex';
      form.querySelector('input').focus();
    }
  }));
  list.querySelectorAll('.cred-cancel').forEach(btn => btn.addEventListener('click', () => {
    list.querySelectorAll('.cred-form').forEach(f => { f.style.display = 'none'; });
  }));
  list.querySelectorAll('.cred-save').forEach(btn => btn.addEventListener('click', async () => {
    const plan = btn.dataset.plan;
    const input = list.querySelector(`.cred-form[data-form="${plan}"] input`);
    const token = input.value.trim();
    if(!token){ showSaveStatus('凭据是空的', 'err'); return; }
    btn.disabled = true;
    try{
      await DangoBridge.setCredential(plan, token);
      input.value = '';
      // 后端保存即触发 refresh，新凭据的效果一次 SSE 内到位。
      showSaveStatus('凭据已存进钥匙串，正在刷新…', 'ok');
    }catch(err){
      showSaveStatus('凭据保存失败: ' + (err.message || err), 'err');
    }
    btn.disabled = false;
    renderCredsPanel();
  }));
  list.querySelectorAll('.cred-clear').forEach(btn => btn.addEventListener('click', async () => {
    const plan = btn.dataset.plan;
    btn.disabled = true;
    try{
      await DangoBridge.deleteCredential(plan);
      showSaveStatus('手动凭据已清除，回到自动读取', 'ok');
    }catch(err){
      showSaveStatus('清除失败: ' + (err.message || err), 'err');
    }
    btn.disabled = false;
    renderCredsPanel();
  }));
}

function renderBallsTab(){
  // 销毁旧的 row ball 实例
  Object.values(activeRowBalls).forEach(b => {
    try { b.destroy(); } catch(e){}
  });
  activeRowBalls = {};

  const plansMap = new Map();
  if(currentSnapshot?.plans){
    currentSnapshot.plans.forEach(p => plansMap.set(p.id, p));
  }

  // 排序：以 currentSettings.order 为准，补全 snapshot 里未列出的
  let order = Array.isArray(currentSettings.order) ? [...currentSettings.order] : [];
  plansMap.forEach((_, id) => {
    if(!order.includes(id)) order.push(id);
  });
  // 过滤掉当前完全不存在的 id（如果 snapshot 存在）
  if(plansMap.size > 0){
    order = order.filter(id => plansMap.has(id));
  }
  if(order.length === 0){
    order = ['claude', 'haze', 'antigravity', 'devin', 'cursor', 'factory'];
  }
  currentSettings.order = order;

  const pane = $('#mainPane');
  pane.innerHTML = `
    <div class="pane-header">
      <div>
        <h2 class="pane-title">小球</h2>
        <p class="pane-desc">拖拽调整悬浮胶囊展示顺序，自定每颗套餐小球的形状外观。</p>
      </div>
    </div>

    <div class="plan-list" id="planList"></div>

    <div class="balls-actions">
      <button id="addBallBtn" class="glass-btn btn-xs">＋ 添加小球</button>
      <button id="resetDefaultsBtn" class="glass-btn btn-xs">恢复默认</button>
    </div>

    <section class="section-panel add-ball ${addBallOpen ? '' : 'is-hidden'}" id="addBallPanel">
      ${renderAddBallPanel()}
    </section>

    <section class="section-panel" id="connectPanel">
      ${renderConnectPanel(plansMap)}
    </section>

    <section class="section-panel">
      <div class="panel-head"><span class="panel-title">偏好</span></div>
      <div class="pref-row ring-row">
        <span class="pref-label">环样式</span>
        <div class="ring-grid" id="ringGrid"></div>
      </div>
      <div class="pref-row">
        <span class="pref-label">外观主题</span>
        <div class="seg-control" id="themeControl">
          <button class="seg-btn" data-value="system">跟随系统</button>
          <button class="seg-btn" data-value="dark">深色</button>
          <button class="seg-btn" data-value="light">浅色</button>
        </div>
      </div>
      <div class="pref-row">
        <span class="pref-label">性能模式</span>
        <div class="seg-control" id="perfControl">
          <button class="seg-btn" data-value="smooth">流畅</button>
          <button class="seg-btn" data-value="balanced">均衡</button>
          <button class="seg-btn" data-value="saver">省电</button>
        </div>
      </div>
      <p class="pref-hint">流畅 ≈ 60fps 满特效（约 5% CPU）· 均衡 ≈ 30fps（约 2.5%）· 省电 = 平时静止，悬停才动（闲置约 0%）。也同步到托盘「性能模式」。</p>
    </section>

    <section class="section-panel" id="credPanel">
      <div class="panel-head"><span class="panel-title">凭据</span></div>
      <p class="pref-hint" style="margin-top:0">小球一般自己读应用登录态；读不到时可以在这里手动粘一个 token——存进 macOS 钥匙串的 dango 专区，碰不到应用自己的凭据。清除后回到自动读取。</p>
      <div id="credList"></div>
    </section>
  `;

  renderCredsPanel();

  // 环样式：示例数据 = 剩余 62%，颜色取第一颗球的本色（双环内圈固定 100%）
  const ringGridEl = $('#ringGrid');
  if(ringGridEl){
    const firstPlan = order[0];
    const ringColor = currentSettings.balls?.[firstPlan]?.color
      || PALETTE[firstPlan] || '#B9B0A4';
    const currentRingMode = RING_MODES.some(m => m.id === currentSettings.ringMode)
      ? currentSettings.ringMode
      : 'plain';
    ringGridEl.innerHTML = RING_MODES.map(m => `
      <button class="ring-tile ${currentRingMode === m.id ? 'active' : ''}" data-mode="${m.id}" title="环换成「${m.label}」">
        <span class="ring-tile-preview">${ringPreviewSVG(m.id, 62, ringColor, 100)}</span>
        <span class="ring-tile-label">${m.label}</span>
      </button>
    `).join('');
    ringGridEl.querySelectorAll('.ring-tile').forEach(btn => {
      btn.addEventListener('click', () => {
        if(btn.dataset.mode === currentSettings.ringMode) return;
        currentSettings.ringMode = btn.dataset.mode;
        ringGridEl.querySelectorAll('.ring-tile').forEach(b =>
          b.classList.toggle('active', b === btn));
        triggerSaveSettings(true);
        const label = RING_MODES.find(m => m.id === btn.dataset.mode)?.label || btn.dataset.mode;
        showToast('已换成 ' + label);
      });
    });
  }

  const perfMode = ['smooth', 'balanced', 'saver'].includes(currentSettings.perfMode)
    ? currentSettings.perfMode
    : 'balanced';
  pane.querySelectorAll('#perfControl .seg-btn').forEach(btn => {
    btn.classList.toggle('active', btn.dataset.value === perfMode);
    btn.addEventListener('click', () => {
      currentSettings.perfMode = btn.dataset.value;
      pane.querySelectorAll('#perfControl .seg-btn').forEach(b =>
        b.classList.toggle('active', b === btn));
      triggerSaveSettings(true);
    });
  });

  const themeMode = ['system', 'dark', 'light'].includes(currentSettings.theme)
    ? currentSettings.theme
    : 'system';
  pane.querySelectorAll('#themeControl .seg-btn').forEach(btn => {
    btn.classList.toggle('active', btn.dataset.value === themeMode);
    btn.addEventListener('click', () => {
      currentSettings.theme = btn.dataset.value;
      applyTheme(btn.dataset.value);
      pane.querySelectorAll('#themeControl .seg-btn').forEach(b =>
        b.classList.toggle('active', b === btn));
      triggerSaveSettings(true);
    });
  });

  const listEl = $('#planList');

  order.forEach((planId, index) => {
    // Never fabricate an ok/percent for a plan that has no snapshot yet — the
    // fallback row reports "待拉取" and renders the empty-face ball, matching
    // the card's rule of 错误如实报 + 不拿缓存冒充.
    const plan = plansMap.get(planId) || {
      id: planId,
      name: planId.charAt(0).toUpperCase() + planId.slice(1),
      ok: false,
      remainingPercent: null,
      pending: true,
    };

    const currentShape = currentSettings.balls?.[planId]?.shape || DEFAULT_SHAPE[planId] || 'blob';
    const currentColor = currentSettings.balls?.[planId]?.color || PALETTE[planId] || '#B9B0A4';
    const pct = plan.ok ? plan.remainingPercent : null;
    const pctText = plan.pending ? '待拉取' : ((plan.ok && pct != null) ? pct.toFixed(0) + '%' : (plan.ok ? '—' : '异常'));
    const pctColor = plan.pending ? 'var(--ink-4)' : colorFor(pct, plan.ok);

    const row = document.createElement('div');
    row.className = 'plan-row';
    row.dataset.planId = planId;
    row.dataset.index = index;

    // 形态选择：一个 popover，不再把 8 种形态平铺进每一行。
    // 平铺时每行要塞 8 个 mini 球 + 8 个文字标签，行高压到 54px 还挤成一团；
    // 收起来后行内只剩一颗当前形态的徽章，点一下才展开全部选项。
    const shapePopoverHtml = AVAILABLE_SHAPES.map(s => `
      <button class="shape-option ${currentShape === s.id ? 'active' : ''}" data-shape="${s.id}" title="${s.label}">
        <span class="mini-ball-box" data-mini="${s.id}"></span>
        <span>${s.label}</span>
      </button>
    `).join('');

    // 色板列表
    const swatchesHtml = PRESET_COLORS.map(c => `
      <button class="swatch-btn ${currentColor.toUpperCase() === c.toUpperCase() ? 'active' : ''}"
              data-color="${c}"
              style="background-color:${c}"
              title="${c}">
      </button>
    `).join('');

    const shapeLabel = AVAILABLE_SHAPES.find(s => s.id === currentShape)?.label || 'Blob';

    row.innerHTML = `
      <div class="drag-handle" title="按住拖拽排序">
        <svg width="12" height="12" viewBox="0 0 24 24" fill="currentColor">
          <circle cx="9" cy="5" r="2.2"/><circle cx="15" cy="5" r="2.2"/>
          <circle cx="9" cy="12" r="2.2"/><circle cx="15" cy="12" r="2.2"/>
          <circle cx="9" cy="19" r="2.2"/><circle cx="15" cy="19" r="2.2"/>
        </svg>
      </div>
      <div class="row-preview"></div>
      <div class="row-info">
        <div class="row-info-line">
          <span class="row-name">${escapeHtml(plan.name)}</span>
          <span class="row-pct" style="color:${pctColor}">${pctText}</span>
        </div>
        ${!plan.pending && !plan.ok && plan.error ? `
        <div class="row-err-msg">${escapeHtml(errorHint(plan.error))} · ${escapeHtml(plan.error)}</div>
        ${CONNECT_GUIDE[planId] ? `
        <div class="row-guide">${CONNECTABLE.has(planId) ? `<button class="glass-btn btn-xs btn-primary connect-btn" data-plan="${planId}">一键登录</button> ` : ''}怎么修：${escapeHtml(CONNECT_GUIDE[planId])}${CRED_SUPPORTED.has(planId) ? ` <a class="jump-cred" data-plan="${planId}">或手动粘凭据 →</a>` : ''}</div>
        ` : ''}
        ` : ''}
      </div>

      <!-- 形态徽章：点开才列出全部 8 种 -->
      <div class="shape-picker-wrap">
        <button class="shape-badge" title="更改小球形态">
          <span class="mini-ball-box" data-mini="${currentShape}"></span>
          <span class="shape-badge-label">${shapeLabel}</span>
          <span class="shape-badge-chevron">
            <svg width="9" height="9" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round">
              <polyline points="6 9 12 15 18 9"></polyline>
            </svg>
          </span>
        </button>
        <div class="shape-popover">
          <div class="shape-popover-title">小球形态</div>
          <div class="shape-options">
            ${shapePopoverHtml}
          </div>
        </div>
      </div>

      <!-- 颜色选择器 (macOS Color Well) -->
      <div class="color-picker-wrap">
        <button class="color-trigger" title="更改小球颜色">
          <span class="color-swatch-circle" style="background-color:${currentColor}"></span>
          <span class="color-trigger-chevron">
            <svg width="9" height="9" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round">
              <polyline points="6 9 12 15 18 9"></polyline>
            </svg>
          </span>
        </button>
        <div class="color-popover">
          <div class="color-popover-title">预设色彩</div>
          <div class="color-swatches-grid">
            ${swatchesHtml}
          </div>
          <div class="color-popover-actions">
            <label class="custom-color-label">
              <svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                <circle cx="12" cy="12" r="10"></circle>
                <line x1="12" y1="8" x2="12" y2="16"></line>
                <line x1="8" y1="12" x2="16" y2="12"></line>
              </svg>
              <span>自定义</span>
              <input type="color" class="custom-color-input" value="${currentColor}">
            </label>
            <button class="reset-color-btn" title="恢复默认套餐色">默认</button>
          </div>
        </div>
      </div>
    `;

    listEl.appendChild(row);

    const previewBox = row.querySelector('.row-preview');
    function updateBallPreview(shape, color){
      if(activeRowBalls[planId]){
        activeRowBalls[planId].destroy();
      }
      previewBox.innerHTML = '';
      activeRowBalls[planId] = GrokBall.create(previewBox, {
        emotion: emotionFor(plan),
        shape: shape,
        color: color,
        eyeColor: '#F5F2ED',
        lite: true,
        eyeScale: 1.15,
      });
    }

    // 初始渲染主预览小球
    updateBallPreview(currentShape, currentColor);

    // 渲染 popover 里各选项的迷你静态小球。
    // 徽章上那颗由 applyNewShape 单独管理（它要跟着选择变），不在这里批量建。
    row.querySelectorAll('.shape-popover .mini-ball-box').forEach(box => {
      const s = box.dataset.mini;
      GrokBall.create(box, {
        emotion: '02',
        shape: s,
        color: '#A89E9A',
        eyeColor: '#F5F2ED',
        lite: true,
        autostart: false,
      });
    });

    // 形态切换：徽章点开 popover，选项点一下即换并收起
    const shapeWrap = row.querySelector('.shape-picker-wrap');
    const shapeBadge = row.querySelector('.shape-badge');
    const shapePopover = row.querySelector('.shape-popover');
    const shapeBadgeBall = shapeBadge.querySelector('.mini-ball-box');
    // 徽章上那颗球用套餐本色，让"当前是哪颗球"一眼可见
    activeRowBalls[planId + ':badge'] = GrokBall.create(shapeBadgeBall, {
      emotion: '02', shape: currentShape, color: currentColor,
      eyeColor: '#F5F2ED', lite: true, autostart: false,
    });

    function closeShapePopover(){
      shapeWrap.classList.remove('open');
      shapePopover.classList.remove('open');
    }

    shapeBadge.addEventListener('click', (e) => {
      e.stopPropagation();
      // 和颜色 popover 互斥：同时只开一个
      document.querySelectorAll('.color-picker-wrap.open').forEach(w => {
        w.classList.remove('open');
        w.querySelector('.color-popover')?.classList.remove('open');
      });
      document.querySelectorAll('.shape-picker-wrap.open').forEach(w => {
        if(w !== shapeWrap){
          w.classList.remove('open');
          w.querySelector('.shape-popover')?.classList.remove('open');
        }
      });
      const isOpen = shapeWrap.classList.contains('open');
      shapeWrap.classList.toggle('open', !isOpen);
      shapePopover.classList.toggle('open', !isOpen);
      if(!isOpen) flipPopoverIfClipped(shapePopover);
    });

    shapePopover.addEventListener('click', (e) => e.stopPropagation());

    function applyNewShape(newShape){
      if(!currentSettings.balls) currentSettings.balls = {};
      if(!currentSettings.balls[planId]) currentSettings.balls[planId] = {};
      currentSettings.balls[planId].shape = newShape;

      row.querySelectorAll('.shape-option').forEach(b => {
        b.classList.toggle('active', b.dataset.shape === newShape);
      });
      // 徽章上的 mini 球换成新形态，让当前选择一眼可见
      if(activeRowBalls[planId + ':badge']) activeRowBalls[planId + ':badge'].destroy();
      shapeBadgeBall.innerHTML = '';
      activeRowBalls[planId + ':badge'] = GrokBall.create(shapeBadgeBall, {
        emotion: '02', shape: newShape, color: currentColor,
        eyeColor: '#F5F2ED', lite: true, autostart: false,
      });
      const label = AVAILABLE_SHAPES.find(s => s.id === newShape)?.label;
      if(label) shapeBadge.querySelector('.shape-badge-label').textContent = label;

      updateBallPreview(newShape, currentColor);
      triggerSaveSettings();
    }

    row.querySelectorAll('.shape-option').forEach(btn => {
      btn.addEventListener('click', (e) => {
        e.stopPropagation();
        applyNewShape(btn.dataset.shape);
        closeShapePopover();
      });
    });

    // 颜色选择交互
    const colorWrap = row.querySelector('.color-picker-wrap');
    const colorTrigger = row.querySelector('.color-trigger');
    const colorPopover = row.querySelector('.color-popover');
    const swatchCircle = row.querySelector('.color-swatch-circle');
    const customInput = row.querySelector('.custom-color-input');
    const resetColorBtn = row.querySelector('.reset-color-btn');

    colorTrigger.addEventListener('click', (e) => {
      e.stopPropagation();
      const isOpen = colorWrap.classList.contains('open');
      // 关闭其他已打开的 popover
      document.querySelectorAll('.color-picker-wrap.open').forEach(w => {
        if(w !== colorWrap){
          w.classList.remove('open');
          const pop = w.querySelector('.color-popover');
          if(pop) pop.classList.remove('open');
        }
      });
      colorWrap.classList.toggle('open', !isOpen);
      colorPopover.classList.toggle('open', !isOpen);
      if(!isOpen) flipPopoverIfClipped(colorPopover);
    });

    colorPopover.addEventListener('click', (e) => {
      e.stopPropagation();
    });

    function applyNewColor(newHex){
      if(!currentSettings.balls) currentSettings.balls = {};
      if(!currentSettings.balls[planId]) currentSettings.balls[planId] = {};
      currentSettings.balls[planId].color = newHex;

      swatchCircle.style.backgroundColor = newHex;
      customInput.value = newHex;

      // 更新高亮 swatch
      row.querySelectorAll('.swatch-btn').forEach(sb => {
        sb.classList.toggle('active', sb.dataset.color.toUpperCase() === newHex.toUpperCase());
      });

      const activeShape = currentSettings.balls[planId]?.shape || DEFAULT_SHAPE[planId] || 'blob';
      updateBallPreview(activeShape, newHex);
      triggerSaveSettings();
    }

    row.querySelectorAll('.swatch-btn').forEach(sb => {
      sb.addEventListener('click', (e) => {
        e.stopPropagation();
        applyNewColor(sb.dataset.color);
        colorWrap.classList.remove('open');
        colorPopover.classList.remove('open');
      });
    });

    customInput.addEventListener('input', (e) => {
      applyNewColor(e.target.value);
    });

    resetColorBtn.addEventListener('click', (e) => {
      e.stopPropagation();
      const defaultCol = PALETTE[planId] || '#B9B0A4';
      if(currentSettings.balls?.[planId]){
        delete currentSettings.balls[planId].color;
      }
      applyNewColor(defaultCol);
      colorWrap.classList.remove('open');
      colorPopover.classList.remove('open');
    });

    // 拖拽排序 Pointer 事件绑定
    bindRowDrag(row, row.querySelector('.drag-handle'));
  });

  // 恢复默认按钮：只重置顺序和形状/颜色，保留 perfMode / theme 等无关设置
  bindAddBallPanel();
  bindConnectButtons();
  $('#resetDefaultsBtn').addEventListener('click', () => {
    currentSettings.order = ['claude', 'haze', 'antigravity', 'devin', 'cursor', 'factory'];
    currentSettings.balls = {
      claude: { shape: 'star' },
      haze: { shape: 'blob' },
      antigravity: { shape: 'gem' },
      devin: { shape: 'wedge' },
      cursor: { shape: 'blob' },
      factory: { shape: 'wedge' },
    };
    renderBallsTab();
    triggerSaveSettings(true);
    showToast('已恢复默认小球设置');
  });
}

function bindRowDrag(row, handle){
  handle.addEventListener('pointerdown', (e) => {
    e.preventDefault();
    const list = $('#planList');
    const rows = Array.from(list.querySelectorAll('.plan-row'));
    const initialIndex = rows.indexOf(row);
    let currentIndex = initialIndex;
    const startY = e.clientY;
    const rowH = row.offsetHeight + 8; // 包含 gap

    row.classList.add('is-dragging');
    isDraggingRow = true;
    handle.setPointerCapture(e.pointerId);

    function onPointerMove(ev){
      const deltaY = ev.clientY - startY;
      row.style.transform = `translateY(${deltaY}px)`;

      const rawTarget = initialIndex + Math.round(deltaY / rowH);
      const newTarget = Math.max(0, Math.min(rows.length - 1, rawTarget));

      if(newTarget !== currentIndex){
        currentIndex = newTarget;
        rows.forEach((r, idx) => {
          if(r === row) return;
          if(initialIndex < currentIndex){
            if(idx > initialIndex && idx <= currentIndex){
              r.style.transform = `translateY(-${rowH}px)`;
            }else{
              r.style.transform = 'translateY(0)';
            }
          }else if(initialIndex > currentIndex){
            if(idx >= currentIndex && idx < initialIndex){
              r.style.transform = `translateY(${rowH}px)`;
            }else{
              r.style.transform = 'translateY(0)';
            }
          }else{
            r.style.transform = 'translateY(0)';
          }
        });
      }
    }

    function onPointerUp(ev){
      isDraggingRow = false;
      handle.releasePointerCapture(ev.pointerId);
      handle.removeEventListener('pointermove', onPointerMove);
      handle.removeEventListener('pointerup', onPointerUp);
      handle.removeEventListener('pointercancel', onPointerUp);

      rows.forEach(r => {
        r.style.transform = 'translateY(0)';
        r.classList.remove('is-dragging');
      });

      if(currentIndex !== initialIndex && currentIndex >= 0){
        const currentOrder = [...currentSettings.order];
        const item = currentOrder.splice(initialIndex, 1)[0];
        currentOrder.splice(currentIndex, 0, item);
        currentSettings.order = currentOrder;
        renderBallsTab();
        triggerSaveSettings();
      }
    }

    handle.addEventListener('pointermove', onPointerMove);
    handle.addEventListener('pointerup', onPointerUp);
    handle.addEventListener('pointercancel', onPointerUp);
  });
}

/* ============================================================
   页签 2: 反代设置（Gemini 账号池桥）
   ============================================================ */
async function loadProxyDetail(planId){
  try{
    const res = await DangoBridge.proxyDetail(planId);
    if(currentTab === planId){
      currentProxyDetail = res;
      renderProxyPane(res, planId);
      updateHeaderTime(Date.now());
    }
  }catch(err){
    console.error('proxy_detail error:', err);
    showSaveStatus('读取反代详情失败: ' + err, 'err');
    if(currentTab === planId){
      renderProxyError(planId, err);
    }
  }
}

// 「多久以前」的小格式化，只给 stats 条用。
function fmtAgo(ms){
  const diff = Date.now() - Number(ms || 0);
  if(diff < 0) return '刚刚';
  const s = Math.floor(diff / 1000);
  if(s < 60) return s + 's 前';
  const m = Math.floor(s / 60);
  if(m < 60) return m + 'm 前';
  const h = Math.floor(m / 60);
  if(h < 24) return h + 'h 前';
  return Math.floor(h / 24) + 'd 前';
}

// 反代页顶的实时计数条：来源随后端（tap 有今日/上游码，桥有错误数/内存），
// 有哪项显示哪项，没有的字段就是后端没报。
function renderStatsStrip(stats){
  if(!stats) return '';
  const chips = [];
  if(stats.listening != null)
    chips.push(`<span class="stat-chip ${stats.listening ? 'chip-ok' : 'chip-err'}">${stats.listening ? '● 监听中' : '○ 未监听'}</span>`);
  if(stats.upstreamStatus != null){
    const ok = stats.upstreamStatus >= 200 && stats.upstreamStatus < 300;
    chips.push(`<span class="stat-chip ${ok ? 'chip-ok' : 'chip-err'}">${ok ? '●' : '○'} 上游 ${stats.upstreamStatus}</span>`);
  }
  if(stats.requestsToday != null)
    chips.push(`<span class="stat-chip">今日 ${stats.requestsToday}</span>`);
  if(stats.requestsTotal != null)
    chips.push(`<span class="stat-chip">总请求 ${stats.requestsTotal}</span>`);
  if(stats.inFlight != null && stats.inFlight > 0)
    chips.push(`<span class="stat-chip chip-live">在途 ${stats.inFlight}</span>`);
  if(stats.errorsTotal != null && stats.errorsTotal > 0)
    chips.push(`<span class="stat-chip chip-err">错误 ${stats.errorsTotal}</span>`);
  if(stats.memoryMb != null)
    chips.push(`<span class="stat-chip">${stats.memoryMb.toFixed(0)} MB</span>`);
  if(stats.lastUpstreamAt != null)
    chips.push(`<span class="stat-chip">最近上游 ${fmtAgo(stats.lastUpstreamAt)}</span>`);
  return chips.length ? `<div class="stats-strip">${chips.join('')}</div>` : '';
}

// 反代入口：快照里带 proxy 的套餐各一个，名字和健康点都来自数据，不写死。
const PROXY_ICON = '<svg viewBox="0 0 14 14" width="13" height="13" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"><rect x="1.5" y="2.5" width="11" height="3.4" rx="1.2"/><rect x="1.5" y="8" width="11" height="3.4" rx="1.2"/><circle cx="4.4" cy="4.2" r="0.6" fill="currentColor" stroke="none"/><circle cx="4.4" cy="9.7" r="0.6" fill="currentColor" stroke="none"/></svg>';
function syncProxyNav(snapshot){
  const nav = $('#proxyNav');
  if(!nav) return;
  const plans = (snapshot?.plans || []).filter(p => p.proxy);
  const html = plans.map(p => `
    <button class="nav-item ${currentTab === p.id ? 'active' : ''}" data-tab="${escapeHtml(p.id)}" title="${escapeHtml(p.proxy.name || p.name)}">
      <span class="nav-icon icon-proxy">${PROXY_ICON}</span>
      <span>${escapeHtml(p.proxy.name || p.name)}</span>
      <span class="nav-dot ${p.proxy.ok ? 'ok' : 'bad'}"></span>
    </button>`).join('');
  if(nav.dataset.html !== html){
    nav.dataset.html = html;
    nav.innerHTML = plans.length ? html : '<div class="nav-empty">暂无本机反代</div>';
  }
}

function proxyTitle(planId){
  const plan = (currentSnapshot?.plans || []).find(p => p.id === planId);
  return plan?.proxy?.name || plan?.name || planId;
}

function renderProxyError(planId, err){
  const pane = $('#mainPane');
  pane.innerHTML = `
    <div class="pane-header">
      <div>
        <h2 class="pane-title">${escapeHtml(proxyTitle(planId))}</h2>
        <p class="pane-desc">本机反代的接入信息、连通测试与请求记录。</p>
      </div>
    </div>
    <div class="error-box">✕ 读取反代详情失败: ${escapeHtml(err || '未知错误')}</div>
  `;
}

// 连通测试的结果留在内存里，10 秒一次的详情刷新重绘页面时不丢。
const proxyTestState = {};

function curlFor(baseUrl, model){
  const body = JSON.stringify({ model: model || 'MODEL', messages: [{ role: 'user', content: 'ping' }], max_tokens: 16 });
  return `curl -s ${baseUrl}/chat/completions -H 'Content-Type: application/json' -d '${body}'`;
}

function renderTestResult(planId){
  const box = $('#testResult');
  if(!box) return;
  const st = proxyTestState[planId];
  if(!st){
    box.innerHTML = `<div class="test-hint">先拉模型列表，再用选中的模型发一句最短的对话，验证「本机反代 → 凭据 → 上游」整条链路。会消耗几个 token。</div>`;
    return;
  }
  if(st.running){
    box.innerHTML = `<div class="test-hint"><span class="spinner"></span> 正在测试…</div>`;
    return;
  }
  if(st.error){
    box.innerHTML = `<div class="error-box">✕ ${escapeHtml(st.error)}</div>`;
    return;
  }
  const r = st.result;
  box.innerHTML = `
    <div class="test-summary ${r.ok ? 'ok' : 'bad'}">${r.ok ? '● 链路通畅' : '○ 链路有问题'}<span>${escapeHtml(fmtAgo(st.at))}</span></div>
    ${r.steps.map(step => `
      <div class="test-step">
        <span class="dot ${step.ok ? 'ok' : 'bad'}"></span>
        <span class="test-step-name">${escapeHtml(step.name)}</span>
        <span class="test-step-meta">${step.status != null ? 'HTTP ' + step.status + ' · ' : ''}${step.ms} ms</span>
        <span class="test-step-detail">${escapeHtml(step.detail)}</span>
      </div>`).join('')}
  `;
}

function renderProxyPane(detail, planId){
  const pane = $('#mainPane');
  if(!detail){
    pane.innerHTML = `<div class="empty-box">未找到该套餐的反代端点信息</div>`;
    return;
  }
  const endpoints = Array.isArray(detail.endpoints) && detail.endpoints.length
    ? detail.endpoints
    : [{ kind: 'openai', label: 'OpenAI 兼容', baseUrl: detail.baseUrl }];
  const models = Array.isArray(detail.models) ? detail.models : [];
  const st = proxyTestState[planId] || {};
  // 默认测「最近一次成功请求用的模型」：列表第一个未必是能用的那个。
  const lastGood = (detail.recent || []).find(r => r.model && r.status >= 200 && r.status < 300)?.model;
  // 跳过 chat_20706 这类内部占位名：拿它测会被上游 429，还会把账号打进冷却。
  const normal = models.find(m => !/^chat_\d+$/.test(m.id))?.id;
  const fallback = lastGood && models.some(m => m.id === lastGood) ? lastGood : (normal || models[0]?.id || '');
  const chosen = st.model && models.some(m => m.id === st.model) ? st.model : fallback;

  // 保留滚动位置和模型过滤词：10 秒一次的自动刷新不该把人顶回页首。
  const keepScroll = pane.scrollTop;
  const keepQuery = $('#modelSearch')?.value || '';

  pane.innerHTML = `
    <div class="pane-header">
      <div>
        <h2 class="pane-title">${escapeHtml(detail.name || proxyTitle(planId))}</h2>
        <p class="pane-desc">${escapeHtml(detail.description || '本机 OpenAI 兼容端点。')}</p>
      </div>
    </div>

    ${renderStatsStrip(detail.stats)}

    <div class="section-panel">
      <div class="panel-head"><span class="panel-title">接入</span>
        <span class="panel-note">客户端里 Base URL、模型名分开填；API Key 随便填一个非空值</span></div>
      <div class="config-box">
        ${endpoints.map((ep, i) => `
          <div class="config-item">
            <span class="config-label"><span class="badge badge-owned">${escapeHtml(ep.label)}</span></span>
            <span class="config-val">${escapeHtml(ep.baseUrl)}</span>
            <button class="glass-btn btn-xs copy-ep" data-i="${i}">复制</button>
          </div>`).join('')}
        ${detail.upstream ? `<div class="config-item"><span class="config-label">上游</span><span class="config-val dim">${escapeHtml(detail.upstream)}</span></div>` : ''}
      </div>
    </div>

    <div class="section-panel">
      <div class="panel-head">
        <span class="panel-title">连通测试</span>
        <div class="test-actions">
          <select id="testModel" class="search-input test-model">${models.map(m => `<option value="${escapeHtml(m.id)}" ${m.id === chosen ? 'selected' : ''}>${escapeHtml(m.id)}</option>`).join('')}</select>
          <button id="copyCurlBtn" class="glass-btn btn-xs">复制 curl</button>
          <button id="runTestBtn" class="glass-btn btn-xs btn-primary" ${st.running ? 'disabled' : ''}>测试连通</button>
        </div>
      </div>
      <div id="testResult"></div>
    </div>

    <div class="section-panel">
      <div class="panel-head">
        <span class="panel-title">
          <span>模型</span>
          <span id="modelCountBadge" class="badge badge-owned" style="margin-left:4px">0</span>
        </span>
        <input type="text" id="modelSearch" class="search-input" placeholder="过滤模型…">
      </div>
      <div id="modelsContainer"></div>
    </div>

    <div class="section-panel">
      <div class="panel-head">
        <span class="panel-title">账号池</span>
        ${planId === 'antigravity' ? `<button id="addAccountBtn" class="glass-btn btn-xs btn-primary" title="浏览器走一遍 Google 授权，账号直接进池子">+ 添加账号</button>` : ''}
      </div>
      <div id="accountsContainer"></div>
      <div id="addAccountStatus" class="add-account-status" style="display:none"></div>
    </div>

    <div class="section-panel">
      <div class="panel-head">
        <span class="panel-title">最近请求</span>
        <span class="panel-note">最多保留 50 条</span>
      </div>
      <div id="recentContainer"></div>
    </div>
  `;

  pane.querySelectorAll('.copy-ep').forEach(btn => btn.addEventListener('click', async e => {
    const ep = endpoints[Number(btn.dataset.i)];
    await copyText(ep.baseUrl, e.currentTarget, `已复制 ${ep.label} 地址`);
  }));
  $('#testModel')?.addEventListener('change', e => {
    proxyTestState[planId] = { ...(proxyTestState[planId] || {}), model: e.target.value };
  });
  $('#copyCurlBtn').addEventListener('click', async e => {
    await copyText(curlFor(detail.baseUrl, $('#testModel')?.value), e.currentTarget, '已复制 curl');
  });
  $('#runTestBtn').addEventListener('click', async () => {
    const model = $('#testModel')?.value || null;
    proxyTestState[planId] = { model, running: true };
    $('#runTestBtn').disabled = true;
    renderTestResult(planId);
    try{
      const result = await DangoBridge.proxyTest(planId, model);
      proxyTestState[planId] = { model, result, at: Date.now() };
    }catch(err){
      proxyTestState[planId] = { model, error: String(err), at: Date.now() };
    }
    if(currentTab === planId){
      const btn = $('#runTestBtn');
      if(btn) btn.disabled = false;
      renderTestResult(planId);
    }
  });
  renderTestResult(planId);

  $('#addAccountBtn')?.addEventListener('click', async () => {
    const btn = $('#addAccountBtn');
    const box = $('#addAccountStatus');
    btn.disabled = true;
    box.style.display = '';
    box.innerHTML = '<span class="spinner"></span> 正在打开浏览器授权页…';
    try {
      await DangoBridge.proxyLogin(planId);
      box.innerHTML = '<span class="spinner"></span> 浏览器已打开 —— 完成 Google 授权后这里会自动更新';
      pollLoginStatus(planId, box, btn);
    } catch (err) {
      box.innerHTML = `✕ 启动登录失败：${escapeHtml(String(err))}`;
      btn.disabled = false;
    }
  });

  $('#modelSearch').value = keepQuery;
  renderModelsBlock(detail);
  renderAccountsBlock(detail);
  renderRecentBlock(detail);
  pane.scrollTop = keepScroll;
}

// 登录进度轮询：2s 一跳，最多等 150s（回环超时 120s + 余量）。
function pollLoginStatus(planId, box, btn){
  const deadline = Date.now() + 150_000;
  const tick = async () => {
    if (Date.now() > deadline) {
      box.innerHTML = '✕ 等授权超时了 —— 可以再点一次「添加账号」重来';
      btn.disabled = false;
      return;
    }
    try {
      const st = await DangoBridge.proxyLoginStatus(planId);
      if (st.state === 'done') {
        box.innerHTML = `✓ ${escapeHtml(st.detail || '账号已添加')}`;
        btn.disabled = false;
        loadTab(planId);  // 账号卡刷新
        return;
      }
      if (st.state === 'error') {
        box.innerHTML = `✕ ${escapeHtml(st.detail || '登录失败')}`;
        btn.disabled = false;
        return;
      }
      setTimeout(tick, 2000);
    } catch (_) {
      setTimeout(tick, 2000);   // 网络抖一下不算死
    }
  };
  setTimeout(tick, 1500);
}

function renderModelsBlock(detail){
  const container = $('#modelsContainer');
  const countBadge = $('#modelCountBadge');
  const searchInput = $('#modelSearch');

  if(detail.modelsError){
    container.innerHTML = `<div class="error-box">✕ 获取模型失败: ${escapeHtml(detail.modelsError)}</div>`;
    if(countBadge) countBadge.textContent = '0';
    return;
  }

  const allModels = Array.isArray(detail.models) ? detail.models : [];
  if(countBadge) countBadge.textContent = String(allModels.length);

  function filterAndDisplay(){
    const query = searchInput.value.trim().toLowerCase();
    const filtered = allModels.filter(m => {
      if(!query) return true;
      return (m.id && m.id.toLowerCase().includes(query)) ||
             (m.ownedBy && m.ownedBy.toLowerCase().includes(query));
    });

    if(filtered.length === 0){
      container.innerHTML = `<div class="empty-box">${allModels.length === 0 ? '暂无可用模型' : '未找到匹配的模型'}</div>`;
      return;
    }

    container.innerHTML = `<div class="model-list"></div>`;
    const listEl = container.querySelector('.model-list');

    filtered.forEach(m => {
      const row = document.createElement('div');
      row.className = 'model-row';
      const ctxText = formatContextWindow(m.contextWindow);

      row.innerHTML = `
        <span class="model-id">${escapeHtml(m.id)}</span>
        ${m.ownedBy ? `<span class="badge badge-owned">${escapeHtml(m.ownedBy)}</span>` : ''}
        ${ctxText ? `<span class="badge badge-ctx">${escapeHtml(ctxText)}</span>` : ''}
        <span class="model-copy-hint">复制名称</span>
      `;

      // 只复制模型名：Base URL 在上面单独复制，客户端也是分开两个框填。
      row.addEventListener('click', async () => {
        await copyText(m.id, null, `已复制 ${m.id}`);
      });

      listEl.appendChild(row);
    });
  }

  searchInput.addEventListener('input', filterAndDisplay);
  filterAndDisplay();
}

function renderAccountsBlock(detail){
  const container = $('#accountsContainer');

  if(detail.accountsError){
    container.innerHTML = `<div class="error-box">✕ 账号池状态异常: ${escapeHtml(detail.accountsError)}</div>`;
    return;
  }

  const accounts = Array.isArray(detail.accounts) ? detail.accounts : [];
  if(accounts.length === 0){
    container.innerHTML = `<div class="empty-box">暂无账号数据</div>`;
    return;
  }

  container.innerHTML = `<div class="accounts-grid"></div>`;
  const grid = container.querySelector('.accounts-grid');

  accounts.forEach(acc => {
    const card = document.createElement('div');
    card.className = 'account-card';

    const statusMap = {
      available: { label: '可用', dot: 'ok' },
      cooling: { label: '冷却中', dot: 'warn' },
      disabled: { label: '已禁用', dot: 'disabled' },
      error: { label: '异常', dot: 'bad' },
    };
    const s = statusMap[acc.status] || { label: acc.status || '未知', dot: 'disabled' };

    card.innerHTML = `
      <div class="account-card-head">
        <span class="account-label">${escapeHtml(acc.label)}</span>
        <span class="account-status-tag">
          <span class="dot ${s.dot}"></span>
          <span>${s.label}</span>
        </span>
      </div>
      ${acc.detail ? `<div class="account-detail">${escapeHtml(acc.detail)}</div>` : ''}
    `;

    grid.appendChild(card);
  });
}

function renderRecentBlock(detail){
  const container = $('#recentContainer');

  if(detail.recentError){
    container.innerHTML = `<div class="error-box">✕ 请求日志不可用: ${escapeHtml(detail.recentError)}</div>`;
    return;
  }

  const list = Array.isArray(detail.recent) ? detail.recent : [];
  if(list.length === 0){
    container.innerHTML = `<div class="empty-box">暂无最近请求记录</div>`;
    return;
  }

  container.innerHTML = `
    <div class="recent-table-wrap">
      <table class="recent-table">
        <thead>
          <tr>
            <th style="width:75px">时间</th>
            <th style="width:55px">方法</th>
            <th>路径</th>
            <th>模型</th>
            <th style="width:55px">状态</th>
            <th style="width:60px;text-align:right">耗时</th>
          </tr>
        </thead>
        <tbody></tbody>
      </table>
    </div>
  `;

  const tbody = container.querySelector('tbody');

  list.forEach(req => {
    const tr = document.createElement('tr');

    let statusClass = '';
    if(req.status != null){
      if(req.status >= 200 && req.status < 300) statusClass = 'ok';
      else if(req.status >= 400 && req.status < 500) statusClass = 'warn';
      else if(req.status >= 500) statusClass = 'danger';
    }

    const timeStr = formatTime(req.at);
    const msStr = req.ms != null ? `${req.ms}ms` : '—';
    const statusStr = req.status != null ? String(req.status) : '—';
    const modelStr = req.model ? escapeHtml(req.model) : '—';

    tr.innerHTML = `
      <td style="font-family:var(--f-mono)">${timeStr}</td>
      <td><span class="method-badge">${escapeHtml(req.method || 'POST')}</span></td>
      <td style="font-family:var(--f-mono)" title="${escapeHtml(req.path || '')}">${escapeHtml(req.path || '—')}</td>
      <td style="font-family:var(--f-mono)">${modelStr}</td>
      <td><span class="status-code ${statusClass}">${statusStr}</span></td>
      <td style="text-align:right;font-family:var(--f-mono);color:var(--ink-4)">${msStr}</td>
    `;

    tbody.appendChild(tr);
  });
}

async function copyText(text, btnEl = null, toastMsg = '已复制到剪贴板'){
  try{
    await DangoBridge.copyText(text);
    if(btnEl){
      const orig = btnEl.innerHTML;
      btnEl.classList.add('btn-copied');
      btnEl.innerHTML = '✓ 已复制';
      setTimeout(() => {
        btnEl.classList.remove('btn-copied');
        btnEl.innerHTML = orig;
      }, 1500);
    }
    showToast(toastMsg);
  }catch(e){
    showSaveStatus('复制失败: ' + e, 'err');
    showToast('复制失败: ' + e);
  }
}

/* ============================================================
   添加小球：模板 → API Key 进钥匙串 → 设置里多一颗 custom-* 球
   ============================================================ */
let addBallOpen = false;
let addBallKind = 'deepseek';

const BALL_TEMPLATES = [
  { kind: 'deepseek', name: 'DeepSeek', what: '余额', how: 'GET api.deepseek.com/user/balance', keyHint: 'sk-…（platform.deepseek.com → API keys）', unit: '元' },
  { kind: 'moonshot', name: 'Kimi', what: '余额', how: 'GET api.moonshot.cn/v1/users/me/balance', keyHint: 'sk-…（platform.moonshot.cn → API Key 管理）', unit: '元' },
  { kind: 'stepfun', name: '阶跃', what: '钱包余额', how: 'GET api.stepfun.com/v1/accounts（Step Plan 包月额度没有接口，查不到）', keyHint: '阶跃开放平台的 API Key', unit: '元' },
  { kind: 'openrouter', name: 'OpenRouter', what: '剩余额度', how: 'GET openrouter.ai/api/v1/credits（充值总额 − 已用，自带百分比）', keyHint: 'sk-or-…', unit: '美元' },
  { kind: 'siliconflow', name: '硅基流动', what: '余额', how: 'GET api.siliconflow.cn/v1/user/info', keyHint: 'sk-…（cloud.siliconflow.cn → API 密钥）', unit: '元' },
];

// 内置这几颗是怎么拿到数的（只读本机登录态或本机服务，不刷新任何 token）
const BUILTIN_SOURCES = [
  ['claude', 'Claude', 'Claude 桌面端自己写的额度采样文件 plan-usage-history.json（5 小时 / 7 天窗口）'],
  ['haze', 'Haze', 'Haze App 的登录态（LocalStorage，老版本走钥匙串）→ usehaze.ai/api/usage'],
  ['antigravity', 'Gemini', '本机桥 8050 的 /quota：Antigravity 账号池里正在用的那个账号'],
  ['devin', 'Devin', 'Devin CLI / 桌面端登录态 → 官方 GetUserStatus（每日额度 %；Pro 不给 ACU 和 token）'],
  ['cursor', 'Cursor', 'Cursor App 登录态 → cursor.com usage-summary，外加 Grok Bot 周额度'],
  ['factory', 'Factory', 'Factory App 存在钥匙串里的登录态 → Factory 额度接口（按池分组）'],
];

// 一键登录：调各家自己的登录（官方 CLI 在终端里跑，或打开它的 App），我们只读登录结果。
const CONNECTABLE = new Set(['claude', 'haze', 'cursor', 'devin', 'factory']);
const CONNECT_LABEL = {
  claude: '打开 Claude', haze: '打开 Haze 登录', cursor: 'cursor-agent login',
  devin: 'devin auth login', factory: '打开 Factory 登录',
};

function renderConnectPanel(plansMap){
  return `
    <div class="panel-head"><span class="panel-title">连接</span>
      <span class="token-legend">断了点一下就能重新登录；用的都是各家自己的登录</span></div>
    <div class="connect-list">
      ${BUILTIN_SOURCES.filter(([id]) => !hiddenPlans().includes(id)).map(([id, name, how]) => {
        const plan = plansMap.get(id);
        const state = !plan ? ['', '还没数据'] : plan.ok ? ['ok', '已连接'] : ['bad', errorHint(plan.error)];
        const action = CONNECTABLE.has(id)
          ? `<button class="glass-btn btn-xs connect-btn" data-plan="${id}" title="${escapeHtml(CONNECT_LABEL[id])}">${plan && !plan.ok ? '一键登录' : '重新登录'}</button>`
          : id === 'antigravity'
            ? `<button class="glass-btn btn-xs" data-goto="antigravity">加账号</button>`
            : '';
        return `<div class="connect-row">
          <span class="nav-dot ${state[0]}"></span>
          <b>${escapeHtml(name)}</b>
          <span class="connect-state ${state[0]}">${escapeHtml(state[1])}</span>
          <span class="connect-how">${escapeHtml(how)}</span>
          <span class="connect-actions">${action}<button class="glass-btn btn-xs connect-remove" data-remove="${id}">移除</button></span>
        </div>`;
      }).join('')}
    </div>`;
}

function bindConnectButtons(){
  $('#mainPane').querySelectorAll('.connect-btn').forEach(btn => {
    btn.addEventListener('click', async e => {
      e.stopPropagation();
      const plan = btn.dataset.plan;
      btn.disabled = true;
      try{
        const out = await DangoBridge.connect(plan);
        showToast(out.message + (out.url ? `：${out.url}` : ''));
        connectPending = Date.now();
      }catch(err){
        showSaveStatus('登录没启动: ' + (err.message || err), 'err');
      }finally{
        setTimeout(() => { btn.disabled = false; }, 1500);
      }
    });
  });
  $('#mainPane').querySelectorAll('[data-remove]').forEach(btn => {
    btn.addEventListener('click', () => removeBuiltinBall(btn.dataset.remove));
  });
  $('#mainPane').querySelectorAll('[data-goto]').forEach(btn => {
    btn.addEventListener('click', () => { location.hash = btn.dataset.goto; });
  });
}

// 从终端 / App 登录完切回来：刷新一次（10 秒内不重复）。
let connectPending = 0;
let lastFocusRefresh = 0;
window.addEventListener('focus', async () => {
  const now = Date.now();
  if(now - lastFocusRefresh < 10000) return;
  if(!connectPending && now - lastFocusRefresh < 60000) return;
  lastFocusRefresh = now;
  connectPending = 0;
  try{ await DangoBridge.refresh(); }catch(_){}
});

function hiddenPlans(){
  return Array.isArray(currentSettings.hidden) ? currentSettings.hidden : [];
}

// 内置球：从隐藏列表里拿出来 → 调那家的官方登录（Gemini 去加账号）。
async function addBuiltinBall(id){
  const next = structuredClone(currentSettings);
  next.hidden = hiddenPlans().filter(x => x !== id);
  next.order = [...(next.order || []).filter(x => x !== id), id];
  const name = (BUILTIN_SOURCES.find(([pid]) => pid === id) || [])[1] || id;
  try{
    currentSettings = await DangoBridge.saveSettings(next);
    if(CONNECTABLE.has(id)){
      const out = await DangoBridge.connect(id);
      showToast(`已添加 ${name} · ${out.message}${out.url ? `：${out.url}` : ''}`);
      connectPending = Date.now();
    }else{
      showToast(`已添加 ${name}`);
    }
    await DangoBridge.refresh();
    if(id === 'antigravity'){ location.hash = 'antigravity'; return; }
  }catch(err){
    showSaveStatus('添加失败: ' + (err.message || err), 'err');
  }
  renderBallsTab();
}

async function removeBuiltinBall(id){
  const name = (BUILTIN_SOURCES.find(([pid]) => pid === id) || [])[1] || id;
  if(!confirm(`从胶囊上移除「${name}」？登录状态不动，随时可以在「添加小球」里加回来。`)) return;
  const next = structuredClone(currentSettings);
  next.hidden = [...new Set([...hiddenPlans(), id])];
  try{
    currentSettings = await DangoBridge.saveSettings(next);
    await DangoBridge.refresh();
    showToast(`已移除 ${name}`);
  }catch(err){
    showSaveStatus('移除失败: ' + (err.message || err), 'err');
  }
  renderBallsTab();
}

function customPlans(){
  return Array.isArray(currentSettings.custom) ? currentSettings.custom : [];
}

function renderAddBallPanel(){
  const t = BALL_TEMPLATES.find(x => x.kind === addBallKind) || BALL_TEMPLATES[0];
  const mine = customPlans();
  return `
    <div class="panel-head"><span class="panel-title">添加小球</span></div>
    <div class="add-sub">现在支持的：点一下加上，并用那家自己的方式登录</div>
    <div class="tpl-grid">
      ${BUILTIN_SOURCES.map(([id, name, how]) => {
        const shown = !hiddenPlans().includes(id);
        return `<button class="tpl-card builtin-card ${shown ? 'is-added' : ''}" data-builtin="${id}" ${shown ? 'disabled' : ''} title="${escapeHtml(how)}">
          <b>${escapeHtml(name)}</b><span>${shown ? '已添加' : (id === 'antigravity' ? '添加并加账号' : '登录并添加')}</span>
        </button>`;
      }).join('')}
    </div>
    <div class="add-sub">按 API Key 查余额：Key 只存进钥匙串，不落盘</div>
    <div class="tpl-grid">
      ${BALL_TEMPLATES.map(x => `
        <button class="tpl-card ${x.kind === t.kind ? 'active' : ''}" data-kind="${x.kind}">
          <b>${escapeHtml(x.name)}</b><span>${escapeHtml(x.what)}</span>
        </button>`).join('')}
    </div>
    <div class="tpl-how"><code>${escapeHtml(t.how)}</code></div>
    <div class="add-ball-form">
      <label><span>名字</span><input id="addBallName" maxlength="24" value="${escapeHtml(t.name)}"></label>
      <label><span>API Key</span><input id="addBallKey" type="password" autocomplete="off" spellcheck="false" placeholder="${escapeHtml(t.keyHint)}"></label>
      <label><span>满额（可选）</span><input id="addBallBudget" inputmode="numeric" placeholder="比如 100，单位${t.unit}；填了才有环和百分比"></label>
      <div class="add-ball-submit"><button id="addBallSave" class="glass-btn btn-xs btn-primary">添加</button></div>
    </div>
    ${mine.length ? `
      <div class="custom-list">
        ${mine.map(c => `
          <div class="custom-row">
            <i style="background:${escapeHtml(currentSettings.balls?.[c.id]?.color || '#B8A9C9')}"></i>
            <b>${escapeHtml(c.name)}</b>
            <span>${escapeHtml((BALL_TEMPLATES.find(x => x.kind === c.kind) || {}).what || c.kind)}${c.budget ? ` · 满额 ${c.budget}` : ''}</span>
            <button class="glass-btn btn-xs custom-del" data-id="${escapeHtml(c.id)}">删除</button>
          </div>`).join('')}
      </div>` : ''}
`;
}

function bindAddBallPanel(){
  $('#addBallBtn')?.addEventListener('click', () => {
    addBallOpen = !addBallOpen;
    $('#addBallPanel')?.classList.toggle('is-hidden', !addBallOpen);
    if(addBallOpen) $('#addBallKey')?.focus();
  });
  const panel = $('#addBallPanel');
  if(!panel) return;
  panel.addEventListener('click', async e => {
    const builtin = e.target.closest('[data-builtin]');
    if(builtin){
      await addBuiltinBall(builtin.dataset.builtin);
      return;
    }
    const card = e.target.closest('.tpl-card');
    if(card){
      addBallKind = card.dataset.kind;
      panel.innerHTML = renderAddBallPanel();
      return;
    }
    const del = e.target.closest('.custom-del');
    if(del){
      await removeCustomBall(del.dataset.id);
      return;
    }
    if(e.target.closest('#addBallSave')) await addCustomBall();
  });
}

function nextCustomId(kind){
  const taken = new Set(customPlans().map(c => c.id));
  let id = `custom-${kind}`;
  for(let n = 2; taken.has(id); n++) id = `custom-${kind}-${n}`;
  return id;
}

async function addCustomBall(){
  const name = $('#addBallName').value.trim();
  const key = $('#addBallKey').value.trim();
  const budgetText = $('#addBallBudget').value.trim();
  if(!name){ showToast('名字不能空'); return; }
  if(!key){ showToast('先填 API Key'); return; }
  let budget;
  if(budgetText){
    budget = Math.round(Number(budgetText));
    if(!Number.isFinite(budget) || budget <= 0){ showToast('满额要填正数'); return; }
  }
  const id = nextCustomId(addBallKind);
  const used = new Set(Object.values(currentSettings.balls || {}).map(b => b.color).filter(Boolean)
    .concat(Object.values(PALETTE)));
  const color = PRESET_COLORS.find(c => !used.has(c)) || PRESET_COLORS[0];
  const next = structuredClone(currentSettings);
  next.custom = [...customPlans(), { id, kind: addBallKind, name, ...(budget ? { budget } : {}) }];
  next.balls = { ...(next.balls || {}), [id]: { color } };
  next.order = [...(next.order || []).filter(x => x !== id), id];
  const btn = $('#addBallSave');
  btn.disabled = true;
  try{
    // 先存 Key 再存设置：设置一落盘小件就会去查，Key 得已经在钥匙串里。
    await DangoBridge.setCredential(id, key);
    currentSettings = await DangoBridge.saveSettings(next);
    await DangoBridge.refresh();
    showToast(`已添加 ${name}`);
  }catch(err){
    // Key 存了但设置没存上：把 Key 也撤掉，别留孤儿。
    try{ if(!customPlans().some(c => c.id === id)) await DangoBridge.deleteCredential(id); }catch(_){}
    showSaveStatus('添加失败: ' + (err.message || err), 'err');
  }finally{
    btn.disabled = false;
  }
  renderBallsTab();
}

async function removeCustomBall(id){
  const plan = customPlans().find(c => c.id === id);
  if(!plan) return;
  if(!confirm(`删除「${plan.name}」？它的 API Key 也会从钥匙串里删掉。`)) return;
  const next = structuredClone(currentSettings);
  next.custom = customPlans().filter(c => c.id !== id);
  if(next.balls) delete next.balls[id];
  next.order = (next.order || []).filter(x => x !== id);
  try{
    currentSettings = await DangoBridge.saveSettings(next);
    try{ await DangoBridge.deleteCredential(id); }catch(_){ /* 没存过 Key 也算删干净 */ }
    await DangoBridge.refresh();
    showToast(`已删除 ${plan.name}`);
  }catch(err){
    showSaveStatus('删除失败: ' + (err.message || err), 'err');
  }
  renderBallsTab();
}

/* ============================================================
   Token 记录：本机工具日志 → 8049 /tokens（只读，不联网）
   ============================================================ */
let tokenPollTimer = null;
let tokenReport = null;
let tokenDays = 14;
let tokenWithCache = true;

const TOKEN_SOURCES = {
  'claude-code': { label: 'Claude Code', color: () => PALETTE.claude },
  factory: { label: 'Factory', color: () => PALETTE.factory },
  cursor: { label: 'Cursor', color: () => PALETTE.cursor },
  devin: { label: 'Devin', color: () => PALETTE.devin },
};
const TOKEN_UNREADABLE = 'Devin 的 token 是本机会话库（sessions.db）里每轮推理的记账，不是官方用量口径。Cursor 的用量取自它官方的用量明细（App、CLI Agent、Grok Bot 都在里面，5 分钟同步一次）。Haze App 自己的用量只在它服务器上。';

function tokenColor(source){
  return TOKEN_SOURCES[source]?.color() || 'var(--ink-4)';
}

function tokenDot(color){
  return `<i class="token-dot" style="background:${color}"></i>`;
}

function fmtTokens(n){
  if(!n) return '0';
  if(n >= 1e8) return (n / 1e8).toFixed(n >= 1e9 ? 1 : 2) + ' 亿';
  if(n >= 1e4) return (n / 1e4).toFixed(n >= 1e6 ? 0 : 1) + ' 万';
  return n.toLocaleString('zh-CN');
}

function cacheRate(c){
  const prompt = c.input + c.cacheRead + c.cacheWrite;
  return prompt ? c.cacheRead / prompt : null;
}
function fmtRate(r){
  return r == null ? '—' : `${(r * 100).toFixed(r >= 0.995 || r < 0.1 ? 1 : 0)}%`;
}

function tokenSum(counts){
  if(!counts) return 0;
  return counts.input + counts.output + (tokenWithCache ? counts.cacheRead + counts.cacheWrite : 0);
}

function addCounts(into, c){
  into.input += c.input; into.output += c.output;
  into.cacheRead += c.cacheRead; into.cacheWrite += c.cacheWrite;
  return into;
}
const zeroCounts = () => ({ input: 0, output: 0, cacheRead: 0, cacheWrite: 0 });

function tokenChartSVG(days, keys, pick, colorOf, labelOf){
  if(!days || !days.length) return '<div class="token-empty">没有足够的天数数据</div>';
  const W = 620, H = 136, padL = 40, padR = 10, padT = 12, padB = 20;
  const innerW = W - padL - padR;
  const innerH = H - padT - padB;
  const N = days.length;
  const gap = N > 20 ? 4 : (N > 10 ? 6 : 10);
  const colW = Math.max(6, (innerW - gap * (N - 1)) / N);

  const totals = days.map(d => {
    let sum = 0;
    for(const k of keys){
      const c = pick(d, k);
      if(c) sum += tokenSum(c);
    }
    return sum;
  });
  const maxVal = Math.max(...totals, 1000);

  const gridSteps = [0.5, 1];
  const gridLines = gridSteps.map(pct => {
    const y = padT + innerH * (1 - pct);
    const val = maxVal * pct;
    return `
      <line class="token-grid" x1="${padL}" y1="${y.toFixed(1)}" x2="${W - padR}" y2="${y.toFixed(1)}" stroke-dasharray="2,3" />
      <text class="token-axis" x="${padL - 6}" y="${(y + 3.5).toFixed(1)}" text-anchor="end">${fmtTokens(val)}</text>
    `;
  }).join('');

  const bars = days.map((d, i) => {
    const x = padL + i * (colW + gap);
    const total = totals[i];
    const dateStr = d.date || '';
    const dateLabel = dateStr.slice(5).replace('-', '/');
    let curY = padT + innerH;
    let stackHtml = '';
    const detailLines = [];

    for(const k of keys){
      const c = pick(d, k);
      const val = c ? tokenSum(c) : 0;
      if(val > 0){
        const segH = (val / maxVal) * innerH;
        curY -= segH;
        const color = colorOf(k);
        stackHtml += `<rect class="token-bar" x="${x.toFixed(1)}" y="${curY.toFixed(1)}" width="${colW.toFixed(1)}" height="${Math.max(1, segH).toFixed(1)}" rx="1.5" fill="${color}" style="animation-delay:${i * 20}ms" />`;
        detailLines.push({ key: k, label: labelOf(k), color, val });
      }
    }

    if(total === 0){
      stackHtml = `<rect class="token-bar-empty" x="${x.toFixed(1)}" y="${(padT + innerH - 2).toFixed(1)}" width="${colW.toFixed(1)}" height="2" rx="1" opacity="0.35" />`;
    }

    const tipPayload = {
      date: dateStr,
      total: fmtTokens(total),
      details: detailLines.map(dl => ({ key: dl.key, label: dl.label, color: dl.color, val: fmtTokens(dl.val) }))
    };
    const tipData = escapeHtml(JSON.stringify(tipPayload));
    const showLabel = N <= 10 || (i % (N > 20 ? 3 : 2) === 0) || i === N - 1;
    const labelSvg = showLabel ? `<text class="token-axis" x="${(x + colW / 2).toFixed(1)}" y="${H - 5}" text-anchor="middle">${escapeHtml(dateLabel)}</text>` : '';

    return `
      <g class="token-bar-group" data-tip="${tipData}">
        <rect x="${(x - gap / 4).toFixed(1)}" y="${padT}" width="${(colW + gap / 2).toFixed(1)}" height="${innerH}" fill="transparent" />
        ${stackHtml}
        ${labelSvg}
      </g>
    `;
  }).join('');

  return `
    <svg class="token-chart" viewBox="0 0 ${W} ${H}">
      ${gridLines}
      <line class="token-grid" x1="${padL}" y1="${padT + innerH}" x2="${W - padR}" y2="${padT + innerH}" />
      ${bars}
    </svg>
  `;
}

function bindChartTooltip(container){
  const tip = container.querySelector('.chart-tooltip');
  if(!tip) return;
  const groups = container.querySelectorAll('.token-bar-group');

  groups.forEach(g => {
    g.addEventListener('mouseenter', () => {
      const raw = g.dataset.tip;
      if(!raw) return;
      try{
        const data = JSON.parse(raw);
        let detailHtml = '';
        if(data.details && data.details.length){
          detailHtml = data.details.map(d => `
            <div class="chart-tooltip-line">
              ${tokenDot(d.color)}
              <span>${escapeHtml(d.label)}</span>
              <b>${escapeHtml(d.val)}</b>
            </div>
          `).join('');
        }
        tip.innerHTML = `
          <div class="chart-tooltip-date">${escapeHtml(data.date)}</div>
          <div class="chart-tooltip-total">合计 ${escapeHtml(data.total)}</div>
          ${detailHtml}
        `;
        tip.style.opacity = '1';
      }catch(_){}
    });

    g.addEventListener('mousemove', e => {
      const rect = container.getBoundingClientRect();
      const x = Math.max(50, Math.min(rect.width - 50, e.clientX - rect.left));
      const y = Math.max(20, e.clientY - rect.top);
      tip.style.left = `${x}px`;
      tip.style.top = `${y}px`;
    });

    g.addEventListener('mouseleave', () => {
      tip.style.opacity = '0';
    });
  });
}

function renderTokensTab(){
  $('#mainPane').innerHTML = `
    <div class="pane-header">
      <div>
        <h2 class="pane-title">Token 记录</h2>
        <p class="pane-desc">从本机各工具的日志里读出来的用量，只读，不联网。</p>
      </div>
      <div class="token-controls">
        <div class="seg-control" id="tokenCache">
          <button class="seg-btn ${tokenWithCache ? 'active' : ''}" data-v="1">含缓存</button>
          <button class="seg-btn ${tokenWithCache ? '' : 'active'}" data-v="0">只算输入输出</button>
        </div>
        <div class="seg-control" id="tokenRange">
          ${[7, 14, 30].map(d => `<button class="seg-btn ${d === tokenDays ? 'active' : ''}" data-d="${d}">${d} 天</button>`).join('')}
        </div>
      </div>
    </div>
    <div id="tokenBody">
      <div class="test-hint"><span class="spinner"></span> 正在读取本机日志…</div>
    </div>`;
  $('#tokenRange').addEventListener('click', e => {
    const d = Number(e.target.closest('.seg-btn')?.dataset.d);
    if(!d || d === tokenDays) return;
    tokenDays = d;
    $('#tokenRange').querySelectorAll('.seg-btn').forEach(b => b.classList.toggle('active', Number(b.dataset.d) === d));
    loadTokens();
  });
  $('#tokenCache').addEventListener('click', e => {
    const v = e.target.closest('.seg-btn')?.dataset.v;
    if(v == null) return;
    tokenWithCache = v === '1';
    $('#tokenCache').querySelectorAll('.seg-btn').forEach(b => b.classList.toggle('active', b.dataset.v === v));
    if(tokenReport) renderTokenBody(tokenReport);
  });
  if(tokenReport && tokenReport.days.length === tokenDays) renderTokenBody(tokenReport);
  loadTokens();
}

async function loadTokens(){
  try{
    const report = await DangoBridge.tokens(tokenDays);
    // 轮询拿到一样的数就别重画（条形动画会重播）。
    const same = tokenReport && JSON.stringify(tokenReport) === JSON.stringify(report) && $('#tokenBody .token-hero');
    tokenReport = report;
    if(currentTab === 'tokens' && !same) renderTokenBody(report);
  }catch(e){
    const body = $('#tokenBody');
    if(body) body.innerHTML = `<div class="token-error">读取失败：${escapeHtml(String(e.message || e))}</div>`;
  }
}

function renderTokenBody(report){
  const body = $('#tokenBody');
  if(!body) return;
  const days = report.days || [];
  const keys = Object.keys(TOKEN_SOURCES);

  const today = days.find(d => d.date === report.today) || days[days.length - 1];
  const todayCounts = zeroCounts();
  for(const c of Object.values(today?.bySource || {})) addCounts(todayCounts, c);

  const range = zeroCounts();
  const bySource = {};
  let activeDays = 0;
  for(const day of days){
    let daySum = 0;
    for(const [k, c] of Object.entries(day.bySource || {})){
      addCounts(range, c);
      addCounts(bySource[k] ||= zeroCounts(), c);
      daySum += tokenSum(c);
    }
    if(daySum > 0) activeDays++;
  }
  const rangeTotal = tokenSum(range);
  const hitRate = cacheRate(range);

  const legend = keys.map(k => {
    const sum = tokenSum(bySource[k]);
    if(!sum) return '';
    const pct = rangeTotal ? Math.round(sum / rangeTotal * 100) : 0;
    return `<span class="token-legend-item">${tokenDot(tokenColor(k))}${escapeHtml(TOKEN_SOURCES[k].label)}<b>${fmtTokens(sum)}</b><em>${pct}%</em></span>`;
  }).join('');

  body.innerHTML = `
    <div class="token-hero">
      <div class="token-hero-main">
        <span class="token-hero-num">${fmtTokens(rangeTotal)}</span>
        <span class="token-hero-unit">近 ${days.length} 天</span>
      </div>
      <div class="token-hero-stats">
        <span>今天 <b>${fmtTokens(tokenSum(todayCounts))}</b></span>
        ${hitRate != null ? `<span>缓存命中 <b>${fmtRate(hitRate)}</b></span>` : ''}
        <span>有用量 <b>${activeDays}/${days.length}</b> 天</span>
      </div>
    </div>

    <div class="section-panel">
      <div class="panel-head"><span class="panel-title">每天</span></div>
      <div class="chart-container" id="tokenChartContainer">
        ${tokenChartSVG(days, keys, (d, k) => d.bySource?.[k], tokenColor, k => TOKEN_SOURCES[k]?.label || k)}
        <div class="chart-tooltip"></div>
      </div>
      <div class="token-legend">${legend}</div>
    </div>

    <div class="section-panel">
      <div class="panel-head"><span class="panel-title">按模型</span></div>
      ${tokenModelRows(report.models)}
    </div>

    <div class="token-sources">
      ${report.sources.map(s => `
        <div class="token-source-row">
          <span class="nav-dot ${s.error ? 'bad' : s.found ? 'ok' : ''}"></span>
          <b>${escapeHtml(s.label)}</b>
          <span>${s.id === 'cursor'
            ? (s.found ? `官方用量接口 · 本次同步 ${s.files} 条` : (s.error ? '' : '还没同步'))
            : (s.found ? `${s.files} 个${s.id === 'factory' ? '会话' : '日志'}` : '本机没找到')}</span>
          ${s.error ? `<span class="token-error-inline">${escapeHtml(s.error)}</span>` : ''}
        </div>`).join('')}
      <div class="token-source-row dim">${escapeHtml(TOKEN_UNREADABLE)}
        Factory 只记每个会话的累计数，按会话最后活动那天入账；套餐按会话第一次被记账时的模型配置判定。</div>
    </div>
  `;

  const chart = $('#tokenChartContainer');
  if(chart) bindChartTooltip(chart);
}

function tokenRouteShort(id){
  const label = (tokenReport?.routes || []).find(r => r.id === id)?.label || id;
  return label.split(' · ')[0];
}

// 一行一个模型：点 · 名字 · 谁付钱 · 细条 · 合计/命中率；输入输出缓存放 tooltip
function tokenModelRows(models){
  const rows = (models || []).filter(m => tokenSum(m.counts) > 0)
    .sort((a, b) => tokenSum(b.counts) - tokenSum(a.counts)).slice(0, 10);
  if(!rows.length) return '<div class="token-empty">这段时间没有用量。</div>';
  const top = tokenSum(rows[0].counts) || 1;
  return `<div class="model-ranks">${rows.map(m => {
    const total = tokenSum(m.counts);
    const color = tokenColor(m.source);
    const plan = m.route && m.route !== 'unknown'
      ? tokenRouteShort(m.route)
      : (TOKEN_SOURCES[m.source]?.label || m.source);
    const c = m.counts;
    const tip = `输入 ${fmtTokens(c.input)} · 输出 ${fmtTokens(c.output)} · 缓存读 ${fmtTokens(c.cacheRead)} · 缓存写 ${fmtTokens(c.cacheWrite)}`;
    return `
      <div class="model-rank" title="${escapeHtml(tip)}">
        ${tokenDot(color)}
        <span class="model-rank-name">${escapeHtml(m.model)}</span>
        <span class="model-rank-plan">${escapeHtml(plan)}</span>
        <span class="model-rank-hit">${fmtRate(cacheRate(c))}</span>
        <span class="model-rank-total">${fmtTokens(total)}</span>
        <span class="model-rank-track"><i style="width:${(total / top * 100).toFixed(1)}%;background:${color}"></i></span>
      </div>`;
  }).join('')}</div>`;
}

/* ============================================================
   页签切换与初始化
   ============================================================ */
function switchTab(tabId, updateHash = true){
  if(!tabId || !document.querySelector(`.sidebar .nav-item[data-tab="${CSS.escape(tabId)}"]`)){
    tabId = 'balls';
  }
  if(updateHash && location.hash !== `#${tabId}`){
    location.hash = tabId;
    return;
  }
  currentTab = tabId;

  document.querySelectorAll('.sidebar .nav-item').forEach(btn => {
    btn.classList.toggle('active', btn.dataset.tab === tabId);
  });

  const pane = $('#mainPane');
  if(pane){
    pane.classList.remove('pane-transition');
    void pane.offsetWidth;
    pane.classList.add('pane-transition');
  }

  if(proxyPollTimer){
    clearInterval(proxyPollTimer);
    proxyPollTimer = null;
  }

  if(tokenPollTimer){
    clearInterval(tokenPollTimer);
    tokenPollTimer = null;
  }

  if(tabId === 'balls'){
    renderBallsTab();
  }else if(tabId === 'tokens'){
    renderTokensTab();
    tokenPollTimer = setInterval(() => {
      if(currentTab === 'tokens') loadTokens();
    }, 30000);
  }else{
    // 桥的 /healthz 偶尔要一两秒：先给标题和占位，别让内容区白着。
    if(currentProxyDetail?.planId !== tabId){
      $('#mainPane').innerHTML = `
        <div class="pane-header"><div>
          <h2 class="pane-title">${escapeHtml(proxyTitle(tabId))}</h2>
          <p class="pane-desc">&nbsp;</p>
        </div></div>
        <div class="test-hint"><span class="spinner"></span> 正在读取反代详情…</div>`;
    }
    loadProxyDetail(tabId);
    proxyPollTimer = setInterval(() => {
      if(currentTab === tabId) loadProxyDetail(tabId);
    }, 10000);
  }
}

async function init(){
  // 侧边栏点击（反代入口是动态生成的，用事件委托）
  $('.sidebar').addEventListener('click', e => {
    const btn = e.target.closest('.nav-item');
    if(!btn) return;
    if(location.hash === `#${btn.dataset.tab}`) switchTab(btn.dataset.tab, false);
    else location.hash = btn.dataset.tab;
  });
  try{
    currentSnapshot = await DangoBridge.snapshot();
    syncProxyNav(currentSnapshot);
  }catch(e){ console.warn('initial snapshot failed:', e); }

  // 绑定顶部立即刷新
  $('#refreshBtn').addEventListener('click', async () => {
    const btn = $('#refreshBtn');
    btn.disabled = true;
    btn.classList.add('refreshing');
    showToast('正在刷新...');
    try{
      await DangoBridge.refresh();
      updateAppMemory();
      if(currentTab === 'tokens'){
        await loadTokens();
      }else if(currentTab !== 'balls'){
        await loadProxyDetail(currentTab);
      }
    }catch(e){
      console.error('refresh_now failed:', e);
      showSaveStatus('刷新失败: ' + e, 'err');
    }finally{
      btn.disabled = false;
      setTimeout(() => btn.classList.remove('refreshing'), 600);
    }
  });

  DangoBridge.on('error', error => {
    console.error('settings bridge error:', error);
    showSaveStatus('连接失败: ' + error, 'err');
  });

  // 快照刷新时如果正在拖拽排序，跳过这次重渲染（拖拽会被 DOM 重建打断）
  let isDraggingRow = false;

  DangoBridge.on('snapshot', payload => {
    currentSnapshot = payload;
    syncProxyNav(payload);
    if(currentSnapshot?.fetchedAt){
      updateHeaderTime(currentSnapshot.fetchedAt);
    }
    if(currentTab === 'balls'){
      renderBallsTab();
    }
  });

  // 监听 settings-changed 事件
  DangoBridge.on('reconnected', async () => {
    try{
      currentSnapshot = await DangoBridge.snapshot();
      if(currentTab === 'balls' && !isDraggingRow){
        renderBallsTab();
      }
      showSaveStatus('', '');
    }catch(e){
      console.error('reconnect refresh failed:', e);
    }
  });

  DangoBridge.on('settings-changed', payload => {
    currentSettings = payload || currentSettings;
    applyTheme(currentSettings.theme);
    if(currentTab === 'balls'){
      renderBallsTab();
    }
  });

  // 初始化拉取数据
  try{
    currentSettings = await DangoBridge.getSettings();
    applyTheme(currentSettings.theme);
  }catch(e){
    console.error('get_settings failed:', e);
    showSaveStatus('读取设置失败: ' + e, 'err');
  }

  try{
    currentSnapshot = await DangoBridge.snapshot();
    if(currentSnapshot?.fetchedAt){
      updateHeaderTime(currentSnapshot.fetchedAt);
    }
  }catch(e){
    console.error('snapshot failed:', e);
    showSaveStatus('读取数据失败: ' + e, 'err');
  }

  updateAppMemory();
  setInterval(updateAppMemory, 15000);

  window.addEventListener('hashchange', () => switchTab(location.hash.slice(1), false));
  switchTab(location.hash.slice(1) || new URLSearchParams(location.search).get('tab') || 'balls', false);
}

init();
