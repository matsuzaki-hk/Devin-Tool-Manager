const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

const source = fs.readFileSync(path.join(__dirname, "extension", "extension.js"), "utf8");

function load(native, runtime) {
  const commands = new Map();
  const output = { lines: [], clear() { this.lines = []; }, appendLine(line) { this.lines.push(line); }, show() {}, dispose() {} };
  const vscode = {
    version: runtime && runtime.editorVersion || "test",
    RelativePattern: class { constructor(base, pattern) { this.base = base; this.pattern = pattern; } },
    workspace: {
      workspaceFolders: [], getConfiguration() { return { get(name) { return name === "projectDirs" ? [] : ""; } }; },
      createFileSystemWatcher() { return { onDidChange() {}, onDidCreate() {}, onDidDelete() {}, dispose() {} }; },
      onDidChangeWorkspaceFolders() { return {}; }, onDidChangeConfiguration() { return {}; },
    },
    lm: {
      registerMcpServerDefinitionProvider() {},
      get mcpServerDefinitions() { return runtime && runtime.mcpDefinitions || []; },
      get tools() { return runtime && runtime.lmTools || []; },
      invokeTool() {}, startMcpGateway() {},
    },
    extensions: { getExtension(id) { assert.equal(id, "codeium.windsurf"); return native; } },
    window: { createOutputChannel() { return output; }, registerWebviewViewProvider() { return {}; } },
    commands: {
      registerCommand(name, callback) { commands.set(name, callback); return {}; },
      async getCommands(filterInternal) {
        assert.equal(filterInternal, false);
        return runtime && runtime.commands || ["windsurf.mcp.open", "workbench.action.reloadWindow", "dtm.diagnoseMcp"];
      },
      executeCommand(command) {
        if (runtime && runtime.acpSnapshot) {
          assert.equal(command, "workbench.action.getAcpState");
          runtime.executed.push(command);
          return runtime.acpSnapshot;
        }
        throw new Error("未検証コマンドを診断から実行してはいけません");
      },
    },
  };
  if (runtime && runtime.acpApi) {
    Object.defineProperty(vscode, "windsurfAcp", { enumerable: true, value: runtime.acpApi });
  } else {
    Object.defineProperty(vscode, "windsurfAcp", {
      enumerable: true, get() { throw new Error("未知のAPI getterを評価してはいけません"); },
    });
  }
  const sandbox = {
    process: { env: { APPDATA: "C:\\appdata" } }, clearTimeout() {},
    setInterval() { return { unref() {} }; },
    setTimeout() { return 0; },
    module: { exports: {} },
    require(name) {
      if (name === "vscode") return vscode;
      if (name === "path") return path;
      if (name === "crypto") return require("crypto");
      if (name === "./media/catalog") return require("./extension/media/catalog");
      if (name === "./livestate") return require("./extension/livestate");
      if (runtime && name === "fs") return { existsSync() { return true; } };
      if (runtime && name === "child_process") return {
        execFile(exe, args, options, callback) {
          runtime.calls.push(args);
          callback(null, JSON.stringify(runtime.responses[args[1]] ?? "ok"), "");
        },
      };
      return new Proxy({}, { get() { throw new Error("診断でファイルやプロセスにアクセスしてはいけません"); } });
    },
  };
  vm.runInNewContext(source + '\nmodule.exports.testApi = { refresh, action, setState(value) { lastState = value; } };', sandbox);
  sandbox.module.exports.activate({ subscriptions: [] });
  return { run: commands.get("dtm.diagnoseMcp"), output, api: sandbox.module.exports.testApi };
}

