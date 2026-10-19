// Devin の実行時 MCP 状態を「読み取り専用」で取得するモジュール。
// 公開 API では取得経路が無いため、Devin 自身が出力するログだけを解析する。
//
// 取得元:
//   <logs>/<session>/window<N>/exthost/codeium.windsurf/MCP <name>.log
//     … エディタ側 MCP ホストの接続イベント
//       「MCP server '<name>' connected successfully」→ 接続済み
//       「connection failed」→ 失敗 / 「Connecting to …」のみ→ 接続中
//   <logs>/<session>/window<N>/exthost/codeium.windsurf/Devin.log (旧形式)
//     … devin-cli エージェント側 MCP クライアント (mcp-go)
//       「Server '<key>' initialized successfully: <表示名> v<版>」→ 稼働版
//       直後の {"result":{"tools":[…]}} 応答 → 実ツール数
//       「All connection methods failed for <url>」→ URL 接続の失敗
//   <logs>/<session>/window<N>/exthost/output_logging_*/N-Devin ACP devin-cli.log (新形式 1.126+)
//     … ACP エージェント (chisel/toolbox) の MCP 接続イベント
//       「MCP server '<name>' connected successfully」→ 接続済み
//       「MCP server '<name>' connection failed: …」→ 失敗
//       「Connecting to/Starting … MCP server '<name>'」→ 接続中
//     ※ 新形式には tools/list 応答が無いためツール数は dtm probe-mcp 由来
//
// ウィンドウ特定: activate 時に診断チャネルへ書き込んだ marker を
//   output_logging_*/*DTM*.log から検索し、所在する window<N> を特定する。
//   marker が見つからない場合は全ウィンドウの最終イベントを併合 (approx=true)。

const fs = require("fs");
const path = require("path");

const LOG_TS = /^(\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2})\.\d+/;
const ACP_TS = /^(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2})(?:\.\d+)?Z/;
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
              out.dirs = [all[all.length - 1]];
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
  out.dirs = all;
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

function acpLineTs(line) {
  const m = line.match(ACP_TS);
  if (!m) return null;
  const t = Date.parse(m[1] + "Z");
  return Number.isFinite(t) ? t : null;
}

// 新形式 (1.126+) のエージェントログ: output_logging_*/N-Devin ACP devin-cli.log
// server="..." 属性と 'name' 表記の両方が同じ設定キー名を指す。
// tools/list 応答は含まれないためツール数は得られない (probe に委譲)。
// return { events: {name: {state, ts, error}} }
function parseAcpAgentLog(file, cutoff) {
  const events = {};
  const upd = (name, ev) => {
    const prev = events[name];
    if (!prev || (ev.ts || 0) >= (prev.ts || 0)) events[name] = ev;
  };
  for (const line of tailLines(file, AGENT_CAP)) {
    if (!line.includes("MCP")) continue;
    const ts = acpLineTs(line);
    if (ts == null || (cutoff && ts < cutoff)) continue;
    let m = line.match(/MCP server '([^']+)' connected successfully/);
    if (m) {
      upd(m[1], { state: "connected", ts });
      continue;
    }
    m = line.match(/MCP server '([^']+)' connection failed:?\s*(.*)/);
    if (m) {
      upd(m[1], { state: "error", ts, error: (m[2] || "").trim().slice(0, 160) });
      continue;
    }
    m = line.match(/Connecting to (?:streamable HTTP |legacy SSE |SSE )?MCP server '([^']+)'/);
    if (!m) m = line.match(/Starting stdio MCP server '([^']+)'/);
    if (m) upd(m[1], { state: "connecting", ts });
  }
  return events;
}

// output_logging_* ディレクトリ群から最新の devin-cli ACP ログを探す。
// ディレクトリは birth 昇順 (末尾が最新)。最新側から最初にファイルが
// 存在するものを返す (最新が空/クラッシュ時は直前を利用)。
function newestAcpLog(outputs) {
  for (let i = outputs.length - 1; i >= 0; i--) {
    let files = [];
    try { files = fs.readdirSync(outputs[i].dir); } catch (_) { continue; }
    for (const f of files) {
      if (/^\d+-Devin ACP devin-cli\.log$/.test(f)) {
        return path.join(outputs[i].dir, f);
      }
    }
  }
  return null;
}

