// Devin Tool Manager — VS Code 拡張エントリ。
// プライマリサイドバー (viewsContainers.dtm / views.dtm.manager) に
// WebviewView を提供し、dtm.exe --json を呼び出して状態表示・操作を中継する。
// ブラウザ側 JS は media/webview.js の外部ファイル (nonce CSP)。HTML に
// スクリプトを直埋めしないためテンプレートの二重エスケープ問題は発生しない。

const vscode = require("vscode");
const cp = require("child_process");
const path = require("path");
const fs = require("fs");
const catalog = require("./media/catalog");
const livestate = require("./livestate");

const VIEW_ID = "dtm.manager";

/** dtm.exe の探索順:
 * 1) 設定 dtm.path
 * 2) 設定 dtm.root + dtm.exe
 * 3) DTM_ROOT env + dtm.exe
 * 4) %LOCALAPPDATA%\Programs\Devin Next\DevinToolManager\dtm.exe
 * 5) PATH 上の dtm
 */
function findDtm() {
  const cfg = vscode.workspace.getConfiguration("dtm");
  const cands = [];
  if (cfg.get("path")) cands.push(cfg.get("path"));
  if (cfg.get("root")) cands.push(path.join(cfg.get("root"), "dtm.exe"));
  if (process.env.DTM_ROOT) cands.push(path.join(process.env.DTM_ROOT, "dtm.exe"));
  cands.push(
    path.join(
      process.env.LOCALAPPDATA || "",
      "Programs",
      "Devin Next",
      "DevinToolManager",
      "dtm.exe"
    )
  );
  for (const c of cands) {
    if (c && fs.existsSync(c)) return c;
  }
  return "dtm"; // PATH 依存
}

function dtmArgs(extra) {
  const cfg = vscode.workspace.getConfiguration("dtm");
  const args = ["--json"];
  const root = cfg.get("root") || process.env.DTM_ROOT;
  if (root) args.push("--root", root);
  return args.concat(extra);
}

function runDtm(args, timeoutMs = 30000) {
  return new Promise((resolve) => {
    const exe = findDtm();
    cp.execFile(
      exe,
      dtmArgs(args),
      { timeout: timeoutMs, windowsHide: true, maxBuffer: 8 * 1024 * 1024 },
      (err, stdout, stderr) => {
        const text = (stdout || "").trim();
        let value = text;
        try {
          value = text ? JSON.parse(text) : null;
        } catch (_) { /* 非JSON出力はテキストのまま */ }
        resolve({ ok: !err, value, stderr, code: err ? err.code : 0 });
      }
    );
  });
}

// ---------- 状態・操作 ----------

let lastState = { installations: [], scan: [], managed: [], log: [], busy: [] };
const inFlight = new Set(); // "cmd:id" — 処理中操作の重複抑制用
let liveMarker = ""; // このウィンドウの exthost を特定するための marker
let probeCache = null;          // dtm probe-cache (mcp-probe-cache.json) の内容
const probeTried = new Set();   // この拡張セッションで試行したサーバー (重複プローブ防止)
const probeQueue = [];
let probeRunning = false;

// Devin ログ解析による実行時状態 (接続・稼働版・実ツール数)
// ツール数は新形式ログに無いため dtm probe-mcp キャッシュを併用する
function computeLive() {
  return livestate.collect({
    marker: liveMarker,
    servers: lastState.scan || [],
    keyOf: (s) => catalog.key(s.configPath, s.name),
    probeCache,
  });
}

// tools が未取得の stdio MCP を順にプローブする (1回/セッション/サーバー。
// 手動「再読み込み」では probeTried を消して再試行させる)
function scheduleProbes(provider) {
  const live = lastState.mcpLive;
  if (!live || !live.byKey || lastState.dtmError) return;
  for (const s of lastState.scan || []) {
    if (!s || s.disabled || s.stale || s.kind === "McpRemote") continue;
    const key = catalog.key(s.configPath, s.name);
    const entry = live.byKey[key];
    if (!entry || entry.tools != null || probeTried.has(key)) continue;
    probeTried.add(key);
    probeQueue.push({ name: s.name, configPath: s.configPath });
  }
  drainProbeQueue(provider);
}

async function drainProbeQueue(provider) {
  if (probeRunning) return;
  probeRunning = true;
  try {
    while (probeQueue.length) {
      const t = probeQueue.shift();
      const args = ["probe-mcp", t.name, "--timeout", "25"];
      if (t.configPath) args.push("--config", t.configPath);
      const r = await runDtm(args, 45000);
      // 成否に関わらずキャッシュを再読込 (失敗も error 付きで記録される)
      if (r.ok) {
        const c = await runDtm(["probe-cache"]);
        if (c.ok && c.value && Array.isArray(c.value.entries)) {
          probeCache = c.value;
        }
        lastState.mcpLive = computeLive();
        provider.post(lastState);
      }
    }
  } finally {
    probeRunning = false;
  }
}

