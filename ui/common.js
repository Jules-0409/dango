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
];

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
