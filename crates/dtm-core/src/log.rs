//! ファイルログ。MCP stdio 経路の stdout を汚染しないため、
//! 管理ログは <root>/state/logs/dtm.log にのみ書き込む (仕様 §9, §15-9)。

use crate::paths::Layout;
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::Mutex;

static LOGGER: Mutex<Option<std::fs::File>> = Mutex::new(None);

/// グローバルロガー初期化。失敗しても処理は継続 (ログは最善努力)。
pub fn init(layout: &Layout) {
    let _ = std::fs::create_dir_all(layout.logs_dir());
    if let Ok(f) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(layout.log_file())
    {
        *LOGGER.lock().unwrap() = Some(f);
    }
}

pub fn log(level: &str, msg: &str) {
    let mut g = LOGGER.lock().unwrap();
    if let Some(f) = g.as_mut() {
        let ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.3f%z");
        let _ = writeln!(f, "{ts} {level} pid={} {msg}", std::process::id());
        let _ = f.flush();
    }
}

#[macro_export]
macro_rules! dlog {
    ($($arg:tt)*) => {
        $crate::log::log("INFO", &format!($($arg)*))
    };
}

#[macro_export]
macro_rules! dlogw {
    ($($arg:tt)*) => {
        $crate::log::log("WARN", &format!($($arg)*))
    };
}

#[macro_export]
macro_rules! dloge {
    ($($arg:tt)*) => {
        $crate::log::log("ERROR", &format!($($arg)*))
    };
}
