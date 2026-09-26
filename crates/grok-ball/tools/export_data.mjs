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

console.log('EB_RINGS keys:', Object.keys(fakeWindow.EB_RINGS));
console.log('EMOTION_GROUPS count:', fakeWindow.EMOTION_GROUPS.length);
console.log('EMOTION_SEED count:', fakeWindow.EMOTION_SEED.length);
console.log('Sample emotion 00:', fakeWindow.EMOTION_SEED[0].id, fakeWindow.EMOTION_SEED[0].name);

const outDir = path.resolve(__dirname, '../data');
fs.mkdirSync(outDir, { recursive: true });

fs.writeFileSync(
  path.join(outDir, 'rings.json'),
  JSON.stringify(fakeWindow.EB_RINGS, null, 2),
  'utf8'
);

fs.writeFileSync(
  path.join(outDir, 'emotions.json'),
  JSON.stringify(fakeWindow.EMOTION_SEED, null, 2),
  'utf8'
);

fs.writeFileSync(
  path.join(outDir, 'groups.json'),
  JSON.stringify(fakeWindow.EMOTION_GROUPS, null, 2),
  'utf8'
);

console.log('Successfully wrote data files to', outDir);
