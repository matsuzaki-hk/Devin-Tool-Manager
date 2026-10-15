// Devin Tool Manager サイドバー Webview 本体。
// 全操作は data-* 属性 + イベント委譲。HTML への動的連結は textContent/
// エスケープ済み文字列のみ (設定内の文字列を実行可能なHTMLに埋め込まない)。
const vscode = acquireVsCodeApi();
let state = {
  installations: [], scan: [], managed: [], log: [],
  busy: [], dtmError: null, dtmPath: "",
};
let itemErrors = {}; // key -> message (トーストとは別に行へ保持)
let itemResults = {}; // key -> {ok, message} 直近操作結果 (成功もカード内へ保持)
let openDetails = new Set();
let showLog = false;
let showHelp = false; // ヘルプ (実行形態→扱い→可能操作 の対応表等) の展開
let formState = null; // {cmd, id, values, result} — インラインフォーム (送信後も値を保持)

// 引数入力が必要な操作のフォーム定義 (コマンドパレット型 InputBox は使わない)
const FORMS = {
  "set-source": { title: "更新元設定", fields: [
    { k: "repo", p: "owner/repo または GitHub URL", need: 1, tip: "GitHub Releases から成果物を取得します。owner/repo または https://github.com/owner/repo(.git) 形式" },
    { k: "asset", p: "アセット選択 glob (例: *windows*.zip)", def: "*.zip", tip: "リリース内の対象ファイルを選ぶ glob。複数一致はエラーになるため絞り込んでください" },
  ]},
  "set-git-source": { title: "ビルド元設定 (git ソース型)", fields: [
    { k: "repo", p: "git リポジトリ URL", need: 1, tip: "表示用のリポジトリ URL。実際の更新取得は作業ディレクトリの origin から行います" },
    { k: "branch", p: "追跡ブランチ (空=現在ブランチ)", tip: "origin 上のブランチ名。大文字小文字を厳密に区別します (main 等)。空なら作業ディレクトリの現在ブランチを採用" },
    { k: "workDir", p: "git 作業ディレクトリ (既存クローン)", need: 1, browse: "folder", tip: "git clone 済みのローカルパス (.git 必須)。準備時にここで git pull --ff-only を実行します。ローカル変更は保持され、競合時はエラーになります" },
    { k: "build", p: "ビルドコマンド (; 区切り)", def: "npm ci; npm run build", tip: "準備時に作業ディレクトリで順に実行するコマンド。失敗すると準備全体が中断されます" },
    { k: "out", p: "成果物ディレクトリ (相対)", def: "dist", need: 1, tip: "ビルド結果が出力される、作業ディレクトリからの相対パス。この内容が世代として保存・適用されます" },
  ]},
  "import": { title: "手動取込", fields: [
    { k: "file", p: "成果物パス (ZIP/EXE)", need: 1, browse: "file", tip: "取得済みの成果物ファイル (ZIP/EXE) を指定" },
    { k: "version", p: "版 (例: 1.2.3)", need: 1, tip: "この取込内容の版識別子。履歴・復元での表示に使われます" },
  ]},
  "preview": { title: "配置プレビュー", fields: [
    { k: "file", p: "ZIP/フォルダ/EXE のパス", need: 1, browse: "file", tip: "配置計画を試算する対象ファイル/フォルダ。実際の適用は行いません" },
  ]},
  "pin": { title: "版固定", fields: [
    { k: "version", p: "固定する版 (例: 0.50.0)", need: 1, tip: "この版以外への更新を抑止します。解除は「版固定解除」ボタン" },
  ]},
  "rollback": { title: "復元 (ロールバック)", fields: [
    { k: "gen", p: "世代ID (空=直前の適用済み世代)", tip: "戻す対象の世代ID。「世代」ボタンで一覧を確認できます" },
  ]},
  "register-cli": { title: "CLI ツール登録", fields: [
    { k: "name", p: "ツール名", need: 1, tip: "一覧上の表示名" },
    { k: "spec", p: "指定子: 絶対パス | dir|相対パス | コマンド名", need: 1, browse: "file", tip: "実行ファイルの指定。絶対パス・dir|相対パス・PATH 上のコマンド名のいずれか" },
  ]},
};

vscode.postMessage({ type: "ready" });

window.addEventListener("message", (e) => {
  const m = e.data;
  if (m.type === "state") {
    state = Object.assign(state, m.payload);
    render();
  } else if (m.type === "collapseEntry") {
    openDetails.delete(m.key);
    if (m.installationId) openDetails.delete(m.installationId);
    if (formState && formState.id === m.installationId) formState = null;
    render();
  } else if (m.type === "browseResult") {
    // ダイアログ選択結果をフォームの該当欄へ書き戻す (再描画せず値のみ更新)
    if (m.field) {
      if (formState) {
        formState.values = formState.values || {};
        formState.values[m.field] = m.value || "";
      }
      const inp = document.querySelector('.form input[data-fk="' + m.field + '"]');
      if (inp && m.value) inp.value = m.value;
    }
  } else if (m.type === "actionResult") {
    if (!m.ok && m.id) itemErrors[m.id] = m.message || "失敗";
    if (m.ok && m.id) delete itemErrors[m.id];
    // 直近操作結果をカード/行へ保持 (トーストは消えるが結果は残る)
    if (m.id) itemResults[m.id] = { ok: !!m.ok, message: m.message || (m.ok ? "完了" : "失敗") };
    // 開いているフォームの実行結果はフォーム内へ保持表示 (値はクリアしない)
    if (formState && formState.cmd === m.cmd && formState.id === m.id) {
      formState.result = {
        ok: m.ok,
        message: m.message || (m.ok ? "正常に完了しました" : "失敗"),
      };
    }
    toast((m.ok ? "完了: " : "失敗: ") + m.cmd + (m.message ? " — " + m.message : ""));
    render();
  }
});

