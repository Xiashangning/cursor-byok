//! Serves user-level Cursor skills to Remote SSH bootstrap sessions.
use std::{collections::BTreeMap, fs, path::PathBuf};

use base64::{engine::general_purpose::STANDARD, Engine};
use uuid::Uuid;
use walkdir::WalkDir;

use crate::{Error, Result};

const ENDPOINT_PREFIX: &str = "/.cursor-byok/remote-ssh/skills";
const MANAGED_DIRECTORY: &str = "cursor-byok-synced";
const MANAGED_MARKER: &str = ".cursor-byok-managed";
const MANAGED_MARKER_CONTENT: &str = "Cursor BYOK managed user skills v1";

#[derive(Clone)]
pub(in crate::local_app) struct SkillSyncServer {
    token: String,
    sources: Vec<PathBuf>,
}

impl SkillSyncServer {
    pub(in crate::local_app) fn new() -> Result<Self> {
        let home = dirs::home_dir()
            .ok_or_else(|| Error::Config("cannot resolve user home directory".into()))?;
        Ok(Self {
            token: Uuid::new_v4().simple().to_string(),
            sources: vec![home.join(".cursor/skills"), home.join(".agents/skills")],
        })
    }

    #[cfg(test)]
    pub(in crate::local_app) fn at(source: PathBuf, token: &str) -> Self {
        Self {
            token: token.into(),
            sources: vec![source],
        }
    }

    pub(in crate::local_app) fn unix_url(&self, port: u16) -> String {
        format!(
            "http://127.0.0.1:{port}{ENDPOINT_PREFIX}/{}/unix",
            self.token
        )
    }

    pub(in crate::local_app) fn windows_url(&self, port: u16) -> String {
        format!(
            "http://127.0.0.1:{port}{ENDPOINT_PREFIX}/{}/windows",
            self.token
        )
    }

    pub(in crate::local_app) fn script_for_path(&self, path: &str) -> Option<Result<String>> {
        let platform = path.strip_prefix(&format!("{ENDPOINT_PREFIX}/{}/", self.token))?;
        match platform {
            "unix" => Some(load_user_skills(&self.sources).map(|skills| render_unix(&skills))),
            "windows" => {
                Some(load_user_skills(&self.sources).map(|skills| render_windows(&skills)))
            }
            _ => None,
        }
    }
}

struct Skill {
    name: String,
    files: Vec<SkillFile>,
}

struct SkillFile {
    relative_path: String,
    contents: Vec<u8>,
    executable: bool,
}

fn load_user_skills(sources: &[PathBuf]) -> Result<Vec<Skill>> {
    let mut seen = BTreeMap::<String, PathBuf>::new();
    let mut skills = Vec::new();
    for source in sources {
        for skill in load_skills(source)? {
            if let Some(first) = seen.get(&skill.name) {
                tracing::warn!(skill = %skill.name, first = %first.display(), duplicate_root = %source.display(), "skipping lower-priority duplicate user-level Cursor skill name");
                continue;
            }
            seen.insert(skill.name.clone(), source.clone());
            skills.push(skill);
        }
    }
    Ok(skills)
}

