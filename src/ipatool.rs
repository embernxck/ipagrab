//! Запуск ipatool.exe в фоне (без окна консоли) и разбор его JSON-вывода.

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub enum LoginResult {
    NeedCode,
    Ok { name: String, email: String },
}

/// Ищет ipatool: рядом с программой, потом на рабочем столе.
pub fn find_ipatool(saved: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = saved {
        if p.is_file() {
            return Some(p.to_path_buf());
        }
    }
    let mut dirs = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            dirs.push(d.to_path_buf());
        }
    }
    if let Ok(home) = std::env::var("USERPROFILE") {
        dirs.push(PathBuf::from(&home).join("Desktop"));
        dirs.push(PathBuf::from(&home).join("Downloads"));
    }
    for d in dirs {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_lowercase();
            if name.starts_with("ipatool") && name.ends_with(".exe") {
                return Some(e.path());
            }
        }
    }
    None
}

/// Запускает ipatool с аргументами, возвращает строки JSON-лога или текст ошибки.
fn run(exe: &Path, passphrase: &str, args: &[&str]) -> Result<Vec<Value>, String> {
    let out = Command::new(exe)
        .args(args)
        .args(["--format", "json", "--non-interactive", "--keychain-passphrase", passphrase])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("Не удалось запустить ipatool: {e}"))?;

    let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    let lines: Vec<Value> = text
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
        .collect();

    if let Some(err) = lines.iter().find(|v| v["level"] == "error") {
        let msg = err["error"].as_str().or(err["message"].as_str()).unwrap_or("неизвестная ошибка");
        return Err(humanize(msg));
    }
    if lines.is_empty() && !out.status.success() {
        return Err(humanize(text.trim()));
    }
    Ok(lines)
}

pub fn account_info(exe: &Path, pp: &str) -> Result<(String, String), String> {
    let lines = run(exe, pp, &["auth", "info"])?;
    let v = lines
        .iter()
        .find(|v| v["success"] == true)
        .ok_or_else(|| "Нужно войти в Apple ID".to_string())?;
    Ok((
        v["name"].as_str().unwrap_or("").to_string(),
        v["email"].as_str().unwrap_or("").to_string(),
    ))
}

pub fn login(exe: &Path, pp: &str, email: &str, password: &str, code: &str) -> Result<LoginResult, String> {
    let mut args = vec!["auth", "login", "-e", email, "-p", password];
    if !code.is_empty() {
        args.push("--auth-code");
        args.push(code);
    }
    let lines = run(exe, pp, &args)?;
    if let Some(v) = lines.iter().find(|v| v["success"] == true) {
        return Ok(LoginResult::Ok {
            name: v["name"].as_str().unwrap_or("").to_string(),
            email: v["email"].as_str().unwrap_or(email).to_string(),
        });
    }
    if lines
        .iter()
        .any(|v| v["message"].as_str().is_some_and(|m| m.contains("2FA")))
    {
        return Ok(LoginResult::NeedCode);
    }
    Err("Apple не ответил, попробуй ещё раз".into())
}

pub fn logout(exe: &Path, pp: &str) {
    let _ = run(exe, pp, &["auth", "revoke"]);
}

pub fn download(exe: &Path, pp: &str, app_id: u64, out: &Path, free: bool) -> Result<PathBuf, String> {
    let id = app_id.to_string();
    let out_s = out.to_string_lossy().to_string();
    let mut args = vec!["download", "-i", id.as_str(), "-o", out_s.as_str()];
    if free {
        args.push("--purchase");
    }
    let lines = run(exe, pp, &args)?;
    match lines.iter().find(|v| v["success"] == true) {
        Some(v) => Ok(v["output"].as_str().map(PathBuf::from).unwrap_or_else(|| out.to_path_buf())),
        None => Err("ipatool не сообщил об успехе".into()),
    }
}

/// Переводит ошибки ipatool/Apple на понятный язык.
pub fn humanize(msg: &str) -> String {
    let m = msg.to_lowercase();
    let nice = if m.contains("license") && m.contains("required") {
        "Этого приложения нет в истории покупок твоего Apple ID, и Apple его не даёт"
    } else if m.contains("password token is expired") {
        "Сессия истекла, войди заново"
    } else if m.contains("failed to get account") || m.contains("keyring") || m.contains("keychain") {
        "Нужно войти в Apple ID"
    } else if m.contains("dial tcp") || m.contains("timeout") || m.contains("connection") {
        "Нет связи с Apple. Попробуй ещё раз или включи/выключи VPN"
    } else if m.contains("invalid") && (m.contains("credential") || m.contains("password")) || m.contains("bad login") {
        "Неверная почта, пароль или код"
    } else if m.contains("paid apps") {
        "Платные приложения программа не покупает"
    } else if m.contains("temporarily unavailable") {
        "Apple сейчас не отдаёт это приложение, попробуй позже"
    } else if m.contains("not found") {
        "Приложение не найдено"
    } else {
        return msg.to_string();
    };
    nice.to_string()
}