test("診断はAPI名・コマンド名のみを出し、設定・プロセスにはアクセスしない", async () => {
  const exported = { client: { token: "DO_NOT_PRINT_SECRET" }, listMcpServers() {} };
  Object.defineProperty(exported, "credentials", {
    enumerable: true, get() { throw new Error("exportのgetterを評価してはいけません"); },
  });
  const { run, output } = load({ id: "codeium.windsurf", isActive: true, exports: exported });
  const report = JSON.parse(JSON.stringify(await run()));
  assert.deepEqual(report.commands, ["windsurf.mcp.open"]);
  assert.ok(report.namespaces.some((n) => n.name === "lm"));
  assert.ok(report.nativeExtension.exports.some((entry) => entry.name === "listMcpServers" && entry.type === "function"));
  assert.ok(report.nativeExtension.exports.some((entry) => entry.name === "credentials" && entry.type === "accessor"));
  assert.ok(!output.lines.join("\n").includes("DO_NOT_PRINT_SECRET"));
});

test("未起動の標準拡張を起動せずexportsにも触れない", async () => {
  const native = { id: "codeium.windsurf", isActive: false };
  Object.defineProperty(native, "exports", { get() { throw new Error("未起動のexportsを読んではいけません"); } });
  const report = JSON.parse(JSON.stringify(await load(native).run()));
  assert.deepEqual(report.nativeExtension.exports, []);
});

test("標準拡張が存在しない場合も診断可能", async () => {
  const report = await load(undefined).run();
  assert.equal(report.nativeExtension, null);
});

const catalog = require("./extension/media/catalog.js");
const scanEntry = (name, fields = {}) => ({ name, configPath: "C:\\config.json", kind: "McpStdio", ...fields });
const installation = (id, name, fields = {}) => ({
  id, name, kind: "McpStdio", provider: "github:owner/repo",
  usages: [{ configPath: "c:/CONFIG.json", serverName: name }], ...fields,
});

test("設定基準で一覧を統合し、管理中・未管理A/B/C・CLIを名前順にする", () => {
  const state = {
    scan: [scanEntry("Zulu"), scanEntry("alpha"), scanEntry("noSource"),
      scanEntry("remote", { kind: "McpRemote" }), scanEntry("official", { registry: "io.github.example/tool" })],
    installations: [installation("z", "Zulu"), installation("a", "alpha"),
      installation("n", "noSource", { provider: null }), installation("r", "remote"),
      installation("o", "official"), { id: "cli", name: "rtk", kind: "CliTool" }],
  };
  const groups = catalog.build(state);
  assert.deepEqual(groups.managed.map((r) => r.name), ["alpha", "Zulu"]);
  assert.deepEqual(groups.unmanaged.map((r) => [r.category, r.name]), [["A", "noSource"], ["B", "remote"], ["C", "official"]]);
  assert.equal(groups.cli[0].name, "rtk");
  assert.deepEqual(catalog.updateIds(state), ["a", "z", "cli"]);
});

test("登録前・無効・未解決も表示し、同じ設定の重複だけを除く", () => {
  const state = { scan: [scanEntry("missing", { disabled: true }), scanEntry("missing"),
    scanEntry("missing", { configPath: "C:\\project\\mcp_config.json" })] };
  const groups = catalog.build(state);
  assert.equal(groups.unmanaged.length, 2);
  assert.equal(groups.unmanaged[0].scan.disabled, true);
});

test("カタログ導入はURL型より優先し、同じ実体への更新も除外する", () => {
  const state = {
    scan: [scanEntry("remote", { kind: "McpRemote", registry: "io.github.example/tool" }), scanEntry("local")],
    installations: [installation("shared", "remote", { usages: [
      { configPath: "C:\\config.json", serverName: "remote" }, { configPath: "C:\\config.json", serverName: "local" },
    ] })],
  };
  assert.equal(catalog.build(state).unmanaged[0].category, "C");
  assert.deepEqual(catalog.updateIds(state), []);
});

test("名前やsourceRepoだけでカタログ導入・更新可能とは推測しない", () => {
  const state = { scan: [scanEntry("official-mcp")], installations: [
    installation("x", "official-mcp", { provider: null, sourceRepo: "owner/repo" }),
  ] };
  assert.equal(catalog.build(state).unmanaged[0].category, "A");
});

