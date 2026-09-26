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

function createHarness(seed) {
  const rng = createMulberry32(seed);
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

  return {
    window: fakeWindow,
    setTime: (t) => { currentTime = t; },
    getTime: () => currentTime,
  };
}

function captureSnapshot(container, timeMs, frameIdx, lite) {
  const svg = container.children[0];
  const defs = svg.children[0];
  const fxBack = svg.children[1];
  const bodyG = svg.children[2];
  const head = bodyG.children[0];
  const eyeL = bodyG.children[1];
  const eyeR = bodyG.children[2];
  const fxFront = svg.children[3];

  const radialGrad = defs.children[0];
  const stops = [];
  if (radialGrad && radialGrad.children) {
    for (const st of radialGrad.children) {
      stops.push({
        offset: st.getAttribute('offset') || '',
        color: st.getAttribute('stop-color') || ''
      });
    }
  }

  const headSnap = {
    d: head.getAttribute('d') || '',
    transform: bodyG.getAttribute('transform') || '',
    fill: head.getAttribute('fill') || '',
    stroke: head.getAttribute('stroke') || '',
    stroke_width: parseFloat(head.getAttribute('stroke-width') || '2'),
    stroke_opacity: head.getAttribute('stroke-opacity') ? parseFloat(head.getAttribute('stroke-opacity')) : null,
    stops: head.getAttribute('fill') === 'none' ? [] : stops
  };

  const eyeLSnap = {
    d: eyeL.getAttribute('d') || '',
    transform: eyeL.getAttribute('transform') || '',
    fill: eyeL.getAttribute('fill') || '',
    stroke: eyeL.getAttribute('stroke') || '',
    stroke_width: parseFloat(eyeL.getAttribute('stroke-width') || '1.6'),
    visible: eyeL.style.display !== 'none'
  };

  const eyeRSnap = {
    d: eyeR.getAttribute('d') || '',
    transform: eyeR.getAttribute('transform') || '',
    fill: eyeR.getAttribute('fill') || '',
    stroke: eyeR.getAttribute('stroke') || '',
    stroke_width: parseFloat(eyeR.getAttribute('stroke-width') || '1.6'),
    visible: eyeR.style.display !== 'none'
  };

  const zzz = [];
  const trails = [];
  const confetti = [];

  if (!lite && fxFront) {
    for (const child of fxFront.children) {
      if (child.tag === 'text') {
        zzz.push({
          opacity: parseFloat(child.getAttribute('opacity') || '0'),
          font_size: parseFloat(child.getAttribute('font-size') || '12'),
          transform: child.getAttribute('transform') || ''
        });
      } else if (child.tag === 'circle' || child.tag === 'rect' || (child.tag === 'path' && !child.getAttribute('fill')?.startsWith('url'))) {
        confetti.push({
          kind: child.tag,
          transform: child.getAttribute('transform') || '',
          opacity: parseFloat(child.getAttribute('opacity') || '0'),
          fill: child.getAttribute('fill') || '',
          d: child.getAttribute('d') || null
        });
      }
    }

    // Capture trails
    if (fxBack && fxBack.children) {
      for (let i = 0; i < fxBack.children.length; i++) {
        const backChild = fxBack.children[i];
        const fillUrl = backChild.getAttribute('fill') || '';
        const gradId = fillUrl.replace(/^url\(#/, '').replace(/\)$/, '');
        const grad = defs.children.find(d => d.getAttribute('id') === gradId);
        const frontChild = fxFront.children.find(c => c.getAttribute('fill') === fillUrl);

        const trailStops = [];
        if (grad && grad.children) {
          for (const s of grad.children) {
            trailStops.push({
              offset: s.getAttribute('offset') || '',
              color: s.getAttribute('stop-color') || ''
            });
          }
        }

        trails.push({
          back_d: backChild.getAttribute('d') || '',
          front_d: frontChild ? (frontChild.getAttribute('d') || '') : '',
          opacity: parseFloat(backChild.getAttribute('opacity') || '0'),
          stops: trailStops,
          x1: grad ? parseFloat(grad.getAttribute('x1') || '0') : 0,
          y1: grad ? parseFloat(grad.getAttribute('y1') || '0') : 0,
          x2: grad ? parseFloat(grad.getAttribute('x2') || '0') : 0,
          y2: grad ? parseFloat(grad.getAttribute('y2') || '0') : 0,
        });
      }
    }
  }

  return {
    frame_idx: frameIdx,
    time_ms: timeMs,
    head: headSnap,
    eye_l: eyeLSnap,
    eye_r: eyeRSnap,
    zzz,
    trails,
    confetti
  };
}

function captureDigest(container, timeMs, frameIdx, lite) {
  const svg = container.children[0];
  const fxBack = svg.children[1];
  const bodyG = svg.children[2];
  const head = bodyG.children[0];
  const eyeL = bodyG.children[1];
  const eyeR = bodyG.children[2];
  const fxFront = svg.children[3];

  let zzzCount = 0;
  let trailsCount = 0;
  let confettiCount = 0;

  if (!lite && fxFront) {
    for (const child of fxFront.children) {
      if (child.tag === 'text') {
        zzzCount++;
      } else if (child.tag === 'circle' || child.tag === 'rect' || (child.tag === 'path' && !child.getAttribute('fill')?.startsWith('url'))) {
        confettiCount++;
      }
    }
  }
  if (!lite && fxBack && fxBack.children) {
    trailsCount = fxBack.children.length;
  }

  return {
    frame_idx: frameIdx,
    time_ms: timeMs,
    head_transform: bodyG.getAttribute('transform') || '',
    head_d_len: (head.getAttribute('d') || '').length,
    head_fill: head.getAttribute('fill') || '',
    eye_l_transform: eyeL.getAttribute('transform') || '',
    eye_r_transform: eyeR.getAttribute('transform') || '',
    eye_l_d_len: (eyeL.getAttribute('d') || '').length,
    eye_r_d_len: (eyeR.getAttribute('d') || '').length,
    eye_l_visible: eyeL.style.display !== 'none',
    eye_r_visible: eyeR.style.display !== 'none',
    zzz_count: zzzCount,
    trails_count: trailsCount,
    confetti_count: confettiCount,
  };
}

function runScenario({ name, opts, durationMs, stepMs, sampleEvery, events }) {
  console.log(`Generating fixture: ${name}...`);
  const harness = createHarness(opts.seed || 1337);
  const container = new MockElement('div');
  const ballOpts = { ...opts, autostart: false };
  const ball = harness.window.EmotionBall.create(container, ballOpts);

  harness.setTime(0);
  ball.setActive(true);

  const sampledFrames = [];
  const digests = [];
  const totalFrames = Math.round(durationMs / stepMs);
  let eventIdx = 0;

  for (let frameIdx = 0; frameIdx <= totalFrames; frameIdx++) {
    const timeMs = Math.round(frameIdx * stepMs * 1000) / 1000;
    harness.setTime(timeMs);

    // Apply any event scheduled at or before this time
    if (events) {
      while (eventIdx < events.length && timeMs >= events[eventIdx].timeMs) {
        const ev = events[eventIdx];
        if (ev.type === 'set_emotion') {
          ball.setEmotion(ev.emotion);
        } else if (ev.type === 'burst') {
          ball.burst(ev.count);
        } else if (ev.type === 'spin') {
          ball.spin(ev.turns, ev.dir);
        } else if (ev.type === 'bounce') {
          ball.bounce();
        } else if (ev.type === 'set_active') {
          ball.setActive(ev.active);
        }
        eventIdx++;
      }
    }

    ball._tick(timeMs);

    digests.push(captureDigest(container, timeMs, frameIdx, !!opts.lite));

    const isSample = (frameIdx % sampleEvery === 0) || frameIdx === totalFrames;
    if (isSample) {
      sampledFrames.push(captureSnapshot(container, timeMs, frameIdx, !!opts.lite));
    }
  }

  const serializedEvents = (events || []).map(e => ({
    time_ms: e.timeMs,
    type: e.type,
    emotion: e.emotion ?? null,
    count: e.count ?? null,
    turns: e.turns ?? null,
    dir: e.dir ?? null,
    active: e.active ?? null,
  }));

  return {
    scenario: name,
    opts,
    step_ms: stepMs,
    duration_ms: durationMs,
    events: serializedEvents,
    sampled_frames: sampledFrames,
    digests
  };
}

const fixturesDir = path.resolve(__dirname, '../tests/fixtures');
fs.mkdirSync(fixturesDir, { recursive: true });

const scenarios = [
  // 1. Three shapes (10s, 60fps)
  {
    name: 'shape_blob',
    opts: { shape: 'blob', emotion: '02', lite: true, seed: 101 },
    durationMs: 10000,
    stepMs: 16.667,
    sampleEvery: 10,
  },
  {
    name: 'shape_wedge',
    opts: { shape: 'wedge', emotion: '02', lite: true, seed: 102 },
    durationMs: 10000,
    stepMs: 16.667,
    sampleEvery: 10,
  },
  {
    name: 'shape_gem',
    opts: { shape: 'gem', emotion: '02', lite: true, seed: 103 },
    durationMs: 10000,
    stepMs: 16.667,
    sampleEvery: 10,
  },

  // 2. lite=false with zzz sleep animation (10s, 60fps)
  {
    name: 'lite_false',
    opts: { shape: 'blob', emotion: '00', lite: false, seed: 104 },
    durationMs: 10000,
    stepMs: 16.667,
    sampleEvery: 10,
  },

  // 3. Emotion groups (10s, 60fps)
  {
    name: 'group_life',
    opts: { shape: 'blob', emotion: '01', lite: true, seed: 105 },
    durationMs: 10000,
    stepMs: 16.667,
    sampleEvery: 10,
  },
  {
    name: 'group_emotion',
    opts: { shape: 'blob', emotion: '10', lite: true, seed: 106 },
    durationMs: 10000,
    stepMs: 16.667,
    sampleEvery: 10,
  },
  {
    name: 'group_agent',
    opts: { shape: 'blob', emotion: '30', lite: true, seed: 107 },
    durationMs: 10000,
    stepMs: 16.667,
    sampleEvery: 10,
  },
  {
    name: 'group_custom',
    opts: { shape: 'blob', emotion: '40', lite: true, seed: 108 },
    durationMs: 10000,
    stepMs: 16.667,
    sampleEvery: 10,
  },

  // 4. Emotion transition interpolation (10s, 60fps)
  {
    name: 'transition',
    opts: { shape: 'blob', emotion: '02', lite: true, seed: 109 },
    durationMs: 10000,
    stepMs: 16.667,
    sampleEvery: 10,
    events: [
      { timeMs: 1000, type: 'set_emotion', emotion: '10' },
      { timeMs: 4000, type: 'set_emotion', emotion: '03' },
      { timeMs: 7000, type: 'set_emotion', emotion: '01' },
    ]
  },

  // 5. Idle standby, sleep, and wake sequence (10s, 60fps, full chain)
  {
    name: 'idle_standby_sleep',
    opts: {
      shape: 'blob',
      emotion: '10',
      lite: false,
      seed: 110,
      idle: { standbyAfter: 2000, sleepAfter: 5000, standbyId: '02', sleepId: '00' }
    },
    durationMs: 10000,
    stepMs: 16.667,
    sampleEvery: 10,
    events: [
      { timeMs: 7500, type: 'set_emotion', emotion: '01' }
    ]
  },

  // 6. Confetti burst effects (10s, 60fps, multiple bursts)
  {
    name: 'confetti_burst',
    opts: { shape: 'blob', emotion: '02', lite: false, seed: 111 },
    durationMs: 10000,
    stepMs: 16.667,
    sampleEvery: 10,
    events: [
      { timeMs: 500, type: 'burst', count: 20 },
      { timeMs: 3000, type: 'burst', count: 25 },
      { timeMs: 6000, type: 'burst', count: 30 },
      { timeMs: 8500, type: 'burst', count: 15 },
    ]
  },

  // 7. Non-empty trails: orbit mode, spin speed-up, and fade-out (10s, 60fps)
  {
    name: 'orbit_trails',
    opts: { shape: 'blob', emotion: '30', lite: false, seed: 112 },
    durationMs: 10000,
    stepMs: 16.667,
    sampleEvery: 10,
    events: [
      { timeMs: 2500, type: 'spin', turns: 2, dir: 1 },
      { timeMs: 6000, type: 'set_emotion', emotion: '01' },
    ]
  },

  // 8. Cycling through >= 8 emotions mid-animation (10s, 60fps)
  {
    name: 'rapid_emotions',
    opts: { shape: 'blob', emotion: '01', lite: false, seed: 113 },
    durationMs: 10000,
    stepMs: 16.667,
    sampleEvery: 10,
    events: [
      { timeMs: 400, type: 'set_emotion', emotion: '03' },
      { timeMs: 700, type: 'set_emotion', emotion: '05' },
      { timeMs: 1050, type: 'set_emotion', emotion: '11' },
      { timeMs: 1400, type: 'set_emotion', emotion: '14' },
      { timeMs: 1800, type: 'set_emotion', emotion: '20' },
      { timeMs: 2200, type: 'set_emotion', emotion: '21' },
      { timeMs: 2600, type: 'set_emotion', emotion: '32' },
      { timeMs: 3100, type: 'set_emotion', emotion: '02' },
    ]
  },

  // 9. Overlapping burst + spin + bounce calls (10s, 60fps)
  {
    name: 'overlapping_fx',
    opts: { shape: 'gem', emotion: '02', lite: false, seed: 114 },
    durationMs: 10000,
    stepMs: 16.667,
    sampleEvery: 10,
    events: [
      { timeMs: 300, type: 'burst', count: 20 },
      { timeMs: 600, type: 'spin', turns: 2, dir: 1 },
      { timeMs: 800, type: 'bounce' },
      { timeMs: 2200, type: 'burst', count: 25 },
      { timeMs: 2500, type: 'bounce' },
      { timeMs: 4000, type: 'spin', turns: 3, dir: -1 },
      { timeMs: 4200, type: 'burst', count: 30 },
      { timeMs: 6000, type: 'bounce' },
      { timeMs: 6500, type: 'burst', count: 20 },
      { timeMs: 7000, type: 'spin', turns: 2, dir: 1 },
      { timeMs: 8500, type: 'bounce' },
    ]
  },
];

for (const sc of scenarios) {
  const result = runScenario(sc);
  const outPath = path.join(fixturesDir, `${sc.name}.json`);
  fs.writeFileSync(outPath, JSON.stringify(result), 'utf8');
  console.log(`Wrote fixture to ${outPath} (${result.sampled_frames.length} sampled frames, ${result.digests.length} digests)`);
}

console.log('All fixtures generated successfully.');
