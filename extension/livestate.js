// Devin の実行時 MCP 状態を「読み取り専用」で取得するモジュール。
// 公開 API では取得経路が無いため、Devin 自身が出力するログだけを解析する。
//
// 取得元:
//   <logs>/<session>/window<N>/exthost/codeium.windsurf/MCP <name>.log
//     … エディタ側 MCP ホストの接続イベント
//       「MCP server '<name>' connected successfully」→ 接続済み
//       「connection failed」→ 失敗 / 「Connecting to …」のみ→ 接続中
//   <logs>/<session>/window<N>/exthost/codeium.windsurf/Devin.log
//     … devin-cli エージェント側 MCP クライアント (mcp-go)
//       「Server '<key>' initialized successfully: <表示名> v<版>」→ 稼働版
//       直後の {"result":{"tools":[…]}} 応答 → 実ツール数
//       「All connection methods failed for <url>」→ URL 接続の失敗
//
// ウィンドウ特定: activate 時に診断チャネルへ書き込んだ marker を
//   output_logging_*/*DTM*.log から検索し、所在する window<N> を特定する。
//   marker が見つからない場合は全ウィンドウの最終イベントを併合 (approx=true)。

const fs = require("fs");
const path = require("path");

const LOG_TS = /^(\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2})\.\d+/;
const LINE_CAP = 256 * 1024;   // MCP <name>.log 用の末尾読み取り上限
const AGENT_CAP = 2 * 1024 * 1024; // Devin.log は大きいため広めに読む

function newestSessionDir(logsRoot) {
  try {
    const dirs = fs.readdirSync(logsRoot, { withFileTypes: true })
      .filter((d) => d.isDirectory() && /^\d{8}T\d{6}$/.test(d.name))
      .map((d) => d.name).sort();
    // 新しい順に、実際に window*/exthost/codeium.windsurf を持つ
    // セッションを選ぶ (空のセッションディレクトリが残ることがある)
    for (let i = dirs.length - 1; i >= 0; i--) {
      const base = path.join(logsRoot, dirs[i]);
      const hasWindow = fs.readdirSync(base, { withFileTypes: true }).some((w) =>
        w.isDirectory() && w.name.startsWith("window")
        && fs.existsSync(path.join(base, w.name, "exthost", "codeium.windsurf")));
      if (hasWindow) return base;
    }
    return null;
  } catch (_) {
    return null;
  }
}

// marker を含む *DTM*.log が存在する window<N>/exthost を返す。
// 見つからない場合は codeium.windsurf を持つ全 exthost ディレクトリを返す。
function findExthostDirs(sessionDir, marker) {
  const out = { dirs: [], cutoff: null, approx: true };
  let all = [];
  try {
    for (const win of fs.readdirSync(sessionDir, { withFileTypes: true })) {
      if (!win.isDirectory() || !win.name.startsWith("window")) continue;
      const exthost = path.join(sessionDir, win.name, "exthost");
      if (!fs.existsSync(path.join(exthost, "codeium.windsurf"))) continue;
      all.push({ exthost, cutoff: null, outputs: [] });
      const exDirs = fs.readdirSync(exthost, { withFileTypes: true });
      for (const outDir of exDirs) {
        if (!outDir.isDirectory() || !outDir.name.startsWith("output_logging_")) continue;
        const full = path.join(exthost, outDir.name);
        let birth = 0;
        try { birth = fs.statSync(full).birthtimeMs; } catch (_) { /* ignore */ }
        all[all.length - 1].outputs.push({ dir: full, birth });
      }
      all[all.length - 1].outputs.sort((a, b) => a.birth - b.birth);
      const newest = all[all.length - 1].outputs[all[all.length - 1].outputs.length - 1];
      if (newest) all[all.length - 1].cutoff = newest.birth;
      // marker 探索: 全 output_logging_* 内の *DTM*.log を走査
      for (const od of all[all.length - 1].outputs) {
        let files = [];
        try { files = fs.readdirSync(od.dir); } catch (_) { continue; }
        for (const file of files) {
          if (!file.includes("DTM")) continue;
          try {
            const text = fs.readFileSync(path.join(od.dir, file), "utf8");
            if (marker && text.includes(marker)) {
              out.dirs = [{ exthost, cutoff: all[all.length - 1].cutoff }];
              out.approx = false;
              out.cutoff = all[all.length - 1].cutoff;
              return out;
            }
          } catch (_) { /* 読み取り中の場合は次へ */ }
        }
      }
    }
  } catch (_) {
    return out;
  }
  out.dirs = all.map((entry) => ({ exthost: entry.exthost, cutoff: entry.cutoff }));
  return out;
}

