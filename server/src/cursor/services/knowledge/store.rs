//! Persists rules as markdown files with a JSON metadata sidecar.
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

const META_FILE: &str = "meta.json";
const TRANSACTION_FILE: &str = ".transaction.json";
const RULE_EXTENSION: &str = "md";
pub const LOCAL_ID_PREFIX: &str = "local-";

/// 一条规则的完整视图:knowledge 来自 md 文件,其余字段来自 meta.json。
#[derive(Clone, Debug, PartialEq)]
pub struct RuleRecord {
    pub id: String,
    pub knowledge: String,
    pub title: String,
    pub created_at: String,
    pub is_generated: bool,
    pub git_origin: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalOp {
    Add,
    Update,
    Remove,
}

/// 离线期间未同步到上游的一次变更。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JournalEntry {
    pub op: JournalOp,
    pub id: String,
    /// 被上游明确拒绝的累计次数;不可达不计数。
    #[serde(default)]
    pub attempts: u32,
}

#[derive(Default, Serialize, Deserialize)]
struct Meta {
    #[serde(default)]
    rules: BTreeMap<String, RuleMeta>,
    #[serde(default)]
    journal: Vec<JournalEntry>,
}

/// A write-ahead transaction makes the Markdown projection and replay metadata
/// one recoverable mutation. Applying it is idempotent, so startup can finish an
/// interrupted commit before serving reads.
#[derive(Serialize, Deserialize)]
struct Transaction {
    meta: Meta,
    #[serde(default)]
    writes: BTreeMap<String, String>,
    #[serde(default)]
    deletes: Vec<String>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct RuleMeta {
    #[serde(default)]
    title: String,
    #[serde(default)]
    created_at: String,
    #[serde(default)]
    is_generated: bool,
    #[serde(default)]
    git_origin: String,
}

/// md 文件为核心的规则存储;调用方需自行串行化并发访问。
pub struct RuleStore {
    root: PathBuf,
}

impl RuleStore {
    pub fn open(root: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&root)?;
        let store = Self { root };
        store.recover()?;
        Ok(store)
    }