fn load_skills(source: &std::path::Path) -> Result<Vec<Skill>> {
    if !source.is_dir() {
        return Ok(Vec::new());
    }

    let mut roots = BTreeMap::<PathBuf, String>::new();
    for entry in WalkDir::new(source).follow_links(false).sort_by_file_name() {
        let entry = entry.map_err(walk_error)?;
        if !entry.file_type().is_file() || entry.file_name() != "SKILL.md" {
            continue;
        }
        let root = entry
            .path()
            .parent()
            .expect("SKILL.md discovered beneath its source root");
        let Some(name) = root.file_name().and_then(|name| name.to_str()) else {
            tracing::warn!(path = %root.display(), "skipping Cursor skill with a non-UTF-8 name");
            continue;
        };
        if !valid_skill_name(name) {
            tracing::warn!(path = %root.display(), "skipping Cursor skill whose directory name is not a valid skill name");
            continue;
        }
        roots.insert(root.to_path_buf(), name.into());
    }

    let root_paths = roots.keys().cloned().collect::<Vec<_>>();
    let mut names = BTreeMap::<String, PathBuf>::new();
    let mut skills = Vec::new();
    for (root, name) in roots {
        if let Some(first) = names.get(&name) {
            tracing::warn!(skill = %name, first = %first.display(), duplicate = %root.display(), "skipping duplicate user-level Cursor skill name");
            continue;
        }
        names.insert(name.clone(), root.clone());

        let mut files = Vec::new();
        let mut walker = WalkDir::new(&root)
            .follow_links(false)
            .sort_by_file_name()
            .into_iter();
        while let Some(entry) = walker.next() {
            let entry = entry.map_err(walk_error)?;
            if entry.depth() > 0
                && entry.file_type().is_dir()
                && root_paths
                    .iter()
                    .any(|candidate| candidate != &root && candidate == entry.path())
            {
                walker.skip_current_dir();
                continue;
            }
            if entry.file_type().is_symlink() {
                tracing::warn!(path = %entry.path().display(), "skipping symlink in user-level Cursor skill");
                continue;
            }
            if !entry.file_type().is_file() {
                continue;
            }
            let relative = entry
                .path()
                .strip_prefix(source)
                .map_err(|error| Error::Config(format!("resolve Cursor skill path: {error}")))?;
            let Some(relative_path) = relative.to_str() else {
                tracing::warn!(path = %entry.path().display(), "skipping Cursor skill file with a non-UTF-8 path");
                continue;
            };
            let metadata = entry.metadata().map_err(|error| {
                Error::Config(format!(
                    "read Cursor skill metadata for {}: {error}",
                    entry.path().display()
                ))
            })?;
            let contents = fs::read(entry.path())?;
            files.push(SkillFile {
                relative_path: relative_path.replace('\\', "/"),
                executable: executable(&metadata, &contents),
                contents,
            });
        }
        skills.push(Skill { name, files });
    }
    Ok(skills)
}

fn walk_error(error: walkdir::Error) -> Error {
    Error::Config(format!("scan user-level Cursor skills: {error}"))
}

fn valid_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

#[cfg(unix)]
fn executable(metadata: &fs::Metadata, _contents: &[u8]) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn executable(_metadata: &fs::Metadata, contents: &[u8]) -> bool {
    contents.starts_with(b"#!")
}

fn render_unix(skills: &[Skill]) -> String {
    let mut script = format!(
        r#"#!/usr/bin/env bash
set +e
if ! (
  set -e
  umask 077
  skills_root="$HOME/.cursor/skills"
  agents_skills_root="$HOME/.agents/skills"
  managed="$skills_root/{MANAGED_DIRECTORY}"
  marker="$managed/{MANAGED_MARKER}"
  marker_content='{MANAGED_MARKER_CONTENT}'
  stage="$HOME/.cursor-byok-v3/remote-ssh/skills-stage.$$"
  trap 'rm -rf "$stage"' EXIT
  if [ -e "$managed" ] && {{ [ ! -f "$marker" ] || [ "$(cat "$marker")" != "$marker_content" ]; }}; then
    printf '%s\n' 'Cursor BYOK: refusing to replace an unowned remote skills directory.' >&2
    exit 1
  fi
  rm -rf "$stage"
  if [ ! -d "$skills_root" ]; then
    mkdir -p "$skills_root"
    chmod 700 "$skills_root"
  fi
  mkdir -p "$stage"
  chmod 700 "$stage"
  has_remote_skill() {{
    for remote_root in "$skills_root" "$agents_skills_root"; do
      [ -d "$remote_root" ] || continue
      while IFS= read -r -d '' skill_file; do
        if [ "$(basename "$(dirname "$skill_file")")" = "$1" ]; then
          return 0
        fi
      done < <(find "$remote_root" -path "$managed" -prune -o -type f -name SKILL.md -print0)
    done
    return 1
  }}
"#
    );

    for skill in skills {
        script.push_str(&format!(
            "  if has_remote_skill '{}'; then\n    printf '%s\\n' 'Cursor BYOK: kept remote skill {}; skipped the local skill with the same name.' >&2\n  else\n",
            skill.name, skill.name
        ));
        for file in &skill.files {
            let path = STANDARD.encode(file.relative_path.as_bytes());
            let contents = STANDARD.encode(&file.contents);
            let mode = if file.executable { "700" } else { "600" };
            script.push_str(&format!(
                "    relative=$(printf %s {path} | base64 --decode 2>/dev/null || printf %s {path} | base64 -D)\n    destination=\"$stage/$relative\"\n    mkdir -p \"$(dirname \"$destination\")\"\n    (printf %s {contents} | base64 --decode 2>/dev/null || printf %s {contents} | base64 -D) > \"$destination\"\n    chmod {mode} \"$destination\"\n"
            ));
        }
        script.push_str("  fi\n");
    }

    script.push_str(&format!(
        r#"  printf '%s\n' "$marker_content" > "$stage/{MANAGED_MARKER}"
  chmod 600 "$stage/{MANAGED_MARKER}"
  if [ -e "$managed" ]; then
    rm -rf "$managed"
  fi
  mv "$stage" "$managed"
); then
  printf '%s\n' 'Cursor BYOK: user skill sync failed; continuing the Remote SSH connection.' >&2
fi
exit 0
"#
    ));
    script
}