function tailLines(file, cap) {
  try {
    const stat = fs.statSync(file);
    const start = Math.max(0, stat.size - (cap || LINE_CAP));
    const fd = fs.openSync(file, "r");
    try {
      const buf = Buffer.alloc(stat.size - start);
      fs.readSync(fd, buf, 0, buf.length, start);
      return buf.toString("utf8").split("\n");
    } finally {
      fs.closeSync(fd);
    }
  } catch (_) {
    return [];
  }
}

function lineTs(line) {
  const m = line.match(LOG_TS);
  if (!m) return null;
  const t = Date.parse(m[1].replace(" ", "T"));
  return Number.isFinite(t) ? t : null;
}

// MCP <name>.log の末尾から最終状態イベントを抽出。
// return: { state: "connected"|"connecting"|"error", ts, error? } | null
function lastExtEvent(file) {
  const lines = tailLines(file);
  for (let i = lines.length - 1; i >= 0 && i >= lines.length - 400; i--) {
    const line = lines[i];
    let state = null;
    if (/connected successfully/.test(line)) state = "connected";
    else if (/connection failed/.test(line)) state = "error";
    else if (/Connecting to .*MCP server|Starting \S+ MCP server/.test(line)) state = "connecting";
    if (!state) continue;
    const fm = state === "error" ? line.match(/connection failed:?\s*(.*)/) : null;
    const error = fm ? fm[1].trim().slice(0, 160) : null;
    return { state, ts: lineTs(line), error };
  }
  return null;
}

// "readResponses got line: {...}" 行から tools/list 応答の tools 配列を取り出す。
// initialize 応答など "tools" を含むが配列でないものは除外する。
function toolsPayload(line) {
  const j = line.indexOf("got line: ");
  if (j < 0) return null;
  const payload = line.slice(j + 10).trim();
  if (!payload.startsWith("{") || !payload.includes('"tools"')) return null;
  let parsed;
  try { parsed = JSON.parse(payload); } catch (_) { return null; }
  const tools = parsed && parsed.result && parsed.result.tools;
  return Array.isArray(tools) ? tools : null;
}

// 先頭ツール名→サーバーの署名を、全行から学習する (cutoff は適用しない)。
// tools 応答にサーバー名が無いため帰属は推測になるが、pending が1件だけの
// 一意な組は事実上確定できるため署名として記録する。
// pending が複数ある曖昧な応答では滞留 pending を全て破棄し、誤った署名を作らない。
function learnSignatures(lines, remoteNames) {
  const sig = {};
  const pending = [];
  for (const line of lines) {
    const m = line.match(/\[MCP\] Server '([^']+)' initialized successfully:/);
    if (m) {
      if (!remoteNames || !remoteNames.has(m[1])) pending.push(m[1]);
      continue;
    }
    const tools = toolsPayload(line);
    if (!tools) continue;
    if (pending.length === 1) {
      const first = tools[0] && tools[0].name;
      if (first && !sig[first]) sig[first] = pending[0];
    }
    pending.length = 0; // 消費(1件)または曖昧のため破棄(複数)
  }
  return sig;
}

// Devin.log を時系列に解析し、初期化済みサーバー・稼働版・実ツール数・URL失敗を返す。
// tools 応答にはサーバー名が無いため、次の規則で帰属する:
//   - McpRemote(HTTP/SSE) は STDIO tools 応答を出さないため pending に入れない
//   - 学習済み署名(先頭ツール名→サーバー)に一致 → そのサーバー (pending 外なら応答の再送として件数のみ更新)
//   - pending が1件だけ → そのサーバーに帰属し署名を学習
//   - pending が複数で署名も無い → 順序が確定できないため帰属しない (誤った件数を出さない)
function parseAgentLog(file, cutoff, remoteNames) {
  const result = { initialized: {}, toolCounts: {}, urlFailures: [] };
  const lines = tailLines(file, AGENT_CAP);
  const sig = learnSignatures(lines, remoteNames);
  const pending = [];
  for (const line of lines) {
    const ts = lineTs(line);
    if (ts == null || (cutoff && ts < cutoff)) continue;
    let m = line.match(/\[MCP\] Server '([^']+)' initialized successfully: (.+?) v(\S+)\s*$/);
    if (m) {
      result.initialized[m[1]] = { version: m[3], display: m[2], ts };
      if (!remoteNames || !remoteNames.has(m[1])) pending.push(m[1]);
      continue;
    }
    m = line.match(/\[MCP\] All connection methods failed for (\S+)/);
    if (m) {
      result.urlFailures.push({ url: m[1].replace(/\/+$/, ""), ts });
      continue;
    }
    const tools = toolsPayload(line);
    if (!tools) continue;
    const first = tools[0] && tools[0].name;
    if (first && sig[first] !== undefined) {
      const key = sig[first];
      const pi = pending.indexOf(key);
      if (pi >= 0) pending.splice(pi, 1);
      result.toolCounts[key] = tools.length;
    } else if (pending.length === 1) {
      const key = pending.pop();
      result.toolCounts[key] = tools.length;
      if (first && !sig[first]) sig[first] = key;
    } else {
      pending.length = 0;
    }
  }
  return result;
}

