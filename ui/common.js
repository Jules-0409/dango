const PALETTE = {
  claude: '#E7A97C',
  haze: '#D8A7A0',
  antigravity: '#8FB5D9',
  devin: '#A8C6A2',
  cursor: '#C9BCA6',
  factory: '#B8A9C9',
  dim: '#DCB770',
  grok: '#7A9BB8',
};

const PRESET_COLORS = [
  '#D8A7A0',
  '#E5989B',
  '#E7A97C',
  '#DCB770',
  '#A8C6A2',
  '#85BAA1',
  '#8FB5D9',
  '#7A9BB8',
  '#B8A9C9',
  '#C5A5B5',
  '#C9BCA6',
  '#8E8A85',
];

const DEFAULT_SHAPE = {
  claude: 'star',
  haze: 'blob',
  antigravity: 'gem',
  devin: 'wedge',
  cursor: 'blob',
  factory: 'wedge',
  dim: 'cloud',
  grok: 'drop',
};

const AVAILABLE_SHAPES = [
  { id: 'blob', label: 'Blob' },
  { id: 'gem', label: 'Gem' },
  { id: 'wedge', label: 'Wedge' },
  { id: 'star', label: 'Star' },
  { id: 'cloud', label: 'Cloud' },
  { id: 'square', label: 'Square' },
  { id: 'drop', label: 'Drop' },
  { id: 'cat', label: 'Cat' },
  { id: 'whale', label: 'Whale', onlyKind: 'deepseek' },
];

// 自己加的小球（settings.custom）按模板认默认形状/颜色；鲸鱼是 DeepSeek 专属。
// 与 crates/dango-widget/src/theme.rs::KIND_DEFAULTS 同步（有测试）。
const KIND_DEFAULTS = {
  deepseek: { shape: 'whale', color: '#7A8EDC' },
};

