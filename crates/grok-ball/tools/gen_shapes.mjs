// Generate 96-point silhouette rings for the extra ball shapes (star, cloud,
// heart, square, drop), matching the built-in blob/gem/wedge data contract:
//   - 96 points, start at angle 0 (3 o'clock), counter-clockwise (y down)
//   - canvas 228.54 x 228.54, centred on HEAD_C = 114.27
//   - shapes fill ~the same 0..228 box as blob so they do not render small
//   - face params are derived from the ring's real geometry so the eyes stay
//     inside the silhouette at the eye line
//
// Usage: node gen_shapes.mjs [preview.svg] [js_out.txt] > rings.rs

import fs from 'node:fs';

const HEAD_C = 114.2705;
const EYE_HALF = 21.0;

/* ---------- Chaikin smoothing ---------- */
function chaikin(points, iterations) {
  let pts = points;
  for (let k = 0; k < iterations; k++) {
    const out = [pts[0]];
    for (let i = 0; i < pts.length - 1; i++) {
      const [ax, ay] = pts[i];
      const [bx, by] = pts[i + 1];
      out.push([ax * 0.75 + bx * 0.25, ay * 0.75 + by * 0.25]);
      out.push([ax * 0.25 + bx * 0.75, ay * 0.25 + by * 0.75]);
    }
    out.push(pts[pts.length - 1]);
    pts = out;
  }
  return pts;
}

/* ---------- resample a closed polyline to 96 CCW points from angle 0 ---------- */
function resample(points) {
  const closed = points.concat([points[0]]);
  const lengths = [0];
  for (let i = 1; i < closed.length; i++) {
    const dx = closed[i][0] - closed[i - 1][0];
    const dy = closed[i][1] - closed[i - 1][1];
    lengths.push(lengths[i - 1] + Math.hypot(dx, dy));
  }
  const total = lengths[lengths.length - 1];
  const at = (d) => {
    d = ((d % total) + total) % total;
    let lo = 0, hi = lengths.length - 1;
    while (lo < hi) {
      const mid = (lo + hi) >> 1;
      if (lengths[mid] < d) lo = mid + 1; else hi = mid;
    }
    const i = Math.max(1, lo);
    const seg = lengths[i] - lengths[i - 1] || 1;
    const t = (d - lengths[i - 1]) / seg;
    return [
      closed[i - 1][0] + (closed[i][0] - closed[i - 1][0]) * t,
      closed[i - 1][1] + (closed[i][1] - closed[i - 1][1]) * t,
    ];
  };
  const angles = points.map(([x, y]) => Math.atan2(y - HEAD_C, x - HEAD_C));
  let maxA = -Infinity, maxI = 0;
  for (let i = 0; i < angles.length; i++) {
    let a = angles[i];
    if (a > Math.PI / 2) a -= 2 * Math.PI; // treat lower-right as near 0
    if (a > maxA) { maxA = a; maxI = i; }
  }
  const startDist = lengths[maxI];
  const out = [];
  for (let i = 0; i < 96; i++) {
    out.push(at(startDist - (i / 96) * total).map((v) => Math.round(v * 100) / 100));
  }
  return out;
}

/* ---------- scale a ring about the centre so it fills the canvas ---------- */
function fillCanvas(ring, margin = 2.0) {
  let minX = 1e9, maxX = -1e9, minY = 1e9, maxY = -1e9;
  for (const [x, y] of ring) {
    minX = Math.min(minX, x); maxX = Math.max(maxX, x);
    minY = Math.min(minY, y); maxY = Math.max(maxY, y);
  }
  const w = maxX - minX, h = maxY - minY;
  const target = HEAD_C * 2 - margin * 2;
  const s = Math.min(target / w, target / h);
  return ring.map(([x, y]) => [
    Math.round((HEAD_C + (x - HEAD_C) * s) * 100) / 100,
    Math.round((HEAD_C + (y - HEAD_C) * s) * 100) / 100,
  ]);
}

/* ---------- silhouette row width at a given y (mirror of SilProfile) ---------- */
function rowWidth(ring, y) {
  let lo = 1e9, hi = -1e9;
  const n = ring.length;
  for (let e = 0; e < n; e++) {
    const [ax, ay] = ring[e];
    const [bx, by] = ring[(e + 1) % n];
    if ((ay <= y && by >= y) || (by <= y && ay >= y)) {
      const t = Math.abs(by - ay) < 1e-9 ? 0 : (y - ay) / (by - ay);
      const x = ax + (bx - ax) * t;
      lo = Math.min(lo, x); hi = Math.max(hi, x);
    }
  }
  return lo > hi ? 8 : hi - lo;
}

