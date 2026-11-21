//! Айфоны по USB (через службу Apple Mobile Device) и установка .ipa.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use eframe::egui;
use idevice::IdeviceService;
use idevice::services::installation_proxy::InstallationProxyClient;
use idevice::services::lockdown::LockdownClient;
use idevice::services::springboardservices::SpringBoardServicesClient;
use idevice::usbmuxd::{Connection, UsbmuxdAddr, UsbmuxdConnection};

#[derive(Clone, Debug, PartialEq)]
pub struct Device {
    pub udid: String,
    pub name: String,
}

pub struct IpaInfo {
    pub name: String,
    pub version: String,
    pub bundle: String,
    pub icon: Option<egui::ColorImage>,
}

/// Список подключённых по кабелю устройств. Имена кэшируются по UDID.
pub async fn list(names: &mut HashMap<String, String>) -> Result<Vec<Device>, String> {
    let mut mux = UsbmuxdConnection::default()
        .await
        .map_err(|_| "Нет драйвера Apple. Установи «Apple Devices» из Microsoft Store или iTunes".to_string())?;
    let devs = mux.get_devices().await.map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for d in devs.iter().filter(|d| matches!(d.connection_type, Connection::Usb)) {
        let name = match names.get(&d.udid) {
            Some(n) => n.clone(),
            None => {
                let provider = d.to_provider(UsbmuxdAddr::default(), "ipagrab");
                let n = match LockdownClient::connect(&provider).await {
                    Ok(mut l) => l
                        .get_value(Some("DeviceName"), None)
                        .await
                        .ok()
                        .and_then(|v| v.as_string().map(str::to_string))
                        .unwrap_or_else(|| "iPhone".into()),
                    Err(_) => "iPhone".into(),
                };
                names.insert(d.udid.clone(), n.clone());
                n
            }
        };
        if !out.iter().any(|x: &Device| x.udid == d.udid) {
            out.push(Device { udid: d.udid.clone(), name });
        }
    }
    Ok(out)
}

/// Приложения из App Store, которые стоят на телефоне (с их номером в App Store).
pub async fn phone_apps(udid: &str) -> Result<Vec<crate::store::AppItem>, String> {
    let provider = provider_for(udid).await?;
    let mut ip = InstallationProxyClient::connect(&provider).await.map_err(|e| humanize(&e.to_string()))?;
    let attrs = [
        "CFBundleIdentifier",
        "CFBundleDisplayName",
        "CFBundleName",
        "CFBundleShortVersionString",
        "iTunesMetadata",
    ];
    let opts = plist::Value::Dictionary(plist::Dictionary::from_iter([
        ("ApplicationType".to_string(), plist::Value::String("User".into())),
        (
            "ReturnAttributes".to_string(),
            plist::Value::Array(attrs.iter().map(|s| plist::Value::String(s.to_string())).collect()),
        ),
    ]));
    let list = ip.browse(Some(opts)).await.map_err(|e| humanize(&e.to_string()))?;

    let mut out = Vec::new();
    for v in list {
        let Some(d) = v.as_dictionary() else { continue };
        let s = |k: &str| d.get(k).and_then(|x| x.as_string()).unwrap_or("").trim_start_matches('\u{200e}').to_string();
        let Some(meta) = d
            .get("iTunesMetadata")
            .and_then(|x| x.as_data())
            .and_then(|b| plist::Value::from_reader(std::io::Cursor::new(b)).ok())
        else {
            continue; // не из App Store (TestFlight, свои сборки) — скачать нельзя
        };
        let Some(m) = meta.as_dictionary() else { continue };
        let Some(id) = m.get("itemId").and_then(|x| x.as_unsigned_integer()) else { continue };
        let price = m.get("price").and_then(|x| x.as_real().or(x.as_unsigned_integer().map(|u| u as f64))).unwrap_or(0.0);
        let mut name = s("CFBundleDisplayName");
        if name.is_empty() {
            name = s("CFBundleName");
        }
        let artist = m.get("artistName").and_then(|x| x.as_string()).unwrap_or("").to_string();
        let ver = s("CFBundleShortVersionString");
        let subtitle = match (artist.is_empty(), ver.is_empty()) {
            (false, false) => format!("{artist} · версия {ver}"),
            (false, true) => artist,
            (true, false) => format!("версия {ver}"),
            _ => String::new(),
        };
        out.push(crate::store::AppItem {
            id,
            name,
            subtitle,
            icon_url: None,
            free: price == 0.0,
            size: None,
            bundle: s("CFBundleIdentifier"),
        });
    }
    out.sort_by_key(|a| a.name.to_lowercase());
    Ok(out)
}