// Dango 自家形状（grok-ball.js 是 vendor 不改，加载后注册进去）。
// 与 crates/grok-ball/src/extra_shapes.rs 逐点一致（theme.rs 测试比对）。
const EXTRA_SHAPES = {"whale":{"ring":[[217.69,62.0],[210.05,64.34],[202.49,67.49],[194.44,69.17],[186.69,71.91],[179.13,70.09],[173.39,64.23],[166.74,59.38],[161.23,53.31],[154.82,49.25],[150.83,56.17],[151.74,59.37],[151.47,65.64],[150.89,73.4],[153.6,81.15],[158.01,88.08],[163.26,94.4],[165.05,102.2],[163.63,104.38],[155.73,104.04],[151.31,99.12],[146.79,93.58],[141.01,87.73],[135.23,81.87],[129.22,76.25],[122.88,71.01],[117.21,65.09],[115.65,57.25],[118.27,49.49],[111.97,45.83],[103.82,46.86],[95.91,49.11],[88.02,51.4],[79.84,52.25],[71.63,51.88],[63.4,51.85],[55.21,52.61],[47.16,54.28],[39.4,56.99],[32.1,60.78],[25.43,65.59],[19.49,71.27],[14.31,77.66],[9.92,84.62],[6.37,92.03],[3.7,99.81],[1.96,107.85],[1.11,116.03],[1.09,124.26],[1.85,132.45],[3.3,140.54],[5.46,148.48],[8.32,156.19],[11.9,163.6],[16.15,170.64],[21.04,177.25],[26.5,183.41],[32.45,189.09],[38.84,194.27],[45.66,198.88],[52.86,202.85],[60.41,206.11],[68.25,208.59],[76.31,210.26],[84.49,211.12],[92.71,211.22],[100.92,210.69],[109.06,209.49],[117.03,207.46],[124.69,204.47],[131.92,200.56],[139.14,196.65],[147.23,195.45],[155.37,196.59],[163.58,197.01],[171.73,196.02],[177.8,191.04],[173.61,184.5],[166.22,180.9],[163.08,174.09],[167.72,167.35],[172.67,160.78],[177.07,153.83],[180.83,146.51],[183.96,138.9],[186.5,131.08],[188.54,123.11],[190.11,115.03],[191.27,106.9],[188.42,103.96],[194.95,99.17],[202.48,95.89],[209.08,91.01],[214.33,84.7],[217.96,77.33],[219.99,69.37]],"face":{"x":-40,"y":20,"sx":0.82,"sy":0.82,"eye":0.95},"tiltScale":0.6},"cat":{"ring":[[114.27,48.0],[121.92,48.2],[129.55,48.74],[137.16,49.55],[144.76,50.43],[152.38,51.08],[160.03,50.92],[167.42,49.06],[173.81,44.91],[178.77,39.11],[182.68,32.56],[188.67,27.94],[196.16,27.64],[201.91,32.44],[204.02,39.73],[205.54,47.23],[207.44,54.64],[209.67,61.96],[212.1,69.21],[214.57,76.46],[216.87,83.75],[218.88,91.13],[220.53,98.61],[221.81,106.15],[222.76,113.74],[223.41,121.36],[223.81,129.01],[223.98,136.65],[223.93,144.31],[223.45,151.94],[222.44,159.52],[220.81,167.0],[218.48,174.28],[215.37,181.27],[211.46,187.84],[206.74,193.86],[201.3,199.23],[195.25,203.91],[188.72,207.89],[181.83,211.21],[174.68,213.95],[167.36,216.17],[159.92,217.94],[152.4,219.32],[144.82,220.37],[137.2,221.12],[129.57,221.63],[121.92,221.91],[114.27,222.0],[106.62,221.91],[98.97,221.63],[91.34,221.12],[83.72,220.37],[76.14,219.32],[68.62,217.94],[61.18,216.17],[53.86,213.95],[46.71,211.21],[39.82,207.89],[33.29,203.91],[27.24,199.23],[21.8,193.86],[17.08,187.84],[13.17,181.27],[10.06,174.28],[7.73,167.0],[6.1,159.52],[5.09,151.94],[4.61,144.31],[4.56,136.65],[4.73,129.01],[5.13,121.36],[5.78,113.74],[6.73,106.15],[8.01,98.61],[9.66,91.13],[11.67,83.75],[13.97,76.46],[16.44,69.21],[18.87,61.96],[21.1,54.64],[23.0,47.23],[24.52,39.73],[26.63,32.44],[32.38,27.64],[39.87,27.94],[45.86,32.56],[49.77,39.11],[54.73,44.91],[61.12,49.06],[68.51,50.92],[76.16,51.08],[83.78,50.43],[91.38,49.55],[98.99,48.74],[106.62,48.2]],"face":{"x":-12,"y":36,"sx":0.9,"sy":0.9,"eye":1.0},"tiltScale":0.6}};

function registerExtraShapes(){
  if(window.EB_RINGS) Object.assign(window.EB_RINGS.SHAPES, EXTRA_SHAPES);
}

function planKind(planId, settings){
  return (settings?.custom || []).find(c => c.id === planId)?.kind;
}

function defaultShapeFor(planId, settings){
  return DEFAULT_SHAPE[planId] || KIND_DEFAULTS[planKind(planId, settings)]?.shape || 'blob';
}

function defaultColorFor(planId, settings){
  return PALETTE[planId] || KIND_DEFAULTS[planKind(planId, settings)]?.color || '#B9B0A4';
}

function shapesFor(planId, settings){
  const kind = planKind(planId, settings);
  return AVAILABLE_SHAPES.filter(s => !s.onlyKind || s.onlyKind === kind);
}

// 环样式：settings.ringMode 的合法取值 + 中文名（设置页选择器用）
const RING_MODES = [
  { id: 'plain',    label: '细环' },
  { id: 'beads',    label: '珠串' },
  { id: 'double',   label: '双环' },
  { id: 'flow',     label: '流光' },
  { id: 'segments', label: '分段' },
  { id: 'trail',    label: '尾迹' },
];

// 环预览 SVG 生成器：viewBox 0 0 58 58，圆心 29,29，主环 r=25，12 点钟起笔。
// 轨道色走 CSS 类 rp-trk / rp-trk-fill（吃 --ring-track 变量，深浅色都有定义），
// 主色直接内联。流光的 mask 要文档级唯一 id，用序号顶着。
let _rpMaskSeq = 0;