// servers: [{ name, configPath, kind, command, disabled }]
// opts.probeCache: {entries: [{serverName, configPath, tools, serverVersion, error}]} (任意)
// return { approx, byKey: { "<configPath|name>": { state, via, version, tools, toolsVia, error } } }
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
    const agent = { initialized: {}, toolCounts: {}, urlFailures: [], events: {} };
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
      // 新形式 ACP エージェントログ (1.126+): 接続状態イベント (最新イベント優先)
      const acpLog = newestAcpLog(entry.outputs || []);
      if (acpLog) {
        for (const [key, ev] of Object.entries(parseAcpAgentLog(acpLog, entry.cutoff))) {
          const prev = agent.events[key];
          if (!prev || (ev.ts || 0) >= (prev.ts || 0)) agent.events[key] = ev;
        }
      }
    }
    const byKey = {};
    const keyOf = (server) => opts.keyOf
      ? opts.keyOf(server) : `${server.configPath}|${server.name}`;
    // dtm probe-mcp のキャッシュ (keyOf と同じ正規化で照合する)
    const probeByKey = {};
    for (const e of (opts.probeCache && opts.probeCache.entries) || []) {
      if (e.error == null && e.tools != null) {
        probeByKey[keyOf({ configPath: e.configPath, name: e.serverName })] = e;
      }
    }
    for (const server of opts.servers || []) {
      const entry = { state: "unknown", via: null, version: null, tools: null, toolsVia: null, error: null };
      const url = server.kind === "McpRemote" && server.command
        ? String(server.command).replace(/\/+$/, "") : null;
      if (agent.events[server.name]) {
        // 新形式 (1.126+): ACP エージェントの接続イベント (版情報は含まれない)
        const ev = agent.events[server.name];
        entry.state = ev.state;
        entry.via = "agent";
        entry.error = ev.error || null;
      } else if (agent.initialized[server.name]) {
        // 旧形式: 初期化成功 = 接続済み + 稼働版 + ツール数
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
      // ツール数: 旧形式の tools/list 応答が無い場合はプローブキャッシュを使う
      if (entry.tools == null) {
        const hit = probeByKey[keyOf(server)];
        if (hit) {
          entry.tools = hit.tools;
          entry.toolsVia = "probe";
          if (!entry.version && hit.serverVersion) entry.version = hit.serverVersion;
        }
      }
      byKey[keyOf(server)] = entry;
    }
    return { approx: found.approx, byKey };
  } catch (_) {
    return empty;
  }
}

// fs.watch 用: 有効セッション内の codeium.windsurf + output_logging_* ディレクトリ一覧
// (ACP エージェントログは output_logging_* 配下に書かれるため両方を監視する)
function windsurfDirs(logsRoot) {
  const sessionDir = newestSessionDir(logsRoot || path.join(process.env.APPDATA || "", "Devin - Next", "logs"));
  if (!sessionDir) return [];
  try {
    const out = [];
    for (const w of fs.readdirSync(sessionDir, { withFileTypes: true })) {
      if (!w.isDirectory() || !w.name.startsWith("window")) continue;
      const exthost = path.join(sessionDir, w.name, "exthost");
      const wind = path.join(exthost, "codeium.windsurf");
      if (fs.existsSync(wind)) out.push(wind);
      try {
        for (const d of fs.readdirSync(exthost, { withFileTypes: true })) {
          if (d.isDirectory() && d.name.startsWith("output_logging_")) {
            out.push(path.join(exthost, d.name));
          }
        }
      } catch (_) { /* ignore */ }
    }
    return out;
  } catch (_) {
    return [];
  }
}

module.exports = { collect, windsurfDirs };