// servers: [{ name, configPath, kind, command, disabled }]
// return { approx, byKey: { "<configPath|name>": { state, via, version, tools, error } } }
function collect(opts) {
  const empty = { approx: true, byKey: {} };
  try {
    const logsRoot = opts.logsRoot
      || path.join(process.env.APPDATA || "", "Devin - Next", "logs");
    const sessionDir = newestSessionDir(logsRoot);
    if (!sessionDir) return empty;
    const found = findExthostDirs(sessionDir, opts.marker);
    if (!found.dirs.length) return empty;
    const extByName = {};   // name → {state, ts, error}
    const agent = { initialized: {}, toolCounts: {}, urlFailures: [] };
    const remoteNames = new Set((opts.servers || [])
      .filter((s) => s.kind === "McpRemote").map((s) => s.name));
    for (const entry of found.dirs) {
      const wind = path.join(entry.exthost, "codeium.windsurf");
      let files = [];
      try { files = fs.readdirSync(wind); } catch (_) { continue; }
      for (const file of files) {
        const m = file.match(/^MCP (.+)\.log$/);
        if (!m) continue;
        const ev = lastExtEvent(path.join(wind, file));
        if (!ev) continue;
        if (entry.cutoff && ev.ts != null && ev.ts < entry.cutoff) continue;
        const prev = extByName[m[1]];
        if (!prev || (ev.ts || 0) >= (prev.ts || 0)) extByName[m[1]] = ev;
      }
      const a = parseAgentLog(path.join(wind, "Devin.log"), entry.cutoff, remoteNames);
      for (const [key, info] of Object.entries(a.initialized)) {
        const prev = agent.initialized[key];
        if (!prev || info.ts >= prev.ts) agent.initialized[key] = info;
      }
      Object.assign(agent.toolCounts, a.toolCounts);
      agent.urlFailures.push(...a.urlFailures);
    }
    const byKey = {};
    const keyOf = (server) => opts.keyOf
      ? opts.keyOf(server) : `${server.configPath}|${server.name}`;
    for (const server of opts.servers || []) {
      const entry = { state: "unknown", via: null, version: null, tools: null, error: null };
      const url = server.kind === "McpRemote" && server.command
        ? String(server.command).replace(/\/+$/, "") : null;
      if (agent.initialized[server.name]) {
        entry.state = "connected";
        entry.via = "agent";
        entry.version = agent.initialized[server.name].version;
        entry.tools = server.name in agent.toolCounts ? agent.toolCounts[server.name] : null;
      } else if (url && agent.urlFailures.some((f) => f.url === url)) {
        entry.state = "error";
        entry.via = "agent";
        entry.error = "接続失敗";
      } else if (extByName[server.name]) {
        const ev = extByName[server.name];
        entry.state = ev.state;
        entry.via = "exthost";
        entry.error = ev.error;
      }
      byKey[keyOf(server)] = entry;
    }
    return { approx: found.approx, byKey };
  } catch (_) {
    return empty;
  }
}

// fs.watch 用: 有効セッション内の codeium.windsurf ディレクトリ一覧
function windsurfDirs(logsRoot) {
  const sessionDir = newestSessionDir(logsRoot || path.join(process.env.APPDATA || "", "Devin - Next", "logs"));
  if (!sessionDir) return [];
  try {
    return fs.readdirSync(sessionDir, { withFileTypes: true })
      .filter((w) => w.isDirectory() && w.name.startsWith("window"))
      .map((w) => path.join(sessionDir, w.name, "exthost", "codeium.windsurf"))
      .filter((dir) => fs.existsSync(dir));
  } catch (_) {
    return [];
  }
}

module.exports = { collect, windsurfDirs };