function ringPreviewSVG(mode, percent, color, innerPercent) {
  const pct = v => Math.max(0, Math.min(100, Number(v) || 0));
  const p = pct(percent);
  const col = color || '#B9B0A4';
  const R = 25, C = 2 * Math.PI * R;

  const track = (r, sw) =>
    `<circle class="rp-trk" cx="29" cy="29" r="${r}" stroke-width="${sw}"/>`;
  const arc = (r, v, sw, extra = '') => {
    const c = 2 * Math.PI * r;
    return `<circle cx="29" cy="29" r="${r}" fill="none" stroke="${col}" stroke-width="${sw}"`
      + ` stroke-linecap="round" stroke-dasharray="${c.toFixed(2)}"`
      + ` stroke-dashoffset="${(c * (1 - pct(v) / 100)).toFixed(2)}" transform="rotate(-90 29 29)"${extra}/>`;
  };

  let body = '';
  if (mode === 'beads') {
    const n = 20, on = Math.round(p / 5);
    for (let i = 0; i < n; i++) {
      const a = (-90 + i * 360 / n) * Math.PI / 180;
      const cx = (29 + R * Math.cos(a)).toFixed(2);
      const cy = (29 + R * Math.sin(a)).toFixed(2);
      body += i < on
        ? `<circle cx="${cx}" cy="${cy}" r="2.2" fill="${col}"/>`
        : `<circle class="rp-trk-fill" cx="${cx}" cy="${cy}" r="1.1"/>`;
    }
  } else if (mode === 'double') {
    const inner = innerPercent == null ? 100 : innerPercent;
    body = track(26.5, 2.4) + arc(26.5, p, 2.4)
      + track(23.2, 1.7) + arc(23.2, inner, 1.7, ' opacity="0.55"');
  } else if (mode === 'flow') {
    body = track(R, 2.4) + arc(R, p, 2.4);
    if (p > 0) {
      const mid = 'rp-mask-' + (++_rpMaskSeq);
      body += `<defs><mask id="${mid}"><circle cx="29" cy="29" r="${R}" fill="none" stroke="#fff"`
        + ` stroke-width="3.2" stroke-dasharray="${C.toFixed(2)}"`
        + ` stroke-dashoffset="${(C * (1 - p / 100)).toFixed(2)}" transform="rotate(-90 29 29)"/></mask></defs>`
        + `<circle class="rp-comet" cx="29" cy="29" r="${R}" stroke-width="2.6"`
        + ` stroke-dasharray="9 ${(C - 9).toFixed(2)}" mask="url(#${mid})" opacity="0.85"/>`;
    }
  } else if (mode === 'segments') {
    const SEG = 8, GAP = 10; // 8 段、段间 10° 空
    const segLen = (360 / SEG - GAP) / 360 * C;
    const seg = (len, i, cls, style) =>
      `<circle cx="29" cy="29" r="${R}" fill="none" stroke-width="2.4" stroke-linecap="round"`
      + (cls ? ` class="${cls}"` : ` stroke="${col}"`)
      + (style || '')
      + ` stroke-dasharray="${len.toFixed(2)} ${(C - len).toFixed(2)}"`
      + ` transform="rotate(${(-90 + GAP / 2 + i * (360 / SEG)).toFixed(2)} 29 29)"/>`;
    for (let i = 0; i < SEG; i++) body += seg(segLen, i, 'rp-trk');
    const lit = p / 100 * SEG, full = Math.floor(lit), frac = lit - full;
    for (let i = 0; i < full && i < SEG; i++) body += seg(segLen, i, null);
    if (frac > 0.001 && full < SEG) body += seg(segLen * frac, full, null);
  } else if (mode === 'trail') {
    body = track(R, 2.4);
    if (p > 0) {
      const N = 24, arcAngle = p / 100 * 360, step = arcAngle / N;
      const len = step / 360 * C;
      for (let i = 0; i < N; i++) {
        const op = (0.12 + 0.88 * i / (N - 1)).toFixed(2);
        body += `<circle cx="29" cy="29" r="${R}" fill="none" stroke="${col}" stroke-width="2.4"`
          + ` stroke-opacity="${op}" stroke-dasharray="${len.toFixed(2)} ${(C - len).toFixed(2)}"`
          + ` transform="rotate(${(-90 + i * step).toFixed(2)} 29 29)"/>`;
      }
      const a = (-90 + arcAngle) * Math.PI / 180;
      body += `<circle cx="${(29 + R * Math.cos(a)).toFixed(2)}" cy="${(29 + R * Math.sin(a)).toFixed(2)}" r="2.8" fill="${col}"/>`;
    }
  } else { // plain 细环（默认）
    body = track(R, 2.4) + arc(R, p, 2.4);
  }

  // 中间示例球：实心圆点，用套餐色
  body += `<circle cx="29" cy="29" r="10.5" fill="${col}"/>`;
  return `<svg viewBox="0 0 58 58" xmlns="http://www.w3.org/2000/svg">${body}</svg>`;
}