let refreshTask = null;
let refreshAgain = false;
function refresh(provider) {
  if (refreshTask) {
    refreshAgain = true;
    return refreshTask;
  }
  refreshTask = (async () => {
    do {
      refreshAgain = false;
      await refreshState(provider);
    } while (refreshAgain);
  })().finally(() => { refreshTask = null; });
  return refreshTask;
}

// 起動時適用完了の通知 (テストVMでは showInformationMessage が無いため隔離)
function notifyApplied(inst) {
  try {
    vscode.window.showInformationMessage(
      `${inst.name} を ${inst.current} へ更新しました (起動時適用)`
    );
  } catch (_) { /* noop */ }
}

async function refreshState(provider) {
  const cfg = vscode.workspace.getConfiguration("dtm");
  const roots = [...new Set((cfg.get("projectDirs") || [])
    .concat((vscode.workspace.workspaceFolders || []).map((folder) => folder.uri.fsPath)))];
  const projects = roots.flatMap((p) => ["--project", p]);
  const [list, scan, managed, logs, probe] = await Promise.all([
    runDtm(["list"]),
    runDtm(["scan", "--diagnostics", ...projects]),
    runDtm(["managed"]),
    runDtm(["logs", "--tail", "30"]),
    runDtm(["probe-cache"]),
  ]);
  const scanErrors = scan.ok && scan.value && Array.isArray(scan.value.errors)
    ? scan.value.errors : [{ message: scan.stderr || "MCP設定スキャンに失敗しました" }];
  const scanned = scan.ok && scan.value && Array.isArray(scan.value.servers) ? scan.value.servers : [];
  const retained = (lastState.scan || []).filter((entry) => scanErrors.some((error) =>
    !error.configPath || catalog.pathKey(error.configPath) === catalog.pathKey(entry.configPath)))
    .map((entry) => ({ ...entry, stale: true }));
  // 起動時適用完了の検出: 前回「準備済み」だった版が今回「現在版」に変わった。
  // 初回ロード (prev 空) では通知しない — 過去の適用を誤報しないため。
  const nextInst = list.ok && Array.isArray(list.value) ? list.value : lastState.installations;
  if ((lastState.installations || []).length) {
    const prevById = new Map(lastState.installations.map((i) => [i.id, i]));
    for (const cur of nextInst) {
      const p = prevById.get(cur.id);
      if (p && p.prepared && p.prepared === cur.current && p.current !== cur.current) {
        notifyApplied(cur);
      }
    }
  }
  lastState = {
    installations: nextInst,
    scan: scanned.concat(retained),
    scanErrors,
    scanRoots: roots,
    catalogEntries: provider.context ? provider.context.globalState.get("catalogEntries", []) : lastState.catalogEntries || [],
    managed: managed.ok && Array.isArray(managed.value) ? managed.value : lastState.managed,
    log: typeof logs.value === "string" ? logs.value.split("\n") : [],
    dtmError: list.ok ? null : (list.stderr || "dtm.exe が見つかりません"),
    dtmPath: findDtm(),
    busy: [...inFlight],
    updatedAt: new Date().toISOString(),
  };
  if (probe.ok && probe.value && Array.isArray(probe.value.entries)) {
    probeCache = probe.value;
  }
  lastState.mcpLive = computeLive();
  provider.post(lastState);
  scheduleProbes(provider);
}

/** webview のインラインフォームから送られた fields を CLI 引数へ変換する */
function argsFromFields(msg) {
  const f = msg.fields || {};
  switch (msg.cmd) {
    case "set-source":
      return f.repo
        ? ["set-source", msg.id, f.repo, "--asset", f.asset || "*.zip"]
        : null;
    case "set-git-source": {
      if (!f.repo || !f.workDir || !f.out) return null;
      // ビルドコマンドは ";" または改行区切りで複数指定 → --build を繰り返す
      const cmds = (f.build || "npm ci; npm run build")
        .split(/[;\n]/)
        .map((s) => s.trim())
        .filter(Boolean);
      const args = [
        "set-git-source", msg.id,
        "--repo", f.repo,
        "--branch", f.branch || "main",
        "--work-dir", f.workDir,
        "--out", f.out,
      ];
      for (const c of cmds) args.push("--build", c);
      return args;
    }
    case "set-npm-source": {
      if (!f.package) return null;
      const args = ["set-npm-source", msg.id, f.package];
      if (f.registry) args.push("--registry", f.registry);
      // 世代内インストールコマンドは ";" または改行区切りで複数指定
      for (const c of (f.install || "").split(/[;\n]/).map((s) => s.trim()).filter(Boolean)) {
        args.push("--install", c);
      }
      return args;
    }
    case "import":
      return f.file && f.version
        ? ["import", msg.id, f.file, "--version", f.version]
        : null;
    case "preview":
      return f.file ? ["preview", msg.id, f.file] : null;
    case "pin":
      return f.version ? ["pin", msg.id, f.version] : null;
    case "rollback":
      return f.gen ? ["rollback", msg.id, f.gen] : ["rollback", msg.id];
    case "register-cli":
      return f.name && f.spec ? ["add-cli", f.name, f.spec] : null;
    default:
      return null;
  }
}

