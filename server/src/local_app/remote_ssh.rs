//! Connects Cursor Remote SSH sessions to the local proxy and remote Semble.
mod skills;

use std::{collections::BTreeMap, fs, path::PathBuf};

use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{config::managed_data_dir, Error, Result};

use super::settings;

pub(super) use skills::SkillSyncServer;

const DIRECTORY_NAME: &str = "remote-ssh";
const CONFIG_FILE_NAME: &str = "ssh-config";
const BACKUP_FILE_NAME: &str = "cursor-settings-backup.json";
const CONFIG_KEY: &str = "remote.SSH.configFile";
const REMOTE_COMMAND_KEY: &str = "remote.SSH.enableRemoteCommand";
const REMOTE_PLATFORM_KEY: &str = "remote.SSH.remotePlatform";

#[derive(Debug, Default, Deserialize, Serialize)]
struct SettingsBackup {
    config_file: Option<Value>,
    enable_remote_command: Option<Value>,
}

pub fn enable(
    proxy_url: &str,
    proxy_port: u16,
    certificate_pem: &str,
    skill_sync: &SkillSyncServer,
) -> Result<()> {
    let directory = directory()?;
    fs::create_dir_all(&directory)?;
    let config_path = directory.join(CONFIG_FILE_NAME);
    let backup_path = directory.join(BACKUP_FILE_NAME);
    let mut cursor_settings = settings::read()?;
    let backup = load_or_create_backup(&cursor_settings, &backup_path)?;
    let source_config = source_config(&backup, &config_path)?;
    let windows_hosts = windows_hosts(&cursor_settings)?;
    let config = render_config(
        &source_config,
        proxy_port,
        certificate_pem,
        &windows_hosts,
        skill_sync,
    );
    write_atomic(&config_path, config.as_bytes())?;

    settings::set_managed_integration(&mut cursor_settings, proxy_url, &config_path);
    settings::write(&cursor_settings)
}

pub fn disable() -> Result<()> {
    let directory = directory()?;
    let config_path = directory.join(CONFIG_FILE_NAME);
    let backup_path = directory.join(BACKUP_FILE_NAME);
    let mut cursor_settings = settings::read()?;
    let backup = read_backup(&backup_path)?;
    let managed = cursor_settings.get(CONFIG_KEY) == Some(&path_value(&config_path));
    if settings::managed_proxy_values(&cursor_settings) {
        settings::clear_proxy_values(&mut cursor_settings);
    }

    match backup {
        Some(backup) if managed => {
            restore_value(&mut cursor_settings, CONFIG_KEY, backup.config_file);
            restore_value(
                &mut cursor_settings,
                REMOTE_COMMAND_KEY,
                backup.enable_remote_command,
            );
        }
        None if managed => {
            cursor_settings.remove(CONFIG_KEY);
            cursor_settings.remove(REMOTE_COMMAND_KEY);
        }
        Some(_) | None => {}
    }
    settings::write(&cursor_settings)?;
    remove_if_exists(&config_path)?;
    remove_if_exists(&backup_path)?;
    Ok(())
}

pub fn settings_match(proxy_url: &str) -> Result<bool> {
    let config_path = directory()?.join(CONFIG_FILE_NAME);
    let cursor_settings = settings::read()?;
    Ok(settings::proxy_values_match(&cursor_settings, proxy_url)
        && cursor_settings.get(CONFIG_KEY) == Some(&path_value(&config_path))
        && cursor_settings.get(REMOTE_COMMAND_KEY) == Some(&Value::Bool(true))
        && config_path.is_file())
}

fn directory() -> Result<PathBuf> {
    Ok(managed_data_dir()?.join(DIRECTORY_NAME))
}

fn load_or_create_backup(
    cursor_settings: &BTreeMap<String, Value>,
    path: &std::path::Path,
) -> Result<SettingsBackup> {
    if let Some(backup) = read_backup(path)? {
        return Ok(backup);
    }
    let backup = SettingsBackup {
        config_file: cursor_settings.get(CONFIG_KEY).cloned(),
        enable_remote_command: cursor_settings.get(REMOTE_COMMAND_KEY).cloned(),
    };
    write_atomic(path, &serde_json::to_vec_pretty(&backup)?)?;
    Ok(backup)
}