/* ---------- cubic bezier helper ---------- */
function sampleCubic(p0, p1, p2, p3, steps) {
  const pts = [];
  for (let i = 0; i <= steps; i++) {
    const t = i / steps;
    const u = 1 - t;
    const tt = t * t, uu = u * u;
    const uuu = uu * u, ttt = tt * t;
    pts.push([
      uuu * p0[0] + 3 * uu * t * p1[0] + 3 * u * tt * p2[0] + ttt * p3[0],
      uuu * p0[1] + 3 * uu * t * p1[1] + 3 * u * tt * p2[1] + ttt * p3[1],
    ]);
  }
  return pts;
}

/* ---------- control polygons (pre-fill; each gets scaled to fill canvas) ---------- */
function starControl() {
  const cx = HEAD_C, cy = HEAD_C;
  // Extra chubby center ("中间胖一点"): inner radius 78 (was 66), outer 104.
  // Arms are cute stubby nubs, center is a fat round belly.
  const outer = 104, inner = 78;
  const pts = [];

  const deg2rad = (d) => (d * Math.PI) / 180;
  const polar = (r, deg) => [cx + r * Math.cos(deg2rad(deg)), cy + r * Math.sin(deg2rad(deg))];

  const t0 = [cx, cy - outer]; // top apex
  const v0 = polar(inner, -54);
  const t1 = polar(outer, -18);
  const v1 = polar(inner, 18);
  const t2 = polar(outer, 54);
  const v2 = [cx, cy + inner]; // bottom center valley

  const tanOut = (deg, len) => [-Math.sin(deg2rad(deg)) * len, Math.cos(deg2rad(deg)) * len];
  const wTip = 26, wVal = 26;

  const s1 = sampleCubic(
    t0,
    [t0[0] + wTip, t0[1]],
    [v0[0] - tanOut(-54, wVal)[0], v0[1] - tanOut(-54, wVal)[1]],
    v0,
    10
  );
  const s2 = sampleCubic(
    v0,
    [v0[0] + tanOut(-54, wVal)[0], v0[1] + tanOut(-54, wVal)[1]],
    [t1[0] - tanOut(-18, wTip)[0], t1[1] - tanOut(-18, wTip)[1]],
    t1,
    10
  );
  const s3 = sampleCubic(
    t1,
    [t1[0] + tanOut(-18, wTip)[0], t1[1] + tanOut(-18, wTip)[1]],
    [v1[0] - tanOut(18, wVal)[0], v1[1] - tanOut(18, wVal)[1]],
    v1,
    10
  );
  const s4 = sampleCubic(
    v1,
    [v1[0] + tanOut(18, wVal)[0], v1[1] + tanOut(18, wVal)[1]],
    [t2[0] - tanOut(54, wTip)[0], t2[1] - tanOut(54, wTip)[1]],
    t2,
    10
  );
  const s5 = sampleCubic(
    t2,
    [t2[0] + tanOut(54, wTip)[0], t2[1] + tanOut(54, wTip)[1]],
    [v2[0] + wVal, v2[1]],
    v2,
    10
  );

  const right = [];
  for (const seg of [s1, s2, s3, s4, s5]) {
    for (let i = 0; i < seg.length - 1; i++) right.push(seg[i]);
  }
  right.push(v2);

  for (let i = right.length - 1; i >= 0; i--) pts.push(right[i]);
  for (let i = 1; i < right.length; i++) pts.push([2 * cx - right[i][0], right[i][1]]);
  pts.push(pts[0]);
  return pts;
}

function cloudControl() {
  const cx = HEAD_C, cy = HEAD_C;
  const lobes = [
    [cx, cy - 40, 52],        // Top crown (head)
    [cx + 52, cy - 14, 46],   // Upper right cheek
    [cx + 46, cy + 34, 42],   // Lower right puff
    [cx + 20, cy + 46, 40],   // Bottom right foot
    [cx - 20, cy + 46, 40],   // Bottom left foot
    [cx - 46, cy + 34, 42],   // Lower left puff
    [cx - 52, cy - 14, 46],   // Upper left cheek
  ];

  const steps = 720;
  const radii = new Array(steps).fill(0);
  for (const [lx, ly, r] of lobes) {
    for (let i = 0; i < steps; i++) {
      const a = (i / steps) * Math.PI * 2;
      const dx = Math.cos(a), dy = Math.sin(a);
      const ox = lx - cx, oy = ly - cy;
      const b = ox * dx + oy * dy;
      const c = ox * ox + oy * oy - r * r;
      const disc = b * b - c;
      if (disc < 0) continue;
      const t = b + Math.sqrt(disc);
      if (t > radii[i]) radii[i] = t;
    }
  }

  const pts = [];
  for (let i = 0; i < steps; i++) {
    if (radii[i] <= 0) continue;
    const a = (i / steps) * Math.PI * 2;
    pts.push([cx + radii[i] * Math.cos(a), cy + radii[i] * Math.sin(a)]);
  }
  pts.push(pts[0]);
  return pts;
}