async function action(provider, msg) {
  const key = `${msg.cmd}:${msg.id || msg.name || "-"}`;
  if (inFlight.has(key)) return; // 重複操作抑制
  const mutations = ["prepare", "apply", "cancel", "rollback", "pm-sync", "set-source", "set-git-source", "set-npm-source", "import", "pin", "unpin", "manage", "register-mcp"];
  const rows = catalog.build(lastState);
  if (msg.cmd === "set-catalog") {
    const row = rows.managed.concat(rows.unmanaged).find((entry) => entry.scan && !entry.scan.stale
      && catalog.key(entry.configPath, entry.name) === catalog.key(msg.config, msg.id));
    if (!row || row.scan.registry || !provider.context || (lastState.scanErrors || []).length) return;
    const entries = (lastState.catalogEntries || []).filter((entry) =>
      catalog.key(entry.configPath, entry.serverName) !== row.key);
    if (row.catalogSource !== "user") entries.push({ configPath: row.configPath, serverName: row.name });
    await provider.context.globalState.update("catalogEntries", entries);
    provider.view && provider.view.webview.postMessage({
      type: "collapseEntry", key: row.key, installationId: row.inst && row.inst.id,
    });
    await refresh(provider);
    return;
  }
  let toggleRow;
  if (msg.cmd === "set-mcp-enabled") {
    toggleRow = rows.managed.concat(rows.unmanaged).find((row) => row.scan && !row.scan.stale
      && catalog.key(row.configPath, row.name) === catalog.key(msg.config, msg.id));
    if (!toggleRow || typeof msg.enabled !== "boolean" || toggleRow.scan.disabled !== msg.enabled
      || toggleRow.scan.command === "設定形式が不正" || (lastState.scanErrors || []).length) {
      provider.view && provider.view.webview.postMessage({ type: "actionResult", cmd: msg.cmd,
        id: msg.ekey || msg.id, ok: false, message: "有効状態または設定を再読み込みしてから操作してください" });
      return;
    }
  }
  const protectedEntry = rows.unmanaged.find((row) => /[BC]/.test(row.category)
    && (row.inst && row.inst.id === msg.id || row.name === msg.id && catalog.pathKey(row.configPath) === catalog.pathKey(msg.config)));
  if ((mutations.includes(msg.cmd) || msg.cmd === "check") && protectedEntry
    || mutations.includes(msg.cmd) && (lastState.scanErrors || []).length) {
    provider.view && provider.view.webview.postMessage({
      type: "actionResult", cmd: msg.cmd, id: msg.ekey || msg.id,
      ok: false, message: protectedEntry ? "このMCPの更新・配置変更はDTM対象外です" : "設定読み取りエラーを解消してから操作してください",
    });
    return;
  }
  let args;
  const map = {
    check: ["check", msg.id],
    "check-all": ["check", "--all"],
    prepare: ["prepare", msg.id],
    // 実引数は未使用 — 後段の特別分岐で対象IDごとに prepare を逐次実行する
    "prepare-all": ["prepare", "-"],
    apply: ["apply", msg.id],
    cancel: ["cancel", msg.id],
    unpin: ["pin", msg.id, "--clear"],
    history: ["history", msg.id],
    generations: ["generations", msg.id],
    cleanup: ["cleanup", msg.id],
    diagnose: ["diagnose"],
    "register-mcp": ["register-mcp", msg.id],
    manage: ["manage", msg.id],
    unmanage: ["unmanage", msg.id],
    remove: ["remove", msg.id],
    "pm-sync": ["pm-sync", msg.id],
    "set-mcp-enabled": ["set-mcp-enabled", msg.id, "--enabled", String(msg.enabled),
      "--expected-disabled", String(toggleRow && toggleRow.scan.disabled)],
  };
  if (msg.cmd in map) {
    args = map[msg.cmd];
    // プロジェクト/ローカルスコープの設定パスを CLI へ渡す
    if (msg.config) args = args.concat(["--config", msg.config]);
  } else {
    args = argsFromFields(msg);
  }
  if (!args) return;
  // 登録解除は registry・状態・世代を消すため実行前にモーダル確認する
  // (実ファイルは削除されない。管理化済み MCP は dtm 側が拒否する)
  if (msg.cmd === "remove") {
    try {
      const target = (lastState.installations || []).find((i) => i.id === msg.id);
      const label = target ? target.name : msg.id;
      const ok = await vscode.window.showWarningMessage(
        `'${label}' を DTM から登録解除します。登録・状態・保存世代を削除します (ツール実体のファイルは削除されません)。`,
        { modal: true }, "登録解除");
      if (ok !== "登録解除") return;
    } catch (_) { /* テストVMに showWarningMessage が無い場合は確認をスキップ */ }
  }
  const longOps = ["prepare", "prepare-all", "check", "check-all", "rollback", "import", "pm-sync"];
  inFlight.add(key);
  provider.post({ busy: [...inFlight] });
  // 長時間操作の間は list のみ定期再取得して進捗 (bytes/phase) を表示する。
  // scan/managed/logs は重いので更新しない。list は読み取り専用で
  // ロック競合しない (heal は try_acquire のみ)。
  // (setInterval/clearInterval はテストのVM環境で未定義になり得るため typeof でゲート)
  const timers = typeof setInterval === "function" && typeof clearInterval === "function";
  const progressTimer = timers && longOps.includes(msg.cmd)
    ? setInterval(() => {
        runDtm(["list"]).then((r) => {
          if (r.ok && Array.isArray(r.value)) {
            lastState.installations = r.value;
            provider.post(lastState);
          }
        });
      }, 2500)
    : null;
  try {
    let r;
    if (msg.cmd === "check-all") {
      const results = await Promise.all(catalog.updateIds(lastState).map((id) => runDtm(["check", id], 300000)));
      r = { ok: results.every((result) => result.ok), value: results.map((result) =>
        typeof result.value === "string" ? result.value : result.stderr || JSON.stringify(result.value)).join("\n") || "更新確認可能な対象はありません" };
    } else if (msg.cmd === "prepare-all") {
      // 「更新あり」状態の非委譲インストールを順に準備する (逐次: ロックと帯域の競合を避ける)
      const byId = new Map((lastState.installations || []).map((i) => [i.id, i]));
      const targets = (lastState.installations || [])
        .filter((i) => (i.status || "").startsWith("更新あり") && !(i.provider || "").startsWith("pkg:"))
        .map((i) => i.id);
      const lines = [];
      let okCount = 0;
      for (const id of targets) {
        const pr = await runDtm(["prepare", id], 900000);
        const label = (byId.get(id) || {}).name || id;
        const text = (pr.value && (typeof pr.value === "string" ? pr.value : pr.value.message || JSON.stringify(pr.value))) || pr.stderr || "";
        lines.push((pr.ok ? "✓ " : "✗ ") + label + ": " + text);
        if (pr.ok) okCount++;
      }
      r = {
        ok: targets.length > 0 && okCount === targets.length,
        value: targets.length
          ? okCount + "/" + targets.length + " 件準備完了\n" + lines.join("\n")
          : "準備可能な更新はありません (先に「全件 更新確認」を実行してください)",
      };
    } else {
      // prepare は大容量アセットのDLを伴い得るため個別に長い上限
      // (dtm側の内部上限780sを先に切らせ、kill由来の状態残留を防ぐ)
      const timeout = msg.cmd === "prepare" ? 900000 : longOps.includes(msg.cmd) ? 300000 : 60000;
      r = await runDtm(args, timeout);
    }
    // 診断バンドルは保存したzipをエクスプローラーで表示する
    if (msg.cmd === "diagnose" && r.ok && r.value && r.value.path) {
      try { await vscode.commands.executeCommand("revealFileInOS", vscode.Uri.file(String(r.value.path))); } catch (_) { /* 表示失敗は無視 */ }
    }
    provider.view && provider.view.webview.postMessage({
      type: "actionResult",
      cmd: msg.cmd,
      // ekey は scan 行のエラー表示キー (行内保持の照合に使用)
      id: msg.ekey || msg.id || msg.name,
      ok: r.ok,
      message:
        (r.value && (typeof r.value === "string" ? r.value : r.value.message || JSON.stringify(r.value))) ||
        r.stderr || "",
    });
    await refresh(provider);
  } finally {
    if (progressTimer) clearInterval(progressTimer);
    inFlight.delete(key);
    provider.post({ busy: [...inFlight] });
  }
}

