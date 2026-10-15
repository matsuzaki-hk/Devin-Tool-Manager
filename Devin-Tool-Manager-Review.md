# Devin Tool Manager 評価・修正依頼

評価日：2026-10-07

対象：本リポジトリ（`Devin-Tool-Manager`）。コード・README・ACCEPTANCE・仕様書の照合、既存テスト実行ファイルの再実行、Webview生成コードの構文検証を実施。プロジェクト、本番MCP設定、RTKの実体、導入済み拡張は変更していない。ソースからの再ビルドとDevin実機UI操作は未実施。

## 総評

更新エンジンとUIの分離、OSファイルロック、GitHubチェックサム照合、ZIP安全検査、設定バックアップの基盤はある。ただし現状を「13項目の実測クリア」「実運用準備完了」と評価することはできない。UIに再現可能な構文エラーがあり、固定パス更新の復旧・並行起動、MCP管理化に優先修正事項がある。

P1＝通常機能の停止・設定破損・起動の取り違え等につながる優先修正。P2＝仕様不整合・特定条件での不具合。下記行番号はレビュー時点。

## 優先修正事項

### R1 [P1] Webview生成結果がJavaScript構文エラー

場所：`extension/extension.js:261`。

外側のテンプレートリテラル内にある `.join('\n')` が、生成HTMLでは引用符内の生の改行になり、スクリプト全体を解析できない。Nodeのvmでhtml()の返却値からscriptを抜き出して解析すると、generated-webview.jsの53行目で `SyntaxError: Invalid or unexpected token` を再現した。

導入済み `local.devin-tool-manager-0.1.0/extension.js` とソースのSHA-256は一致。したがって導入済み版にも同じ欠陥がある。拡張の再起動では解消しない。

修正：ブラウザー側JSを別ファイル化してテンプレートの二重エスケープを避ける。インラインonclickの文字列連結も廃止し、data属性とイベントリスナーで処理する。生成HTML内の全スクリプトとボタン操作を検証する。

### R2 [P1] Windowsのatomic_writeが原子的でない

場所：`crates/dtm-core/src/paths.rs:184–205`。

既存の宛先を旧ファイル名へrenameしてからtmpを宛先へrenameするため、間に宛先が存在しない時間がある。その間の読者はactiveポインターや状態ファイルを見失う。そこでプロセスが終了すると、旧ファイルの自動復旧も行われない。通常の関数エラー時の復元では強制終了を処理できない。

修正：Windowsの適切なファイル置換API等を使用し、読者が旧版または新版だけを観測する方式へ変更。初回作成・既存置換・途中終了のそれぞれを検証する。

### R3 [P1] 固定パス差し替えのジャーナルが事後記録で復旧不能な区間がある

場所：`crates/dtm-core/src/apply.rs:158–175, 183–190, 225–253`。

rename/copyの後に操作をジャーナルへ記録している。rename成功直後・記録前に終了すると退避済みファイルが復旧対象に含まれない。copy途中終了も、未記録の部分ファイルを残し得る。さらにrollback_journalは削除・復元エラーを伝播せず、呼び出し元は復旧失敗でもrolled-backと記録する。

修正：操作の意図を先行記録し、実ファイル状態と照合して再実行可能な復旧を行う。復旧失敗を完了扱いにせず起動を停止する。各ファイル操作の前後で強制終了する故障注入試験が必要。

### R4 [P1] 適用ロックと実プロセス起動の間に競合がある

場所：`crates/dtm-core/src/launcher.rs:26–41, 85–112`。

適用ロックを解放してからEXE解決・spawnする。別プロセスのapplyがその間に走れる。またロックを10秒で取得できない場合、更新が進行中でも「現行版で起動」へ進む。固定パス方式ではそのパスが差し替え中であり、旧版が安定して存在する保証はない。

修正：復旧確認・世代解決・利用登録・spawnを同じ同期設計の中で扱う。固定パス更新中はタイムアウトを旧版起動の根拠にしない。更新失敗時も復旧が確認できた場合だけ起動する。

### R5 [P1] npx/uvxとスクリプト型MCPを管理化すると起動経路が壊れる

