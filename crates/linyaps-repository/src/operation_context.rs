use std::collections::HashMap;

use serde::Serialize;

/// 替换 D-Bus TaskContext 的简单进度报告器
/// 所有输出直接写到 stderr
pub struct OperationContext;

impl OperationContext {
    pub fn new() -> Self {
        Self
    }

    pub fn is_canceled(&self) -> bool {
        false
    }

    pub fn update_progress(&self, _progress: f64, message: &str) {
        if !message.is_empty() {
            eprintln!("  {message}");
        }
    }

    pub fn update_state_message(&self, message: &str) {
        if !message.is_empty() {
            eprintln!("{message}");
        }
    }

    /// 重置进度（兼容旧接口，等价于 update_state_message）
    pub fn reset_progress(&self, message: &str) {
        self.update_state_message(message);
    }

    pub fn send_message(&self, message: &str) {
        eprintln!("{message}");
    }

    /// 请求用户交互（升级确认）
    /// message_id == 4 表示升级确认
    pub fn request_interaction(&self, message_id: i32, additional: &HashMap<String, String>) -> bool {
        if message_id == 4 {
            let local = additional
                .get("LocalRef")
                .map(|s| s.as_str())
                .unwrap_or("unknown");
            let remote = additional
                .get("RemoteRef")
                .map(|s| s.as_str())
                .unwrap_or("unknown");
            eprint!("The lower version {local} is currently installed. Do you want to continue installing the latest version {remote}? [y/N] ");
            let mut answer = String::new();
            std::io::stdin().read_line(&mut answer).is_ok()
                && matches!(answer.trim(), "Y" | "y" | "Yes" | "yes")
        } else {
            false
        }
    }
}

/// 替换 D-Bus TaskCompletion 的简单操作结果
#[derive(Clone, Debug, Serialize)]
pub struct OperationResult {
    pub code: i64,
    pub message: String,
}

impl OperationResult {
    pub fn success(message: impl Into<String>) -> Self {
        Self {
            code: 0,
            message: message.into(),
        }
    }

    pub fn failed(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn canceled(message: impl Into<String>) -> Self {
        Self {
            code: 1,
            message: message.into(),
        }
    }
}