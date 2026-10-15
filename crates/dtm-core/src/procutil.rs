//! 子プロセス生成の共通補助。
//! コンソールを持たない親 (DTM拡張から windowsHide で起動された dtm.exe 等)
//! からコンソールアプリを spawn すると、Windows が子用の新規コンソール
//! ウィンドウを都度作成して点滅する。stdio をパイプ回収する子プロセスには
//! CREATE_NO_WINDOW を付けて抑制する。

use std::process::Command;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// パイプ回収等でコンソールを必要としない子プロセスに適用する。
/// 子の入出力は stdio 設定に従うためコンソールは不要。
#[cfg(windows)]
pub fn hide_window(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
pub fn hide_window(_cmd: &mut Command) {}

/// 呼び出し元プロセスがコンソールを持つ場合だけ true。
/// Stdio::inherit で対話的に使う子は、親にコンソールがある時だけ
/// そのコンソールを継承させる (無い時は hide_window で新規窓を抑制)。
#[cfg(windows)]
pub fn parent_has_console() -> bool {
    extern "system" {
        fn GetConsoleWindow() -> *mut core::ffi::c_void;
    }
    unsafe { !GetConsoleWindow().is_null() }
}

#[cfg(not(windows))]
pub fn parent_has_console() -> bool {
    true
}