場所：`crates/dtm-cli/src/main.rs:442–460`、`crates/dtm-core/src/register.rs:35–91`、`crates/dtm-core/src/launcher.rs:119–122`。

manageはPackageManager対象でも設定をdtm launchへ書き換えるが、その登録にはartifactがなく、launchで「artifact 未定義」になる。node/python等のスクリプト型はentrypointにスクリプトを登録するが、launchは元のインタプリタを使わずスクリプト自体を実行しようとする。

修正：対応していない起動形式は設定を変更する前に拒否する。または元のcommand/args/cwdを保持する起動アダプターを実装する。一覧登録とランチャー管理化を区別する。

### R6 [P1] プロジェクト間の同名MCPが別の実体へ解決される

場所：`crates/dtm-core/src/mcpcfg.rs:53–58`、`crates/dtm-core/src/launcher.rs:49–60`。

管理マップはconfig_pathとserver_nameで識別しているのに、書き込む起動引数はmcp:名前のみ。ランチャーは名前の最初の一致を採用する。異なるプロジェクトでserena等の同名サーバーを管理化すると、別プロジェクトのインストールへ接続し得る。

修正：起動引数へ一意な利用元IDまたはinstallationIdを渡し、利用元ごとの引数・作業ディレクトリを維持する。UIからの操作にも設定パスを含める。

### R7 [P1] 版固定がprepare/applyを制約していない

場所：`crates/dtm-core/src/ops.rs:123–135, 256–288, 498–514`。

pinはpolicy.pinned_versionを保存するが、prepareはその値を参照せずリリースを選択する。固定した旧版があっても最新版を準備し、適用できる。checkの「候補とpinが一致したら最新」判定も、現在版が一致していることを確認していない。

修正：確認・準備・適用の共通ポリシー判定に版固定を組み込み、既存の適用予約に対する扱いも定義する。

### R8 [P1] 検証失敗世代をrollbackで有効化できる

場所：`crates/dtm-core/src/ops.rs:339–364`、`crates/dtm-core/src/apply.rs:295–330`。

genmetaを書いて世代を公開した後に起動検証する。検証失敗時は世代を残す一方、rollbackはgenmetaがある世代を候補とし、既定では現行以外の作成日時が最新のものを選ぶ。失敗した新版が復元候補になる。

修正：準備完了・検証成功を明示した世代だけを適用対象にする。復元は「過去に正常適用した世代」の履歴から選択し、未適用の候補と分ける。

### R9 [P2] ランチャーが作業ディレクトリをEXEの親へ変更する

場所：`crates/dtm-core/src/launcher.rs:36, 163–167`。

元の呼び出し元cwdや設定上のcwdが失われる。RTK経由のgit等、現在のプロジェクトを基準とするコマンドがツール配置フォルダーで動く。

修正：既定は呼び出し元cwdを継承し、利用元の明示cwdだけを適用する。版確認のcwdと通常実行のcwdを分離する。

### R10 [P2] 固定パス更新が新版にない旧配布ファイルを残す

場所：`crates/dtm-core/src/apply.rs:111–117, 151`。

コメントでは旧側のみのファイルを除去するとあるが、実際の対象はnew_filesのみ。新版で削除されたDLLやリソースが残る。

修正：前世代の配布物マニフェストと新版との差分を使い、旧配布物だけを退避する。配置先全体を削除してユーザーデータまで消す方法は使わない。

### R11 [P2] 起動引数をそのままログへ記録する

場所：`crates/dtm-core/src/launcher.rs:32`。

tool_argsをDebug形式でログへ出すため、トークンや接続文字列を引数で渡すMCP/CLIでは秘密値がログに残る。

修正：既定では引数本文を記録しない。診断上必要なものだけを明示的に許可する。

## プライマリサイドバー対応

現行package.jsonにはcommandsとconfigurationしかなく、viewsContainers/viewsの宣言がない。extension.js:120はcreateWebviewPanelを使用しており、エディタータブとして開く実装。導入済みでもサイドバーには自動表示されない。

変更要件：