    pub fn list(&self) -> Result<Vec<RuleRecord>> {
        let meta = self.read_meta()?;
        let mut records = Vec::new();
        for entry in std::fs::read_dir(&self.root)? {
            let path = entry?.path();
            if path.extension().and_then(|value| value.to_str()) != Some(RULE_EXTENSION) {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|value| value.to_str()) else {
                continue;
            };
            if validate_id(id).is_err() {
                continue;
            }
            let knowledge = std::fs::read_to_string(&path)?;
            records.push(assemble(id, knowledge, meta.rules.get(id), &path));
        }
        records.sort_by(|left, right| {
            timestamp(&right.created_at)
                .cmp(&timestamp(&left.created_at))
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(records)
    }

    pub fn get(&self, id: &str) -> Result<Option<RuleRecord>> {
        validate_id(id)?;
        let meta = self.read_meta()?;
        let path = self.rule_path(id);
        let knowledge = match std::fs::read_to_string(&path) {
            Ok(knowledge) => knowledge,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        Ok(Some(assemble(id, knowledge, meta.rules.get(id), &path)))
    }

    pub fn upsert(&self, record: &RuleRecord) -> Result<()> {
        validate_id(&record.id)?;
        let mut meta = self.read_meta()?;
        meta.rules.insert(record.id.clone(), rule_meta(record));
        self.commit(Transaction {
            meta,
            writes: BTreeMap::from([(record.id.clone(), record.knowledge.clone())]),
            deletes: Vec::new(),
        })
    }

    pub fn upsert_and_record_add(&self, record: &RuleRecord) -> Result<()> {
        validate_id(&record.id)?;
        let mut meta = self.read_meta()?;
        meta.rules.insert(record.id.clone(), rule_meta(record));
        meta.journal.push(JournalEntry {
            op: JournalOp::Add,
            id: record.id.clone(),
            attempts: 0,
        });
        self.commit(Transaction {
            meta,
            writes: BTreeMap::from([(record.id.clone(), record.knowledge.clone())]),
            deletes: Vec::new(),
        })
    }

    pub fn upsert_and_record_update(&self, record: &RuleRecord) -> Result<()> {
        validate_id(&record.id)?;
        let mut meta = self.read_meta()?;
        meta.rules.insert(record.id.clone(), rule_meta(record));
        if !journal_contains(&meta.journal, &record.id, JournalOp::Add) {
            let op = if record.id.starts_with(LOCAL_ID_PREFIX) {
                JournalOp::Add
            } else {
                JournalOp::Update
            };
            if !journal_contains(&meta.journal, &record.id, op) {
                meta.journal.push(JournalEntry {
                    op,
                    id: record.id.clone(),
                    attempts: 0,
                });
            }
        }
        self.commit(Transaction {
            meta,
            writes: BTreeMap::from([(record.id.clone(), record.knowledge.clone())]),
            deletes: Vec::new(),
        })
    }

    pub fn remove(&self, id: &str) -> Result<()> {
        validate_id(id)?;
        let mut meta = self.read_meta()?;
        meta.rules.remove(id);
        self.commit(Transaction {
            meta,
            writes: BTreeMap::new(),
            deletes: vec![id.into()],
        })
    }

    pub fn remove_and_record(&self, id: &str) -> Result<()> {
        validate_id(id)?;
        let mut meta = self.read_meta()?;
        meta.rules.remove(id);
        let never_synced = journal_contains(&meta.journal, id, JournalOp::Add);
        meta.journal.retain(|entry| entry.id != id);
        if !never_synced && !id.starts_with(LOCAL_ID_PREFIX) {
            meta.journal.push(JournalEntry {
                op: JournalOp::Remove,
                id: id.into(),
                attempts: 0,
            });
        }
        self.commit(Transaction {
            meta,
            writes: BTreeMap::new(),
            deletes: vec![id.into()],
        })
    }

    /// 用上游的完整列表覆盖本地镜像;仅应在日志为空(已全部回放)时调用。
    pub fn replace_all(&self, records: &[RuleRecord]) -> Result<()> {
        let mut meta = self.read_meta()?;
        meta.rules.clear();
        let mut writes = BTreeMap::new();
        for record in records {
            validate_id(&record.id)?;
            writes.insert(record.id.clone(), record.knowledge.clone());
            meta.rules.insert(record.id.clone(), rule_meta(record));
        }
        let mut deletes = Vec::new();
        for entry in std::fs::read_dir(&self.root)? {
            let path = entry?.path();
            if path.extension().and_then(|value| value.to_str()) != Some(RULE_EXTENSION) {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|value| value.to_str()) else {
                continue;
            };
            // 与 list 一致:跳过非法文件名(手写的 "My Notes.md" 等)。它们从未进入
            // 元数据,也不该在镜像时被删除,否则一次全量镜像会因这个文件名而失败。
            if validate_id(id).is_err() {
                continue;
            }
            if !meta.rules.contains_key(id) {
                deletes.push(id.to_owned());
            }
        }
        self.commit(Transaction {
            meta,
            writes,
            deletes,
        })
    }

    pub fn journal_front(&self) -> Result<Option<JournalEntry>> {
        Ok(self.read_meta()?.journal.first().cloned())
    }

    /// 记录队头条目被上游明确拒绝一次,返回累计拒绝次数;不可达不计数。
    pub fn record_rejection(&self) -> Result<u32> {
        let mut meta = self.read_meta()?;
        let Some(front) = meta.journal.first_mut() else {
            return Ok(0);
        };
        front.attempts += 1;
        let attempts = front.attempts;
        self.write_meta(&meta)?;
        Ok(attempts)
    }

    pub fn pop_journal(&self) -> Result<()> {
        let mut meta = self.read_meta()?;
        if !meta.journal.is_empty() {
            meta.journal.remove(0);
            self.write_meta(&meta)?;
        }
        Ok(())
    }

    pub fn promote_and_pop(&self, old_id: &str, new_id: &str) -> Result<()> {
        validate_id(old_id)?;
        validate_id(new_id)?;
        let knowledge = std::fs::read_to_string(self.rule_path(old_id))?;
        let mut meta = self.read_meta()?;
        if let Some(rule) = meta.rules.remove(old_id) {
            meta.rules.insert(new_id.into(), rule);
        }
        if meta
            .journal
            .first()
            .is_some_and(|entry| entry.id == old_id && entry.op == JournalOp::Add)
        {
            meta.journal.remove(0);
        }
        self.commit(Transaction {
            meta,
            writes: BTreeMap::from([(new_id.into(), knowledge)]),
            deletes: vec![old_id.into()],
        })
    }

    fn rule_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.{RULE_EXTENSION}"))
    }

    fn meta_path(&self) -> PathBuf {
        self.root.join(META_FILE)
    }

    fn transaction_path(&self) -> PathBuf {
        self.root.join(TRANSACTION_FILE)
    }

    fn read_meta(&self) -> Result<Meta> {
        self.recover()?;
        let bytes = match std::fs::read(self.meta_path()) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Meta::default())
            }
            Err(error) => return Err(error.into()),
        };
        match serde_json::from_slice(&bytes) {
            Ok(meta) => Ok(meta),
            // 元数据损坏时整个规则功能都不可用。md 文件本身仍完整,把损坏文件改名保留
            // 现场后从空元数据继续,用户手动放回的 md 会以文件名为标题重新列出。
            Err(error) => {
                let backup = format!(
                    "{META_FILE}.corrupt-{}",
                    chrono::Utc::now().timestamp_millis()
                );
                tracing::error!(
                    backup,
                    %error,
                    "rules metadata is corrupt; moving it aside and continuing with empty metadata"
                );
                std::fs::rename(self.meta_path(), self.root.join(&backup))?;
                Ok(Meta::default())
            }
        }
    }

    fn write_meta(&self, meta: &Meta) -> Result<()> {
        write_atomic(&self.meta_path(), &serde_json::to_vec_pretty(meta)?)
    }

    fn commit(&self, transaction: Transaction) -> Result<()> {
        write_atomic(
            &self.transaction_path(),
            &serde_json::to_vec_pretty(&transaction)?,
        )?;
        self.apply_transaction(&transaction)?;
        remove_file_if_exists(&self.transaction_path())
    }

    fn recover(&self) -> Result<()> {
        let bytes = match std::fs::read(self.transaction_path()) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let transaction: Transaction = serde_json::from_slice(&bytes)?;
        self.apply_transaction(&transaction)?;
        remove_file_if_exists(&self.transaction_path())
    }

    fn apply_transaction(&self, transaction: &Transaction) -> Result<()> {
        for (id, knowledge) in &transaction.writes {
            validate_id(id)?;
            write_atomic(&self.rule_path(id), knowledge.as_bytes())?;
        }
        for id in &transaction.deletes {
            validate_id(id)?;
            remove_file_if_exists(&self.rule_path(id))?;
        }
        self.write_meta(&transaction.meta)
    }
}