test("npm/PyPI委譲は管理中、未対応のgithub specと手動取込はA", () => {
  for (const [provider, expected] of [["pkg:npm:package", true], ["pkg:uv:package", true],
    ["pkg:npm:github:owner/repo", false], ["manual", false]]) {
    const state = { scan: [scanEntry("tool")], installations: [installation("x", "tool", { provider })] };
    assert.equal(catalog.build(state).managed.length === 1, expected);
  }
});

test("設定未検出の登録記録は孤立行として残し、CLIに混入させない", () => {
  const groups = catalog.build({ installations: [installation("x", "old")] });
  assert.equal(groups.managed[0].orphan, true);
  assert.equal(groups.cli.length, 0);
});

test("導入記録のないカタログMCPは、設定元と名前の明示指定だけでCにする", () => {
  const state = { scan: [scanEntry("same"), scanEntry("same", { configPath: "C:/other.json" })],
    catalogEntries: [{ configPath: "c:/CONFIG.json", serverName: "same" }] };
  const groups = catalog.build(state);
  assert.deepEqual(groups.unmanaged.map((row) => row.category), ["A", "C"]);
  assert.equal(groups.unmanaged[1].catalogSource, "user");
});

test("設定読取失敗時は前回行を残し、成功した設定の行も表示する", async () => {
  const runtime = { calls: [], responses: { list: [], managed: [], scan: {
    servers: [scanEntry("new", { configPath: "C:/good.json" })],
    errors: [{ configPath: "C:/config.json", message: "invalid JSON" }],
  } } };
  const { api } = load(undefined, runtime);
  api.setState({ installations: [], managed: [], scan: [scanEntry("old")] });
  let result;
  await api.refresh({ post(state) { result = state; } });
  const rows = JSON.parse(JSON.stringify(result.scan));
  assert.deepEqual(rows.map((row) => row.name), ["new", "old"]);
  assert.equal(rows[1].stale, true);
  assert.equal(result.scanErrors.length, 1);
  assert.ok(runtime.calls.some((args) => args.includes("--diagnostics")));
});

test("C分類の更新コマンドは拡張ホスト側でも拒否する", async () => {
  const runtime = { calls: [], responses: {} };
  const { api } = load(undefined, runtime);
  api.setState({ scan: [scanEntry("official", { registry: "example/tool" })], installations: [installation("x", "official")] });
  let result;
  await api.action({ view: { webview: { postMessage(message) { result = message; } } } }, { cmd: "prepare", id: "x" });
  assert.equal(result.ok, false);
  assert.equal(runtime.calls.length, 0);
});

test("カタログ導入指定はDTMメタデータだけを保存し、解除もできる", async () => {
  const scan = [scanEntry("official")];
  const runtime = { calls: [], responses: { scan: { servers: scan, errors: [] }, list: [], managed: [] } };
  const { api } = load(undefined, runtime);
  let saved = [];
  const notifications = [];
  const provider = { post() {}, view: { webview: { postMessage(message) { notifications.push(message); } } }, context: { globalState: {
    get(key) { assert.equal(key, "catalogEntries"); return saved; },
    async update(key, entries) { assert.equal(key, "catalogEntries"); saved = entries; },
  } } };
  api.setState({ scan, installations: [], catalogEntries: [] });
  await api.action(provider, { cmd: "set-catalog", id: "official", config: "C:/config.json" });
  assert.equal(saved.length, 1);
  assert.equal(saved[0].serverName, "official");
  assert.equal(notifications[0].type, "collapseEntry");
  assert.equal(notifications[0].key, catalog.key("C:/config.json", "official"));
  await api.action(provider, { cmd: "set-catalog", id: "official", config: "C:/config.json" });
  assert.equal(saved.length, 0);
  assert.ok(runtime.calls.every((args) => ["list", "scan", "managed", "logs", "probe-cache", "probe-mcp"].includes(args[1])));
});

