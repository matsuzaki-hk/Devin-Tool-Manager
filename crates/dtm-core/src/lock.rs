//! プロセス間排他 (仕様 §7)。
//! - ロック判定は「ファイルの存在」ではなく OS レベルの排他ロック取得で行う
//!   (fs2 → Windows では LockFileEx)。所有者死亡時に自動解放される。
//! - 永続ジョブ状態で進行中処理を共有し、後続要求は結果を再利用/待機する。

use crate::error::{Error, Result};
use crate::models::{InstallState, JobState};
use crate::paths::{self, Layout};
use chrono::Utc;
use fs2::FileExt;
use std::fs::{self, File, OpenOptions};
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub struct InstallationLock {
    file: File,
    pub lock_path: PathBuf,
}

impl InstallationLock {
    /// 非ブロッキングで取得。他プロセス保持中は Error::Busy。
    pub fn try_acquire(layout: &Layout, installation_id: &str) -> Result<Self> {
        fs::create_dir_all(layout.locks_dir())?;
        let p = layout.lock_file(installation_id);
        let f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&p)?;
        match f.try_lock_exclusive() {
            Ok(()) => Ok(Self {
                file: f,
                lock_path: p,
            }),
            Err(e) if is_busy_error(&e) => Err(Error::Busy(format!(
                "installation {installation_id} は他プロセスがロック中"
            ))),
            Err(e) => Err(e.into()),
        }
    }

    /// タイムアウト付きで取得。待機中はジョブ状態を参照できるよう呼び出し元で判断する。
    pub fn acquire_timeout(
        layout: &Layout,
        installation_id: &str,
        timeout: Duration,
    ) -> Result<Self> {
        let start = Instant::now();
        loop {
            match Self::try_acquire(layout, installation_id) {
                Ok(l) => return Ok(l),
                Err(Error::Busy(m)) => {
                    if start.elapsed() > timeout {
                        return Err(Error::Busy(m));
                    }
                    std::thread::sleep(Duration::from_millis(150));
                }
                Err(e) => return Err(e),
            }
        }
    }
}

impl Drop for InstallationLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// ロック競合の判定。Windows では LockFileEx 失敗が
/// ERROR_LOCK_VIOLATION(33) で返り、ErrorKind が WouldBlock にならない
/// 場合があるため raw_os_error も見る。
fn is_busy_error(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::WouldBlock || matches!(e.raw_os_error(), Some(33) | Some(997))
}

/// ジョブ状態の永続化。ロック中にだけ書き込むこと。
pub struct JobGuard<'a> {
    layout: &'a Layout,
    installation_id: String,
    pub job: JobState,
}

impl<'a> JobGuard<'a> {
    /// 新規ジョブを開始記録。ロック取得後に呼ぶ。
    /// 前回ジョブが "running" のまま残っている場合は中断済みとしてマークする
    /// (ロック再取得済み=前所有者は死亡、仕様 §7「異常終了後はロックを再取得」)。
    pub fn begin(layout: &'a Layout, installation_id: &str, kind: &str) -> Result<Self> {
        let job_path = layout.job_file(installation_id);
        if let Ok(prev) = paths::read_json::<JobState>(&job_path) {
            if prev.status == "running" {
                let mut p = prev;
                p.status = "interrupted".into();
                p.finished_at = Some(Utc::now());
                let _ = paths::atomic_write_json(&job_path, &p);
                crate::dlogw!("前回ジョブ {} ({}) は中断扱いで復旧", p.job_id, p.kind);
            }
        }
        let job = JobState {
            job_id: format!("{}-{}", Utc::now().timestamp_millis(), std::process::id()),
            kind: kind.into(),
            pid: std::process::id(),
            started_at: Utc::now(),
            status: "running".into(),
            result: None,
            finished_at: None,
        };
        paths::atomic_write_json(&job_path, &job)?;
        // 前回ジョブの進捗ファイルが残っていても新規ジョブでは無効
        clear_progress(layout, installation_id);
        Ok(Self {
            layout,
            installation_id: installation_id.into(),
            job,
        })
    }

    pub fn finish(mut self, result: &str) -> Result<()> {
        self.job.status = "done".into();
        self.job.result = Some(result.into());
        self.job.finished_at = Some(Utc::now());
        paths::atomic_write_json(&self.layout.job_file(&self.installation_id), &self.job)
    }

    pub fn fail(mut self, err: &str) -> Result<()> {
        self.job.status = "failed".into();
        self.job.result = Some(err.into());
        self.job.finished_at = Some(Utc::now());
        paths::atomic_write_json(&self.layout.job_file(&self.installation_id), &self.job)
    }
}