// フッターの Devin 本体操作 (Webview からの委譲)。
// いずれも Devin/ワークベンチが提供する公開コマンドの実行のみ。
async function uiAction(action) {
  try {
    if (action === "new-window") {
      await vscode.commands.executeCommand("workbench.action.newWindow");
    } else if (action === "open-rules") {
      await vscode.commands.executeCommand("devin.openCustomizationsTab", { tabId: "rules" });
    } else if (action === "open-skills") {
      await vscode.commands.executeCommand("devin.openCustomizationsTab", { tabId: "skills" });
    } else if (action === "open-mcps") {
      await vscode.commands.executeCommand("devin.openCustomizationsTab", { tabId: "mcps" });
    } else if (action === "open-mcp-config") {
      // ユーザースコープの MCP 設定をエディタで開く (変更はDevin側の編集責任)
      const cfg = process.env.APPDATA
        ? path.join(process.env.APPDATA, "devin", "mcp_config.json")
        : null;
      if (!cfg || !fs.existsSync(cfg)) {
        vscode.window.showWarningMessage("MCP設定ファイルが見つかりません");
        return;
      }
      await vscode.commands.executeCommand("vscode.open", vscode.Uri.file(cfg));
    } else if (action === "open-dtm-log") {
      // dtm.exe の隣接または設定の root から dtm.log を開く
      const exe = findDtm();
      const root = path.isAbsolute(exe)
        ? path.dirname(exe)
        : (vscode.workspace.getConfiguration("dtm").get("root") || process.env.DTM_ROOT || "");
      const log = root ? path.join(root, "state", "logs", "dtm.log") : null;
      if (!log || !fs.existsSync(log)) {
        vscode.window.showWarningMessage("DTMログが見つかりません");
        return;
      }
      await vscode.commands.executeCommand("vscode.open", vscode.Uri.file(log));
    } else if (action === "open-folder") {
      const folder = vscode.workspace.workspaceFolders && vscode.workspace.workspaceFolders[0];
      if (!folder) {
        vscode.window.showWarningMessage("開いているワークスペースフォルダがありません");
        return;
      }
      await vscode.commands.executeCommand("revealFileInOS", folder.uri);
    }
  } catch (e) {
    vscode.window.showErrorMessage("操作に失敗しました: " + (e && e.message ? e.message : e));
  }
}

