//! Integrates local application settings.
use std::{collections::BTreeMap, fs, path::PathBuf};

use serde_json::Value;

use crate::{Error, Result};

const PROXY_KEYS: [&str; 5] = [
    "http.proxy",
    "http.proxyKerberosServicePrincipal",
    "http.proxySupport",
    "cursor.general.disableHttp2",
    "http.experimental.systemCertificatesV2",
];

fn path() -> Result<PathBuf> {
    let home = dirs::home_dir()
        .ok_or_else(|| Error::Config("cannot resolve user home directory".into()))?;
    match std::env::consts::OS {
        "macos" => Ok(home.join("Library/Application Support/Cursor/User/settings.json")),
        "windows" => Ok(std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("AppData/Roaming"))
            .join("Cursor/User/settings.json")),
        "linux" => Ok(std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
            .join("Cursor/User/settings.json")),
        platform => Err(Error::Config(format!(
            "Cursor settings are unsupported on {platform}"
        ))),
    }
}

pub(super) fn read() -> Result<BTreeMap<String, Value>> {
    let path = path()?;
    let data = match fs::read_to_string(path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(error.into()),
    };
    if data.trim().is_empty() {
        return Ok(BTreeMap::new());
    }
    json5::from_str(&data)
        .map_err(|error| Error::Config(format!("parse Cursor settings JSONC: {error}")))
}

pub(super) fn write(settings: &BTreeMap<String, Value>) -> Result<()> {
    let path = path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let data = serde_json::to_vec_pretty(settings)?;
    let temp = path.with_extension("json.tmp");
    fs::write(&temp, [data.as_slice(), b"\n"].concat())?;
    fs::rename(temp, path)?;
    Ok(())
}

pub(super) fn set_managed_integration(
    settings: &mut BTreeMap<String, Value>,
    proxy_url: &str,
    remote_ssh_config: &std::path::Path,
) {
    settings.insert(PROXY_KEYS[0].into(), Value::String(proxy_url.into()));
    settings.insert(PROXY_KEYS[1].into(), Value::String(proxy_url.into()));
    settings.insert(PROXY_KEYS[2].into(), Value::String("on".into()));
    settings.insert(PROXY_KEYS[3].into(), Value::Bool(true));
    settings.insert(PROXY_KEYS[4].into(), Value::Bool(true));
    settings.insert(
        "remote.SSH.configFile".into(),
        Value::String(remote_ssh_config.to_string_lossy().into_owned()),
    );
    settings.insert("remote.SSH.enableRemoteCommand".into(), Value::Bool(true));
}

pub(super) fn clear_proxy_values(settings: &mut BTreeMap<String, Value>) {
    for key in PROXY_KEYS {
        settings.remove(key);
    }
}

pub(super) fn proxy_values_match(settings: &BTreeMap<String, Value>, proxy_url: &str) -> bool {
    settings.get(PROXY_KEYS[0]) == Some(&Value::String(proxy_url.into()))
        && settings.get(PROXY_KEYS[1]) == Some(&Value::String(proxy_url.into()))
        && settings.get(PROXY_KEYS[2]) == Some(&Value::String("on".into()))
        && settings.get(PROXY_KEYS[3]) == Some(&Value::Bool(true))
        && settings.get(PROXY_KEYS[4]) == Some(&Value::Bool(true))
}

pub(super) fn managed_proxy_values(settings: &BTreeMap<String, Value>) -> bool {
    let managed_signature = settings.get(PROXY_KEYS[2]) == Some(&Value::String("on".into()))
        && settings.get(PROXY_KEYS[3]) == Some(&Value::Bool(true))
        && settings.get(PROXY_KEYS[4]) == Some(&Value::Bool(true));
    let loopback = settings
        .get(PROXY_KEYS[0])
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<reqwest::Url>().ok())
        .and_then(|url| url.host_str().map(str::to_owned))
        .is_some_and(|host| matches!(host.as_str(), "127.0.0.1" | "localhost" | "::1"));
    managed_signature && loopback
}

pub fn clear_stale_proxy_settings() -> Result<()> {
    let settings = read()?;
    if managed_proxy_values(&settings) {
        let mut settings = settings;
        clear_proxy_values(&mut settings);
        write(&settings)?;
    }
    Ok(())
}
