#!/usr/bin/env node
// cursor-acp-probe.mjs —— P0 spike：用一个最小假 ACP 客户端摸 `agent acp` 的真实形状。
//
// 对应 docs/CURSOR-PLAN.md §7 的 P0 五问。回答 S1/S2/S5 只需要握手 —— **绝不发
// session/prompt**，所以 --handshake 一个字额度都不烧；--prompt 才会真推理。
//
//   node scripts/cursor-acp-probe.mjs --handshake --use-login
//   node scripts/cursor-acp-probe.mjs --handshake --key      # key 来自 ~/.cursor-bridge/accounts.json
//   node scripts/cursor-acp-probe.mjs --prompt "你好" --key --yes-burn-quota
//
// 产物：research/cursor-acp-<mode>-<auth>-<时间戳>.json 收下全部 JSON-RPC 帧（先脱敏），
// 单测可以直接拿去当夹具（probe::LINES 的规矩：用真抓包，不手编形状）。

import { spawn } from "node:child_process";
import { accessSync, existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), ".."); // antigravity-bridge/
const argv = process.argv.slice(2);
const MODE = argv.includes("--prompt") ? "prompt" : "handshake";
const USE_KEY = argv.includes("--key");
const USE_LOGIN = argv.includes("--use-login");
const PROMPT_TEXT = MODE === "prompt" ? argv[argv.indexOf("--prompt") + 1] : null;
if (MODE === "prompt" && !PROMPT_TEXT) die("--prompt 后面要给文本");
if (MODE === "prompt" && !argv.includes("--yes-burn-quota"))
  die("--prompt 会真推理、烧账号额度：确认要烧再加 --yes-burn-quota");
if (MODE === "handshake" && !USE_KEY && !USE_LOGIN)
  die("握手也要说清用哪种认证：--key（日抛号）或 --use-login（CLI 登录态）");
if (USE_KEY && USE_LOGIN) die("--key 和 --use-login 二选一");

function die(msg) { console.error(msg); process.exit(2); }

function findAgent() {
  const env = process.env.CURSOR_BRIDGE_AGENT_BIN;
  const cands = env ? [env] : [];
  for (const dir of [path.join(os.homedir(), ".local", "bin"), "/usr/local/bin", "/opt/homebrew/bin"])
    for (const name of ["cursor-agent", "agent"]) cands.push(path.join(dir, name));
  return cands.find((p) => { try { accessSync(p); return true; } catch { return false; } });
}

function dailyKey() {
  try {
    const j = JSON.parse(readFileSync(path.join(os.homedir(), ".cursor-bridge", "accounts.json"), "utf8"));
    const a = (j.accounts || []).find((x) => x.apiKey && !x.disabled);
    return a && { name: a.name || "?", key: a.apiKey };
  } catch { return null; }
}

// —— 脱敏：帧里任何像凭据的字段/串都不落盘 ——
const SECRET_KEY_RE = /api[_-]?key|authorization|token|secret|password/i;
const SECRET_STR_RE = /(api[_-]?key|authorization|bearer|token|sk-)[\w:./+\-=]{6,}/gi;
function scrub(v) {
  if (typeof v === "string") return v.replace(SECRET_STR_RE, (m) => m.slice(0, Math.min(m.length, 6)) + "<scrub>");
  if (Array.isArray(v)) return v.map(scrub);
  if (v && typeof v === "object") {
    const o = {};
    for (const [k, x] of Object.entries(v)) o[k] = SECRET_KEY_RE.test(k) ? "<scrubbed>" : scrub(x);
    return o;
  }
  return v;
}

const AGENT = findAgent();
if (!AGENT) die("找不到 cursor-agent / agent（CURSOR_BRIDGE_AGENT_BIN 可以指路）");
let agentArgs = ["acp"];
if (USE_KEY) {
  const acct = dailyKey();
  if (!acct) die("~/.cursor-bridge/accounts.json 里没有可用的 key");
  // S1 的第一问：全局 flag 放子命令前面认不认 —— 先试 --api-key 前置。
  agentArgs = ["--api-key", acct.key, "acp"];
}
const WORKSPACE = path.join(os.homedir(), ".cursor-bridge", "workspace");
if (!existsSync(WORKSPACE)) mkdirSync(WORKSPACE, { recursive: true });

const T0 = Date.now();
const frames = [];
const record = (dir, msg) => frames.push({ dir, ms: Date.now() - T0, msg: typeof msg === "string" ? msg.replace(SECRET_STR_RE, (m) => m.slice(0, 6) + "<scrub>") : scrub(msg) });

