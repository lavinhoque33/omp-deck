//! Public runtime Firebase options for new Android companions. Read once at gateway startup;
//! only the selected, validated public fields cross the authenticated encrypted device feed.
//! Put `google-services.json` beside the private FCM account, or set
//! `OMP_DECK_FCM_ANDROID_CONFIG` to its file. Both must describe the same Firebase project.

use std::io::Read as _;
use std::path::Path;

use serde_json::{Value, json};
use zeroize::Zeroizing;

use super::fcm;

pub(super) const FILE: &str = "google-services.json";
pub(super) const ENV: &str = "OMP_DECK_FCM_ANDROID_CONFIG";
const MAX_BYTES: usize = 256 * 1024;
const PACKAGE: &str = "io.github.lavinhoque33.ompdeck";

pub(super) fn load(dir: &Path, sender: Option<&fcm::Config>) -> Option<String> {
    load_from(dir, std::env::var_os(ENV).filter(|v| !v.is_empty()), sender)
}

fn load_from(
    dir: &Path,
    explicit: Option<std::ffi::OsString>,
    sender: Option<&fcm::Config>,
) -> Option<String> {
    let (path, explicit) = match explicit {
        Some(path) => (std::path::PathBuf::from(path), true),
        None => (dir.join(FILE), false),
    };
    let read = std::fs::File::open(path).and_then(|file| {
        let mut bytes = Zeroizing::new(Vec::new());
        file.take(MAX_BYTES as u64 + 1).read_to_end(&mut bytes)?;
        Ok(bytes)
    });
    let bytes = match read {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !explicit => return None,
        Err(error) => {
            // Neither paths nor user-controlled file contents belong in these diagnostics.
            log::error!(
                "Android automatic setup is unavailable: cannot read public Firebase config ({:?}); \
                 check {ENV}, or put {FILE} in the gateway secrets directory, then restart the gateway",
                error.kind()
            );
            return None;
        }
    };
    match normalize(&bytes, sender.map(fcm::Config::project)) {
        Ok(config) => Some(config),
        Err(reason) => {
            log::error!(
                "Android automatic setup is unavailable: {reason}; use the Firebase Android \
                 {FILE} for {PACKAGE} and the same project as the gateway's valid \
                 fcm-service-account.json ({credentials_env}); check {ENV} or the gateway \
                 secrets directory, then restart the gateway",
                credentials_env = fcm::CREDENTIALS_ENV
            );
            None
        }
    }
}

/// Mirrors the Android GoogleServices parser, with a strict sender-project match and a
/// whitelist-only output. Error strings never include data supplied by the file.
fn normalize(bytes: &[u8], sender_project: Option<&str>) -> Result<String, &'static str> {
    if bytes.len() > MAX_BYTES {
        return Err("public Firebase config exceeds 256 KiB");
    }
    let root: Value =
        serde_json::from_slice(bytes).map_err(|_| "public Firebase config is not valid JSON")?;
    let info = root.get("project_info").ok_or("project_info is missing")?;
    let project = info["project_id"]
        .as_str()
        .ok_or("project_id is missing or malformed")?;
    let project_bytes = project.as_bytes();
    if !(5..=30).contains(&project_bytes.len())
        || !project_bytes[0].is_ascii_lowercase()
        || !project_bytes[project_bytes.len() - 1].is_ascii_alphanumeric()
        || !project_bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
    {
        return Err("project_id is missing or malformed");
    }
    let number = info["project_number"]
        .as_str()
        .ok_or("project_number is missing or malformed")?;
    if !(6..=20).contains(&number.len()) || !number.bytes().all(|b| b.is_ascii_digit()) {
        return Err("project_number is missing or malformed");
    }
    let bucket = match info.get("storage_bucket") {
        None | Some(Value::Null) => None,
        Some(Value::String(bucket)) if bucket.is_empty() => None,
        Some(Value::String(bucket))
            if (3..=222).contains(&bucket.len())
                && bucket.bytes().all(|b| {
                    b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
                }) =>
        {
            Some(bucket)
        }
        _ => return Err("storage_bucket is malformed"),
    };
    let clients = root["client"]
        .as_array()
        .ok_or("Android clients are missing")?;
    let client = clients
        .iter()
        .find(|client| {
            client["client_info"]["android_client_info"]["package_name"].as_str() == Some(PACKAGE)
        })
        .ok_or("there is no Android client for the OmpDeck package")?;
    let app = client["client_info"]["mobilesdk_app_id"]
        .as_str()
        .ok_or("mobilesdk_app_id is missing or malformed")?;
    let mut app_parts = app.split(':');
    if app_parts.next() != Some("1")
        || app_parts.next() != Some(number)
        || app_parts.next() != Some("android")
        || !app_parts.next().is_some_and(|suffix| {
            (8..=64).contains(&suffix.len())
                && suffix
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
        || app_parts.next().is_some()
    {
        return Err("mobilesdk_app_id is malformed or belongs to another project");
    }
    let key = client["api_key"]
        .as_array()
        .and_then(|keys| {
            keys.iter()
                .filter_map(|key| key["current_key"].as_str())
                .find(|key| {
                    (20..=128).contains(&key.len())
                        && key
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
                })
        })
        .ok_or("there is no usable Android API key")?;
    let sender_project = sender_project.ok_or("the gateway has no valid FCM service account")?;
    if project != sender_project {
        return Err("public Firebase config and FCM service account projects do not match");
    }

    let mut project_info = json!({"project_id": project, "project_number": number});
    if let Some(bucket) = bucket {
        project_info["storage_bucket"] = json!(bucket);
    }
    Ok(json!({
        "project_info": project_info,
        "client": [{
            "client_info": {
                "mobilesdk_app_id": app,
                "android_client_info": {"package_name": PACKAGE},
            },
            "api_key": [{"current_key": key}],
        }],
    })
    .to_string())
}

#[cfg(test)]
mod tests;