test("CLIと共有するカタログ実体も全件確認から除外する", () => {
  const state = { scan: [scanEntry("official", { registry: "example/tool" })],
    installations: [installation("shared", "official", { kind: "CliTool" })] };
  assert.deepEqual(catalog.updateIds(state), []);
});

test("C分類のMCPも有効切替でき、設定元と期待状態をCLIへ渡す", async () => {
  const scan = [scanEntry("official", { registry: "example/tool", disabled: false })];
  const runtime = { calls: [], responses: { scan: { servers: scan, errors: [] }, list: [], managed: [] } };
  const { api } = load(undefined, runtime);
  api.setState({ scan, installations: [] });
  await api.action({ post() {}, view: { webview: { postMessage() {} } } },
    { cmd: "set-mcp-enabled", id: "official", config: "C:/config.json", enabled: false });
  const args = runtime.calls.find((args) => args[1] === "set-mcp-enabled");
  assert.ok(args);
  assert.equal(args[args.indexOf("--enabled") + 1], "false");
  assert.equal(args[args.indexOf("--expected-disabled") + 1], "false");
  assert.equal(args[args.indexOf("--config") + 1], "C:/config.json");
});

test("古い状態や不正なトグル要求をCLIへ流さない", async () => {
  const runtime = { calls: [], responses: {} };
  const { api } = load(undefined, runtime);
  api.setState({ scan: [scanEntry("tool", { disabled: true })], installations: [] });
  let result;
  await api.action({ view: { webview: { postMessage(message) { result = message; } } } },
    { cmd: "set-mcp-enabled", id: "tool", config: "C:/config.json", enabled: false });
  assert.equal(result.ok, false);
  assert.equal(runtime.calls.length, 0);
});

test("既知の読み取りアクセサだけからクライアントのメソッド名を診断する", async () => {
  const Native = { LanguageServerClient: { getInstance() { return { devClient: { getState() {}, token: "DO_NOT_PRINT" } }; } } };
  const { run, output } = load({ id: "codeium.windsurf", isActive: true,
    exports: { devClient: () => Native.LanguageServerClient.getInstance().devClient } });
  const report = await run();
  assert.equal(report.clientProbe.status, "available");
  assert.ok(report.clientProbe.members.some((member) => member.name === "getState"));
  assert.ok(!output.lines.join("\n").includes("DO_NOT_PRINT"));
});

test("実CLIで有効状態を往復変更し、バックアップを別々に保存する", { skip: !process.env.DTM_TEST_CLI }, () => {
  const cp = require("node:child_process");
  const os = require("node:os");
  const temp = fs.mkdtempSync(path.join(os.tmpdir(), "dtm-toggle-test-"));
  try {
    const config = path.join(temp, "mcp_config.json");
    fs.writeFileSync(config, JSON.stringify({ mcpServers: { demo: { command: "node", args: ["demo.js"], unknown: "keep" } } }));
    for (const enabled of [false, true]) {
      const result = cp.spawnSync(process.env.DTM_TEST_CLI, ["--json", "--root", path.join(temp, "dtm"),
        "set-mcp-enabled", "demo", "--config", config, "--enabled", String(enabled), "--expected-disabled", String(enabled)],
        { encoding: "utf8", windowsHide: true });
      assert.equal(result.status, 0, result.stderr);
      const saved = JSON.parse(fs.readFileSync(config, "utf8"));
      assert.equal(saved.mcpServers.demo.disabled, !enabled);
      assert.equal(saved.mcpServers.demo.unknown, "keep");
    }
    assert.equal(fs.readdirSync(path.join(temp, "dtm", "backups", "mcp_config")).length, 2);
  } finally {
    fs.rmSync(temp, { recursive: true });
  }
});