function heartControl() {
  const cx = HEAD_C, cy = HEAD_C;
  const pts = [];

  // Chubby plush heart ("两边圆润 + 中间胖一点 + 顶缝不深"):
  // - Top lobes: Bubbly, proud, round circular domes.
  // - Cleft: Smooth, rounded U-dip at [cx, cy - 40], shallow so it never cuts into the eyes.
  // - Middle/waist: Extra chubby and plump (stays wide down to cy + 48).
  // - Bottom: Soft, rounded, bouncy tip.

  // 1. From cleft [cx, cy - 40] up to top lobe apex [cx + 48, cy - 82]:
  const s1 = sampleCubic(
    [cx, cy - 40],
    [cx + 18, cy - 40],
    [cx + 26, cy - 82],
    [cx + 48, cy - 82],
    16
  );

  // 2. From top lobe apex [cx + 48, cy - 82] around bubbly cheek [cx + 105, cy - 12]:
  const s2 = sampleCubic(
    [cx + 48, cy - 82],
    [cx + 80, cy - 82],
    [cx + 105, cy - 50],
    [cx + 105, cy - 12],
    16
  );

  // 3. Middle waist ("中间胖一点"): maintains full plump width from cy - 12 down to cy + 48:
  const s3 = sampleCubic(
    [cx + 105, cy - 12],
    [cx + 105, cy + 22],
    [cx + 100, cy + 48],
    [cx + 82, cy + 74],
    16
  );

  // 4. From lower cheek down to soft rounded bottom [cx, cy + 104]:
  const s4 = sampleCubic(
    [cx + 82, cy + 74],
    [cx + 64, cy + 96],
    [cx + 25, cy + 104],
    [cx, cy + 104],
    16
  );

  const right = [];
  for (const seg of [s1, s2, s3, s4]) {
    for (let i = 0; i < seg.length - 1; i++) right.push(seg[i]);
  }
  right.push([cx, cy + 104]);

  // CCW order: bottom to cleft (right side reversed), then cleft to bottom (left side mirrored)
  for (let i = right.length - 1; i >= 0; i--) pts.push(right[i]);
  for (let i = 1; i < right.length; i++) pts.push([2 * cx - right[i][0], right[i][1]]);
  pts.push(pts[0]);
  return pts;
}

function squareControl() {
  const a = 104, b = 104, n = 3.3;
  const p = 2 / n;
  const steps = 144;
  const pts = [];

  for (let i = 0; i < steps; i++) {
    const t = (i / steps) * Math.PI * 2;
    const cosT = Math.cos(t), sinT = Math.sin(t);
    const x = HEAD_C + a * Math.sign(cosT) * Math.pow(Math.abs(cosT), p);
    const y = HEAD_C + b * Math.sign(sinT) * Math.pow(Math.abs(sinT), p);
    pts.push([x, y]);
  }
  pts.push(pts[0]);
  return pts;
}

function dropControl() {
  const cx = HEAD_C, cy = HEAD_C;
  const pts = [];

  // Classic water droplet: round bulbous ball lower body + cute arched top peak
  // Lower body is a round sphere: center [cx, cy + 18], radius R = 86
  const R = 86;
  const ballCy = cy + 18;
  const tanDeg = -18;
  const tanRad = (tanDeg * Math.PI) / 180;
  const pTangent = [cx + R * Math.cos(tanRad), ballCy + R * Math.sin(tanRad)];

  const cTan = [-Math.sin(tanRad) * 42, Math.cos(tanRad) * 42];

  // Top peak: [cx, cy - 96]. Horizontal tangent [20, 0]
  const topSeg = sampleCubic(
    [cx, cy - 96],
    [cx + 18, cy - 96],
    [pTangent[0] - cTan[0] * 0.8, pTangent[1] - cTan[1] * 0.8],
    pTangent,
    16
  );

  // Lower ball arc: from tanDeg (-18 deg) down to 90 deg (bottom apex)
  const arcPts = [];
  const stepsArc = 24;
  const aStart = tanRad;
  const aEnd = Math.PI / 2;
  for (let i = 0; i <= stepsArc; i++) {
    const a = aStart + (i / stepsArc) * (aEnd - aStart);
    arcPts.push([cx + R * Math.cos(a), ballCy + R * Math.sin(a)]);
  }

  const right = [];
  for (let i = 0; i < topSeg.length - 1; i++) right.push(topSeg[i]);
  for (let i = 0; i < arcPts.length; i++) right.push(arcPts[i]);

  for (let i = right.length - 1; i >= 0; i--) pts.push(right[i]);
  for (let i = 1; i < right.length; i++) pts.push([2 * cx - right[i][0], right[i][1]]);
  pts.push(pts[0]);
  return pts;
}