fn render_windows(skills: &[Skill]) -> String {
    let mut script = format!(
        r#"$previousErrorActionPreference = $ErrorActionPreference
try {{
  $ErrorActionPreference = 'Stop'
  $skillsRoot = Join-Path $HOME '.cursor/skills'
  $agentsSkillsRoot = Join-Path $HOME '.agents/skills'
  $managed = Join-Path $skillsRoot '{MANAGED_DIRECTORY}'
  $marker = Join-Path $managed '{MANAGED_MARKER}'
  $markerContent = '{MANAGED_MARKER_CONTENT}'
  $stage = Join-Path $HOME ".cursor-byok-v3/remote-ssh/skills-stage-$PID"
  if ((Test-Path -LiteralPath $managed) -and ((-not (Test-Path -LiteralPath $marker -PathType Leaf)) -or ((Get-Content -LiteralPath $marker -Raw) -ne $markerContent))) {{
    throw 'refusing to replace an unowned remote skills directory'
  }}
  Remove-Item -LiteralPath $stage -Recurse -Force -ErrorAction SilentlyContinue
  New-Item -ItemType Directory -Force -Path $skillsRoot, $stage | Out-Null
  $existing = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::OrdinalIgnoreCase)
  @($skillsRoot, $agentsSkillsRoot) | ForEach-Object {{
    $remoteRoot = $_
    if (Test-Path -LiteralPath $remoteRoot -PathType Container) {{
      Get-ChildItem -LiteralPath $remoteRoot -Filter 'SKILL.md' -File -Recurse -Force | ForEach-Object {{
        if (-not $_.FullName.StartsWith($managed + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {{
          [void]$existing.Add($_.Directory.Name)
        }}
      }}
    }}
  }}
"#
    );

    for skill in skills {
        script.push_str(&format!(
            "  if ($existing.Contains('{}')) {{\n    [Console]::Error.WriteLine('Cursor BYOK: kept remote skill {}; skipped the local skill with the same name.')\n  }} else {{\n",
            skill.name, skill.name
        ));
        for file in &skill.files {
            let path = STANDARD.encode(file.relative_path.as_bytes());
            let contents = STANDARD.encode(&file.contents);
            script.push_str(&format!(
                "    $relative = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{path}')).Replace('/', [IO.Path]::DirectorySeparatorChar)\n    $destination = Join-Path $stage $relative\n    New-Item -ItemType Directory -Force -Path ([IO.Path]::GetDirectoryName($destination)) | Out-Null\n    [IO.File]::WriteAllBytes($destination, [Convert]::FromBase64String('{contents}'))\n"
            ));
        }
        script.push_str("  }\n");
    }

    script.push_str(&format!(
        r#"  [IO.File]::WriteAllText((Join-Path $stage '{MANAGED_MARKER}'), $markerContent)
  if (Test-Path -LiteralPath $managed) {{
    Remove-Item -LiteralPath $managed -Recurse -Force
  }}
  Move-Item -LiteralPath $stage -Destination $managed
}} catch {{
  [Console]::Error.WriteLine("Cursor BYOK: user skill sync failed; continuing the Remote SSH connection. $($_.Exception.Message)")
}} finally {{
  if ($stage) {{ Remove-Item -LiteralPath $stage -Recurse -Force -ErrorAction SilentlyContinue }}
  $ErrorActionPreference = $previousErrorActionPreference
}}
"#
    ));
    script
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_only_valid_skills_and_preserves_file_contents() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("skills");
        fs::create_dir_all(root.join("category/my-skill/scripts")).unwrap();
        fs::write(root.join("category/my-skill/SKILL.md"), "instructions").unwrap();
        fs::write(
            root.join("category/my-skill/scripts/run.sh"),
            b"echo test\n",
        )
        .unwrap();
        fs::create_dir_all(root.join("not_a_skill")).unwrap();
        fs::write(root.join("not_a_skill/SKILL.md"), "ignored").unwrap();

        let skills = load_skills(&root).unwrap();

        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "my-skill");
        assert_eq!(skills[0].files.len(), 2);
        assert!(skills[0].files.iter().any(|file| file.relative_path
            == "category/my-skill/scripts/run.sh"
            && file.contents == b"echo test\n"));
    }

    #[test]
    fn cursor_user_skills_take_priority_over_open_standard_user_skills() {
        let directory = tempfile::tempdir().unwrap();
        let cursor = directory.path().join(".cursor/skills");
        let agents = directory.path().join(".agents/skills");
        fs::create_dir_all(cursor.join("shared")).unwrap();
        fs::create_dir_all(agents.join("shared")).unwrap();
        fs::write(cursor.join("shared/SKILL.md"), "cursor").unwrap();
        fs::write(agents.join("shared/SKILL.md"), "agents").unwrap();

        let skills = load_user_skills(&[cursor, agents]).unwrap();

        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].files[0].contents, b"cursor");
    }

    #[test]
    fn endpoint_requires_its_random_token() {
        let server = SkillSyncServer::at(PathBuf::from("/missing"), "secret");

        assert!(server
            .script_for_path("/.cursor-byok/remote-ssh/skills/secret/unix")
            .is_some());
        assert!(server
            .script_for_path("/.cursor-byok/remote-ssh/skills/wrong/unix")
            .is_none());
    }

    #[cfg(unix)]
    #[test]
    fn unix_sync_keeps_remote_collisions_and_prunes_deleted_local_skills() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("local");
        let home = directory.path().join("remote-home");
        fs::create_dir_all(source.join("keep-remote")).unwrap();
        fs::create_dir_all(source.join("synced/scripts")).unwrap();
        fs::write(source.join("keep-remote/SKILL.md"), "local").unwrap();
        fs::write(source.join("synced/SKILL.md"), "first").unwrap();
        fs::write(source.join("synced/scripts/run.sh"), "#!/bin/sh\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                source.join("synced/scripts/run.sh"),
                fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
        fs::create_dir_all(home.join(".cursor/skills/keep-remote")).unwrap();
        fs::write(home.join(".cursor/skills/keep-remote/SKILL.md"), "remote").unwrap();

        let first = render_unix(&load_skills(&source).unwrap());
        let output = std::process::Command::new("bash")
            .env("HOME", &home)
            .arg("-c")
            .arg(first)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            fs::read_to_string(home.join(".cursor/skills/keep-remote/SKILL.md")).unwrap(),
            "remote"
        );
        assert!(!home
            .join(".cursor/skills/cursor-byok-synced/keep-remote/SKILL.md")
            .exists());
        assert_eq!(
            fs::read_to_string(home.join(".cursor/skills/cursor-byok-synced/synced/SKILL.md"))
                .unwrap(),
            "first"
        );
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(home.join(".cursor/skills/cursor-byok-synced/synced/SKILL.md"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(home.join(".cursor/skills/cursor-byok-synced/synced/scripts/run.sh"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }

        fs::remove_dir_all(source.join("synced")).unwrap();
        let second = render_unix(&load_skills(&source).unwrap());
        let output = std::process::Command::new("bash")
            .env("HOME", &home)
            .arg("-c")
            .arg(second)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(!home
            .join(".cursor/skills/cursor-byok-synced/synced/SKILL.md")
            .exists());
    }

    #[cfg(unix)]
    #[test]
    fn unix_sync_refuses_an_unowned_managed_directory_without_blocking_ssh() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("remote-home");
        let managed = home.join(".cursor/skills/cursor-byok-synced");
        fs::create_dir_all(&managed).unwrap();
        fs::write(managed.join("user-file"), "keep").unwrap();

        let output = std::process::Command::new("bash")
            .env("HOME", &home)
            .arg("-c")
            .arg(render_unix(&[]))
            .output()
            .unwrap();

        assert!(output.status.success());
        assert_eq!(
            fs::read_to_string(managed.join("user-file")).unwrap(),
            "keep"
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains("continuing"));
    }

    #[test]
    fn powershell_sync_uses_a_separate_owned_directory() {
        let skills = vec![Skill {
            name: "my-skill".into(),
            files: vec![SkillFile {
                relative_path: "my-skill/SKILL.md".into(),
                contents: b"instructions".to_vec(),
                executable: false,
            }],
        }];

        let script = render_windows(&skills);

        assert!(script.contains("cursor-byok-synced"));
        assert!(script.contains("skills-stage-$PID"));
        assert!(script.contains(".cursor-byok-managed"));
        assert!(script.contains("$existing.Contains('my-skill')"));
        assert!(script.contains(&STANDARD.encode("instructions")));
    }
}