/// 他プロセスのジョブ完了を待つ (ロック不要で呼べる)。
/// timeout 内に done/failed/interrupted になれば状態を返す。
pub fn wait_job(layout: &Layout, installation_id: &str, timeout: Duration) -> Result<JobState> {
    let start = Instant::now();
    let path = layout.job_file(installation_id);
    loop {
        if let Ok(j) = paths::read_json::<JobState>(&path) {
            if j.status != "running" {
                return Ok(j);
            }
        }
        if start.elapsed() > timeout {
            return Err(Error::Busy(format!(
                "installation {installation_id} のジョブ完了待ちがタイムアウト"
            )));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// 現在のジョブ状態を読むだけ。
pub fn peek_job(layout: &Layout, installation_id: &str) -> Option<JobState> {
    paths::read_json::<JobState>(&layout.job_file(installation_id)).ok()
}

/// 進行中ジョブの進捗書き込み (state/jobs/<id>.progress.json)。
/// ダウンロードのワーカースレッドから使うため Arc で共有できる。
/// 書き込みは 400ms に間引き、最終確定 (done==total) は必ず書く。
pub struct Progress {
    layout: Layout,
    installation_id: String,
    last_write: std::sync::Mutex<Instant>,
}

impl Progress {
    pub fn new(layout: &Layout, installation_id: &str) -> Self {
        Self {
            layout: layout.clone(),
            installation_id: installation_id.into(),
            last_write: std::sync::Mutex::new(Instant::now() - Duration::from_secs(1)),
        }
    }

    /// フェーズ遷移のみ記録 (バイト進捗なし)。
    pub fn phase(&self, phase: &str) {
        self.write(phase, 0, None, true);
    }

    /// ダウンロード進捗。total は Content-Length が不明なら None。
    pub fn bytes(&self, done: u64, total: Option<u64>) {
        let final_write = total.is_some_and(|t| done >= t);
        self.write("download", done, total, final_write);
    }

    fn write(&self, phase: &str, done: u64, total: Option<u64>, force: bool) {
        {
            let mut last = self.last_write.lock().unwrap();
            if !force && last.elapsed() < Duration::from_millis(400) {
                return;
            }
            *last = Instant::now();
        }
        let p = crate::models::ProgressState {
            updated_at: Utc::now(),
            phase: phase.into(),
            bytes_done: done,
            bytes_total: total,
        };
        let _ = paths::atomic_write_json(&self.layout.progress_file(&self.installation_id), &p);
    }
}

/// 進捗ファイルを消す (新規ジョブ開始時・完了時の残骸防止)。
pub fn clear_progress(layout: &Layout, installation_id: &str) {
    let _ = fs::remove_file(layout.progress_file(installation_id));
}

/// 実行中ジョブの進捗を読む。ジョブが "running" でないときは
/// 残留ファイルを進捗として返さない (中断残骸と区別するため)。
pub fn active_progress(
    layout: &Layout,
    installation_id: &str,
) -> Option<crate::models::ProgressState> {
    let running = peek_job(layout, installation_id).map(|j| j.status == "running")?;
    if !running {
        return None;
    }
    paths::read_json(&layout.progress_file(installation_id)).ok()
}

/// 進行中ステータス (Checking/Preparing/Applying) が残っているのに
/// 実行者がいない場合に、中断として状態を回復する。
/// 読み取り経路 (list/status) から呼ぶ。ロック取得成功 = 実行者不在
/// (所有者死亡時に OS が自動解放するため、ファイル残存では判定しない)。
/// 戻り値は回復を行ったか (呼び出し元で st を保存する)。
pub fn heal_interrupted(layout: &Layout, installation_id: &str, st: &mut InstallState) -> bool {
    use crate::models::{StateEvent, Status};
    match st.status {
        Some(Status::Checking) | Some(Status::Preparing) | Some(Status::Applying) => {}
        _ => return false,
    }
    let _lock = match InstallationLock::try_acquire(layout, installation_id) {
        Ok(l) => l,
        Err(_) => return false,
    };
    // ジョブ記録も中断扱いへ (JobGuard::begin と同規則)
    let job_path = layout.job_file(installation_id);
    if let Ok(mut j) = paths::read_json::<JobState>(&job_path) {
        if j.status == "running" {
            j.status = "interrupted".into();
            j.finished_at = Some(Utc::now());
            let _ = paths::atomic_write_json(&job_path, &j);
        }
    }
    clear_progress(layout, installation_id);
    st.status = Some(Status::Failed);
    st.last_error = Some("前回の処理が中断されました（再試行できます）".into());
    st.events.insert(
        0,
        StateEvent {
            at: Utc::now(),
            kind: "interrupted".into(),
            detail: "進行中状態が残留していたため中断扱いに復旧".into(),
        },
    );
    true
}