1. `contributes.viewsContainers.activitybar`へ専用コンテナー（id: dtm、タイトル: Devin Tool Manager、SVGアイコン）を追加する。
2. `contributes.views.dtm`に`type: webview`の`dtm.manager`を追加する。
3. `registerWebviewViewProvider('dtm.manager', provider)`でサイドバー用ビューを登録する。
4. `dtm.open`はサイドバーのコンテナー／ビューへフォーカスする。必要なら従来の広い詳細画面は別コマンドとして残す。
5. 8列の横長テーブルを、狭い幅で使える縦型のカードまたはツリー＋詳細に変更する。
6. 「MCP」「関連ツール」を区分し、名前、状態、現在版→準備版を常時表示。取得元や実パス等は展開表示にする。
7. 「確認」「準備」「適用」等は処理中に重複操作を抑制する。失敗理由は消えるトーストだけでなく対象行にも保持する。
8. 新規登録、ソース・配布アセット条件編集、保持／除外ルール、プレビュー、版固定を画面から扱えるようにする。現行UIにはこれらの主要操作の入口が不足している。
9. プロジェクトMCPの操作は設定パスをCLIに渡す。現行scanのprojectDirsに対し、actionのmanageは名前のみでユーザー設定へ向かうため、そのまま流用しない。
10. CSPとnonce、外部JSファイル、イベントリスナーを使用し、設定内のMCP名等を実行可能なHTMLへ直接連結しない。

最小manifest例（既存contributesへ統合）：

```json
{
  "viewsContainers": {
    "activitybar": [
      { "id": "dtm", "title": "Devin Tool Manager", "icon": "media/dtm.svg" }
    ]
  },
  "views": {
    "dtm": [
      { "type": "webview", "id": "dtm.manager", "name": "ツール管理" }
    ]
  }
}
```

Devin Next実機でアイコン表示、初回表示、再起動後の復元、表示／非表示、幅320px程度での操作、複数ウィンドウでの状態反映を確認する。VS Code標準API上の方法であり、対象Devin版での互換性の実機確認は別途必要。

参考：
- https://code.visualstudio.com/api/references/contribution-points#contributes.viewsContainers
- https://code.visualstudio.com/api/extension-guides/webview

## 検証結果とACCEPTANCEの訂正

- 既存の`target/debug/deps/core_tests-be5b80f213d3f3bd.exe`を再実行し、16件合格を確認。ただし再ビルドしていないため、現在のソースとの一致を保証する結果ではない。
- テストの対象は版比較、パス、ZIP、アセット選択、npx/uvx判定。更新中断復旧やMCP通信を網羅していない。
- Webview生成後スクリプトの構文検査は失敗。ソースと導入済み拡張の該当JSのハッシュ一致を確認。
- #3はR2/R3/R4により合格扱いを撤回し、故障注入・並行起動試験を追加する。
- #9の`--version`実行はMCP initialize/tools/list/tools/callの検証ではない。実際のstdio MCPプロトコル試験へ置換する。
- #10は異常終了コードだけでなく、引数境界、Unicode、stdin/stdout/stderr、cwd、終了・キャンセルを観測する専用子プロセスで試験する。
- #11は本番差し替え未実施と記録されているため、「実環境の更新後の保持」まで実測済みとは記載しない。隔離したRTK環境で適用・利用・復元まで検証する。
- #13のnpx/uvx識別だけでは、winget/Cargoで入れたEXEを絶対パス登録した場合の保護を証明できない。インストール由来を登録項目として保持し、対応ケースを追加する。

## 推奨する修正順序

1. R1を修正し、サイドバーの構成へ変更する。
2. R2/R3/R4/R8を修正し、中断復旧・適用の安全性を確立する。
3. R5/R6/R9を修正し、MCPと一般CLIの起動互換性を確立する。
4. R7/R10/R11とUIの不足操作を修正する。
5. ソースから再ビルドし、隔離環境で受け入れ試験を再実施。結果を「実測」「コード確認」「未検証」に分けてACCEPTANCEへ記録する。
6. 修正版VSIXを導入し、Devin Nextでサイドバーを実機確認した後、本番の管理対象を段階的に移行する。

この評価は上記範囲のレビューであり、すべての不具合を網羅した保証ではない。