test("ACP状態は構造だけを診断し、チャット本文・設定値・認証情報を出力しない", async () => {
  const runtime = { editorVersion: "1.126.0-next", commands: ["workbench.action.getAcpState"], executed: [], acpSnapshot: {
    reduxStore: { acp: { sessions: { secret: "CHAT_SECRET" }, token: "AUTH_SECRET", mcpVersion: 3 } },
    acpConfig: { credentials: "CONFIG_SECRET" },
  } };
  const { run, output } = load(undefined, runtime);
  const report = await run();
  assert.equal(report.acpStateProbe.status, "schemaAvailable");
  assert.equal(runtime.executed.length, 1);
  for (const secret of ["CHAT_SECRET", "AUTH_SECRET", "CONFIG_SECRET"]) assert.ok(!output.lines.join("\n").includes(secret));
  assert.ok(report.acpStateProbe.slices[0].fields.some((field) => field.name === "mcpVersion"));
});

test("未検証のDevin版では内部状態取得コマンドを実行しない", async () => {
  const runtime = { editorVersion: "other-version", commands: ["workbench.action.getAcpState"], executed: [], acpSnapshot: { reduxStore: {} } };
  assert.equal((await load(undefined, runtime).run()).acpStateProbe.status, "unsupportedVersion");
  assert.equal(runtime.executed.length, 0);
});

test("lm診断はツール名・タグ・source構造を列挙し、アクセサ型windsurfAcpを評価しない", async () => {
  const runtime = { mcpDefinitions: [{ id: "srv-1", label: "serena", extra: 1 }], lmTools: [
    { name: "mcp_serena_find", tags: ["a"], source: { label: "serena" } },
    { name: "mcp_other_x", tags: [], source: "opaque" },
  ] };
  const { run } = load(undefined, runtime);
  const report = await run();
  assert.equal(report.lmProbe.mcpServerDefinitions[0].id, "srv-1");
  assert.equal(report.lmProbe.tools.count, 2);
  assert.equal(report.lmProbe.tools.entries[0].name, "mcp_serena_find");
  assert.equal(report.lmProbe.tools.entries[1].source, "string");
  assert.equal(typeof report.lmProbe.signatures["lm.invokeTool"], "string");
  assert.ok(!Object.keys(report.lmProbe.signatures).some((name) => name.startsWith("windsurfAcp.")));
});

test("acp診断はdevin-cliとcascadeへlistServersを送り、状態とフィールド名だけを記録する", async () => {
  const calls = [];
  const runtime = { editorVersion: "1.126.0-next", acpApi: {
    handleClientRequest(id, request) {
      calls.push([id, request.method]);
      if (id === "devin-cli" && request.method === "_cognition.ai/mcp/listServers") return { servers: [
        { serverId: "serena", connectionStatus: "connected", disabled: false, disabledTools: ["a"], sourcePath: "p" },
        { serverId: "broken", connectionStatus: "error", error: "spawn failed", extra: 1 },
      ] };
      if (id === "devin-cli" && request.method === "_cognition.ai/mcp/listTools") return { tools: [{ name: "t1" }, { name: "t2" }] };
      if (id === "cascade") return { servers: [] };
      throw new Error("unexpected call");
    },
  } };
  const report = await load(undefined, runtime).run();
  assert.equal(report.acpMcpProbe.status, "ok");
  assert.equal(report.acpMcpProbe["devin-cli"].serverCount, 2);
  assert.equal(report.acpMcpProbe["devin-cli"].servers[0].connectionStatus, "connected");
  assert.equal(report.acpMcpProbe["devin-cli"].servers[0].disabledToolsCount, 1);
  assert.equal(report.acpMcpProbe["devin-cli"].servers[1].error, "spawn failed");
  assert.deepEqual(JSON.parse(JSON.stringify(report.acpMcpProbe.listToolsProbe)), { params: "serverId", count: 2, keys: ["tools"] });
  assert.deepEqual(calls[0], ["devin-cli", "_cognition.ai/mcp/listServers"]);
  assert.deepEqual(calls[1], ["cascade", "_cognition.ai/mcp/listServers"]);
});

