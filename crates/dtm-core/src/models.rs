//! ドメインモデル定義。
//! 仕様書 §4(管理対象), §5(レシピ), §8(状態), §13(設定・履歴) に対応する。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const SCHEMA_VERSION: u32 = 1;

// ---------- 種別 (仕様 §4.1) ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolKind {
    McpStdio,
    McpRemote,
    CliTool,
}

// ---------- レシピ (仕様 §5, §12) ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Recipe {
    pub schema_version: u32,
    /// 人間可読スラッグ
    pub id: String,
    pub kind: ToolKind,
    /// 正規化配置先+実行環境から生成される安定ID (仕様 §4.2)
    pub installation_id: String,
    /// ソースリポジトリ(開発元)。更新取得先とは別項目 (仕様 §5)
    #[serde(default)]
    pub source_repository: Option<String>,
    #[serde(default)]
    pub update_provider: Option<UpdateProvider>,
    #[serde(default)]
    pub artifact: Option<ArtifactSpec>,
    #[serde(default)]
    pub installation: Option<InstallStrategy>,
    #[serde(default)]
    pub policy: Option<Policy>,
    /// 非破壊的な版確認・簡易起動確認 (仕様 §6-6)
    #[serde(default)]
    pub verify: Option<VerifySpec>,
    /// 保持データ (設定・DB・ログ等) (仕様 §5.3, §9)
    #[serde(default)]
    pub data_preserve: Vec<String>,
    /// データ移行・バックアップ手順の有無 (仕様 §6.1)
    #[serde(default)]
    pub data_migration_required: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum UpdateProvider {
    GithubRelease(GithubReleaseProvider),
    /// 手動成果物取込のみ(自動更新なし)
    Manual,
    /// パッケージマネージャ管理下(委譲。第2段階)
    PackageManager {
        manager: String,
        package: String,
    },
    /// git 作業ツリー + ローカルビルド (リリース資産を持たないソース型ツール)
    GitBuild(GitBuildProvider),
    /// npm 等のパッケージレジストリから公開 tarball を取得し世代管理する。
    /// PM ランチャー経由起動の委譲 (PackageManager) とは別経路 — 起動形態が
    /// ローカルファイルのもの (インタプリタ+スクリプト等) 向け。
    NpmRegistry(NpmRegistryProvider),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NpmRegistryProvider {
    /// パッケージ名 ("rea-agents", "@scope/name" 形式可)
    pub package: String,
    /// レジストリのベース URL (省略時 https://registry.npmjs.org)
    #[serde(default)]
    pub registry: Option<String>,
    /// 世代化後に世代ルートで順に実行するインストールコマンド
    /// (npm tarball は node_modules を含まないため、実行時依存が
    /// あるパッケージは "npm install --omit=dev --no-audit --no-fund"
    /// 等をここに設定する。git-build の buildCommands と同型)
    #[serde(default)]
    pub install_commands: Vec<String>,
    #[serde(default)]
    pub version_scheme: VersionScheme,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitBuildProvider {
    /// 参照リポジトリ (表示用。実際の取得元は work_dir の origin)
    pub repository: String,
    /// 追跡ブランチ
    #[serde(default = "default_git_branch")]
    pub branch: String,
    /// ローカルの git 作業ディレクトリ (既存クローン。.git を含むこと)
    pub work_dir: PathBuf,
    /// 準備時に work_dir で順に実行するコマンド (cmd /c)
    #[serde(default)]
    pub build_commands: Vec<String>,
    /// work_dir 相対の成果物ディレクトリ (これが世代ペイロードになる)
    #[serde(default)]
    pub output_dir: Option<String>,
}

fn default_git_branch() -> String {
    "main".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GithubReleaseProvider {
    /// "owner/repo"
    pub repository: String,
    #[serde(default)]
    pub channel: ReleaseChannel,
    pub asset: AssetSelector,
    /// 版体系 (仕様 §5.1: semver/date/custom。比較不能時は候補提示のみ)
    #[serde(default)]
    pub version_scheme: VersionScheme,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ReleaseChannel {
    /// 安定版のみ
    #[default]
    Stable,
    /// プレリリースを含む
    IncludePrerelease,
    /// 特定タグ固定
    PinnedTag,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetSelector {
    #[serde(default)]
    pub match_type: AssetMatchType,
    /// 完全一致名 / glob パターン / 正規表現
    pub pattern: String,
    #[serde(default)]
    pub tag: Option<String>,
    /// 例 "windows"
    #[serde(default)]
    pub platform: Option<String>,
    /// 例 "x64"
    #[serde(default)]
    pub architecture: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum AssetMatchType {
    Exact,
    #[default]
    Glob,
    Regex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum VersionScheme {
    #[default]
    Semver,
    Date,
    /// 比較不能。常に候補表示のみ
    Custom,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactSpec {
    #[serde(default)]
    pub kind: ArtifactKind,
    /// ZIP 内の展開ルート選択 (glob。一意一致必須)
    #[serde(default)]
    pub root_pattern: Option<String>,
    /// 起動ファイルの配置先からの相対パス
    pub entrypoint: String,
    /// include/exclude (glob。区切りは `/`、`**` 再帰、大小文字は Windows: 不区別)
    #[serde(default = "default_include")]
    pub include: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
    /// 配布物と同じ相対パスが保持データに該当する場合の扱い (仕様 §5.3)
    #[serde(default)]
    pub data_priority: DataPriority,
}

fn default_include() -> Vec<String> {
    vec!["**/*".to_string()]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ArtifactKind {
    /// EXE 直接配布
    #[default]
    Exe,
    Zip,
    /// gzip 圧縮 tar アーカイブ (npm tarball 等)
    TarGz,
    /// ローカルフォルダ/手動成果物
    Folder,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum DataPriority {
    /// 保持データ優先
    #[default]
    DataWins,
    /// 新版採用
    NewWins,
    /// 手動確認
    Manual,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallStrategy {
    #[serde(default)]
    pub strategy: StrategyKind,
    #[serde(default)]
    pub apply_when: ApplyWhen,
    /// fixed-path 方式の配置先(正規化前の元パス。versioned では世代ルート)
    #[serde(default)]
    pub fixed_path: Option<PathBuf>,
    /// 保持する旧世代数(最低1は強制)
    #[serde(default = "default_keep")]
    pub keep_generations: u32,
    /// 保持容量上限 MiB (0=無制限)
    #[serde(default)]
    pub max_generations_mib: u64,
}

fn default_keep() -> u32 {
    3
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum StrategyKind {
    /// 世代別配置 (推奨)
    #[default]
    Versioned,
    /// 固定パス差し替え
    FixedPath,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ApplyWhen {
    /// 次回起動(MCP起動/ツール実行)時
    #[default]
    NextLaunch,
    /// 手動適用のみ
    Manual,
    /// Devin全ウィンドウ終了後
    AllWindowsClosed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Policy {
    #[serde(default)]
    pub prepare_automatically: bool,
    #[serde(default)]
    pub pinned_version: Option<String>,
    #[serde(default = "default_check_interval")]
    pub check_interval_hours: u64,
    #[serde(default)]
    pub on_preparation_failure: OnFailure,
}

fn default_check_interval() -> u64 {
    24
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum OnFailure {
    #[default]
    KeepCurrent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifySpec {
    /// 版確認コマンド。先頭要素 "{entrypoint}" は起動ファイルに置換
    pub command: Vec<String>,
    /// 出力から版を取り出す正規表現(省略時は出力全体トリム)
    #[serde(default)]
    pub version_regex: Option<String>,
    /// 許容終了コード
    #[serde(default = "default_exit_codes")]
    pub allowed_exit_codes: Vec<i32>,
    #[serde(default = "default_verify_timeout")]
    pub timeout_secs: u64,
}

fn default_exit_codes() -> Vec<i32> {
    vec![0]
}
fn default_verify_timeout() -> u64 {
    15
}

// ---------- レジストリ / 利用元 (仕様 §4.2, §13) ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Registry {
    pub schema_version: u32,
    #[serde(default)]
    pub installations: Vec<Installation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Installation {
    /// installationId (正規化配置先+実行環境のハッシュ)
    pub id: String,
    /// 表示名
    pub name: String,
    pub kind: ToolKind,
    /// 実行環境識別子 (例 "windows-x64")
    pub environment: String,
    /// 正規化済み配置ルート。
    /// versioned: <root>/generations/<id>、fixed-path: 実配置先
    pub install_root: PathBuf,
    /// 固定パス参照(外部参照が要求するパス。fixed-path 方式や
    /// 固定EXEを管理下に置いた場合の元実体パス)
    #[serde(default)]
    pub fixed_path: Option<PathBuf>,
    pub recipe: Recipe,
    /// この実体を参照する利用元の一覧 (仕様 §4.2)
    #[serde(default)]
    pub usages: Vec<UsageRef>,
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum UsageRef {
    /// MCP 設定ファイル上のサーバーエントリ
    McpServer {
        /// 設定ファイルスコープ: user / project / local
        scope: String,
        /// 設定ファイルパス
        config_path: PathBuf,
        /// mcpServers 内のキー名
        server_name: String,
    },
    /// 手動登録の CLI ツール(PATH 解決元含む)
    Cli {
        /// "abs-path" | "dir+rel" | "path-resolve"
        via: String,
        /// 登録時に使われた指定子(表示用)
        specifier: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchSpec {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
}

// ---------- 動的状態 (仕様 §8) ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    SourceUnset,
    Checking,
    UpToDate,
    Available,
    Preparing,
    Ready,
    WaitingInUse,
    Applying,
    Failed,
    Unmanaged,
}

impl Status {
    /// 仕様 §8 の表示例に対応する日本語表示
    pub fn display(&self, st: &InstallState) -> String {
        let cur = st.current_version.as_deref().unwrap_or("不明");
        let base = match self {
            Status::SourceUnset => "更新元未設定".into(),
            Status::Checking => "更新確認中".into(),
            Status::UpToDate => format!("最新：{}", cur),
            Status::Available => format!(
                "更新あり：{} → {}",
                cur,
                st.latest_version().unwrap_or("不明")
            ),
            Status::Preparing => format!("更新を準備中：{}", st.latest_version().unwrap_or("不明")),
            Status::Ready => {
                let next = st.prepared_version.as_deref().unwrap_or("不明");
                format!("次回起動時に更新：{} → {}", cur, next)
            }
            Status::WaitingInUse => "更新待機中：他のウィンドウで使用中".into(),
            Status::Applying => "更新を適用中".into(),
            Status::Failed => format!("更新失敗：{}を継続使用", cur),
            Status::Unmanaged => "一覧管理のみ／外部管理".into(),
        };
        // 委譲更新の予約は末尾に追記 (前方一致で表示分類する呼び出し側を壊さない)
        if st.pending_pm_sync.is_some() {
            format!("{base}＋外部更新を次回起動時に再実行")
        } else {
            base
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct InstallState {
    #[serde(default)]
    pub status: Option<Status>,
    /// 有効世代が指す版(今後の起動に使う版)
    #[serde(default)]
    pub current_version: Option<String>,
    /// 準備済み(適用待ち)の版
    #[serde(default)]
    pub prepared_version: Option<String>,
    /// 有効世代ID
    #[serde(default)]
    pub active_generation: Option<String>,
    /// 適用待ち世代ID
    #[serde(default)]
    pub prepared_generation: Option<String>,
    /// 現在実行中のプロセスが使っている版(世代別配置で旧版が動作中の場合)
    #[serde(default)]
    pub running_version: Option<String>,
    #[serde(default)]
    pub latest_candidate: Option<Candidate>,
    #[serde(default)]
    pub last_checked: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_error: Option<String>,
    /// 対象ツール実行中で委譲更新がファイルロック失敗した場合の
    /// 「次回起動時適用」予約。dtm launch 境界で実行し成功時に解除。
    #[serde(default)]
    pub pending_pm_sync: Option<PendingPmSync>,
    #[serde(default)]
    pub recipe_revision: u64,
    /// 直近イベント(新しい順、上限保持)
    #[serde(default)]
    pub events: Vec<StateEvent>,
}

impl InstallState {
    pub fn latest_version(&self) -> Option<&str> {
        self.latest_candidate.as_ref().map(|c| c.version.as_str())
    }
}

/// 次回起動時に再実行する委譲更新の予約。
/// 実行中ツールのファイルロック (os error 32) で失敗した pm-sync を記録する。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingPmSync {
    pub manager: String,
    pub package: String,
    #[serde(default)]
    pub scheduled_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub version: String,
    /// タグ名 or 日付等の生識別子
    pub tag: String,
    pub release_url: String,
    #[serde(default)]
    pub asset_name: Option<String>,
    #[serde(default)]
    pub asset_url: Option<String>,
    /// 版比較不能の場合 true (仕様 §5.1: 自動ダウングレードしない)
    #[serde(default)]
    pub incomparable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateEvent {
    pub at: DateTime<Utc>,
    pub kind: String,
    pub detail: String,
}

// ---------- 世代メタ (generations/<id>/<gen>/genmeta.json) ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerationMeta {
    pub generation_id: String,
    pub version: String,
    pub created_at: DateTime<Utc>,
    pub recipe_revision: u64,
    pub source: GenerationSource,
    /// 配布元署名/チェックサム検証結果 (仕様 §6-3)
    pub verification: VerificationResult,
    /// 版確認コマンドの実測出力(任意)
    #[serde(default)]
    pub probed_version: Option<String>,
    /// 起動検証に合格済みか (検証失敗世代を適用・復元対象にしない。仕様 §15-3)
    /// verify 未登録レシピは検査不能のため true として扱う
    #[serde(default)]
    pub verified: bool,
    /// 実際に有効化された時刻 (復元候補の選定に使用。未適用世代は None)
    #[serde(default)]
    pub applied_at: Option<DateTime<Utc>>,
    /// 世代ペイロードの相対パス一覧 (fixed-path 適用時の旧配布物差分用)
    #[serde(default)]
    pub files: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerationSource {
    /// "github-release" | "manual-import" | "build"
    pub kind: String,
    #[serde(default)]
    pub release_url: Option<String>,
    #[serde(default)]
    pub asset_name: Option<String>,
    /// 取得物の SHA-256
    pub sha256: String,
    #[serde(default)]
    pub imported_from: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerificationResult {
    /// "verified" | "unverified" | "failed"
    pub status: String,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub detail: Option<String>,
}

// ---------- ジョブ (仕様 §7: 複数プロセス集約) ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobState {
    pub job_id: String,
    /// "check" | "prepare" | "apply" | "rollback" | "import"
    pub kind: String,
    pub pid: u32,
    pub started_at: DateTime<Utc>,
    /// "running" | "done" | "failed"
    pub status: String,
    #[serde(default)]
    pub result: Option<String>,
    #[serde(default)]
    pub finished_at: Option<DateTime<Utc>>,
}

/// 準備中の進捗 (state/jobs/<id>.progress.json)。
/// ダウンロードのワーカースレッドが単独で書き込むため JobState とは
/// 別ファイルにする (atomic_write は全置換のため同一ファイルでは
/// finish/fail の書き込みと競合し得る)。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressState {
    pub updated_at: DateTime<Utc>,
    /// "release" | "download" | "verify" | "extract" | "probe"
    pub phase: String,
    #[serde(default)]
    pub bytes_done: u64,
    #[serde(default)]
    pub bytes_total: Option<u64>,
}

// ---------- MCP 管理マッピング (state/managed-mcp.json) ----------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ManagedMap {
    pub schema_version: u32,
    #[serde(default)]
    pub entries: Vec<ManagedEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedEntry {
    /// 利用元の一意キー (config_path+server_name のハッシュ)。
    /// 同名サーバーが複数設定にある場合の識別に使用 (mcpkey:<key> で指定)
    #[serde(default)]
    pub usage_key: String,
    /// 管理対象 installationId
    pub installation_id: String,
    /// 設定ファイルパス
    pub config_path: PathBuf,
    /// mcpServers キー名
    pub server_name: String,
    /// 管理化前の元起動設定(管理解除時の復元用。仕様 §13)
    pub original: LaunchSpec,
    /// インタプリタ+スクリプト形式: original.args 内のスクリプト引数位置。
    /// 管理化時に確定し、ランチャーがこの位置を世代内エントリポイントへ差し替える。
    /// None = 従来形式 (EXE 直接起動)。後方互換のため serde default。
    #[serde(default)]
    pub script_arg_index: Option<usize>,
    /// 書き換え時点のファイル SHA-256 (競合検知用。仕様 §13)
    pub config_sha256_at_write: String,
    pub managed_at: DateTime<Utc>,
}
