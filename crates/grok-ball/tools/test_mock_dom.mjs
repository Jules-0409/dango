import fs from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const grokBallPath = path.resolve(__dirname, '../../../ui/grok-ball.js');
const code = fs.readFileSync(grokBallPath, 'utf8');

function createMulberry32(seed) {
  let a = seed | 0;
  return function mulberry32() {
    a |= 0;
    a = (a + 0x6D2B79F5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t >>> 0) / 4294967296);
  };
}

class MockElement {
  constructor(tag, attrs = {}) {
    this.tag = tag;
    this.attrs = { ...attrs };
    this.style = {};
    this.children = [];
    this.parentNode = null;
    this.textContent = '';
  }

  setAttribute(k, v) {
    this.attrs[k] = String(v);
  }

  getAttribute(k) {
    return this.attrs[k] ?? null;
  }

  appendChild(child) {
    child.parentNode = this;
    this.children.push(child);
    return child;
  }

  removeChild(child) {
    const idx = this.children.indexOf(child);
    if (idx >= 0) {
      this.children.splice(idx, 1);
      child.parentNode = null;
    }
    return child;
  }

  remove() {
    if (this.parentNode) {
      this.parentNode.removeChild(this);
    }
  }
}

const rng = createMulberry32(42);
let currentTime = 0;

const fakeWindow = {
  Math: Object.create(Math),
  performance: {
    now: () => currentTime
  },
  requestAnimationFrame: () => 0,
  document: {
    createElementNS: (ns, tag) => new MockElement(tag)
  },
  console
};
fakeWindow.Math.random = () => rng();
fakeWindow.window = fakeWindow;

vm.runInNewContext(code, fakeWindow);

const container = new MockElement('div');
const ball = fakeWindow.EmotionBall.create(container, {
  shape: 'blob',
  emotion: '02',
  lite: true,
  autostart: false
});

console.log('Ball created successfully!');
currentTime = 0;
ball.setActive(true);
ball._tick(0);

// Find svg element
const svg = container.children[0];
console.log('SVG children count:', svg.children.length);
// defs, fxBack, bodyG, fxFront
const bodyG = svg.children[2];
console.log('bodyG transform:', bodyG.getAttribute('transform'));
const head = bodyG.children[0];
console.log('head fill:', head.getAttribute('fill'));
const eyeL = bodyG.children[1];
console.log('eyeL transform:', eyeL.getAttribute('transform'));
console.log('eyeL fill:', eyeL.getAttribute('fill'));
console.log('eyeL d length:', eyeL.getAttribute('d').length);

ball._tick(16.667);
console.log('Frame 1 eyeL transform:', eyeL.getAttribute('transform'));