test("acp診断は未検証版・API不在・エラー時に実呼び出しを続行しない", async () => {
  const badVersion = await load(undefined, { editorVersion: "other", acpApi: { handleClientRequest() { throw new Error("呼ばれるべきではない"); } } }).run();
  assert.equal(badVersion.acpMcpProbe.status, "unsupportedVersion");
  const missing = await load(undefined, { editorVersion: "1.126.0-next", acpApi: {} }).run();
  assert.equal(missing.acpMcpProbe.status, "notAvailable");
  const failing = await load(undefined, { editorVersion: "1.126.0-next", acpApi: { handleClientRequest() { throw new Error("connection missing"); } } }).run();
  assert.equal(failing.acpMcpProbe.status, "ok");
  assert.equal(failing.acpMcpProbe["devin-cli"].ok, false);
});

test("不明なdevClient実装は診断でも呼び出さない", async () => {
  const { run } = load({ id: "codeium.windsurf", isActive: true,
    exports: { devClient() { throw new Error("呼び出してはいけません"); } } });
  assert.equal((await run()).clientProbe.status, "notAvailable");
});

const livestate = require("./extension/livestate.js");

function fixtureLogs(t, windowFiles) {
  const os = require("node:os");
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "dtm-live-"));
  const session = path.join(root, "logs", "20260101T000000");
  for (const [win, files] of Object.entries(windowFiles)) {
    const wind = path.join(session, win, "exthost", "codeium.windsurf");
    fs.mkdirSync(wind, { recursive: true });
    for (const [name, text] of Object.entries(files)) fs.writeFileSync(path.join(wind, name), text);
  }
  t.after(() => fs.rmSync(root, { recursive: true }));
  return path.join(root, "logs");
}

const TS = "2026-10-13 08:26:07.000 [info] ";
const agentLog = [
  "2026-10-13 08:26:03.000 [info] I1013 mcp_server_client_instance.go:344] [MCP] Server 'good' initialized successfully: good-server v1.2.3",
  '2026-10-13 08:26:03.100 [info] I1013 mcp_server_client_instance.go:38] [mcp-go] readResponses got line: {"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"t1"},{"name":"t2"},{"name":"t3"}]}}',
  "2026-10-13 08:26:04.000 [info] E1013 mcp_server_client_instance.go:273] [MCP] All connection methods failed for http://127.0.0.1:9/sse",
].join("\n");

test("livestate: Devinログの初期化行とtools応答を対にし、URL失敗と拡張側状態を併合する", (t) => {
  const logsRoot = fixtureLogs(t, { window3: {
    "Devin.log": agentLog,
    "MCP extOnly.log": TS + "MCP server 'extOnly' connected successfully\n",
    "MCP broken.log": TS + "MCP server 'broken' connection failed: spawn failed: ENOENT\n",
    "MCP slow.log": TS + "Connecting to MCP server 'slow'\n",
  } });
  const servers = [
    { name: "good", configPath: "c", kind: "McpStdio" },
    { name: "extOnly", configPath: "c", kind: "McpStdio" },
    { name: "broken", configPath: "c", kind: "McpStdio" },
    { name: "slow", configPath: "c", kind: "McpStdio" },
    { name: "remote", configPath: "c", kind: "McpRemote", command: "http://127.0.0.1:9/sse" },
    { name: "absent", configPath: "c", kind: "McpStdio" },
  ];
  const result = livestate.collect({ logsRoot, marker: null, servers });
  const byName = {};
  for (const [key, entry] of Object.entries(result.byKey)) byName[key.split("|")[1]] = entry;
  assert.deepEqual(byName.good, { state: "connected", via: "agent", version: "1.2.3", tools: 3, toolsVia: null, error: null });
  assert.equal(byName.extOnly.state, "connected"); assert.equal(byName.extOnly.via, "exthost");
  assert.equal(byName.broken.state, "error"); assert.equal(byName.broken.error, "spawn failed: ENOENT");
  assert.equal(byName.slow.state, "connecting");
  assert.equal(byName.remote.state, "error"); assert.equal(byName.remote.via, "agent");
  assert.equal(byName.absent.state, "unknown"); assert.equal(byName.absent.tools, null);
});

