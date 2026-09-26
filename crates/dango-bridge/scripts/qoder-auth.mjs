#!/usr/bin/env node
// qoder-auth.mjs —— 为 Qoder CN 直连通道刷新凭据（不打印任何 token 值）。
//
// 为什么这么做：Qoder CN 的 PAT 通道用不了 3.8-Flash 的免费活动（活动只走客户端通道），
// 而官方 CLI `qoderclicn` 就是一个已登录的客户端；它在调用 openapi 时会带上自己的会话 token。
// 我们跑一条只读命令（--list-models，不做推理、不花额度），用一个 fetch 钩子旁观它自己的
// 请求头，拿到 token 后去换 job token（jt-），把结果写进 ~/.qoder-bridge/auth.json（0600）。
//
// 用法：node scripts/qoder-auth.mjs [--check]
//   --check                         只读看一眼现在这份凭据还有多久过期（不联网、不改文件）
//   QODER_CLI=/path/to/qoderclicn   指定 CLI
//   QODER_OUT=/path/to/auth.json    指定输出（默认 ~/.qoder-bridge/auth.json）
// 凭据文件字段：jobToken / jobRefreshToken / accessToken / expiresAt / machineID / userID / name / email

import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const HOME = process.env.HOME || os.homedir();
const CLI = process.env.QODER_CLI || path.join(HOME, '.local/bin/qoderclicn');
const OUT = process.env.QODER_OUT || path.join(HOME, '.qoder-bridge/auth.json');
const OPENAPI = 'https://openapi.qoder.com.cn';
const CLIENT_ID = '732aef47-9cf2-46a2-95fe-4cebb5d0d1fa';
const MACHINE_ID_FILE = path.join(HOME, '.qoder-cn/.auth/machine_id');

const say = (m) => console.log(`[qoder-auth] ${m}`);
const die = (m) => {
  console.error(`[qoder-auth] ✗ ${m}`);
  process.exit(1);
};

// --check：只看现有凭据（还没建临时钩子文件，省得白留一份）
if (process.argv.includes('--check')) {
  if (!fs.existsSync(OUT)) die(`还没有凭据文件：${OUT}（跑一次不带 --check 的就会生成）`);
  let state = {};
  try {
    state = JSON.parse(fs.readFileSync(OUT, 'utf8'));
  } catch {
    die(`凭据文件不是合法 JSON：${OUT}`);
  }
  const left = (state.expiresAt || 0) - Date.now();
  say(`凭据文件：${OUT}`);
  say(`身份：uid=${state.userID || '-'} name=${state.name || '-'}`);
  say(
    left > 0
      ? `job token 还有 ${(left / 3600_000).toFixed(1)} 小时过期`
      : 'job token 已过期（桥会先用 accessToken 换新的，换不到才跑 refresh_command）',
  );
  process.exit(0);
}

// 把 fetch 钩子写成临时文件（0600），只为旁观 CLI 自己的请求头
const HOOK = path.join(os.tmpdir(), `qoder-auth-hook-${process.pid}.cjs`);
const DUMP = path.join(os.tmpdir(), `qoder-auth-dump-${process.pid}.jsonl`);
fs.writeFileSync(
  HOOK,
  `const fs=require('node:fs');const out=process.env.QODER_DUMP;const orig=globalThis.fetch;
if(typeof orig==='function'){globalThis.fetch=function(i,init){try{const u=typeof i==='string'?i:(i&&i.url)||String(i);
const h={};const src=(init&&init.headers)||(i&&i.headers);if(src){if(typeof src.forEach==='function'&&!Array.isArray(src))src.forEach((v,k)=>h[k]=v);
else if(Array.isArray(src))for(const kv of src)h[kv[0]]=kv[1];else for(const k of Object.keys(src))h[k]=src[k];}
fs.appendFileSync(out,JSON.stringify({url:u,headers:h})+'\\n',{mode:0o600});}catch(e){}
return orig.call(this,i,init);};}\n`,
  { mode: 0o600 },
);

