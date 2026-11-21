//! Запрос кода 2FA так же, как в Xcode/Impactor: вход по SRP на gsa.apple.com и явная просьба
//! прислать код на доверенные устройства (`GET /auth/verify/trusteddevice`) или по SMS.
//! У ipatool (старый вход iTunes) такой команды нет: там код приходит, только если Apple сам
//! решит его прислать, — отсюда задержки в десятки минут.
//!
//! Пароль уходит только в Apple (SRP) и нигде не сохраняется.

use std::path::PathBuf;

use plume_core::AnisetteConfiguration;
use plume_core::auth::anisette_data::AnisetteData;
use plume_core::auth::{Account, LoginState};

pub enum Start {
    /// Код ушёл на доверенные устройства (айфон, мак).
    CodeOnDevices,
    /// Код ушёл по SMS.
    CodeBySms,
    /// Apple пустил без кода.
    NoCode,
}

pub enum StartError {
    /// Apple перестал слать коды на этот Apple ID — ждать.
    RateLimited,
    Other(String),
}

fn anisette_dir() -> PathBuf {
    let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into());
    let p = PathBuf::from(base).join("ipagrab").join("anisette");
    let _ = std::fs::create_dir_all(&p);
    p
}

/// Один вход: пароль → одна просьба прислать код. Без повторов.
pub fn request_code(email: &str, password: &str) -> Result<Start, StartError> {
    // в сборке есть и ring, и aws-lc — rustls надо сказать, какой брать
    let _ = rustls::crypto::ring::default_provider().install_default();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| StartError::Other(e.to_string()))?;
    let (email, password) = (email.to_string(), password.to_string());
    // plume_core местами делает unwrap — ловим панику, чтобы окно не падало
    rt.block_on(async move {
        tokio::spawn(async move { flow(&email, &password).await })
            .await
            .unwrap_or_else(|_| Err(StartError::Other("Apple ответил неожиданно, попробуй позже".into())))
    })
}

async fn flow(email: &str, password: &str) -> Result<Start, StartError> {
    let fail = |e: plume_core::Error| classify(&format!("{e} {e:?}"));
    let anisette = AnisetteData::new(AnisetteConfiguration::default().set_configuration_path(anisette_dir()))
        .await
        .map_err(|e| StartError::Other(format!("Не удалось подготовить вход (anisette): {e}")))?;
    let mut account = Account::new_with_anisette(anisette).map_err(fail)?;
    let state = account.login_email_pass(email, password).await.map_err(fail)?;
    match state {
        LoginState::LoggedIn => Ok(Start::NoCode),
        LoginState::NeedsDevice2FA | LoginState::Needs2FAVerification => {
            account.send_2fa_to_devices().await.map_err(fail)?;
            Ok(Start::CodeOnDevices)
        }
        LoginState::NeedsSMS2FA | LoginState::NeedsSMS2FAVerification(_) => {
            // `GET /auth` знает номер; ответ 201 значит, что SMS Apple уже отправил сам
            let extras = account.get_auth_extras().await.map_err(fail)?;
            if matches!(extras.new_state, Some(LoginState::NeedsSMS2FAVerification(_))) {
                return Ok(Start::CodeBySms);
            }
            let id = extras.trusted_phone_numbers.first().map(|p| p.id).unwrap_or(1);
            account.send_sms_2fa_to_devices(id).await.map_err(fail)?;
            Ok(Start::CodeBySms)
        }
        LoginState::NeedsExtraStep(step) => Err(StartError::Other(format!(
            "Apple просит сначала сделать что-то в аккаунте ({step}). Зайди на appleid.apple.com"
        ))),
        LoginState::NeedsLogin => Err(StartError::Other("Apple не принял вход, попробуй позже".into())),
    }
}

fn classify(raw: &str) -> StartError {
    if std::env::var("IPAGRAB_DEBUG").is_ok() {
        eprintln!("gsa raw: {raw}");
    }
    let l = raw.to_lowercase();
    // как в Luvcore installer: признаки того, что Apple временно не шлёт коды
    let limited = ["-22979", "-22981", "-28248", "too many verification codes", "failed to send sms", "failed to send 2fa"]
        .iter()
        .any(|k| l.contains(k));
    if limited {
        return StartError::RateLimited;
    }
    let says = |k: &[&str]| k.iter().any(|n| l.contains(n));
    // тексты и коды — те же, что разбирает Luvcore installer
    let msg = if l.contains("failed to send") && l.contains("error 403") {
        "Apple не отправляет код: у этого Apple ID включены ключи безопасности. Убери их на айфоне (Вход и безопасность)"
    } else if l.contains("failed to send") && l.contains("error 500") {
        "Apple не смог отправить код (ошибка у Apple). Подожди пару минут"
    } else if says(&["error 429", "too many requests"]) {
        "Apple просит подождать: слишком много запросов. Подожди минуту"
    } else if says(&["-20209", "account has been locked", "account locked", "account is locked", "has been disabled", "account is disabled"]) {
        "Apple ID заблокирован. Разблокируй его через айфон (Настройки → имя) или на iforgot.apple.com"
    } else if says(&["-20101", "-22406", "incorrect", "enter the correct password", "invalid apple id or password"]) {
        "Неверная почта или пароль"
    } else if says(&["extra step required"]) {
        "Apple просит включить двухфакторную защиту или принять условия на account.apple.com"
    } else if says(&["error 401", "error 403"]) {
        "Apple не пустил: проверь почту и пароль. Если всё верно — Apple ID может быть заблокирован (iforgot.apple.com)"
    } else if says(&["error sending request", "dns error", "timed out", "connection refused", "connection reset", "tcp connect"]) {
        "Нет связи с Apple. Проверь интернет или VPN"
    } else {
        return StartError::Other(raw.split(' ').take(30).collect::<Vec<_>>().join(" "));
    };
    let msg = msg.to_string();
    StartError::Other(msg)
}