// ---------- WebviewView provider ----------

function nonce() {
  return [...Array(32)].map(() => ((Math.random() * 36) | 0).toString(36)).join("");
}

class DtmViewProvider {
  constructor(context) {
    this.context = context;
    this.view = null;
  }

  post(partial) {
    if (this.view) {
      this.view.webview.postMessage({ type: "state", payload: partial });
    }
  }

  resolveWebviewView(view) {
    this.view = view;
    const mediaRoot = vscode.Uri.joinPath(this.context.extensionUri, "media");
    view.webview.options = {
      enableScripts: true,
      localResourceRoots: [mediaRoot],
    };
    const js = view.webview.asWebviewUri(vscode.Uri.joinPath(mediaRoot, "webview.js"));
    const css = view.webview.asWebviewUri(vscode.Uri.joinPath(mediaRoot, "webview.css"));
    const catalogJs = view.webview.asWebviewUri(vscode.Uri.joinPath(mediaRoot, "catalog.js"));
    const n = nonce();
    view.webview.html = `<!DOCTYPE html>
<html lang="ja">
<head>
<meta charset="utf-8">
<meta http-equiv="Content-Security-Policy"
      content="default-src 'none'; style-src ${view.webview.cspSource}; script-src 'nonce-${n}';">
<link rel="stylesheet" href="${css}">
</head>
<body>
  <div id="app"></div>
  <script nonce="${n}" src="${catalogJs}"></script>
  <script nonce="${n}" src="${js}"></script>
</body></html>`;
    view.webview.onDidReceiveMessage(async (msg) => {
      if (msg.type === "ready" || msg.type === "refresh") {
        // 手動再読み込みではプローブ再試行を許可 (失敗したサーバーも再取得)
        if (msg.type === "refresh") probeTried.clear();
        await refresh(this);
      } else if (msg.type === "reload") {
        // ウィンドウ再読み込み = 拡張ホスト再起動 + MCP サーバー再起動。
        // 完全なアプリ再起動は VS Code API に無いため確認のうえ reloadWindow を使う
        const ok = await vscode.window.showWarningMessage(
          "ウィンドウを再読み込みしますか？ (拡張機能と MCP サーバーが再起動します)",
          { modal: true },
          "再読み込み"
        );
        if (ok === "再読み込み") {
          await vscode.commands.executeCommand("workbench.action.reloadWindow");
        }
      } else if (msg.type === "restart") {
        // 完全再起動: workbench.action.quit で全ウィンドウ終了。
        // 拡張ホストも道連れになるため、再起動の実実行は
        // 切り離された `dtm restart-devin` ヘルパーへ委譲する
        const ok = await vscode.window.showWarningMessage(
          "Devin の全ウィンドウを終了して再起動します。\n実行中・思考中のチャットは中断されます。よろしいですか？",
          { modal: true },
          "再起動"
        );
        if (ok === "再起動") {
          // 先に終了検知ヘルパーを起動してから quit する
          const r = await runDtm(["restart-devin", "--exe", process.execPath]);
          if (r.ok) {
            await vscode.commands.executeCommand("workbench.action.quit");
          } else {
            vscode.window.showErrorMessage(
              "再起動ヘルパー起動失敗: " +
                ((r.value && r.value.error) || r.stderr || "不明なエラー")
            );
          }
        }
      } else if (msg.type === "browse") {
        // フォームの参照ボタン: ファイル/フォルダ選択ダイアログ
        const isFolder = msg.pick === "folder";
        const picked = await vscode.window.showOpenDialog({
          canSelectFiles: !isFolder,
          canSelectFolders: isFolder,
          canSelectMany: false,
          title: msg.title || "選択",
          openLabel: "選択",
        });
        const v = picked && picked[0] ? picked[0].fsPath : null;
        if (v && this.view) {
          this.view.webview.postMessage({
            type: "browseResult",
            field: msg.field,
            value: v,
          });
        }
      } else if (msg.type === "ui") {
        await uiAction(msg.action);
      } else if (msg.type === "action") {
        await action(this, msg);
      }
    });
    view.onDidDispose(() => (this.view = null));
  }
}

