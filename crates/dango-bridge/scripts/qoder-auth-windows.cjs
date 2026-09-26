#!/usr/bin/env node
// qoder-auth-windows.cjs —— 从 Qoder CN 桌面应用的本机凭证里刷新桥的凭据（Windows）。
//
// 背景：Windows 那台没跑过官方 CLI 的登录，但**桌面应用**是登录状态（同一个账号）。应用把
// 会话存在 `%APPDATA%\com.qodercn.app.stable\auth.v1.dat`：Chromium/Electron 的 OSCrypt
// 格式 —— 头部 "v10"（3 字节）+ AES-256-GCM（nonce 12 + 密文 + tag 16），密钥在同目录的
// `Local State` → `os_crypt.encrypted_key`（DPAPI 保护，当前用户身份即可解）。
//
// 本脚本只做四件事：解出设备令牌（dt-）→ 换 job token（jt-）→ 问一次 userinfo → 写
// `~/.qoder-bridge/auth.json`。**不打印任何 token 值**，**不调用任何轮换接口**
// （`/api/v1/deviceToken/refresh` 一概不碰，免得把桌面端/CLI 挤下线）。
//
// 用法：node qoder-auth-windows.cjs
// 桥的 `refresh_command` 可以指向一个 .cmd 包装（见 README 的 Windows 段），30 秒超时内完成。

const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const crypto = require('node:crypto');
const { execFileSync } = require('node:child_process');

const OPENAPI = 'https://openapi.qoder.com.cn';
const CLIENT_ID = '732aef47-9cf2-46a2-95fe-4cebb5d0d1fa';
const APP_DIR = path.join(
  process.env.APPDATA || path.join(os.homedir(), 'AppData', 'Roaming'),
  'com.qodercn.app.stable',
);
const OUT = process.env.QODER_OUT || path.join(os.homedir(), '.qoder-bridge', 'auth.json');

const say = (m) => console.log(`[qoder-auth-win] ${m}`);
const die = (m) => {
  console.error(`[qoder-auth-win] ✗ ${m}`);
  process.exit(1);
};

/// DPAPI 解开 OSCrypt 的 AES-256 密钥（只走内存，不落盘、不打印）
function unwrapKey() {
  const ps = [
    'Add-Type -AssemblyName System.Security;',
    "$d = Join-Path $env:APPDATA 'com.qodercn.app.stable';",
    "$ls = Get-Content -Raw (Join-Path $d 'Local State') | ConvertFrom-Json;",
    '$b = [Convert]::FromBase64String($ls.os_crypt.encrypted_key);',
    '[Convert]::ToBase64String([System.Security.Cryptography.ProtectedData]::Unprotect($b[5..($b.Length-1)], $null, [System.Security.Cryptography.DataProtectionScope]::CurrentUser))',
  ].join(' ');
  const out = execFileSync('powershell', ['-NoProfile', '-NonInteractive', '-Command', ps], {
    encoding: 'utf8',
    timeout: 20_000,
  });
  const key = Buffer.from(out.trim(), 'base64');
  if (key.length !== 32) die(`解出来的 OSCrypt 密钥长度不对（${key.length}）`);
  return key;
}

function decryptAuth(key) {
  const buf = fs.readFileSync(path.join(APP_DIR, 'auth.v1.dat'));
  if (buf.slice(0, 3).toString() !== 'v10') die(`不认识的凭据格式（${buf.slice(0, 3)}）`);
  const d = crypto.createDecipheriv('aes-256-gcm', key, buf.slice(3, 15));
  d.setAuthTag(buf.slice(buf.length - 16));
  const text = Buffer.concat([d.update(buf.slice(15, buf.length - 16)), d.final()]).toString('utf8');
  const o = JSON.parse(text);
  if (!o.token || String(o.token).length < 20) die('应用凭证里没有可用的 token');
  return o;
}

async function request(method, url, token, body) {
  const res = await fetch(url, {
    method,
    headers: {
      Accept: 'application/json',
      Authorization: `Bearer ${token}`,
      'Cosy-Version': '1.0.1',
      'Cosy-ClientType': '5',
      'User-Agent': 'antigravity-bridge-refresh',
      ...(body ? { 'Content-Type': 'application/json' } : {}),
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

(async () => {
  const app = decryptAuth(unwrapKey());
  say(
    `应用凭证：token ${String(app.token).slice(0, 3)}… 长度 ${String(app.token).length}，` +
      `应用侧到期 ${app.expiresAt || '-'}`,
  );

  const minted = await request('POST', `${OPENAPI}/api/v1/me/jobToken`, app.token, {
    clientId: CLIENT_ID,
  });
  if (minted.status !== 200 || !(minted.j.token || minted.j.device_token)) {
    die(`换 job token 失败：HTTP ${minted.status} ${JSON.stringify(minted.j).slice(0, 160)}`);
  }
  const job = minted.j.token || minted.j.device_token;
  const refresh = minted.j.refresh_token || '';
  let expiresAt = Date.now() + 3600_000;
  const exp = minted.j.expires_at ?? minted.j.expire_time ?? minted.j.expires_in;
  if (typeof exp === 'number') expiresAt = exp > 1e12 ? exp : Date.now() + exp * 1000;
  else if (typeof exp === 'string' && !Number.isNaN(Date.parse(exp))) expiresAt = Date.parse(exp);
  say(
    `换到 job token：${job.slice(0, 3)}… 长度 ${job.length}，过期 ${new Date(expiresAt).toISOString()}`,
  );

  const who = await request('GET', `${OPENAPI}/api/v1/userinfo`, job);
  const uid = who.j.id || who.j.uid || '';
  if (!uid) die(`userinfo 没返回 uid（HTTP ${who.status}）`);

  let machineID = '';
  try {
    machineID = fs.readFileSync(path.join(APP_DIR, 'auth.machine-id'), 'utf8').trim();
  } catch {
    /* 没有就让桥自己生成 */
  }

  fs.mkdirSync(path.dirname(OUT), { recursive: true });
  fs.writeFileSync(
    OUT,
    JSON.stringify(
      {
        jobToken: job,
        jobRefreshToken: refresh,
        accessToken: app.token,
        expiresAt,
        machineID,
        userID: uid,
        name: who.j.name || '',
        email: who.j.email || '',
      },
      null,
      2,
    ),
  );
  say(`✓ 已写入 ${OUT}（uid=${uid} name=${who.j.name || '-'}）`);
})().catch((e) => die(e && e.message ? e.message : String(e)));
