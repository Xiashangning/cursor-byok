//! Pre-installs bundled built-in plugins into the user's installed directory.
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::{Error, Result};

const COMPLETE_FILE: &str = ".complete";

/// 随二进制打包的内置插件文件;发布构建没有源码目录,靠这里预装。
const CODEX_AUTH: &[(&str, &str)] = &[
    (
        "plugin.json",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/plugins/build-in/codex-auth/plugin.json"
        )),
    ),
    (
        "main.ts",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/plugins/build-in/codex-auth/main.ts"
        )),
    ),
    (
        "provider.ts",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/plugins/build-in/codex-auth/provider.ts"
        )),
    ),
    (
        "models.ts",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/plugins/build-in/codex-auth/models.ts"
        )),
    ),
    (
        "oauth.ts",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/plugins/build-in/codex-auth/oauth.ts"
        )),
    ),
    (
        "resources.ts",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/plugins/build-in/codex-auth/resources.ts"
        )),
    ),
    (
        "assets/codex.svg",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/plugins/build-in/codex-auth/assets/codex.svg"
        )),
    ),
];

const GROK_AUTH: &[(&str, &str)] = &[
    (
        "plugin.json",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/plugins/build-in/grok-auth/plugin.json"
        )),
    ),
    (
        "main.ts",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/plugins/build-in/grok-auth/main.ts"
        )),
    ),
    (
        "provider.ts",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/plugins/build-in/grok-auth/provider.ts"
        )),
    ),
    (
        "models.ts",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/plugins/build-in/grok-auth/models.ts"
        )),
    ),
    (
        "oauth.ts",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/plugins/build-in/grok-auth/oauth.ts"
        )),
    ),
    (
        "resources.ts",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/plugins/build-in/grok-auth/resources.ts"
        )),
    ),
    (
        "assets/grok.svg",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/plugins/build-in/grok-auth/assets/grok.svg"
        )),
    ),
];

const KIMI_AUTH: &[(&str, &str)] = &[
    (
        "plugin.json",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/plugins/build-in/kimi-auth/plugin.json"
        )),
    ),
    (
        "main.ts",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/plugins/build-in/kimi-auth/main.ts"
        )),
    ),
    (
        "provider.ts",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/plugins/build-in/kimi-auth/provider.ts"
        )),
    ),
    (
        "models.ts",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/plugins/build-in/kimi-auth/models.ts"
        )),
    ),
    (
        "oauth.ts",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/plugins/build-in/kimi-auth/oauth.ts"
        )),
    ),
    (
        "token.ts",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/plugins/build-in/kimi-auth/token.ts"
        )),
    ),
    (
        "resources.ts",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/plugins/build-in/kimi-auth/resources.ts"
        )),
    ),
    (
        "assets/kimi.svg",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/plugins/build-in/kimi-auth/assets/kimi.svg"
        )),
    ),
];

const PLUGINS: &[(&str, &[(&str, &str)])] = &[
    ("codex-auth", CODEX_AUTH),
    ("grok-auth", GROK_AUTH),
    ("kimi-auth", KIMI_AUTH),
];

/// Installs each built-in through a complete staging directory. A bundle is
/// accepted only when every embedded file and the completion fingerprint match.
pub(super) fn install(installed: &Path) -> Result<()> {
    std::fs::create_dir_all(installed)?;
    for (name, files) in PLUGINS {
        install_one(installed, name, files)?;
    }
    Ok(())
}

fn install_one(installed: &Path, name: &str, files: &[(&str, &str)]) -> Result<()> {
    let target = installed.join(name);
    let staging = installed.join(format!(".{name}.staging"));
    let backup = installed.join(format!(".{name}.backup"));
    let fingerprint = bundle_fingerprint(files)?;

    if complete(&target, files, &fingerprint) {
        remove_dir_if_exists(&staging)?;
        remove_dir_if_exists(&backup)?;
        return Ok(());
    }

    if !target.exists() {
        if complete(&staging, files, &fingerprint) {
            publish(&staging, &target, &backup)?;
            return Ok(());
        }
        if backup.exists() {
            std::fs::rename(&backup, &target)?;
            if complete(&target, files, &fingerprint) {
                remove_dir_if_exists(&staging)?;
                return Ok(());
            }
        }
    }

    remove_dir_if_exists(&staging)?;
    write_bundle(&staging, files, &fingerprint)?;
    if !complete(&staging, files, &fingerprint) {
        return Err(Error::Config(format!(
            "built-in plugin staging validation failed: {name}"
        )));
    }
    publish(&staging, &target, &backup)
}

