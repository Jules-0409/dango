import fs from 'node:fs';
import { execSync } from 'node:child_process';

// Run gen_shapes.mjs
execSync('node crates/grok-ball/tools/gen_shapes.mjs /tmp/preview.svg /tmp/shapes_js.txt', { stdio: 'inherit' });

// Read generated js
const newShapesJs = fs.readFileSync('/tmp/shapes_js.txt', 'utf8');

// Read ui/grok-ball.js
let grokBall = fs.readFileSync('ui/grok-ball.js', 'utf8');

// In grok-ball.js, replace from "star":{ to \n};\n
const marker = '"star":{';
const idx = grokBall.indexOf(marker);
if (idx !== -1) {
  // Find where SHAPES ends: before `\n};\n`
  const endMarker = '\n};\n';
  const endIdx = grokBall.indexOf(endMarker, idx);
  if (endIdx !== -1) {
    grokBall = grokBall.slice(0, idx) + newShapesJs.trim().replace(/,\s*$/, '') + '}' + grokBall.slice(endIdx);
    fs.writeFileSync('ui/grok-ball.js', grokBall, 'utf8');
    console.log('Updated ui/grok-ball.js successfully');
  } else {
    console.error('Could not find end of SHAPES in grok-ball.js');
  }
} else {
  console.error('Could not find "star":{ in grok-ball.js');
}

// Now run generate_rust_data.mjs
execSync('node crates/grok-ball/tools/generate_rust_data.mjs', { stdio: 'inherit' });