fn read_backup(path: &std::path::Path) -> Result<Option<SettingsBackup>> {
    let data = match fs::read(path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    serde_json::from_slice(&data)
        .map(Some)
        .map_err(|error| Error::Config(format!("parse Remote SSH settings backup: {error}")))
}

fn source_config(backup: &SettingsBackup, managed_config: &std::path::Path) -> Result<PathBuf> {
    if let Some(path) = backup.config_file.as_ref().and_then(Value::as_str) {
        let path = PathBuf::from(path);
        if path != managed_config {
            return Ok(path);
        }
    }
    let home = dirs::home_dir()
        .ok_or_else(|| Error::Config("cannot resolve user home directory".into()))?;
    Ok(home.join(".ssh/config"))
}

fn windows_hosts(settings: &BTreeMap<String, Value>) -> Result<Vec<String>> {
    let Some(platforms) = settings.get(REMOTE_PLATFORM_KEY) else {
        return Ok(Vec::new());
    };
    let platforms = platforms
        .as_object()
        .ok_or_else(|| Error::Config("remote.SSH.remotePlatform must be a hostname map".into()))?;
    let mut hosts = platforms
        .iter()
        .filter(|(_, platform)| {
            platform
                .as_str()
                .is_some_and(|platform| platform.eq_ignore_ascii_case("windows"))
        })
        .map(|(host, _)| host.clone())
        .collect::<Vec<_>>();
    for host in &hosts {
        if host.is_empty()
            || !host.bytes().all(|byte| {
                byte.is_ascii_alphanumeric()
                    || matches!(byte, b'.' | b'-' | b'_' | b':' | b'*' | b'?' | b'!')
            })
        {
            return Err(Error::Config(format!(
                "invalid Windows host pattern in remote.SSH.remotePlatform: {host}"
            )));
        }
    }
    hosts.sort();
    Ok(hosts)
}

fn render_config(
    source_config: &std::path::Path,
    proxy_port: u16,
    certificate_pem: &str,
    windows_hosts: &[String],
    skill_sync: &SkillSyncServer,
) -> String {
    let unix_command = unix_remote_command(proxy_port, certificate_pem, skill_sync);
    let windows_command = windows_remote_command(proxy_port, certificate_pem, skill_sync);
    let mut output = String::from(
        "# Generated by Cursor BYOK. Changes are replaced while integration is enabled.\n",
    );
    if !windows_hosts.is_empty() {
        output.push_str(&format!(
            "Host {}\n    RemoteCommand {}\n\n",
            windows_hosts.join(" "),
            windows_command
        ));
    }
    output.push_str(&format!(
        "Host *\n    ClearAllForwardings no\n    ExitOnForwardFailure no\n    RemoteForward 127.0.0.1:{proxy_port} 127.0.0.1:{proxy_port}\n    RemoteCommand {unix_command}\n\nInclude \"{}\"\n",
        escape_ssh_quoted(source_config)
    ));
    output
}

fn unix_remote_command(
    proxy_port: u16,
    certificate_pem: &str,
    skill_sync: &SkillSyncServer,
) -> String {
    let script = unix_bootstrap(
        proxy_port,
        certificate_pem,
        &skill_sync.unix_url(proxy_port),
    );
    let encoded = STANDARD.encode(script);
    format!(
        "bash -c 'd=\"$HOME/.cursor-byok-v3/remote-ssh\"; mkdir -p \"$d\"; f=\"$d/start.$$\"; (printf %%s {encoded} | base64 --decode 2>/dev/null || printf %%s {encoded} | base64 -D) > \"$f\" && chmod 700 \"$f\" && exec bash \"$f\"'"
    )
}

fn unix_bootstrap(proxy_port: u16, certificate_pem: &str, skill_sync_url: &str) -> String {
    let certificate = STANDARD.encode(certificate_pem);
    format!(
        r#"#!/usr/bin/env bash
set +e
rm -f "$0"
byok_dir="$HOME/.cursor-byok-v3"
mkdir -p "$byok_dir"
(printf %s {certificate} | base64 --decode 2>/dev/null || printf %s {certificate} | base64 -D) > "$byok_dir/ca.crt.tmp" && mv "$byok_dir/ca.crt.tmp" "$byok_dir/ca.crt"
export NODE_EXTRA_CA_CERTS="$byok_dir/ca.crt"
export HTTP_PROXY="http://127.0.0.1:{proxy_port}"
export HTTPS_PROXY="$HTTP_PROXY"
export NO_PROXY="localhost,127.0.0.1,::1"
skill_sync="$byok_dir/remote-ssh/sync-skills.$$"
if curl -fsS --noproxy '*' '{skill_sync_url}' -o "$skill_sync.tmp"; then
  chmod 700 "$skill_sync.tmp" && mv "$skill_sync.tmp" "$skill_sync"
  bash "$skill_sync" >&2 || printf '%s\n' 'Cursor BYOK: user skill sync failed; continuing the Remote SSH connection.' >&2
  rm -f "$skill_sync"
else
  rm -f "$skill_sync.tmp"
  printf '%s\n' 'Cursor BYOK: user skill sync download failed; continuing the Remote SSH connection.' >&2
fi
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
if ! command -v uv >/dev/null 2>&1; then
  curl -LsSf https://astral.sh/uv/install.sh | sh >&2
  export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
fi
if command -v uv >/dev/null 2>&1 && ! command -v semble >/dev/null 2>&1; then
  uv tool install semble >&2
fi
if command -v semble >/dev/null 2>&1; then
  semble install --agent cursor --type mcp --yes >&2
else
  printf '%s\n' 'Cursor BYOK: Semble installation failed; continuing without remote code search.' >&2
fi
exec bash
"#
    )
}

fn windows_remote_command(
    proxy_port: u16,
    certificate_pem: &str,
    skill_sync: &SkillSyncServer,
) -> String {
    let script = windows_bootstrap(
        proxy_port,
        certificate_pem,
        &skill_sync.windows_url(proxy_port),
    );
    let encoded = STANDARD.encode(
        script
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>(),
    );
    format!("powershell -NoProfile -EncodedCommand {encoded}")
}

fn windows_bootstrap(proxy_port: u16, certificate_pem: &str, skill_sync_url: &str) -> String {
    let certificate = certificate_pem.replace('`', "``").replace('"', "`\"");
    format!(
        r#"$ErrorActionPreference = 'Continue'
$byokDir = Join-Path $HOME '.cursor-byok-v3'
New-Item -ItemType Directory -Force -Path $byokDir | Out-Null
$certificate = @"
{certificate}"@
$certificatePath = Join-Path $byokDir 'ca.crt'
[System.IO.File]::WriteAllText($certificatePath, $certificate)
$env:NODE_EXTRA_CA_CERTS = $certificatePath
$env:HTTP_PROXY = 'http://127.0.0.1:{proxy_port}'
$env:HTTPS_PROXY = $env:HTTP_PROXY
$env:NO_PROXY = 'localhost,127.0.0.1,::1'
$skillSyncPath = Join-Path $byokDir "sync-skills-$PID.ps1"
try {{
  $client = New-Object System.Net.WebClient
  $client.Proxy = [System.Net.GlobalProxySelection]::GetEmptyWebProxy()
  $client.DownloadFile('{skill_sync_url}', "$skillSyncPath.tmp")
  Move-Item -Force -LiteralPath "$skillSyncPath.tmp" -Destination $skillSyncPath
  & powershell -NoProfile -ExecutionPolicy Bypass -File $skillSyncPath 1>&2
}} catch {{
  [Console]::Error.WriteLine("Cursor BYOK: user skill sync download failed; continuing the Remote SSH connection. $($_.Exception.Message)")
}} finally {{
  if ($client) {{ $client.Dispose() }}
  Remove-Item -Force -ErrorAction SilentlyContinue -LiteralPath $skillSyncPath, "$skillSyncPath.tmp"
}}
$env:PATH = "$HOME\.local\bin;$env:PATH"
if (-not (Get-Command uv -ErrorAction SilentlyContinue)) {{
  Invoke-RestMethod https://astral.sh/uv/install.ps1 | Invoke-Expression
  $env:PATH = "$HOME\.local\bin;$env:PATH"
}}
if ((Get-Command uv -ErrorAction SilentlyContinue) -and -not (Get-Command semble -ErrorAction SilentlyContinue)) {{
  uv tool install semble 1>&2
}}
if (Get-Command semble -ErrorAction SilentlyContinue) {{
  semble install --agent cursor --type mcp --yes 1>&2
}} else {{
  [Console]::Error.WriteLine('Cursor BYOK: Semble installation failed; continuing without remote code search.')
}}
& powershell -NoProfile -Command -
exit $LASTEXITCODE
"#
    )
}

fn escape_ssh_quoted(path: &std::path::Path) -> String {
    path.to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

fn path_value(path: &std::path::Path) -> Value {
    Value::String(path.to_string_lossy().into_owned())
}

fn restore_value(settings: &mut BTreeMap<String, Value>, key: &str, value: Option<Value>) {
    match value {
        Some(value) => {
            settings.insert(key.into(), value);
        }
        None => {
            settings.remove(key);
        }
    }
}

fn write_atomic(path: &std::path::Path, data: &[u8]) -> Result<()> {
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
    }
    fs::rename(temporary, path)?;
    Ok(())
}

fn remove_if_exists(path: &std::path::Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill_sync() -> SkillSyncServer {
        SkillSyncServer::at(PathBuf::from("/missing"), "test-token")
    }

    #[test]
    fn generated_config_forwards_the_proxy_and_preserves_user_config() {
        let rendered = render_config(
            std::path::Path::new("/Users/test/My Config/ssh"),
            31_245,
            "certificate",
            &[],
            &skill_sync(),
        );

        assert!(rendered.contains("RemoteForward 127.0.0.1:31245 127.0.0.1:31245"));
        assert!(rendered.contains("ExitOnForwardFailure no"));
        assert!(rendered.contains("RemoteCommand bash -c"));
        assert!(rendered.contains("start.$$"));
        assert!(rendered.contains("Include \"/Users/test/My Config/ssh\""));
    }

    #[cfg(unix)]
    #[test]
    fn generated_config_is_accepted_by_openssh() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source config");
        let generated = directory.path().join("generated config");
        fs::write(&source, "Host example\n    HostName example.com\n").unwrap();
        fs::write(
            &generated,
            render_config(&source, 31_245, "certificate", &[], &skill_sync()),
        )
        .unwrap();

        let output = match std::process::Command::new("ssh")
            .args(["-G", "-F"])
            .arg(&generated)
            .arg("example")
            .output()
        {
            Ok(output) => output,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => panic!("run ssh -G: {error}"),
        };

        assert!(
            output.status.success(),
            "ssh rejected generated config: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let resolved = String::from_utf8_lossy(&output.stdout);
        assert!(resolved.contains("hostname example.com"));
        assert!(
            resolved.contains("remoteforward [127.0.0.1]:31245 [127.0.0.1]:31245"),
            "resolved config omitted remote forward: {resolved}"
        );
    }

    #[test]
    fn windows_hosts_receive_powershell_before_the_unix_default() {
        let rendered = render_config(
            std::path::Path::new("/home/test/.ssh/config"),
            31_245,
            "certificate",
            &["windows-box".into()],
            &skill_sync(),
        );
        let windows = rendered.find("Host windows-box").unwrap();
        let powershell = rendered.find("RemoteCommand powershell").unwrap();
        let fallback = rendered.find("Host *").unwrap();

        assert!(windows < powershell && powershell < fallback);
    }

    #[test]
    fn bootstrap_configures_proxy_trust_and_official_semble_installer() {
        let script = unix_bootstrap(
            31_245,
            "certificate",
            "http://127.0.0.1:31245/skills/test/unix",
        );

        assert!(script.contains("NODE_EXTRA_CA_CERTS"));
        assert!(script.contains("http://127.0.0.1:31245"));
        assert!(script.contains("uv tool install semble"));
        assert!(script.contains("semble install --agent cursor --type mcp --yes"));
        assert!(script.ends_with("exec bash\n"));
    }
}