fn assemble(id: &str, knowledge: String, meta: Option<&RuleMeta>, path: &Path) -> RuleRecord {
    match meta {
        Some(meta) => RuleRecord {
            id: id.into(),
            knowledge,
            title: meta.title.clone(),
            created_at: meta.created_at.clone(),
            is_generated: meta.is_generated,
            git_origin: meta.git_origin.clone(),
        },
        // 用户手放的 md 文件没有元数据,用文件名当标题、修改时间当创建时间。
        None => RuleRecord {
            id: id.into(),
            knowledge,
            title: id.into(),
            created_at: file_modified_at(path),
            is_generated: false,
            git_origin: String::new(),
        },
    }
}

fn rule_meta(record: &RuleRecord) -> RuleMeta {
    RuleMeta {
        title: record.title.clone(),
        created_at: record.created_at.clone(),
        is_generated: record.is_generated,
        git_origin: record.git_origin.clone(),
    }
}

fn journal_contains(journal: &[JournalEntry], id: &str, op: JournalOp) -> bool {
    journal.iter().any(|entry| entry.id == id && entry.op == op)
}

fn timestamp(created_at: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(created_at)
        .map(|time| time.timestamp_millis())
        .unwrap_or(0)
}

fn file_modified_at(path: &Path) -> String {
    let modified = std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .unwrap_or_else(|_| std::time::SystemTime::now());
    chrono::DateTime::<chrono::Utc>::from(modified)
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(Error::Protocol(format!("invalid rule id: {id:?}")));
    }
    Ok(())
}