async function probeAcpState(commands) {
  const command = "workbench.action.getAcpState";
  if (!commands.includes(command)) return { status: "commandNotAvailable" };
  if (vscode.version !== "1.126.0-next") return { status: "unsupportedVersion" };
  try {
    const snapshot = await vscode.commands.executeCommand(command);
    const root = snapshot && Object.getOwnPropertyDescriptor(snapshot, "reduxStore")?.value;
    if (!root || typeof root !== "object" || Array.isArray(root)) return { status: "notReady" };
    const slices = Object.entries(Object.getOwnPropertyDescriptors(root)).slice(0, 32).map(([name, descriptor]) => {
      const value = "value" in descriptor ? descriptor.value : null;
      const fields = value && typeof value === "object" && !Array.isArray(value)
        ? Object.entries(Object.getOwnPropertyDescriptors(value)).slice(0, 64).map(([field, entry]) => ({
            name: field, type: "value" in entry ? typeof entry.value : "accessor",
          })) : [];
      return { name, fields };
    });
    const interesting = /mcp|tool|serverstate|listserver/i;
    const nestedMatches = {};
    for (const [sliceName, descriptor] of Object.entries(Object.getOwnPropertyDescriptors(root))) {
      const value = "value" in descriptor ? descriptor.value : null;
      if (!value || typeof value !== "object" || Array.isArray(value)) continue;
      for (const [field, entry] of Object.entries(Object.getOwnPropertyDescriptors(value)).slice(0, 64)) {
        const inner = "value" in entry ? entry.value : null;
        if (!inner || typeof inner !== "object" || Array.isArray(inner)) continue;
        const matches = Object.getOwnPropertyNames(inner).filter((key) => interesting.test(key)).slice(0, 24);
        if (matches.length) nestedMatches[`${sliceName}.${field}`] = matches;
      }
    }
    return { status: "schemaAvailable", slices, nestedMatches };
  } catch (_) {
    return { status: "unavailable" };
  }
}

async function probeAcpMcp() {
  if (vscode.version !== "1.126.0-next") return { status: "unsupportedVersion" };
  const descriptor = Object.getOwnPropertyDescriptor(vscode, "windsurfAcp");
  const acp = descriptor && "value" in descriptor ? descriptor.value : null;
  if (!acp || typeof acp.handleClientRequest !== "function") return { status: "notAvailable" };
  const send = async (connection, method, params) => {
    try {
      const response = await acp.handleClientRequest(connection, { method, params });
      return { ok: true, keys: response && typeof response === "object" ? Object.keys(response).slice(0, 12) : typeof response, response };
    } catch (error) {
      return { ok: false, message: String(error && (error.message || error)).slice(0, 200) };
    }
  };
  const report = { status: "ok" };
  const dirs = (vscode.workspace.workspaceFolders || []).map((folder) => folder.uri.fsPath);
  const params = dirs.length ? { workspaceDirs: dirs } : {};
  for (const connection of ["devin-cli", "cascade"]) {
    const result = await send(connection, "_cognition.ai/mcp/listServers", params);
    const entry = { ok: result.ok };
    if (result.ok) {
      entry.keys = result.keys;
      const servers = result.response && Array.isArray(result.response.servers) ? result.response.servers : [];
      entry.serverCount = servers.length;
      entry.servers = servers.slice(0, 64).map((server) => ({
        serverId: server && server.serverId,
        connectionStatus: server && server.connectionStatus,
        disabled: !!(server && server.disabled),
        disabledToolsCount: Array.isArray(server && server.disabledTools) ? server.disabledTools.length : null,
        error: server && server.error ? String(server.error).slice(0, 160) : null,
        fields: server && typeof server === "object" ? Object.keys(server).slice(0, 16) : [],
      }));
    } else {
      entry.message = result.message;
    }
    report[connection] = entry;
  }
  const connected = (report["devin-cli"].servers || []).find((server) => server.connectionStatus === "connected");
  if (connected) {
    for (const toolParams of [{ serverId: connected.serverId }, { serverName: connected.serverId }]) {
      const result = await send("devin-cli", "_cognition.ai/mcp/listTools", toolParams);
      if (result.ok) {
        const tools = result.response && (result.response.tools || result.response.items);
        report.listToolsProbe = { params: Object.keys(toolParams)[0], count: Array.isArray(tools) ? tools.length : typeof result.response, keys: result.keys };
        break;
      }
      (report.listToolsErrors ||= []).push({ params: Object.keys(toolParams)[0], message: result.message });
    }
  }
  return report;
}