/// Иконка приложения прямо с экрана «Домой» телефона.
pub async fn phone_icons(udid: &str, apps: &[(u64, String)], mut each: impl FnMut(u64, Vec<u8>)) -> Result<(), String> {
    let provider = provider_for(udid).await?;
    let mut sb = SpringBoardServicesClient::connect(&provider).await.map_err(|e| e.to_string())?;
    for (id, bundle) in apps {
        if let Ok(png) = sb.get_icon_pngdata(bundle.clone()).await {
            if !png.is_empty() {
                each(*id, png);
            }
        }
    }
    Ok(())
}

async fn provider_for(udid: &str) -> Result<idevice::provider::UsbmuxdProvider, String> {
    let mut mux = UsbmuxdConnection::default().await.map_err(|e| e.to_string())?;
    let devs = mux.get_devices().await.map_err(|e| e.to_string())?;
    let dev = devs.iter().find(|d| d.udid == udid).ok_or("Телефон отключился")?;
    Ok(dev.to_provider(UsbmuxdAddr::default(), "ipagrab"))
}

pub async fn install(
    udid: &str,
    path: &Path,
    on_progress: impl Fn(u64) + Clone,
) -> Result<(), String> {
    let provider = provider_for(udid).await?;
    idevice::utils::installation::install_package_with_callback(
        &provider,
        path,
        None,
        |(p, cb): (u64, _)| {
            cb(p);
            async {}
        },
        on_progress,
    )
    .await
    .map_err(|e| humanize(&e.to_string()))
}

fn humanize(e: &str) -> String {
    let l = e.to_lowercase();
    if l.contains("password") || l.contains("locked") {
        "Разблокируй телефон и попробуй ещё раз".into()
    } else if l.contains("pair") || l.contains("trust") {
        "Телефон не доверяет компьютеру: разблокируй его и нажми «Доверять»".into()
    } else if l.contains("applicationverificationfailed") || l.contains("verification") {
        "Телефон не принял приложение (не та подпись или не тот Apple ID)".into()
    } else if l.contains("devicefamily") || l.contains("minimumos") || l.contains("incompatible") {
        "Эта версия приложения не подходит к iOS на телефоне".into()
    } else {
        e.to_string()
    }
}

/// Название, версия и иконка из .ipa (iTunesMetadata.plist + iTunesArtwork).
pub fn ipa_info(path: &PathBuf) -> IpaInfo {
    let fallback = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let mut info = IpaInfo { name: fallback, version: String::new(), bundle: String::new(), icon: None };
    let Ok(file) = std::fs::File::open(path) else { return info };
    let Ok(mut zip) = zip::ZipArchive::new(file) else { return info };

    let mut read = |name: &str| -> Option<Vec<u8>> {
        let mut f = zip.by_name(name).ok()?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).ok()?;
        Some(buf)
    };

    if let Some(meta) = read("iTunesMetadata.plist") {
        if let Ok(v) = plist::Value::from_reader(std::io::Cursor::new(meta)) {
            if let Some(d) = v.as_dictionary() {
                let s = |k: &str| d.get(k).and_then(|x| x.as_string()).unwrap_or("").to_string();
                if !s("itemName").is_empty() {
                    info.name = s("itemName");
                }
                info.version = s("bundleShortVersionString");
                info.bundle = s("softwareVersionBundleId");
            }
        }
    }
    if let Some(art) = read("iTunesArtwork") {
        info.icon = crate::store::decode_icon(&art);
    }
    info
}