test("livestate: markerがある場合はそのウィンドウだけを読み、無ければ併合する", (t) => {
  const logsRoot = fixtureLogs(t, {
    window1: { "MCP srv.log": TS + "MCP server 'srv' connection failed: old err\n" },
    window2: { "MCP srv.log": TS + "MCP server 'srv' connected successfully\n" },
  });
  const outDir = path.join(logsRoot, "20260101T000000", "window2", "exthost", "output_logging_20260101T010000");
  fs.mkdirSync(outDir, { recursive: true });
  fs.writeFileSync(path.join(outDir, "1-DTM x.log"), "junk\ndtm-window-TEST");
  const servers = [{ name: "srv", configPath: "c", kind: "McpStdio" }];
  const merged = livestate.collect({ logsRoot, marker: "none", servers });
  assert.equal(merged.approx, true);
  const own = livestate.collect({ logsRoot, marker: "dtm-window-TEST", servers });
  assert.equal(own.approx, false);
});

test("livestate: 新形式ACPログ(1.126+)の接続状態とプローブキャッシュのツール数を併合する", (t) => {
  // output_logging_*/N-Devin ACP devin-cli.log (toolbox形式・ISO T日付)。
  // cutoff=output_dir生成時刻を通過するため ts は未来日付にする。
  const T = "2099-10-19T00:08:00.000Z";
  const acpLog = [
    `${T}  INFO toolbox::tools::mcp: output_channel="MCP: good" server="good" Connecting to MCP server 'good'`,
    `${T}  INFO toolbox::tools::mcp: output_channel="MCP: broken" server="broken" MCP server 'broken' connection failed: spawn failed: ENOENT`,
    `2099-10-19T00:08:10.000Z  INFO toolbox::tools::mcp: output_channel="MCP: good" server="good" MCP server 'good' connected successfully`,
    `2099-10-19T00:08:11.000Z  INFO toolbox::tools::mcp: output_channel="MCP: slow" server="slow" Connecting to MCP server 'slow'`,
    // セッション実行ログ等の雑多な行は無視される
    `2099-10-19T00:09:00.000Z  INFO toolbox::tools::exec::session_manager: session_id=x [create_session] creating PTY session`,
  ].join("\n");
  const logsRoot = fixtureLogs(t, { window1: {} });
  const outDir = path.join(logsRoot, "20260101T000000", "window1", "exthost", "output_logging_20990101T000000");
  fs.mkdirSync(outDir, { recursive: true });
  fs.writeFileSync(path.join(outDir, "4-Devin ACP devin-cli.log"), acpLog);
  const servers = ["good", "broken", "slow", "absent"].map((n) => ({ name: n, configPath: "C:\\cfg.json", kind: "McpStdio" }));
  const probeCache = { entries: [
    { serverName: "good", configPath: "C:\\cfg.json", tools: 7, serverVersion: "1.0.0", error: null },
    { serverName: "broken", configPath: "C:\\cfg.json", tools: null, error: "timeout" },
  ] };
  // 実拡張は catalog.key (JSON配列+小文字パス) を渡す — その形式でも照合できることを検証
  const catKey = (s) => JSON.stringify([String(s.configPath).replace(/\\/g, "/").toLowerCase(), s.name]);
  const r = livestate.collect({ logsRoot, marker: null, servers, probeCache, keyOf: catKey });
  const byName = {};
  for (const [key, entry] of Object.entries(r.byKey)) byName[JSON.parse(key)[1]] = entry;
  assert.equal(byName.good.state, "connected"); assert.equal(byName.good.via, "agent");
  assert.equal(byName.good.tools, 7); assert.equal(byName.good.toolsVia, "probe");
  assert.equal(byName.good.version, "1.0.0"); // probe の serverVersion を版補完に利用
  assert.equal(byName.broken.state, "error"); assert.ok(byName.broken.error.includes("ENOENT"));
  assert.equal(byName.broken.tools, null); // エラー記録はツール数に使わない
  assert.equal(byName.slow.state, "connecting");
  assert.equal(byName.absent.state, "unknown");
});

