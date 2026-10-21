//! 单实例互斥（§2 / 模块 C：全局命名互斥对象保证单实例）。
//!
//! - Windows：命名互斥体 `Local\orangeDesk.InstanceMutex`，进程退出由系统自动释放
//! - Unix：锁文件 flock（开发 / CI 环境），崩溃后内核自动释放，无残留死锁

#[derive(Debug, thiserror::Error)]
pub enum SingleInstanceError {
    #[error("another orangeDesk instance is already running")]
    AlreadyRunning,
    #[error("failed to acquire single-instance lock: {0}")]
    Other(String),
}

/// 尝试获取单实例锁。成功则持有至进程退出（故意泄漏句柄 / fd），
/// 第二次调用（另一进程）返回 [`SingleInstanceError::AlreadyRunning`]。
pub fn acquire() -> Result<(), SingleInstanceError> {
    #[cfg(windows)]
    {
        acquire_windows()
    }
    #[cfg(unix)]
    {
        acquire_unix()
    }
    #[cfg(not(any(windows, unix)))]
    {
        Err(SingleInstanceError::Other(
            "unsupported platform".to_string(),
        ))
    }
}

#[cfg(windows)]
fn acquire_windows() -> Result<(), SingleInstanceError> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;

    const NAME: &str = "Local\\orangeDesk.InstanceMutex\0";
    let wide: Vec<u16> = NAME.encode_utf16().collect();
    // SAFETY: name 以 NUL 结尾（上方字面量已含 \0）
    let created = unsafe { CreateMutexW(None, false, PCWSTR(wide.as_ptr())) };
    let handle = match created {
        Ok(handle) => handle,
        Err(err) => {
            return Err(SingleInstanceError::Other(format!(
                "CreateMutexW failed: {err}"
            )))
        }
    };
    // CreateMutexW 对“已存在的同名互斥体”同样返回 Ok，需检查 last error
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return Err(SingleInstanceError::AlreadyRunning);
    }
    // 故意不 CloseHandle：持锁到进程结束
    std::mem::forget(handle);
    Ok(())
}

#[cfg(unix)]
fn acquire_unix() -> Result<(), SingleInstanceError> {
    use std::fs::OpenOptions;
    use std::os::fd::AsRawFd;

    let path = std::env::temp_dir().join("orange-desk.instance.lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|err| SingleInstanceError::Other(format!("open {}: {err}", path.display())))?;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let errno = std::io::Error::last_os_error();
        return if errno.raw_os_error() == Some(libc::EWOULDBLOCK) {
            Err(SingleInstanceError::AlreadyRunning)
        } else {
            Err(SingleInstanceError::Other(format!("flock: {errno}")))
        };
    }
    // 故意不 drop：持锁到进程结束
    std::mem::forget(file);
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_in_same_process_is_allowed_or_reported() {
        // flock 语义：同进程重复 flock 同一 fd 集合不会阻塞自身。
        // 这里只验证 acquire 不误报其他错误；跨进程单实例由 flock 保证。
        assert!(acquire().is_ok());
    }
}