fn bundle_fingerprint(files: &[(&str, &str)]) -> Result<String> {
    let manifest = files
        .iter()
        .find(|(name, _)| *name == "plugin.json")
        .map(|(_, content)| *content)
        .expect("built-in plugin bundles plugin.json");
    let value: serde_json::Value = serde_json::from_str(manifest)?;
    if value
        .get("version")
        .and_then(serde_json::Value::as_str)
        .is_none()
    {
        return Err(Error::Config(
            "built-in plugin manifest requires version".into(),
        ));
    }
    let mut digest = Sha256::new();
    for (relative, content) in files {
        digest.update(relative.as_bytes());
        digest.update([0]);
        digest.update(content.as_bytes());
        digest.update([0]);
    }
    Ok(hex::encode(digest.finalize()))
}

fn complete(directory: &Path, files: &[(&str, &str)], fingerprint: &str) -> bool {
    if std::fs::read_to_string(directory.join(COMPLETE_FILE))
        .ok()
        .as_deref()
        != Some(fingerprint)
    {
        return false;
    }
    for (relative, content) in files {
        if std::fs::read_to_string(directory.join(relative))
            .ok()
            .as_deref()
            != Some(*content)
        {
            return false;
        }
    }
    let expected = files.len() + 1;
    walkdir::WalkDir::new(directory)
        .min_depth(1)
        .into_iter()
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .count()
        == expected
}

fn write_bundle(directory: &Path, files: &[(&str, &str)], fingerprint: &str) -> Result<()> {
    for (relative, content) in files {
        let path = directory.join(relative);
        let parent = path.parent().expect("plugin file path has a parent");
        std::fs::create_dir_all(parent)?;
        set_directory_permissions(parent)?;
        std::fs::write(&path, content)?;
        set_file_permissions(&path)?;
    }
    let marker = directory.join(COMPLETE_FILE);
    std::fs::write(&marker, fingerprint)?;
    set_file_permissions(&marker)
}

fn publish(staging: &Path, target: &Path, backup: &Path) -> Result<()> {
    if !target.exists() {
        std::fs::rename(staging, target)?;
        remove_dir_if_exists(backup)?;
        return Ok(());
    }
    remove_dir_if_exists(backup)?;
    std::fs::rename(target, backup)?;
    if let Err(error) = std::fs::rename(staging, target) {
        if backup.exists() {
            let _ = std::fs::rename(backup, target);
        }
        return Err(error.into());
    }
    remove_dir_if_exists(backup)
}

fn remove_dir_if_exists(path: &Path) -> Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn set_directory_permissions(_path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(_path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn set_file_permissions(_path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(_path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn embedded_main() -> &'static str {
        CODEX_AUTH
            .iter()
            .find(|(name, _)| *name == "main.ts")
            .unwrap()
            .1
    }

    #[test]
    fn install_repairs_modified_and_version_changed_bundles() {
        let root = tempfile::tempdir().unwrap();
        let plugin = root.path().join("codex-auth");

        install(root.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(plugin.join("main.ts")).unwrap(),
            embedded_main()
        );

        // A matching manifest alone is not completion: modified and stale files
        // are repaired from a newly validated staging directory.
        std::fs::write(plugin.join("main.ts"), "edited").unwrap();
        std::fs::write(plugin.join("stale.ts"), "extra").unwrap();
        install(root.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(plugin.join("main.ts")).unwrap(),
            embedded_main()
        );
        assert!(!plugin.join("stale.ts").exists());

        // 版本变化:整目录同步回嵌入内容并清理残留。
        let manifest = std::fs::read_to_string(plugin.join("plugin.json")).unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        value["version"] = serde_json::Value::String("0.0.1".into());
        std::fs::write(plugin.join("plugin.json"), value.to_string()).unwrap();
        install(root.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(plugin.join("main.ts")).unwrap(),
            embedded_main()
        );
        assert!(!plugin.join("stale.ts").exists());
    }

    #[test]
    fn incomplete_install_repairs_on_next_startup() {
        let root = tempfile::tempdir().unwrap();
        let plugin = root.path().join("codex-auth");
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(plugin.join("plugin.json"), CODEX_AUTH[0].1).unwrap();

        install(root.path()).unwrap();

        assert_eq!(
            std::fs::read_to_string(plugin.join("main.ts")).unwrap(),
            embedded_main()
        );
        assert!(plugin.join(COMPLETE_FILE).exists());
    }

    #[test]
    fn complete_staging_directory_is_published_after_interruption() {
        let root = tempfile::tempdir().unwrap();
        let staging = root.path().join(".codex-auth.staging");
        let fingerprint = bundle_fingerprint(CODEX_AUTH).unwrap();
        write_bundle(&staging, CODEX_AUTH, &fingerprint).unwrap();

        install(root.path()).unwrap();

        assert!(root.path().join("codex-auth").join(COMPLETE_FILE).exists());
        assert!(!staging.exists());
    }
}