fn remove_file_if_exists(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let directory = path.parent().expect("rule path has a parent");
    let temporary = directory.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let mut file = std::fs::File::create(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    crate::fs::replace_file(&temporary, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, knowledge: &str, created_at: &str) -> RuleRecord {
        RuleRecord {
            id: id.into(),
            knowledge: knowledge.into(),
            title: format!("title-{id}"),
            created_at: created_at.into(),
            is_generated: false,
            git_origin: String::new(),
        }
    }

    fn journal(store: &RuleStore) -> Vec<JournalEntry> {
        store.read_meta().unwrap().journal
    }

    #[test]
    fn upserts_lists_and_removes_rules() {
        let root = tempfile::tempdir().unwrap();
        let store = RuleStore::open(root.path().join("rules")).unwrap();
        store
            .upsert(&record("100", "older", "2026-01-01T00:00:00.000Z"))
            .unwrap();
        store
            .upsert(&record("200", "newer", "2026-02-01T00:00:00.000Z"))
            .unwrap();

        let listed = store.list().unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|rule| rule.id.as_str())
                .collect::<Vec<_>>(),
            ["200", "100"],
            "list is sorted by created_at descending"
        );
        assert_eq!(listed[0].knowledge, "newer");
        assert_eq!(listed[0].title, "title-200");

        store.remove("200").unwrap();
        assert!(store.get("200").unwrap().is_none());
        assert_eq!(store.list().unwrap().len(), 1);
    }

    #[test]
    fn rejects_path_traversal_ids() {
        let root = tempfile::tempdir().unwrap();
        let store = RuleStore::open(root.path().join("rules")).unwrap();
        assert!(store.get("../escape").is_err());
        assert!(store.get("a/b").is_err());
        assert!(store.get("").is_err());
    }

    #[test]
    fn compacts_offline_journal() {
        let root = tempfile::tempdir().unwrap();
        let store = RuleStore::open(root.path().join("rules")).unwrap();

        // 离线新增后再更新:回放 add 即可携带最新内容,不产生 update 日志。
        store
            .upsert_and_record_add(&record("local-a", "v1", "2026-01-01T00:00:00.000Z"))
            .unwrap();
        store
            .upsert_and_record_update(&record("local-a", "v2", "2026-01-01T00:00:00.000Z"))
            .unwrap();
        assert_eq!(
            journal(&store),
            vec![JournalEntry {
                op: JournalOp::Add,
                id: "local-a".into(),
                attempts: 0,
            }]
        );

        // 离线新增后又删除:上游从未见过它,日志清空。
        store.remove_and_record("local-a").unwrap();
        assert!(journal(&store).is_empty());

        // 更新上游已有规则:多次更新合并为一条;删除后 update 日志被顶替。
        let existing = record("42", "v1", "2026-01-01T00:00:00.000Z");
        store.upsert_and_record_update(&existing).unwrap();
        store.upsert_and_record_update(&existing).unwrap();
        assert_eq!(
            journal(&store),
            vec![JournalEntry {
                op: JournalOp::Update,
                id: "42".into(),
                attempts: 0,
            }]
        );
        store.remove_and_record("42").unwrap();
        assert_eq!(
            journal(&store),
            vec![JournalEntry {
                op: JournalOp::Remove,
                id: "42".into(),
                attempts: 0,
            }]
        );
    }

    #[test]
    fn promote_renames_rule_and_journal_ids() {
        let root = tempfile::tempdir().unwrap();
        let store = RuleStore::open(root.path().join("rules")).unwrap();
        store
            .upsert_and_record_add(&record("local-a", "content", "2026-01-01T00:00:00.000Z"))
            .unwrap();

        store.promote_and_pop("local-a", "17353272").unwrap();

        assert!(store.get("local-a").unwrap().is_none());
        let promoted = store.get("17353272").unwrap().unwrap();
        assert_eq!(promoted.knowledge, "content");
        assert_eq!(promoted.title, "title-local-a");
        assert!(journal(&store).is_empty());
    }

    #[test]
    fn replace_all_mirrors_upstream_state() {
        let root = tempfile::tempdir().unwrap();
        let store = RuleStore::open(root.path().join("rules")).unwrap();
        store
            .upsert(&record("stale", "gone soon", "2026-01-01T00:00:00.000Z"))
            .unwrap();

        store
            .replace_all(&[record(
                "17353272",
                "from upstream",
                "2026-02-01T00:00:00.000Z",
            )])
            .unwrap();

        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "17353272");
        assert_eq!(listed[0].knowledge, "from upstream");
        assert!(store.get("stale").unwrap().is_none());
    }

    #[test]
    fn corrupt_metadata_is_moved_aside_and_reset() {
        let root = tempfile::tempdir().unwrap();
        let rules = root.path().join("rules");
        let store = RuleStore::open(rules.clone()).unwrap();
        store
            .upsert(&record("100", "kept", "2026-01-01T00:00:00.000Z"))
            .unwrap();
        std::fs::write(rules.join(META_FILE), b"{broken").unwrap();

        // md 文件仍然列出,损坏的元数据被改名保留以便人工恢复。
        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].knowledge, "kept");
        let backups = std::fs::read_dir(&rules)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("meta.json.corrupt-"))
            .collect::<Vec<_>>();
        assert_eq!(backups.len(), 1);
        assert_eq!(
            std::fs::read_to_string(rules.join(&backups[0])).unwrap(),
            "{broken"
        );

        // 重置后仍可正常写入。
        store
            .upsert(&record("200", "newer", "2026-02-01T00:00:00.000Z"))
            .unwrap();
        assert_eq!(store.list().unwrap().len(), 2);
    }

    #[test]
    fn replace_all_ignores_invalid_markdown_names() {
        let root = tempfile::tempdir().unwrap();
        let rules = root.path().join("rules");
        let store = RuleStore::open(rules.clone()).unwrap();
        std::fs::write(rules.join("My Notes.md"), "hand written").unwrap();

        store
            .replace_all(&[record(
                "17353272",
                "from upstream",
                "2026-02-01T00:00:00.000Z",
            )])
            .unwrap();

        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "17353272");
        // 非法文件名不是本地镜像的一部分,原样保留而不是让镜像失败。
        assert!(rules.join("My Notes.md").exists());
    }

    #[test]
    fn metadata_io_errors_are_reported() {
        let root = tempfile::tempdir().unwrap();
        let rules = root.path().join("rules");
        let store = RuleStore::open(rules.clone()).unwrap();
        std::fs::create_dir(rules.join(META_FILE)).unwrap();

        assert!(store.list().is_err());
    }

    #[test]
    fn recovers_interrupted_markdown_and_journal_commit() {
        let root = tempfile::tempdir().unwrap();
        let rules = root.path().join("rules");
        let store = RuleStore::open(rules.clone()).unwrap();
        let pending = record("local-a", "durable", "2026-01-01T00:00:00.000Z");
        let transaction = Transaction {
            meta: Meta {
                rules: BTreeMap::from([(pending.id.clone(), rule_meta(&pending))]),
                journal: vec![JournalEntry {
                    op: JournalOp::Add,
                    id: pending.id.clone(),
                    attempts: 0,
                }],
            },
            writes: BTreeMap::from([(pending.id.clone(), pending.knowledge.clone())]),
            deletes: Vec::new(),
        };
        write_atomic(
            &rules.join(TRANSACTION_FILE),
            &serde_json::to_vec(&transaction).unwrap(),
        )
        .unwrap();
        drop(store);

        let recovered = RuleStore::open(rules.clone()).unwrap();
        assert_eq!(
            recovered.get("local-a").unwrap().unwrap().knowledge,
            "durable"
        );
        assert_eq!(
            recovered.journal_front().unwrap().unwrap().op,
            JournalOp::Add
        );
        assert!(!rules.join(TRANSACTION_FILE).exists());
    }

    #[test]
    fn lists_hand_written_markdown_without_metadata() {
        let root = tempfile::tempdir().unwrap();
        let store = RuleStore::open(root.path().join("rules")).unwrap();
        std::fs::write(root.path().join("rules/manual_rule.md"), "hand written").unwrap();

        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "manual_rule");
        assert_eq!(listed[0].title, "manual_rule");
        assert_eq!(listed[0].knowledge, "hand written");
    }
}
