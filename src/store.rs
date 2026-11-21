//! Поиск в App Store (iTunes Search API) и иконки.

use std::path::PathBuf;

use eframe::egui;
use serde_json::Value;

#[derive(Clone, Debug)]
pub struct AppItem {
    pub id: u64,
    pub name: String,
    pub subtitle: String,
    pub icon_url: Option<String>,
    pub free: bool,
    pub size: Option<u64>,
    pub bundle: String,
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(20)))
        .build()
        .into()
}

fn item_from(v: &Value) -> Option<AppItem> {
    Some(AppItem {
        id: v["trackId"].as_u64()?,
        name: v["trackName"].as_str()?.to_string(),
        subtitle: v["sellerName"].as_str().unwrap_or("").to_string(),
        icon_url: v["artworkUrl512"]
            .as_str()
            .or(v["artworkUrl100"].as_str())
            .map(str::to_string),
        free: v["price"].as_f64().unwrap_or(0.0) == 0.0,
        size: v["fileSizeBytes"].as_str().and_then(|s| s.parse().ok()),
        bundle: v["bundleId"].as_str().unwrap_or("").to_string(),
    })
}

pub fn search(term: &str) -> Result<Vec<AppItem>, String> {
    let mut resp = agent()
        .get("https://itunes.apple.com/search")
        .query("term", term)
        .query("country", "ru")
        .query("entity", "software")
        .query("limit", "30")
        .call()
        .map_err(|e| format!("Поиск не удался: {e}"))?;
    let v: Value = resp.body_mut().read_json().map_err(|e| e.to_string())?;
    Ok(v["results"].as_array().map(|a| a.iter().filter_map(item_from).collect()).unwrap_or_default())
}

/// Ищет приложение по ID в нескольких странах — удалённые из РФ часто есть в других.
pub fn lookup(id: u64) -> Option<AppItem> {
    for country in ["ru", "us", "kz", "by", "am", "ge", "de", "tr"] {
        let Ok(mut resp) = agent()
            .get("https://itunes.apple.com/lookup")
            .query("id", id.to_string())
            .query("country", country)
            .call()
        else {
            continue;
        };
        let Ok(v) = resp.body_mut().read_json::<Value>() else { continue };
        if let Some(item) = v["results"].as_array().and_then(|a| a.first()).and_then(item_from) {
            return Some(item);
        }
    }
    None
}

pub fn icon_cache(id: u64) -> PathBuf {
    let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into());
    PathBuf::from(base).join("ipagrab").join("icons").join(format!("{id}.img"))
}

/// Возвращает иконку 96×96 в RGBA (из кэша или из сети).
pub fn icon(id: u64, url: Option<String>) -> Option<egui::ColorImage> {
    let cache = icon_cache(id);
    let bytes = match std::fs::read(&cache) {
        Ok(b) => b,
        Err(_) => {
            let url = url.or_else(|| lookup(id).and_then(|i| i.icon_url))?;
            let mut resp = agent().get(&url).call().ok()?;
            let b = resp.body_mut().read_to_vec().ok()?;
            let _ = std::fs::create_dir_all(cache.parent()?);
            let _ = std::fs::write(&cache, &b);
            b
        }
    };
    decode_icon(&bytes)
}

/// Иконка только из кэша на диске (для приложений с телефона — их иконки приносит телефон).
pub fn cached_icon(id: u64) -> Option<egui::ColorImage> {
    decode_icon(&std::fs::read(icon_cache(id)).ok()?)
}

pub fn save_icon(id: u64, bytes: &[u8]) {
    let p = icon_cache(id);
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let _ = std::fs::write(p, bytes);
}

pub fn decode_icon(bytes: &[u8]) -> Option<egui::ColorImage> {
    let img = image::load_from_memory(bytes).ok()?.thumbnail(96, 96).to_rgba8();
    let size = [img.width() as usize, img.height() as usize];
    Some(egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw()))
}