// ---------- ヘルパー ----------
function esc(s) {
  return String(s ?? "").replace(/[&<>"]/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));
}
function toast(t) {
  const el = document.getElementById("toast");
  el.textContent = t;
  el.classList.add("show");
  setTimeout(() => el.classList.remove("show"), 4500);
}
function busy(cmd, id) { return (state.busy || []).includes(cmd + ":" + id); }
function btn(cmd, id, label, opts) {
  if (["check", "prepare", "pm-sync"].includes(cmd) && !DtmCatalog.updateIds(state).includes(id)) return "";
  const dis = busy(cmd, id) ? " disabled" : "";
  const cfg = opts && opts.config ? ' data-config="' + esc(opts.config) + '"' : "";
  const sec = opts && opts.sec ? " sec" : "";
  const name = opts && opts.name ? ' data-name="' + esc(opts.name) + '"' : "";
  const ek = opts && opts.ekey ? ' data-ekey="' + esc(opts.ekey) + '"' : "";
  const tip = opts && opts.tip ? ' title="' + esc(opts.tip) + '"' : "";
  return '<button class="' + sec.trim() + '"' + dis + cfg + name + ek + tip +
    ' data-cmd="' + esc(cmd) + '" data-id="' + esc(id) + '">' + esc(label) + "</button>";
}

// ---------- イベント委譲 ----------
document.addEventListener("input", (e) => {
  if (formState && e.target.dataset.fk) {
    formState.values = formState.values || {};
    formState.values[e.target.dataset.fk] = e.target.value;
  }
});
document.addEventListener("click", (e) => {
  const el = e.target.closest("[data-cmd], [data-toggle], [data-log], [data-help], [data-form-cmd], [data-form-close], [data-browse]");
  if (!el) return;
  if (el.dataset.help) { showHelp = !showHelp; render(); return; }
  // 参照ボタン: 拡張ホスト経由でファイル/フォルダ選択ダイアログを開く
  if (el.dataset.browse) {
    vscode.postMessage({
      type: "browse",
      field: el.dataset.browse,
      pick: el.dataset.pick || "file",
      title: el.dataset.blabel || "選択",
    });
    return;
  }
  if (el.dataset.toggle) {
    const id = el.dataset.toggle;
    openDetails.has(id) ? openDetails.delete(id) : openDetails.add(id);
    render();
    return;
  }
  if (el.dataset.log) { showLog = !showLog; render(); return; }
  if (el.dataset.formClose) { formState = null; render(); return; }
  if (el.dataset.formCmd) {
    const wrap = el.closest(".form");
    const spec = FORMS[el.dataset.formCmd];
    if (!wrap || !spec) return;
    const fields = {};
    let missing = false;
    wrap.querySelectorAll("input[data-fk]").forEach((inp) => {
      const fs = spec.fields.find((x) => x.k === inp.dataset.fk);
      fields[inp.dataset.fk] = inp.value.trim();
      if (fs && fs.need && !inp.value.trim()) missing = true;
    });
    if (missing) { toast("必須項目が未入力です"); return; }
    const cmd = el.dataset.formCmd;
    const id = el.dataset.formId;
    // フォームは開いたまま値を保持し、実行結果をインライン表示する
    formState = { cmd, id, values: fields, result: null };
    vscode.postMessage({ type: "action", cmd, id, fields });
    render();
    return;
  }
  const cmd = el.dataset.cmd;
  const id = el.dataset.id;
  // 拡張ホスト側の特別コマンド
  if (cmd === "__refresh") { vscode.postMessage({ type: "refresh" }); return; }
  if (cmd === "__reload") { vscode.postMessage({ type: "reload" }); return; }
  if (cmd === "__restart") { vscode.postMessage({ type: "restart" }); return; }
  // Devin 本体の操作 (拡張ホストで vscode.commands.executeCommand に転送)
  if (cmd && cmd.startsWith("__ui:")) {
    vscode.postMessage({ type: "ui", action: cmd.slice(5) });
    return;
  }
  // 引数が必要な操作はフォームを開き、直接実行しない
  if (FORMS[cmd]) {
    const inst = (state.installations || []).find((x) => x.id === id);
    // set-source は現在の設定値をフォーム初期値にする (現在値の確認・部分修正を可能に)
    const prefill =
      cmd === "set-source" && inst
        ? { repo: inst.sourceRepo && inst.sourceRepo !== "未設定" ? inst.sourceRepo : "",
            asset: inst.sourceAsset || "*.zip" }
        : cmd === "set-git-source" && inst && inst.gitSource
          ? inst.gitSource
          : null;
    formState = formState && formState.cmd === cmd && formState.id === id
      ? null : { cmd, id, values: prefill, result: null };
    openDetails.add(id); // フォームは詳細パネル内に表示
    render();
    return;
  }
  vscode.postMessage({
    type: "action", cmd, id,
    config: el.dataset.config || undefined,
    name: el.dataset.name || undefined,
    ekey: el.dataset.ekey || undefined,
    enabled: cmd === "set-mcp-enabled" ? el.dataset.enabled === "true" : undefined,
  });
});

// ---------- 描画 ----------
function statusClass(s) {
  if (!s) return "";
  if (s.startsWith("更新失敗")) return "err";
  if (s.startsWith("次回") || s.startsWith("更新待機")) return "warn";
  if (s.startsWith("最新")) return "ok";
  if (s.startsWith("更新あり")) return "warn";
  return "muted";
}

// 準備中の進捗表示 (list JSON の progress フィールド。job=running の間のみ来る)
const PHASE_LABELS = {
  release: "リリース取得",
  download: "ダウンロード",
  verify: "チェックサム検証",
  extract: "展開・登録",
  probe: "版確認",
};
function progressHtml(p) {
  if (!p) return "";
  const label = PHASE_LABELS[p.phase] || p.phase || "処理";
  let text = label + "中";
  let bar = "";
  if (p.bytesDone) {
    const doneMB = (p.bytesDone / 1048576).toFixed(1);
    if (p.bytesTotal) {
      const totalMB = (p.bytesTotal / 1048576).toFixed(1);
      const pct = Math.min(100, Math.round((p.bytesDone / p.bytesTotal) * 100));
      text += " " + doneMB + " / " + totalMB + " MB (" + pct + "%)";
      bar = '<progress class="dl" value="' + p.bytesDone + '" max="' + p.bytesTotal + '"></progress>';
    } else {
      text += " " + doneMB + " MB";
    }
  }
  return '<div class="muted small progress-row">' + esc(text) + bar + "</div>";
}

function instCard(i, embedded) {
  const err = itemErrors[i.id];
  const res = itemResults[i.id];
  const open = embedded === true || openDetails.has(i.id);
  const isMcp = (i.kind || "").startsWith("Mcp");
  // pkg 委譲エントリ: 更新系操作は外部マネージャ管轄のため非表示 (仕様 §9)
  const delegated = (i.provider || "").indexOf("pkg:") === 0;
  const btns =
    (delegated
      ? // 委譲エントリ: 外部最新かの「確認」とマネージャ委譲の「外部更新」 (仕様 §9)
        btn("check", i.id, "確認", { sec: 1, tip: "現在版とレジストリ最新版を照合します" }) +
          btn("pm-sync", i.id, "外部更新", { sec: 1, tip: "外部パッケージマネージャの正規更新を実行します (uv tool upgrade / npx キャッシュ更新等)" })
      : btn("check", i.id, "確認", { sec: 1, tip: "現在版の検出と更新元の最新候補を照合します" }) +
        btn("prepare", i.id, "準備", { tip: "最新成果物の取得・検証・世代化を行い、適用を予約します (実適用は次回起動時)" }) +
        (i.prepared
          ? btn("apply", i.id, "適用", { tip: "準備済み更新の適用を試みます。稼働中でファイルがロックされている場合は次回起動時の適用予約になります" })
            + btn("cancel", i.id, "取消", { sec: 1, tip: "準備済み更新と適用予約を取り消します (現行版は維持)" })
          : "") +
        btn("rollback", i.id, "復元", { sec: 1, tip: "過去に適用された世代へ戻します (反映は次回起動時の場合あり)" })) +
    (embedded === true ? "" : '<button class="sec" data-toggle="' + esc(i.id) + '">詳細' + (open ? "▲" : "▼") + "</button>");
  let detail = "";
  if (open) {
    detail = '<div class="detail mono">'
      + "<div>id: " + esc(i.id) + "</div>"
      + "<div>配置: " + esc(i.installRoot || "-") + "</div>"
      + "<div>方式: " + esc(i.strategy || "-") + (i.fixedPath ? " / " + esc(i.fixedPath) : "") + "</div>"
      + "<div>更新元: " + esc(i.sourceRepo || "未設定") + "</div>"
      + "<div>版固定: " + esc(i.pinned || "なし") + "</div>"
      // 予約済み委譲更新が残る間の lastError は予約の記録なので警告色にする
      + (i.lastError ? '<div class="' + (i.pendingPmSync ? "warn" : "err") + '">最終エラー: ' + esc(i.lastError) + "</div>" : "")
      + '<div class="detail-btns">'
      // pkg 委譲ではソース/取込/プレビュー/版固定/世代は無効 → 履歴のみ表示
      + (delegated
          ? ""
          : btn("set-source", i.id, "更新元設定", { sec: 1, tip: "GitHub Releases の取得元を設定します" })
            + btn("set-git-source", i.id, "ビルド元設定", { sec: 1, tip: "git クローン+ローカルビルド型の更新元を設定します (リリース資産の無いソースツール向け)" })
            + btn("import", i.id, "手動取込", { sec: 1, tip: "手元の成果物 (ZIP/EXE) を世代として取り込み、適用を予約します" })
            + btn("preview", i.id, "配置プレビュー", { sec: 1, tip: "指定ファイルを仮に配置した場合の展開計画を試算します" })
            + (i.pinned ? btn("unpin", i.id, "版固定解除", { sec: 1 }) : btn("pin", i.id, "版固定", { sec: 1, tip: "指定版以外への更新を抑止します" }))
            + btn("generations", i.id, "世代", { sec: 1, tip: "保存済み世代 (ロールバック候補) の一覧を表示します" })
            + btn("cleanup", i.id, "世代整理", { sec: 1, tip: "保持世代数を超える旧世代を削除します (現行・適用待ちは保護)" }))
      + btn("history", i.id, "履歴", { sec: 1, tip: "操作イベント履歴を表示します" })
      + "</div>"
      + (formState && formState.id === i.id && FORMS[formState.cmd]
          ? formHtml(formState.cmd, i.id) : "")
      + "</div>";
  }
  return '<div class="card">'
    + '<div class="card-head"><b>' + esc(i.name) + "</b>"
    + '<span class="badge">' + esc(isMcp ? "MCP" : "CLI") + "</span>"
    + (i.pinned ? '<span class="badge pin">固定 ' + esc(i.pinned) + "</span>" : "")
    + "</div>"
    + '<div class="' + statusClass(i.status) + '">' + esc(i.status || "未確認") + "</div>"
    + progressHtml(i.progress)
    // 矢印表記は「適用待ち (prepared)」か「更新あり」のときのみ — 最新時は現行版のみ表示
    + '<div class="ver mono">' + esc(i.current || "-")
    + (i.prepared
        ? " → " + esc(i.prepared)
        : (i.status || "").startsWith("更新あり") && i.candidate
          ? " → " + esc(i.candidate)
          : "")
    + "</div>"
    // pkg 委譲エントリ: 版は管理しないため、代わりに委譲先パッケージを表示
    + (i.provider && i.provider.indexOf("pkg:") === 0
        ? '<div class="mono muted small">委譲: ' + esc(i.provider.slice(4)) + "</div>"
        : "")
    // 実行中ロックで失敗した外部更新の次回起動時再実行予約 (pending_pm_sync)
    + (i.pendingPmSync
        ? '<div class="warn small">予約: 次回起動時に外部更新を再実行 ('
          + esc(i.pendingPmSync.manager + ":" + i.pendingPmSync.package) + ")</div>"
        : "")
    + (err ? '<div class="err small">' + esc(err) + "</div>" : "")
    // 直近操作結果 (成功=緑/失敗=赤)。外部更新では実行コマンドと版遷移を表示
    + (res && !err
        ? '<div class="' + (res.ok ? "ok" : "err") + ' small">'
          + esc((res.ok ? "✓ " : "✗ ") + res.message) + "</div>"
        : "")
    + '<div class="btns">' + btns + "</div>"
    + detail
    + "</div>";
}

function formHtml(cmd, id) {
  const f = FORMS[cmd];
  if (!f) return "";
  // 送信後も入力値を保持 (エラー時に再入力不要)。次回フォームを開き直すと values は初期化される。
  const vals = (formState && formState.values) || {};
  const inputs = f.fields
    .map((fl) => '<div class="frow"><input data-fk="' + esc(fl.k) + '" placeholder="' + esc(fl.p)
      + '" title="' + esc(fl.tip || fl.p)
      + '" value="' + esc(vals[fl.k] ?? fl.def ?? "") + '">'
      // パス系フィールドは参照ボタンで選択ダイアログを開く (拡張ホスト側で showOpenDialog)
      + (fl.browse
          ? '<button class="sec small browse-btn" data-browse="' + esc(fl.k)
            + '" data-pick="' + esc(fl.browse) + '" data-blabel="' + esc(fl.p)
            + '" title="ダイアログで選択">参照…</button>'
          : "")
      + "</div>")
    .join("");
  const res = formState && formState.result
    ? '<span class="form-result ' + (formState.result.ok ? "ok" : "err") + '">'
      + esc((formState.result.ok ? "✓ " : "✗ ") + (formState.result.message || "")) + "</span>"
    : "";
  const dis = busy(cmd, id) ? " disabled" : "";
  return '<div class="form"><div class="muted small">' + esc(f.title) + "</div>"
    + inputs
    + '<div class="form-actions">'
    + '<button data-form-cmd="' + esc(cmd) + '" data-form-id="' + esc(id) + '"' + dis + '>実行</button>'
    + '<button class="sec" data-form-close="1">閉じる</button>' + res + "</div></div>";
}

// ヘルプ本文: 実行形態→分類→可能操作 の対応表 + 操作/状態/用語の説明
function helpHtml() {
  return '<div class="detail help">'
    + '<div class="help-h">一覧の分類・状態</div>'
    + '<ul class="help-ul"><li>DTM管理中MCP：更新元が設定されたMCP。npm／uvへの更新委譲も含みます。</li>'
    + '<li>DTM未管理MCP：A＝更新元未設定または更新確認未対応、B＝URL接続型でサーバー本体は管理対象外、C＝設定にカタログ導入記録がありDTMから更新しないMCP。</li>'
    + '<li>各大分類は名前順、未管理はA→B→C・各分類内で名前順です。有効／無効によって一覧から消えません。</li>'
    + '<li>カタログ導入記録が設定にない場合、詳細の「カタログ導入として指定」でCに分類できます。実際の導入元を確認してから指定してください。名前だけでは判定しません。</li>'
    + '<li>有効／無効ボタンはDevinのdisabled設定を変更します。変更前にバックアップし、競合時は中止します。接続の開始・終了が即時に反映されない場合はDevinで再読込が必要です。</li>'
    + '<li>有効状態は設定上の値です。接続状態と更新状態は別です。接続マークはDevin自身のログを読み取った実値です: 緑=接続済み、黄=接続中、赤=失敗、輪郭のみ=未取得、灰=無効。ツール数はDevinエージェントが取得した実数で、未取得時は「—」です。</li>'
    + '<li>名前をクリックすると版・ツール数・設定元・操作を展開します。狭い幅では版、ツール数の順に一覧列を省略します。</li></ul>'
    + '<div class="help-h">実行形態 → DTM の扱い → できること</div>'
    + '<table class="help-t">'
    + '<tr><th>起動コマンドの形</th><th>分類</th><th>操作</th></tr>'
    + '<tr><td class="mono">C:\\path\\tool.exe</td><td>管理対象</td><td>確認・準備・適用・取消・復元・世代・管理化 すべて可</td></tr>'
    + '<tr><td class="mono">node xxx.js</td><td>一覧登録のみ</td><td>ビルド元設定 (git+build) または手動取込で更新追跡。管理化は不可 (ランチャーが直接実行できないため)</td></tr>'
    + '<tr><td class="mono">npx / npm exec</td><td>外部管理 (npm 委譲)</td><td>確認 (レジストリ最新照合)・外部更新 (npx キャッシュ更新)</td></tr>'
    + '<tr><td class="mono">uvx / uv tool run</td><td>外部管理 (uv 委譲)</td><td>確認・外部更新 (uv tool run --refresh-package)</td></tr>'
    + '<tr><td class="mono">.local\\bin\\x.exe (uv tool shim)</td><td>外部管理 (uv-tool 委譲)</td><td>確認・外部更新 (uv tool upgrade)</td></tr>'
    + '<tr><td class="mono">https://… (リモート MCP)</td><td>対象外</td><td>一覧表示のみ</td></tr>'
    + '</table>'
    + '<div class="help-h">操作の意味</div>'
    + '<ul class="help-ul">'
    + '<li><b>確認</b> — 現在版を検出し、更新元の最新候補と照合 (ファイルは変更しません)</li>'
    + '<li><b>準備</b> — 最新成果物の取得・検証・世代保存を行い適用を予約。実際の差替えは次回そのツールの起動時</li>'
    + '<li><b>適用</b> — 準備済み更新を即座に適用。稼働中プロセスがファイルを掴んでいる場合は次回起動時の適用予約として成立します (失敗ではありません)</li>'
    + '<li><b>取消</b> — 準備済み世代と適用予約を取り消します。現行版は維持されます</li>'
    + '<li><b>復元</b> — 過去に実適用された世代へ戻します。同様に次回起動時の反映になる場合があります</li>'
    + '<li><b>外部更新</b> — 委譲エントリ専用。外部マネージャの正規更新コマンドを実行します</li>'
    + '<li><b>管理化</b> — MCP の起動経路を <span class="mono">dtm.exe launch</span> 経由に差し替え、起動時適用・版検出を自動化 (設定は自動バックアップ)</li>'
    + '<li><b>一覧登録</b> — 管理化せず、一覧表示と更新追跡だけを行います</li>'
    + '<li><b>更新元設定</b> — GitHub Releases の取得元 (owner/repo + アセット glob) を設定します</li>'
    + '<li><b>ビルド元設定</b> — リリース資産の無いツール向け。git 作業ディレクトリで pull --ff-only → ビルドコマンド → 成果物を世代化します</li>'
    + '<li><b>手動取込</b> — 手元の ZIP/EXE を世代として取り込み、適用を予約します</li>'
    + '<li><b>版固定</b> — 指定版以外への更新を抑止します</li>'
    + '</ul>'
    + '<div class="help-h">状態表示</div>'
    + '<ul class="help-ul">'
    + '<li><span class="ok">最新: x.y.z</span> — 現行版が更新元の最新と一致</li>'
    + '<li><span class="warn">更新あり: A → B</span> — 更新元に新しい版あり。「準備」で取り込めます</li>'
    + '<li><span class="warn">適用待ち / 更新待機</span> — 準備済み。次回そのツールの起動時に自動適用されます</li>'
    + '<li>更新元未設定 — 管理可能だが取得元が未設定。「更新元設定」または「ビルド元設定」「手動取込」へ</li>'
    + '<li>一覧管理のみ / 外部管理 — 外部マネージャ (npm/uv 等) 管轄のため DTM は実体を更新しません</li>'
    + '<li><span class="err">更新失敗</span> — 最終エラーを詳細欄に表示します</li>'
    + '</ul>'
    + '<div class="help-h">用語</div>'
    + '<ul class="help-ul">'
    + '<li><b>委譲</b> — npx/uvx/uv tool 等、外部パッケージマネージャ管理下の実体。整合性を壊さないため DTM は直接上書きせず、マネージャの正規コマンドに委譲します</li>'
    + '<li><b>世代</b> — 適用のたびに保存される版ごとのスナップショット。復元の対象です</li>'
    + '<li><b>fixed-path</b> — 固定パスのファイルを直接差し替える方式。稼働中は次回起動時適用になります</li>'
    + '<li><b>mcpkey</b> — 管理化された MCP の起動指定子 (dtm launch mcpkey:xxxx)</li>'
    + '<li><b>Devin 再読込 / 再起動</b> — 再読込=このウィンドウの再読み込み (拡張+MCP再起動)。再起動=Devin 全体のプロセス終了→自動再実行 (実行中チャットは中断)</li>'
    + '</ul>'
    + "</div>";
}

function scanRow(s) {
  const managedEntry = (state.managed || []).find(
    (m) => m.serverName === s.name && (m.configPath || "") === (s.configPath || "")
  );
  // 一覧登録済み (未管理化) の検出: installation.usages と (name+configPath) で照合
  const registered = !managedEntry && (state.installations || []).some((i) =>
    (i.usages || []).some((u) =>
      u.serverName === s.name && (u.configPath || "") === (s.configPath || "")));
  const ekey = "scan:" + s.name + (s.configPath || "");
  const scopeTag = s.scope === "user" ? "" : ' <span class="badge">' + esc(s.scope) + "</span>";
  const pm = s.packageManager ? ' <span class="warn">[' + esc(s.packageManager) + "管理]</span>" : "";
  let btns;
  if (managedEntry) {
    btns = '<span class="ok">管理済</span>'
      + btn("unmanage", s.name, "解除", { sec: 1, config: s.configPath, ekey,
          tip: "管理化を解除し、MCP 設定を元の直接起動へ戻します (自動バックアップ)" });
  } else if (s.kind === "McpRemote") {
    btns = '<span class="muted" title="リモート (http/https) の MCP は更新管理の対象外です">リモート</span>';
  } else {
    // 登録ボタン: 登録済みなら状態表示のみ
    const reg = registered
      ? '<span class="ok">登録済</span>'
      : btn("register-mcp", s.name, s.manageable ? "登録" : "一覧登録",
          { sec: 1, config: s.configPath, ekey,
            tip: s.manageable
              ? "DTM の管理対象として登録します (起動設定の変更は「管理化」で別途行います)"
              : "一覧・更新追跡のみ行います。外部マネージャ管轄/スクリプト型は実体更新を行いません" });
    // 管理化ボタン: 可能な起動形式のみ (pkg管理/スクリプト/未解決は理由表示)
    const mg = s.manageable
      ? btn("manage", s.name, "管理化", { config: s.configPath, ekey,
          tip: "起動経路を dtm.exe launch 経由へ差し替え、版検出と世代適用を自動化します (設定は自動バックアップ)" })
      : ' <span class="muted small">' + esc(s.manageNote || "管理化不可") + "</span>";
    btns = reg + mg;
  }
  return '<div class="scan-row">'
    + '<div><b>' + esc(s.name) + "</b>" + scopeTag + (s.disabled ? ' <span class="muted">(無効)</span>' : "")
    + pm + "</div>"
    + '<div class="mono muted small">' + esc((s.command || "").slice(0, 90)) + "</div>"
    + (itemErrors[ekey]
        ? '<div class="err small">' + esc(itemErrors[ekey]) + "</div>" : "")
    + '<div class="btns">' + btns + "</div></div>";
}

function mcpRow(row) {
  const s = row.scan;
  const i = row.inst;
  const open = openDetails.has(row.key) || formState && i && formState.id === i.id;
  const current = i && i.current || "不明";
  const enabled = s ? s.disabled ? "無効" : s.command === "設定形式が不正" ? "不正" : "有効" : "未検出";
  // 実行時状態: Devin 自身のログ (エージェント側優先、無ければエディタ側) を解析した実値。
  // disabled/前回値/未取得は推測せずそのまま表示する
  const live = (state.mcpLive && state.mcpLive.byKey && state.mcpLive.byKey[row.key]) || null;
  const connState = s && s.disabled ? "disabled" : !s || s.stale ? "unknown" : (live && live.state) || "unknown";
  const connLabel = { connected: "接続済み", connecting: "接続中", error: "接続失敗", disabled: "無効" }[connState] || "状態不明（未取得）";
  const connVia = live && live.via === "agent" ? "（Devinエージェント側ログ）"
    : live && live.via === "exthost" ? "（エディタ側ログ）" : "";
  const connTitle = connLabel + connVia
    + (live && live.error ? " — " + live.error : "")
    + (live && state.mcpLive && state.mcpLive.approx ? "（複数ウィンドウ併合の近似値）" : "");
  const toolCount = live && live.tools != null ? live.tools : null;
  const toolsText = toolCount != null ? String(toolCount) : "—";
  const toolsTitle = toolCount != null
    ? "Devinエージェントがサーバーから取得した実ツール数"
    : "ツール数は未取得（Devin側ログに tools/list 応答がありません）";
  const version = current + (i && i.prepared ? " → " + i.prepared
    : i && (i.status || "").startsWith("更新あり") && i.candidate ? " → " + i.candidate : "");
  const update = row.category ? row.category : i && i.prepared ? "待機" : i && (i.status || "").startsWith("更新あり") ? "更新"
    : i && (i.status || "").startsWith("最新") ? "最新" : "未確認";
  let h = '<div class="mcp-entry"><div class="mcp-line">'
    + '<button class="enabled-state ' + (enabled === "有効" ? "on" : "off") + '" role="switch" aria-checked="' + (enabled === "有効")
      + '" data-cmd="set-mcp-enabled" data-id="' + esc(row.name) + '" data-config="' + esc(row.configPath || "")
      + '" data-ekey="' + esc(row.key) + '" data-enabled="' + !!(s && s.disabled) + '"'
      + (!s || s.stale || s.command === "設定形式が不正" || (state.scanErrors || []).length || busy("set-mcp-enabled", row.name) ? ' disabled' : '')
      + ' title="Devinのdisabled設定を切り替えます。変更前にバックアップします。接続反映はDevinの監視・再読込に依存します">' + esc(enabled) + '</button>'
    + '<span class="connection ' + connState + '" role="img" aria-label="' + esc(connTitle) + '" title="' + esc(connTitle) + '"></span>'
    + '<button class="row-name" data-toggle="' + esc(row.key) + '" aria-expanded="' + !!open + '" title="' + esc(row.name + "：クリックで情報・操作を展開") + '">'
    + (open ? "▾ " : "▸ ") + esc(row.name) + (s && s.stale ? "（前回値）" : "") + '</button>'
    + '<span class="mcp-tools mono" title="' + esc(toolsTitle) + '">' + esc(toolsText) + '</span>'
    + '<span class="mcp-version mono" title="現在版：' + esc(current) + '">' + esc(current) + '</span>'
    + '<span class="mcp-update ' + statusClass(i && i.status) + '" title="' + esc(row.category ? DtmCatalog.labels[row.category] : i && i.status || "未確認") + '">' + esc(update) + '</span>'
    + '</div>'
    + (itemErrors[row.key] ? '<div class="err small">' + esc(itemErrors[row.key]) + '</div>' : '')
    + (itemResults[row.key] && !itemErrors[row.key] ? '<div class="small muted">' + esc(itemResults[row.key].message) + '</div>' : '');
  if (open) {
    h += '<div class="mcp-detail">'
      + '<div>設定元：<span class="mono">' + esc(row.configPath || "設定未検出・DTM登録記録のみ") + '</span></div>'
      + '<div>有効状態：' + esc(enabled) + ' ／ 接続：' + esc(connTitle) + '</div>'
      + '<div>ツール：' + (toolCount != null ? "実数 " + esc(String(toolCount)) + "（Devinエージェント取得）" : "未取得（ログに tools/list 応答なし）") + '</div>'
      + '<div>バージョン：<span class="mono">' + esc(version) + '</span>'
      + (live && live.version ? ' ／ 稼働版：<span class="mono">' + esc(live.version) + '</span>' : "") + '</div>'
      + (row.category ? '<div>' + esc(DtmCatalog.labels[row.category]) + '</div>' : '')
      + (s && s.scope ? '<div>スコープ：' + esc(s.scope) + '</div>' : '')
      + (s && s.registry ? '<div>導入元の記録：' + esc(s.registry) + '</div>' : '')
      + (row.catalogSource === "user" ? '<div>導入元：ユーザーがカタログ導入として指定</div>' : '')
      + (s && s.stale ? '<div class="warn">設定を読めないため前回取得値です。操作前に読み取りエラーを解消してください。</div>' : '')
      + (s && s.manageNote ? '<div class="muted small">' + esc(s.manageNote) + '</div>' : '');
    if (row.category === "B" || row.category === "C") {
      h += '<div class="muted small">DTMからサーバー本体の更新・配置変更は行いません。</div>';
    } else {
      if (i) h += instCard(i, true);
      if (s && !s.stale) h += scanRow(s);
    }
    if (s && !s.stale && !s.registry) h += '<div class="detail-btns">'
      + btn("set-catalog", row.name, row.catalogSource === "user" ? "カタログ導入の指定を解除" : "カタログ導入として指定", {
        sec: 1, config: row.configPath,
        tip: "Devinカタログから導入したことを確認できる場合に指定します。DTMでの分類・更新対象外方針だけを保存し、MCP設定や実体は変更しません",
      }) + '</div>';
    h += '</div>';
  }
  return h + '</div>';
}

function cliRow(i) {
  const key = "cli:" + i.id;
  const open = openDetails.has(key) || formState && formState.id === i.id;
  const update = i.prepared ? "待機" : (i.status || "").startsWith("更新あり") ? "更新"
    : (i.status || "").startsWith("最新") ? "最新" : "未確認";
  return '<div class="mcp-entry cli-entry"><div class="cli-line">'
    + '<button class="row-name" data-toggle="' + esc(key) + '" aria-expanded="' + !!open + '" title="' + esc(i.name + "：クリックで情報・操作を展開") + '">'
    + (open ? "▾ " : "▸ ") + esc(i.name) + '</button>'
    + '<span class="cli-version mono" title="現在版：' + esc(i.current || "不明") + '">' + esc(i.current || "不明") + '</span>'
    + '<span class="mcp-update ' + statusClass(i.status) + '" title="' + esc(i.status || "未確認") + '">' + esc(update) + '</span>'
    + '</div>' + (open ? '<div class="mcp-detail">' + instCard(i, true) + '</div>' : '') + '</div>';
}

function render() {
  const app = document.getElementById("app");
  const active = document.activeElement;
  const focusData = active && app.contains(active) ? { ...active.dataset } : null;
  const selection = active && active.tagName === "INPUT" ? [active.selectionStart, active.selectionEnd] : null;

  let h = "";
  if (state.dtmError) {
    h += '<div class="err small">dtm エラー: ' + esc(state.dtmError)
      + "<br>path: " + esc(state.dtmPath || "") + "</div>";
  }
  h += '<div class="toolbar">'
    + btn("check-all", "-", "全件 更新確認", { sec: 1, tip: "全エントリの現在版・更新元最新版を一括照合します" })
    + btn("prepare-all", "-", "全件 準備", { sec: 1, tip: "「更新あり」の全エントリを順に準備します (委譲型は対象外)" })
    + '<button class="sec" data-cmd="__refresh" data-id="-" title="一覧・状態の表示を再取得します">再読み込み</button>'
    + '<button class="sec" data-cmd="__reload" data-id="-" title="ウィンドウ再読み込み (MCP サーバーも再起動)">Devin 再読込</button>'
    + '<button class="sec" data-cmd="__restart" data-id="-" title="Devin を完全再起動 (全ウィンドウ終了→自動で再実行)">Devin 再起動</button>'
    + '<button class="sec" data-log="1">ログ' + (showLog ? "▲" : "▼") + "</button>"
    + '<button class="sec" data-help="1" title="実行形態別の扱い・操作意味・用語の説明">? ヘルプ' + (showHelp ? "▲" : "") + "</button>"
    + "</div>"
    + (showHelp ? helpHtml() : "");

  const groups = DtmCatalog.build(state);
  for (const error of state.scanErrors || []) {
    h += '<div class="err small">設定読み取りエラー：' + esc(error.configPath || "スキャン") + ' — ' + esc(error.message) + '</div>';
  }
  h += '<h3>DTM管理中MCP<span class="count">' + groups.managed.length + "件</span></h3>";
  h += groups.managed.length ? groups.managed.map(mcpRow).join("") : '<div class="muted small">該当なし</div>';
  h += '<h3>DTM未管理MCP<span class="count">' + groups.unmanaged.length + "件</span></h3>";
  for (const category of ["A", "B", "C"]) {
    const rows = groups.unmanaged.filter((row) => row.category === category);
    if (rows.length) h += '<h4 title="' + esc(DtmCatalog.labels[category]) + '">' + esc(DtmCatalog.labels[category]) + ' (' + rows.length + '件)</h4>' + rows.map(mcpRow).join("");
  }
  if (!groups.unmanaged.length) h += '<div class="muted small">該当なし</div>';
  h += '<h3>CLIツール<span class="count">' + groups.cli.length + "件</span></h3>";
  h += groups.cli.length ? groups.cli.map(cliRow).join("") : '<div class="muted small">管理対象なし</div>';
  h += '<div class="toolbar">' + btn("register-cli", "-", "+ CLI ツール登録", { sec: 1, tip: "MCP 以外の CLI ツールを更新管理対象として登録します" }) + "</div>";
  if (formState && formState.cmd === "register-cli") h += formHtml("register-cli", "-");

  h += '<div class="muted small scan-scope" title="PC全体のプロジェクトは探索しません">探索対象：ユーザー設定・開いているワークスペース・明示登録プロジェクト（' + esc((state.scanRoots || []).length) + 'ルート）</div>';

  if (showLog) {
    h += '<h3>ログ (末尾)</h3><div id="log" class="mono small">'
      + esc((state.log || []).join("\n")) + "</div>";
  }

  // 末尾: Devin 本体操作 (ワークベンチ/カスタマイズ画面を開く)
  h += '<div class="toolbar footer">'
    + '<button class="sec" data-cmd="__ui:new-window" data-id="-" title="新しい空の Devin ウィンドウを開きます">新しいDevinウィンドウ</button>'
    + '<button class="sec" data-cmd="__ui:open-rules" data-id="-" title="Devin のカスタマイズ画面をルール一覧タブで開きます">ルール一覧</button>'
    + '<button class="sec" data-cmd="__ui:open-skills" data-id="-" title="Devin のカスタマイズ画面をスキル一覧タブで開きます">スキル一覧</button>'
    + '<button class="sec" data-cmd="__ui:open-mcps" data-id="-" title="Devin のカスタマイズ画面をMCP一覧タブで開きます">MCP一覧</button>'
    + '<button class="sec" data-cmd="__ui:open-mcp-config" data-id="-" title="ユーザースコープの mcp_config.json をエディタで開きます">MCP設定</button>'
    + '<button class="sec" data-cmd="__ui:open-dtm-log" data-id="-" title="DTM の動作ログ (dtm.log) をエディタで開きます">DTMログ</button>'
    + '<button class="sec" data-cmd="__ui:open-folder" data-id="-" title="現在のワークスペースフォルダをエクスプローラーで表示します">プロジェクトフォルダ</button>'
    + btn("diagnose", "-", "診断ZIP", { sec: 1, tip: "状態・ログ・設定(マスク済み)をzipにまとめ、保存先をエクスプローラーで開きます" })
    + "</div>";
  h += '<div id="toast"></div>';
  app.innerHTML = h;
  if (focusData) {
    const target = [...app.querySelectorAll("button,input")].find((element) =>
      focusData.fk ? element.dataset.fk === focusData.fk
        : focusData.toggle ? element.dataset.toggle === focusData.toggle
          : focusData.cmd && element.dataset.cmd === focusData.cmd && element.dataset.id === focusData.id);
    if (target) {
      target.focus({ preventScroll: true });
      if (selection) target.setSelectionRange(...selection);
    }
  }
}


