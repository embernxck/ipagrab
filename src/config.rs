//! Настройки в %APPDATA%\ipagrab\config.json. Пароль хранилища ipatool шифруется DPAPI (только этот пользователь Windows).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Cryptography::{
    BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom, CRYPT_INTEGER_BLOB, CryptProtectData, CryptUnprotectData,
};

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Config {
    pub email: String,
    pub ipatool: Option<PathBuf>,
    pub out_dir: Option<PathBuf>,
    /// DPAPI-шифрованный пароль хранилища ipatool, hex
    pub vault: Option<String>,
}

fn path() -> PathBuf {
    let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    PathBuf::from(base).join("ipagrab").join("config.json")
}

impl Config {
    pub fn load() -> Self {
        std::fs::read_to_string(path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let p = path();
        let _ = std::fs::create_dir_all(p.parent().unwrap());
        let _ = std::fs::write(p, serde_json::to_string_pretty(self).unwrap_or_default());
    }

    pub fn out_dir(&self) -> PathBuf {
        self.out_dir.clone().unwrap_or_else(|| {
            let home = std::env::var("USERPROFILE").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join("Downloads").join("IPA")
        })
    }

    /// Пароль хранилища: расшифровать сохранённый или придумать новый.
    pub fn passphrase(&mut self) -> String {
        if let Some(p) = self.vault.as_deref().and_then(unprotect) {
            return p;
        }
        let p = random_passphrase();
        self.vault = protect(&p);
        self.save();
        p
    }
}

fn random_passphrase() -> String {
    const CH: &[u8] = b"abcdefghjkmnpqrstuvwxyzABCDEFGHJKMNPQRSTUVWXYZ23456789";
    let mut buf = [0u8; 24];
    unsafe {
        BCryptGenRandom(std::ptr::null_mut(), buf.as_mut_ptr(), buf.len() as u32, BCRYPT_USE_SYSTEM_PREFERRED_RNG);
    }
    buf.iter().map(|b| CH[*b as usize % CH.len()] as char).collect()
}

fn protect(s: &str) -> Option<String> {
    let input = CRYPT_INTEGER_BLOB { cbData: s.len() as u32, pbData: s.as_ptr() as *mut u8 };
    let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
    unsafe {
        if CryptProtectData(&input, std::ptr::null(), std::ptr::null(), std::ptr::null(), std::ptr::null(), 0, &mut out) == 0 {
            return None;
        }
        let bytes = std::slice::from_raw_parts(out.pbData, out.cbData as usize);
        let hex = bytes.iter().map(|b| format!("{b:02x}")).collect();
        LocalFree(out.pbData as _);
        Some(hex)
    }
}

fn unprotect(hex: &str) -> Option<String> {
    let mut bytes: Vec<u8> = (0..hex.len() / 2)
        .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16))
        .collect::<Result<_, _>>()
        .ok()?;
    let input = CRYPT_INTEGER_BLOB { cbData: bytes.len() as u32, pbData: bytes.as_mut_ptr() };
    let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
    unsafe {
        if CryptUnprotectData(&input, std::ptr::null_mut(), std::ptr::null(), std::ptr::null(), std::ptr::null(), 0, &mut out) == 0 {
            return None;
        }
        let s = String::from_utf8_lossy(std::slice::from_raw_parts(out.pbData, out.cbData as usize)).to_string();
        LocalFree(out.pbData as _);
        Some(s)
    }
}