const proc = spawn(AGENT, agentArgs, { cwd: WORKSPACE, stdio: ["pipe", "pipe", "pipe"] });
let nextId = 1;
const pending = new Map();
const send = (msg) => { record("out", msg); proc.stdin.write(JSON.stringify(msg) + "\n"); };
const rpc = (method, params) => new Promise((resolve, reject) => {
  const id = nextId++;
  pending.set(id, { resolve, reject });
  send({ jsonrpc: "2.0", id, method, params });
});
const notify = (method, params) => send({ jsonrpc: "2.0", method, params });

let buf = "";
proc.stdout.on("data", (d) => {
  buf += d.toString("utf8");
  let nl;
  while ((nl = buf.indexOf("\n")) >= 0) {
    const line = buf.slice(0, nl).trim(); buf = buf.slice(nl + 1);
    if (!line) continue;
    let msg; try { msg = JSON.parse(line); } catch { record("stdout-nonjson", line); continue; }
    record("in", msg);
    onMessage(msg);
  }
});
proc.stderr.on("data", (d) => record("stderr", d.toString("utf8").trim()));
proc.on("exit", (code, sig) => { record("exit", { code, sig }); finish(); });

function onMessage(msg) {
  if (msg.id !== undefined && (msg.result !== undefined || msg.error !== undefined)) {
    const p = pending.get(msg.id); if (!p) return;
    pending.delete(msg.id);
    if (msg.error) p.reject(Object.assign(new Error(JSON.stringify(scrub(msg.error)))), {}); else p.resolve(msg.result);
    return;
  }
  if (msg.id !== undefined && msg.method) { // agent → client 的请求：能接的接，接不了明说
    if (msg.method === "session/request_permission") {
      const opts = msg.params?.options || [];
      const pick = opts.find((o) => /allow_once/.test(o.kind || "") || o.optionId === "allow_once") || opts[0];
      send({ jsonrpc: "2.0", id: msg.id, result: { outcome: pick ? { outcome: "selected", optionId: pick.optionId } : { outcome: "cancelled" } } });
    } else {
      send({ jsonrpc: "2.0", id: msg.id, error: { code: -32601, message: "not implemented by probe" } });
    }
    return;
  }
  if (msg.method) return; // 通知：已录帧，prompt 模式要分析的就是它
}

function finish() {
  const auth = USE_KEY ? "key" : "login";
  const ts = new Date().toISOString().replace(/[:.]/g, "-");
  const out = path.join(ROOT, "research", `cursor-acp-${MODE}-${auth}-${ts}.json`);
  writeFileSync(out, JSON.stringify({
    note: "captured by scripts/cursor-acp-probe.mjs; agent=" + AGENT,
    argsKind: USE_KEY ? ["--api-key", "<redacted>", "acp"] : ["acp"],
    frames,
  }, null, 1) + "\n");
  console.log("帧存到：", out, "（in/out/stderr 共", frames.length, "条）");
  if (summary) { console.log("--- 摘要 ---"); console.log(summary.join("\n")); }
}
let summary = null;

const hardTimeout = MODE === "prompt" ? 300_000 : 30_000;
setTimeout(() => { console.error("超时，掐掉 CLI"); proc.kill("SIGKILL"); }, hardTimeout).unref();

async function main() {
  const init = await rpc("initialize", {
    protocolVersion: 1,
    clientCapabilities: { fs: { readTextFile: false, writeTextFile: false } },
  });
  summary = [
    "protocolVersion(协商后): " + init.protocolVersion,
    "agentCapabilities: " + JSON.stringify(scrub(init.agentCapabilities ?? {})),
    "authMethods: " + JSON.stringify((init.authMethods || []).map((a) => ({ id: a.id, name: a.name, type: a.type }))),
  ];
  if (MODE === "handshake") {
    const sess = await rpc("session/new", { cwd: WORKSPACE, mcpServers: [] });
    summary.push("session/new 结果键: " + JSON.stringify(Object.keys(sess)));
    summary.push("sessionId: " + String(sess.sessionId).slice(0, 8) + "…");
    if (sess.models) summary.push("models: " + JSON.stringify((sess.models.available || sess.models).map?.((m) => m.modelId || m.id) || sess.models));
    if (sess.modes) summary.push("modes: " + JSON.stringify(scrub(sess.modes)));
    if (sess.configOptions) summary.push("configOptions: " + JSON.stringify(scrub(sess.configOptions)));
    proc.kill("SIGTERM");
    return;
  }
  const sess = await rpc("session/new", { cwd: WORKSPACE, mcpServers: [] });
  const res = await rpc("session/prompt", { sessionId: sess.sessionId, prompt: [{ type: "text", text: PROMPT_TEXT }] });
  summary = summary || [];
  summary.push("stopReason: " + JSON.stringify(res));
  proc.kill("SIGTERM");
}

main().catch((e) => { console.error("跑挂了：", e.message || e); try { proc.kill("SIGKILL"); } catch {} finish(); process.exit(1); });