function probeLm() {
  const safe = (fn) => { try { return fn(); } catch (_) { return { status: "error" }; } };
  const report = {};
  report.mcpServerDefinitions = safe(() => {
    const defs = vscode.lm.mcpServerDefinitions;
    return Array.isArray(defs) ? defs.slice(0, 64).map((def) => ({
      fields: Object.getOwnPropertyNames(def).slice(0, 12),
      id: typeof def.id === "string" ? def.id : typeof def.id,
      label: typeof def.label === "string" ? def.label : typeof def.label,
    })) : typeof defs;
  });
  report.tools = safe(() => {
    const tools = vscode.lm.tools;
    if (!Array.isArray(tools)) return typeof tools;
    return {
      count: tools.length,
      entries: tools.slice(0, 500).map((tool) => ({
        name: typeof tool.name === "string" ? tool.name : typeof tool.name,
        tags: Array.isArray(tool.tags) ? tool.tags.slice(0, 8) : typeof tool.tags,
        source: tool.source && typeof tool.source === "object" ? {
          label: typeof tool.source.label === "string" ? tool.source.label : typeof tool.source.label,
          constructorName: tool.source.constructor && tool.source.constructor.name,
        } : typeof tool.source,
      })),
    };
  });
  const signatures = {};
  const lmApi = Object.getOwnPropertyDescriptors(vscode.lm || {});
  const targets = {
    "lm.invokeTool": "value" in (lmApi.invokeTool || {}) ? lmApi.invokeTool.value : undefined,
    "lm.startMcpGateway": "value" in (lmApi.startMcpGateway || {}) ? lmApi.startMcpGateway.value : undefined,
  };
  const acpDescriptor = Object.getOwnPropertyDescriptor(vscode, "windsurfAcp");
  const acp = acpDescriptor && "value" in acpDescriptor ? acpDescriptor.value : null;
  for (const [name, member] of Object.entries(Object.getOwnPropertyDescriptors(acp || {}))) {
    if ("value" in member && /request|connection|connector/i.test(name)) targets[`windsurfAcp.${name}`] = member.value;
  }
  for (const [name, member] of Object.entries(targets)) {
    signatures[name] = safe(() => typeof member === "function"
      ? Function.prototype.toString.call(member).slice(0, 200) : typeof member);
  }
  report.signatures = signatures;
  return report;
}

async function diagnoseMcp(output) {
  const describe = (value) => value && (typeof value === "object" || typeof value === "function")
    ? Object.entries(Object.getOwnPropertyDescriptors(value)).map(([name, descriptor]) => ({
        name, type: "value" in descriptor ? typeof descriptor.value : "accessor",
      })).sort((a, b) => a.name.localeCompare(b.name))
    : [];
  const api = Object.getOwnPropertyDescriptors(vscode);
  const namespaces = Object.keys(api).filter((name) => /mcp|acp|^lm$/i.test(name));
  const native = vscode.extensions.getExtension("codeium.windsurf");
  const commands = await vscode.commands.getCommands(false);
  const acpStateProbe = await probeAcpState(commands);
  let clientProbe = { status: "notAvailable" };
  if (native && native.isActive) {
    const getter = Object.getOwnPropertyDescriptor(native.exports || {}, "devClient");
    if (getter && typeof getter.value === "function"
      && /^\(\)\s*=>\s*[\w$]+\.LanguageServerClient\.getInstance\(\)\.devClient$/.test(Function.prototype.toString.call(getter.value))) {
      try {
        const client = getter.value();
        clientProbe = { status: client ? "available" : "notReady", members: describe(client),
          prototypeMembers: client ? describe(Object.getPrototypeOf(client)) : [] };
      } catch (_) {
        clientProbe = { status: "notReady" };
      }
    }
  }
  let liveProbe = { status: "error" };
  try {
    const live = computeLive();
    const states = {};
    for (const [key, entry] of Object.entries(live.byKey)) {
      states[key.split("|").pop()] = { state: entry.state, via: entry.via, tools: entry.tools };
    }
    liveProbe = { approx: live.approx, states };
  } catch (_) { /* 診断の失敗は本体に影響させない */ }
  const report = {
    editorVersion: vscode.version,
    clientProbe,
    acpStateProbe,
    acpMcpProbe: await probeAcpMcp(),
    lmProbe: probeLm(),
    liveProbe,
    namespaces: namespaces.map((name) => ({
      name, members: "value" in api[name] ? describe(api[name].value) : [],
    })),
    nativeExtension: native ? {
      id: native.id, active: native.isActive,
      exports: native.isActive ? describe(native.exports) : [],
    } : null,
    commands: commands
      .filter((name) => /mcp|acp|languageServer|client/i.test(name) && !name.startsWith("dtm."))
      .sort(),
  };
  output.clear();
  output.appendLine("DTM MCP連携診断：API・コマンド名と状態の構造のみ。設定値・チャット本文は出力しません。MCPの起動・設定変更は行っていません。");
  output.appendLine(JSON.stringify(report, null, 2));
  output.show(true);
  return report;
}