test("livestate: tools応答がINIT順と交差しても署名で正しく帰属し、曖昧な応答は帰属しない", (t) => {
  const init = (name, ts) => `${ts} [info] I1013 mcp_server_client_instance.go:344] [MCP] Server '${name}' initialized successfully: ${name} v1.0.0`;
  const tools = (names, ts) => `${ts} [info] I1013 mcp_server_client_instance.go:38] [mcp-go] readResponses got line: {"jsonrpc":"2.0","id":1,"result":{"tools":${JSON.stringify(names.map((n) => ({ name: n })))}}}`;
  // wave1: 厳密な INIT→応答 (署名を学習) / wave2: INIT A→INIT B→応答B→応答A の交差
  const log = [
    init("srvA", "2026-10-13 08:00:00.000"), tools(["a_first", "a2"], "2026-10-13 08:00:01.000"),
    init("srvB", "2026-10-13 08:00:02.000"), tools(["b_first"], "2026-10-13 08:00:03.000"),
    init("srvA", "2026-10-13 09:00:00.000"), init("srvB", "2026-10-13 09:00:01.000"),
    tools(["b_first"], "2026-10-13 09:00:02.000"), tools(["a_first", "a2"], "2026-10-13 09:00:03.000"),
    // wave3: 署名のない新規2件が交差 → 両方とも帰属しない
    init("newA", "2026-10-13 10:00:00.000"), init("newB", "2026-10-13 10:00:01.000"),
    tools(["unknown_x"], "2026-10-13 10:00:02.000"), tools(["unknown_y"], "2026-10-13 10:00:03.000"),
  ].join("\n");
  const logsRoot = fixtureLogs(t, { window3: { "Devin.log": log } });
  const servers = ["srvA", "srvB", "newA", "newB"].map((name) => ({ name, configPath: "c", kind: "McpStdio" }));
  const byName = {};
  for (const [key, e] of Object.entries(livestate.collect({ logsRoot, marker: null, servers }).byKey)) byName[key.split("|")[1]] = e;
  assert.equal(byName.srvA.tools, 2);
  assert.equal(byName.srvB.tools, 1);
  assert.equal(byName.newA.tools, null);
  assert.equal(byName.newB.tools, null);
});

test("livestate: セッション・ログ欠落・不正構造でも例外なくunknownを返す", (t) => {
  const os = require("node:os");
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "dtm-live-empty-"));
  t.after(() => fs.rmSync(root, { recursive: true }));
  const servers = [{ name: "x", configPath: "c", kind: "McpStdio" }];
  const result = livestate.collect({ logsRoot: path.join(root, "logs"), marker: "m", servers });
  assert.ok(!result.byKey["c|x"] || result.byKey["c|x"].state === "unknown");
  assert.equal(livestate.windsurfDirs(path.join(root, "logs")).length, 0);
});

test("全件確認はB/Cを除外したIDだけを照合する", async () => {
  const scan = [scanEntry("local"), scanEntry("official", { registry: "example/tool" })];
  const installations = [installation("localId", "local"), installation("officialId", "official")];
  const runtime = { calls: [], responses: { scan: { servers: scan, errors: [] }, list: installations, managed: [] } };
  const { api } = load(undefined, runtime);
  api.setState({ scan, installations });
  await api.action({ post() {}, view: { webview: { postMessage() {} } } }, { cmd: "check-all", id: "-" });
  const checks = runtime.calls.filter((args) => args[1] === "check");
  assert.equal(checks.length, 1);
  assert.ok(checks[0].includes("localId"));
  assert.ok(!runtime.calls.some((args) => args.includes("--all")));
});