function escapeHtml(str) {
  if (str == null) return '';
  return String(str)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');
}

function hexToRgba(hex, alpha) {
  let c = hex.replace('#', '');
  if (c.length === 3) c = c.split('').map(x => x + x).join('');
  const num = parseInt(c, 16);
  return `rgba(${(num >> 16) & 255},${(num >> 8) & 255},${num & 255},${alpha})`;
}

function emotionFor(plan) {
  if (!plan || !plan.ok) return '34';
  const p = plan.remainingPercent;
  if (p == null) return '02';
  if (p >= 60) return '10';
  if (p >= 15) return '19';
  return '12';
}

// 错误文案分诊：原始 plan.error 保持技术形态，这里给出人能行动的一句。
// 与 Rust 侧 theme.rs::error_hint 同一张映射表。
function errorHint(err) {
  const e = String(err || '');
  // keychain 先于 auth：凭据串是 "keychain:<service>.auth.<kind>"，两类都命中。
  if (/keychain/.test(e)) return '钥匙串里没找到凭据，先登录一次对应 App';
  if (/401|403|auth|Unauthorized/i.test(e)) return '登录态过期，打开对应 App 重新登录即恢复';
  if (/超时|timeout/i.test(e)) return '网络超时，检查一下代理或网络';
  if (/net:/.test(e)) return '网络异常，检查一下代理或网络';
  if (/parse/.test(e)) return '接口变了，解析失败，等探针更新';
  return '查询失败';
}

function colorFor(pct, ok) {
  if (!ok) return 'var(--danger)';
  if (pct == null) return 'var(--ink-3)';
  if (pct >= 40) return 'var(--ok)';
  if (pct >= 15) return 'var(--warn)';
  return 'var(--danger)';
}

function formatReset(resetsAt) {
  if (resetsAt == null) return null;
  const now = Date.now();
  const diff = Number(resetsAt) - now;
  if (diff <= 0) return '待刷新';
  const dayMs = 24 * 60 * 60 * 1000;
  const hourMs = 60 * 60 * 1000;
  const minMs = 60 * 1000;
  if (diff >= dayMs) {
    const days = Math.floor(diff / dayMs);
    const hours = Math.floor((diff % dayMs) / hourMs);
    return `${days}d ${hours}h 后重置`;
  }
  if (diff < hourMs) {
    const mins = Math.max(1, Math.floor(diff / minMs));
    return `${mins}m 后重置`;
  }
  const hours = Math.floor(diff / hourMs);
  const mins = Math.floor((diff % hourMs) / minMs);
  return `${hours}h${mins}m 后重置`;
}

function formatTime(timestamp) {
  if (!timestamp) return '—';
  // If timestamp is in seconds (e.g. 1700000000), convert to ms
  const t = Number(timestamp) < 10000000000 ? Number(timestamp) * 1000 : Number(timestamp);
  const d = new Date(t);
  if (isNaN(d.getTime())) return '—';
  const pad = n => String(n).padStart(2, '0');
  return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
}

function formatContextWindow(tokens) {
  if (tokens == null) return null;
  const num = Number(tokens);
  if (num >= 1000000) {
    const m = num / 1000000;
    return (m % 1 === 0 ? m : m.toFixed(1)) + 'M';
  }
  if (num >= 1000) {
    const k = num / 1000;
    return (k % 1 === 0 ? k : k.toFixed(0)) + 'k';
  }
  return String(num);
}