function activate(context) {
  const provider = new DtmViewProvider(context);
  const diagnostics = vscode.window.createOutputChannel("DTM MCP連携診断");
  // このウィンドウの exthost ディレクトリを logs/<session>/window*/ から特定する
  // ための目印。診断出力にだけ書き込まれ、設定や状態には触れない。
  liveMarker = "dtm-window-" + require("crypto").randomUUID();
  diagnostics.appendLine("[marker] " + liveMarker);
  let watchers = [];
  let timer;
  let liveTimer;
  // MCP ログの更新で実行時状態だけを軽く再計算 (dtm.exe は呼ばない)。
  // ツール呼び出し時にもログが書かれるため、内容が変わった時だけ送信する
  const scheduleLive = () => {
    clearTimeout(liveTimer);
    liveTimer = setTimeout(() => {
      const next = computeLive();
      if (JSON.stringify(next) !== JSON.stringify(lastState.mcpLive)) {
        lastState.mcpLive = next;
        provider.post({ mcpLive: next });
      }
    }, 1200);
  };
  const liveWatchers = [];
  const armLiveWatcher = () => {
    if (liveWatchers.length) return;
    for (const dir of livestate.windsurfDirs()) {
      try {
        liveWatchers.push(fs.watch(dir, () => scheduleLive()));
      } catch (_) { /* 個別失敗は他へ影響させない */ }
    }
  };
  // セッションディレクトリが未作成の間は定期的に試行する
  const armTimer = setInterval(armLiveWatcher, 15000);
  armLiveWatcher();
  const scheduleRefresh = () => {
    clearTimeout(timer);
    timer = setTimeout(() => refresh(provider), 200);
  };
  const watchConfigs = () => {
    watchers.forEach((watcher) => watcher.dispose());
    const cfg = vscode.workspace.getConfiguration("dtm");
    const roots = (cfg.get("projectDirs") || []).concat((vscode.workspace.workspaceFolders || []).map((folder) => folder.uri.fsPath));
    const patterns = [new vscode.RelativePattern(path.join(process.env.APPDATA || "", "devin"), "mcp_config.json")]
      .concat([...new Set(roots)].map((root) => new vscode.RelativePattern(root, ".devin/mcp_config*.json")));
    watchers = patterns.map((pattern) => {
      const watcher = vscode.workspace.createFileSystemWatcher(pattern);
      watcher.onDidChange(scheduleRefresh);
      watcher.onDidCreate(scheduleRefresh);
      watcher.onDidDelete(scheduleRefresh);
      return watcher;
    });
  };
  watchConfigs();
  context.subscriptions.push(
    { dispose() { clearTimeout(timer); clearTimeout(liveTimer); clearInterval(armTimer); watchers.forEach((watcher) => watcher.dispose()); liveWatchers.forEach((watcher) => watcher.close()); } },
    vscode.workspace.onDidChangeWorkspaceFolders(() => { watchConfigs(); scheduleRefresh(); }),
    vscode.workspace.onDidChangeConfiguration((event) => {
      if (event.affectsConfiguration("dtm")) { watchConfigs(); scheduleRefresh(); }
    }),
    diagnostics,
    vscode.commands.registerCommand("dtm.diagnoseMcp", () => diagnoseMcp(diagnostics)),
    vscode.window.registerWebviewViewProvider(VIEW_ID, provider),
    // サイドバーのビューへフォーカス (mcpname.focus 形式の標準コマンド)
    vscode.commands.registerCommand("dtm.open", () =>
      vscode.commands.executeCommand(`${VIEW_ID}.focus`)
    ),
    vscode.commands.registerCommand("dtm.refresh", () => refresh(provider))
  );
}

function deactivate() {}
module.exports = { activate, deactivate };