/* ---------- shape table ---------- */
// eyeY: where the eye line sits (fraction of ring height from the top).
// eyeFrac: eyes span at most this fraction of the row width at the eye line.
const SHAPES = {
  STAR:  { control: starControl(),  smooth: 1, eyeY: 0.50, eyeFrac: 0.66, tilt: 0.6 },
  CLOUD: { control: cloudControl(), smooth: 2, eyeY: 0.48, eyeFrac: 0.66, tilt: 0.5 },
  HEART: { control: heartControl(), smooth: 1, eyeY: 0.42, eyeFrac: 0.66, tilt: 0.7 },
  SQUARE:{ control: squareControl(),smooth: 0, eyeY: 0.48, eyeFrac: 0.76, tilt: 0.9 },
  DROP:  { control: dropControl(),  smooth: 1, eyeY: 0.52, eyeFrac: 0.66, tilt: 0.7 },
};

/* ---------- derive face params from real ring geometry ---------- */
function deriveFace(ring, def) {
  let minY = 1e9, maxY = -1e9;
  for (const [, y] of ring) { minY = Math.min(minY, y); maxY = Math.max(maxY, y); }
  const h = maxY - minY;
  const eyeLineY = minY + h * def.eyeY;
  const width = rowWidth(ring, eyeLineY);

  // eye scale so the two eyes (each ~2*EYE_HALF wide, separated) fit inside
  // the row width at the eye line. Baseline blob: width ~228, eye 1.0.
  const eye = Math.min(1.0, (width / (HEAD_C * 2)) * (1 / def.eyeFrac) * 0.5 + 0.35);
  const eyeClamped = Math.round(Math.max(0.6, Math.min(1.0, eye)) * 100) / 100;

  // face.y: engine eye Y = HEAD_C + face.y + (base.y - HEAD_C)*sy.
  // With sy ~1 the (base.y-HEAD_C) term is the eye ring's own upper bias.
  // We target eyeLineY; solve face.y = eyeLineY - HEAD_C - bias, bias ~ -8.
  const bias = -8;
  const faceY = Math.round((eyeLineY - HEAD_C - bias) * 10) / 10;
  const sx = Math.min(0.98, Math.round((width / (HEAD_C * 2)) * 100) / 100);
  const sy = Math.min(0.98, sx);

  return {
    x: 0,
    y: faceY,
    sx,
    sy,
    eye: eyeClamped,
  };
}

/* ---------- emit ---------- */
const toF64 = (n) => (Number.isInteger(n) ? `${n}.0` : `${n}`);
let rust = '';
let js = '';
const results = {};

for (const [name, def] of Object.entries(SHAPES)) {
  const smoothed = chaikin(def.control, def.smooth);
  const filled = fillCanvas(smoothed);
  const ring = resample(filled);
  const face = deriveFace(ring, def);
  results[name] = { ring, face, tilt: def.tilt };
  const key = name.toLowerCase();

  rust += `pub const ${name}_RING: [Point; 96] = [\n`;
  for (const [x, y] of ring) rust += `    Point { x: ${toF64(x)}, y: ${toF64(y)} },\n`;
  rust += `];\n\n`;

  const pts = ring.map((p) => `[${toF64(p[0])},${toF64(p[1])}]`).join(',');
  js += `"${key}":{"ring":[${pts}],"face":{"x":${face.x},"y":${face.y},"sx":${face.sx},"sy":${face.sy},"eye":${face.eye}},"tiltScale":${def.tilt}},\n`;
}

fs.writeFileSync(process.argv[3] || '/tmp/shapes_js.txt', js);
console.log(rust);
for (const [name, r] of Object.entries(results)) {
  console.error(`${name}: face=${JSON.stringify(r.face)} tilt=${r.tilt}`);
}