function harvest() {
  if (!fs.existsSync(DUMP)) return '';
  let token = '';
  for (const line of fs.readFileSync(DUMP, 'utf8').split('\n')) {
    try {
      const j = JSON.parse(line);
      for (const [k, v] of Object.entries(j.headers || {})) {
        if (!/^authorization$/i.test(k)) continue;
        const t = String(v).replace(/^Bearer\s+/i, '');
        if (t.length > 20 && !/^COSY\./i.test(t)) token = t;
      }
    } catch {
      /* 跳过坏行 */
    }
  }
  return token;
}

async function post(url, token, body) {
  const res = await fetch(url, {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      Accept: 'application/json',
      Authorization: `Bearer ${token}`,
      'Cosy-Version': '1.0.1',
      'Cosy-ClientType': '5',
      'User-Agent': 'qoder-auth-refresh',
    },
    body: body ? JSON.stringify(body) : undefined,
  });
  let j = {};
  try {
    j = await res.json();
  } catch {
    /* 空响应 */
  }
  return { status: res.status, j };
}

async function get(url, token) {
  const res = await fetch(url, {
    headers: { Accept: 'application/json', Authorization: `Bearer ${token}`, 'Cosy-Version': '1.0.1', 'Cosy-ClientType': '5', 'User-Agent': 'qoder-auth-refresh' },
  });
  let j = {};
  try {
    j = await res.json();
  } catch {
    /* 空响应 */
  }
  return { status: res.status, j };
}

try {
  if (!fs.existsSync(CLI)) die(`找不到官方 CLI：${CLI}（用 QODER_CLI=… 指定）`);
  say('跑一次官方 CLI 的只读命令（--list-models），旁观它自己的会话 token…');
  const run = spawnSync(CLI, ['--list-models'], {
    env: { ...process.env, NODE_OPTIONS: `--require ${HOOK}`, QODER_DUMP: DUMP },
    encoding: 'utf8',
    timeout: 120_000,
  });
  if (run.status !== 0) say(`CLI 退出码 ${run.status}（继续尝试解析已有的请求记录）`);

  const session = harvest();
  if (!session) die('没能从 CLI 的请求里拿到会话 token（CLI 版本或出网方式可能变了）');
  say(`拿到会话 token：${session.slice(0, 3)}… 长度 ${session.length}`);

  let job = session;
  let refresh = '';
  let expiresAt = Date.now() + 3600_000;
  const minted = await post(`${OPENAPI}/api/v1/me/jobToken`, session, { clientId: CLIENT_ID });
  if (minted.status === 200 && (minted.j.token || minted.j.device_token)) {
    job = minted.j.token || minted.j.device_token;
    refresh = minted.j.refresh_token || '';
    const exp = minted.j.expires_at ?? minted.j.expire_time ?? minted.j.expires_in;
    if (typeof exp === 'number') expiresAt = exp > 1e12 ? exp : Date.now() + exp;
    else if (typeof exp === 'string' && !Number.isNaN(Date.parse(exp))) expiresAt = Date.parse(exp);
    say(`换到 job token：${job.slice(0, 3)}… 长度 ${job.length}，过期 ${new Date(expiresAt).toISOString()}`);
  } else {
    say(`换 job token 未成功（HTTP ${minted.status}），先直接用会话 token`);
  }

  const who = await get(`${OPENAPI}/api/v1/userinfo`, job);
  const uid = who.j.id || who.j.uid || '';
  if (!uid) die(`userinfo 没返回 uid（HTTP ${who.status}）——网关会报 Login expired`);
  say(`身份：uid=${uid} name=${who.j.name || '-'} email=${who.j.email || '-'}`);

  let machineID = '';
  try {
    machineID = fs.readFileSync(MACHINE_ID_FILE, 'utf8').trim();
  } catch {
    /* 没有就用随机 UUID，由使用方生成 */
  }

  fs.mkdirSync(path.dirname(OUT), { recursive: true, mode: 0o700 });
  fs.writeFileSync(
    OUT,
    JSON.stringify({ jobToken: job, jobRefreshToken: refresh, accessToken: session, expiresAt, machineID, userID: uid, name: who.j.name || '', email: who.j.email || '' }, null, 2),
    { mode: 0o600 },
  );
  say(`✓ 已写入 ${OUT}（0600）`);
} finally {
  for (const f of [HOOK, DUMP]) {
    try {
      fs.unlinkSync(f);
    } catch {
      /* 已删 */
    }
  }
}
