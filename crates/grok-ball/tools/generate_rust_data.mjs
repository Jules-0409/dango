import fs from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const grokBallPath = path.resolve(__dirname, '../../../ui/grok-ball.js');
const code = fs.readFileSync(grokBallPath, 'utf8');

const fakeWindow = {
  Math,
  performance: { now: () => 0 },
  requestAnimationFrame: () => 0,
  document: {
    createElementNS: () => ({
      setAttribute: () => {},
      appendChild: () => {},
      style: {}
    })
  }
};
fakeWindow.window = fakeWindow;

vm.runInNewContext(code, fakeWindow);

const rings = fakeWindow.EB_RINGS;
const groups = fakeWindow.EMOTION_GROUPS;
const seed = fakeWindow.EMOTION_SEED;

let out = `// Generated from ui/grok-ball.js - DO NOT EDIT MANUALLY
// Grok Ball standalone engine | MIT License

use crate::types::*;

pub const HEAD_C: f64 = 114.2705;
pub const EYE_HALF: f64 = 21.0;
pub const STAR_GOLD: &str = "#f4c34e";
pub const CONFETTI_COLORS: [&str; 6] = [
    "#f9705c", "#5b95f0", "#3fbe86", "#f5b13f", "#9a72ee", "#35c3bd",
];

pub const STAR_PATH: &str = "M0.000 -1.000L0.247 -0.340L0.951 -0.309L0.400 0.130L0.588 0.809L0.000 0.420L-0.588 0.809L-0.400 0.130L-0.951 -0.309L-0.247 -0.340Z";

pub const BOUNCE_SEGS: [BounceSeg; 4] = [
    BounceSeg { h: 48.0, d: 0.5 },
    BounceSeg { h: 28.0, d: 0.382 },
    BounceSeg { h: 14.0, d: 0.27 },
    BounceSeg { h: 6.0, d: 0.177 },
];
pub const BOUNCE_TOTAL: f64 = 0.5 + 0.382 + 0.27 + 0.177;

pub const EXPRESSIONS: [[[Point; 48]; 2]; 25] = [
`;

const toF64 = (n) => (Number.isInteger(n) ? `${n}.0` : `${n}`);

for (let i = 0; i < rings.EXPRESSIONS.length; i++) {
  const pair = rings.EXPRESSIONS[i];
  out += `    // Expression ${i}\n    [\n`;
  for (let eye = 0; eye < 2; eye++) {
    const pts = pair[eye];
    out += `        [\n`;
    for (let p = 0; p < pts.length; p++) {
      out += `            Point { x: ${toF64(pts[p][0])}, y: ${toF64(pts[p][1])} },\n`;
    }
    out += `        ],\n`;
  }
  out += `    ],\n`;
}
out += `];\n\n`;

// Shapes: emit every shape present in the JS SHAPES table (blob/wedge/gem
// plus any extras like star/cloud/heart/square/drop), in a stable order.
const SHAPE_ORDER = ['blob', 'wedge', 'gem', 'star', 'cloud', 'heart', 'square', 'drop'];
const shapeKeys = SHAPE_ORDER.filter((k) => rings.SHAPES[k]);
const variant = (k) => k[0].toUpperCase() + k.slice(1);

for (const shapeKey of shapeKeys) {
  const shape = rings.SHAPES[shapeKey];
  const ringName = `${shapeKey.toUpperCase()}_RING`;
  out += `pub const ${ringName}: [Point; 96] = [\n`;
  for (const pt of shape.ring) {
    out += `    Point { x: ${toF64(pt[0])}, y: ${toF64(pt[1])} },\n`;
  }
  out += `];\n\n`;
}

// Face + tilt constants, straight from each shape's JS params.
for (const shapeKey of shapeKeys) {
  const f = rings.SHAPES[shapeKey].face;
  out += `pub const ${shapeKey.toUpperCase()}_FACE: FaceParams = FaceParams { x: ${toF64(f.x)}, y: ${toF64(f.y)}, sx: ${toF64(f.sx)}, sy: ${toF64(f.sy)}, eye: ${toF64(f.eye)} };\n`;
}
out += `\n`;
for (const shapeKey of shapeKeys) {
  const t = rings.SHAPES[shapeKey].tiltScale;
  out += `pub const ${shapeKey.toUpperCase()}_TILT_SCALE: f64 = ${toF64(t)};\n`;
}

out += `\npub fn get_shape_data(shape: ShapeKind) -> ShapeData {\n    match shape {\n`;
for (const shapeKey of shapeKeys) {
  const U = shapeKey.toUpperCase();
  out += `        ShapeKind::${variant(shapeKey)} => ShapeData {\n            ring: &${U}_RING,\n            face: ${U}_FACE,\n            tilt_scale: ${U}_TILT_SCALE,\n        },\n`;
}
out += `    }\n}\n`;

// Also serialize full EMOTIONS JSON for embedding
const targetSrcDir = path.resolve(__dirname, '../src');
fs.mkdirSync(targetSrcDir, { recursive: true });
fs.writeFileSync(path.join(targetSrcDir, 'data.rs'), out, 'utf8');

// Write emotions raw json and groups raw json so serde can load them cleanly
fs.writeFileSync(
  path.join(targetSrcDir, 'emotions.json'),
  JSON.stringify(seed, null, 2),
  'utf8'
);
fs.writeFileSync(
  path.join(targetSrcDir, 'groups.json'),
  JSON.stringify(groups, null, 2),
  'utf8'
);

console.log('Generated data.rs, emotions.json, groups.json successfully.');
