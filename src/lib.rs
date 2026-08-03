//! Jinfu Search 的可复用索引核心与本地控制协议。
//!
//! 第一版刻意只索引文件系统元数据，不读取或保存文件正文。这样既能让
//! Agent 快速定位文件，也不会把私人文件内容复制到本地索引库中。

use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicI64, Ordering},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use rusqlite::{Connection, params, params_from_iter, types::Value as SqlValue};
#[cfg(windows)]
use rusqlite::{OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use unicode_normalization::UnicodeNormalization;
use walkdir::WalkDir;

pub mod app;

#[cfg(windows)]
mod pipe;

#[cfg(windows)]
pub use pipe::{DEFAULT_PIPE_NAME, call_pipe, serve_pipe, serve_pipe_controlled};

#[cfg(windows)]
pub mod ntfs;

#[cfg(windows)]
pub mod search_ui;

#[cfg(all(windows, feature = "tray"))]
pub mod tray;

const MAX_QUERY_LIMIT: usize = 1_000;
static LAST_GENERATION: AtomicI64 = AtomicI64::new(0);

#[derive(Debug, Error)]
pub enum SearchError {
    #[error("数据库错误：{0}")]
    Database(#[from] rusqlite::Error),
    #[error("无法访问路径 {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("索引根目录不是目录：{0}")]
    NotDirectory(PathBuf),
    #[error("搜索词不能为空")]
    EmptyQuery,
}

pub type Result<T> = std::result::Result<T, SearchError>;

/// 一个可克隆的 SQLite 索引句柄。每次操作各自打开连接，便于 CLI、托盘和
/// Named Pipe 服务并发使用同一个数据库。
#[derive(Debug, Clone)]
pub struct SearchIndex {
    database_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchRequest {
    pub query: String,
    pub limit: usize,
    pub root: Option<PathBuf>,
}

impl SearchRequest {
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            limit: 50,
            root: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SearchItem {
    pub path: String,
    pub name: String,
    pub is_directory: bool,
    pub size_bytes: u64,
    pub modified_unix_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScanReport {
    pub root: String,
    pub indexed_files: u64,
    pub indexed_directories: u64,
    pub skipped_entries: u64,
    pub complete: bool,
    pub elapsed_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IndexStatus {
    pub indexed_files: u64,
    pub indexed_directories: u64,
    pub roots: u64,
    pub incomplete_roots: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IndexedRoot {
    pub root: String,
    pub last_scan_unix_ms: i64,
    pub complete: bool,
    pub skipped_entries: u64,
    pub uses_ntfs_usn: bool,
}

#[cfg(windows)]
#[derive(Debug, Clone)]
struct NtfsNode {
    file_reference_number: String,
    parent_file_reference_number: String,
    relative_path: String,
    name: String,
    is_directory: bool,
    modified_unix_ms: Option<i64>,
}

impl SearchIndex {
    pub fn open(database_path: impl Into<PathBuf>) -> Result<Self> {
        let database_path = database_path.into();
        if let Some(parent) = database_path.parent() {
            fs::create_dir_all(parent).map_err(|source| SearchError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        let index = Self { database_path };
        index.initialize()?;
        Ok(index)
    }

    pub fn database_path(&self) -> &Path {
        &self.database_path
    }

    pub fn scan_root(&self, root: impl AsRef<Path>) -> Result<ScanReport> {
        let started = Instant::now();
        let root = absolute_root(root.as_ref())?;
        let root_display = path_to_storage(&root);
        let generation = next_generation();
        let mut indexed_files = 0_u64;
        let mut indexed_directories = 0_u64;
        let mut skipped_entries = 0_u64;

        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;

        // 先创建根记录，随后写入的条目才能通过外键校验；最终状态在扫描结束
        // 时一次性更新，读者不会看到半完成的快照。
        transaction.execute(
            "INSERT INTO roots (root, last_scan_unix_ms, last_scan_complete, skipped_entries)
             VALUES (?1, ?2, 0, 0)
             ON CONFLICT(root) DO NOTHING",
            params![&root_display, current_unix_ms()],
        )?;

        for entry in WalkDir::new(&root).follow_links(false).into_iter().skip(1) {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    skipped_entries += 1;
                    continue;
                }
            };

            let relative = match entry.path().strip_prefix(&root) {
                Ok(relative) => relative,
                Err(_) => {
                    skipped_entries += 1;
                    continue;
                }
            };
            let metadata = match fs::symlink_metadata(entry.path()) {
                Ok(metadata) => metadata,
                Err(_) => {
                    skipped_entries += 1;
                    continue;
                }
            };

            let is_directory = metadata.is_dir();
            if is_directory {
                indexed_directories += 1;
            } else {
                indexed_files += 1;
            }

            let name = entry.file_name().to_string_lossy().into_owned();
            let relative_path = path_to_storage(relative);
            let modified_unix_ms = metadata.modified().ok().and_then(system_time_to_ms);
            let size_bytes = metadata.len().min(i64::MAX as u64) as i64;

            transaction.execute(
                "INSERT INTO entries (
                    root, relative_path, name, normalized_name, normalized_path,
                    is_directory, size_bytes, modified_unix_ms, generation, file_reference_number
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL)
                ON CONFLICT(root, relative_path) DO UPDATE SET
                    name = excluded.name,
                    normalized_name = excluded.normalized_name,
                    normalized_path = excluded.normalized_path,
                    is_directory = excluded.is_directory,
                    size_bytes = excluded.size_bytes,
                    modified_unix_ms = excluded.modified_unix_ms,
                    generation = excluded.generation,
                    file_reference_number = excluded.file_reference_number",
                params![
                    &root_display,
                    &relative_path,
                    &name,
                    normalize_for_search(&name),
                    normalize_for_search(&relative_path),
                    i64::from(is_directory),
                    size_bytes,
                    modified_unix_ms,
                    generation,
                ],
            )?;
        }

        // 一次扫描若遇到无权访问等错误，保留旧条目比误删结果更安全。
        let complete = skipped_entries == 0;
        if complete {
            transaction.execute(
                "DELETE FROM entries WHERE root = ?1 AND generation <> ?2",
                params![&root_display, generation],
            )?;
        }
        transaction.execute(
            "INSERT INTO roots (root, last_scan_unix_ms, last_scan_complete, skipped_entries)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(root) DO UPDATE SET
                last_scan_unix_ms = excluded.last_scan_unix_ms,
                last_scan_complete = excluded.last_scan_complete,
                skipped_entries = excluded.skipped_entries",
            params![
                &root_display,
                current_unix_ms(),
                i64::from(complete),
                skipped_entries.min(i64::MAX as u64) as i64,
            ],
        )?;
        transaction.commit()?;

        Ok(ScanReport {
            root: root_display,
            indexed_files,
            indexed_directories,
            skipped_entries,
            complete,
            elapsed_ms: started.elapsed().as_millis(),
        })
    }

    pub fn search(&self, request: &SearchRequest) -> Result<Vec<SearchItem>> {
        let query = normalize_for_search(&request.query);
        if query.is_empty() {
            return Err(SearchError::EmptyQuery);
        }
        let limit = request.limit.clamp(1, MAX_QUERY_LIMIT) as i64;
        let terms = query.split_whitespace().collect::<Vec<_>>();
        let root = request
            .root
            .as_deref()
            .map(root_for_lookup)
            .transpose()?
            .map(|root| path_to_storage(&root));

        let mut conditions = Vec::with_capacity(terms.len());
        let mut name_only_conditions = Vec::with_capacity(terms.len());
        let mut parameters = Vec::with_capacity(terms.len() + 4);
        for (index, term) in terms.iter().enumerate() {
            let parameter = index + 1;
            conditions.push(format!(
                "(normalized_name LIKE ?{parameter} ESCAPE '\\' OR relative_path LIKE ?{parameter} ESCAPE '\\')"
            ));
            name_only_conditions.push(format!("normalized_name LIKE ?{parameter} ESCAPE '\\'"));
            parameters.push(SqlValue::Text(format!("%{}%", escape_like(term))));
        }
        let extension_parameter = parameters.len() + 1;
        parameters.push(SqlValue::Text(format!(
            "%.{}",
            escape_like(query.trim_start_matches('.'))
        )));
        let exact_parameter = parameters.len() + 1;
        parameters.push(SqlValue::Text(query));
        let root_parameter = parameters.len() + 1;
        parameters.push(root.map(SqlValue::Text).unwrap_or(SqlValue::Null));
        let limit_parameter = parameters.len() + 1;
        parameters.push(SqlValue::Integer(limit));
        let sql = format!(
            "SELECT root, relative_path, name, is_directory, size_bytes, modified_unix_ms
             FROM entries
             WHERE {}
               AND (?{root_parameter} IS NULL OR root = ?{root_parameter})
             ORDER BY
                 CASE WHEN is_directory = 0 AND normalized_name LIKE ?{extension_parameter} ESCAPE '\\' THEN 0 ELSE 1 END,
                 CASE WHEN normalized_name = ?{exact_parameter} THEN 0 ELSE 1 END,
                 CASE WHEN {} THEN 0 ELSE 1 END,
                 is_directory ASC,
                 name COLLATE NOCASE,
                 relative_path COLLATE NOCASE
             LIMIT ?{limit_parameter}",
            conditions.join(" AND "),
            name_only_conditions.join(" AND ")
        );

        let connection = self.connection()?;
        let mut statement = connection.prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(parameters.iter()), |row| {
            let root: String = row.get(0)?;
            let relative_path: String = row.get(1)?;
            let path = path_to_storage(&PathBuf::from(root).join(relative_path));
            let size_bytes: i64 = row.get(4)?;
            Ok(SearchItem {
                path,
                name: row.get(2)?,
                is_directory: row.get::<_, i64>(3)? != 0,
                size_bytes: size_bytes.max(0) as u64,
                modified_unix_ms: row.get(5)?,
            })
        })?;

        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(SearchError::from)
    }

    /// 以 NTFS MFT 元数据建立整卷索引。它不读取文件正文，也不创建/修改 USN
    /// 日志；没有卷访问权限时调用方应回退到 `scan_root`。
    #[cfg(windows)]
    pub fn scan_ntfs_volume(&self, volume: char) -> Result<ScanReport> {
        let started = Instant::now();
        let snapshot = ntfs::snapshot_volume(volume).map_err(|source| SearchError::Io {
            path: PathBuf::from(format!("{volume}:")),
            source,
        })?;
        let root = path_to_storage(&PathBuf::from(format!("{}:\\", snapshot.volume)));
        let records = snapshot
            .records
            .into_iter()
            .filter(|record| record.file_reference_number != ntfs::ROOT_FILE_REFERENCE)
            .collect::<Vec<_>>();
        self.persist_ntfs_records(
            root,
            records,
            snapshot.journal_id,
            snapshot.next_usn,
            started,
        )
    }

    /// 重放持久化的 USN 游标后的变更。日志换代、截断或父树无法闭合时会安全地
    /// 回退为一次 MFT 快照，而不会猜测删除哪些路径。
    #[cfg(windows)]
    pub fn sync_ntfs_volume(&self, volume: char) -> Result<ScanReport> {
        self.sync_ntfs_volume_inner(volume, true)?
            .ok_or_else(|| SearchError::Io {
                path: PathBuf::from(format!("{volume}:")),
                source: std::io::Error::other("显式 NTFS 同步未生成索引报告"),
            })
    }

    /// 供托盘宿主低频调用：只重放已有 USN 游标，不会在日志换代时自行触发整卷
    /// MFT 扫描。需要完整重建时把根标为不完整，等待用户或 Agent 明确发起。
    #[cfg(windows)]
    pub fn sync_indexed_ntfs_volumes(&self) -> Result<Vec<ScanReport>> {
        let connection = self.connection()?;
        let mut statement =
            connection.prepare("SELECT root FROM ntfs_volume_state ORDER BY root")?;
        let roots = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(statement);
        drop(connection);

        let mut reports = Vec::with_capacity(roots.len());
        for root in roots {
            let Some(volume) = volume_from_ntfs_root(&root) else {
                self.mark_ntfs_requires_snapshot(&root)?;
                continue;
            };
            if let Some(report) = self.sync_ntfs_volume_inner(volume, false)? {
                reports.push(report);
            }
        }
        Ok(reports)
    }

    #[cfg(windows)]
    fn sync_ntfs_volume_inner(
        &self,
        volume: char,
        allow_full_snapshot: bool,
    ) -> Result<Option<ScanReport>> {
        let started = Instant::now();
        let volume = volume.to_ascii_uppercase();
        let root = path_to_storage(&PathBuf::from(format!("{volume}:\\")));
        let connection = self.connection()?;
        let state = connection
            .query_row(
                "SELECT journal_id, next_usn FROM ntfs_volume_state WHERE root = ?1",
                [&root],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        let Some((journal_id, next_usn)) = state else {
            return self.handle_ntfs_snapshot_required(&root, volume, allow_full_snapshot);
        };
        let (journal_id, next_usn) = match (journal_id.parse::<u64>(), next_usn.parse::<i64>()) {
            (Ok(journal_id), Ok(next_usn)) => (journal_id, next_usn),
            _ => return self.handle_ntfs_snapshot_required(&root, volume, allow_full_snapshot),
        };
        drop(connection);

        let delta = match ntfs::read_journal_since(volume, journal_id, next_usn) {
            Ok(delta) => delta,
            Err(ntfs::JournalReadError::Reset) => {
                return self.handle_ntfs_snapshot_required(&root, volume, allow_full_snapshot);
            }
            Err(ntfs::JournalReadError::Io(source)) => {
                return Err(SearchError::Io {
                    path: PathBuf::from(format!("{volume}:")),
                    source,
                });
            }
            Err(ntfs::JournalReadError::Parse(error)) => {
                return Err(SearchError::Io {
                    path: PathBuf::from(format!("{volume}:")),
                    source: std::io::Error::new(std::io::ErrorKind::InvalidData, error),
                });
            }
        };

        if delta.records.is_empty() {
            self.update_ntfs_cursor(&root, delta.journal.journal_id, delta.next_usn)?;
            return self.report_for_root(&root, started).map(Some);
        }

        if !self.apply_ntfs_delta(&root, delta.journal, delta.next_usn, delta.records)? {
            return self.handle_ntfs_snapshot_required(&root, volume, allow_full_snapshot);
        }
        self.report_for_root(&root, started).map(Some)
    }

    #[cfg(windows)]
    fn handle_ntfs_snapshot_required(
        &self,
        root: &str,
        volume: char,
        allow_full_snapshot: bool,
    ) -> Result<Option<ScanReport>> {
        if allow_full_snapshot {
            self.scan_ntfs_volume(volume).map(Some)
        } else {
            self.mark_ntfs_requires_snapshot(root)?;
            Ok(None)
        }
    }

    pub fn status(&self) -> Result<IndexStatus> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT
                 SUM(CASE WHEN is_directory = 0 THEN 1 ELSE 0 END),
                 SUM(CASE WHEN is_directory = 1 THEN 1 ELSE 0 END),
                 (SELECT COUNT(*) FROM roots),
                 (SELECT COUNT(*) FROM roots WHERE last_scan_complete = 0)
             FROM entries",
                [],
                |row| {
                    Ok(IndexStatus {
                        indexed_files: row.get::<_, Option<i64>>(0)?.unwrap_or(0).max(0) as u64,
                        indexed_directories: row.get::<_, Option<i64>>(1)?.unwrap_or(0).max(0)
                            as u64,
                        roots: row.get::<_, i64>(2)?.max(0) as u64,
                        incomplete_roots: row.get::<_, i64>(3)?.max(0) as u64,
                    })
                },
            )
            .map_err(SearchError::from)
    }

    pub fn indexed_roots(&self) -> Result<Vec<IndexedRoot>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT r.root, r.last_scan_unix_ms, r.last_scan_complete, r.skipped_entries,
                    EXISTS(SELECT 1 FROM ntfs_volume_state n WHERE n.root = r.root)
             FROM roots r
             ORDER BY r.root",
        )?;
        let rows = statement.query_map([], |row| {
            let skipped_entries = row.get::<_, i64>(3)?.max(0) as u64;
            Ok(IndexedRoot {
                root: row.get(0)?,
                last_scan_unix_ms: row.get(1)?,
                complete: row.get::<_, i64>(2)? != 0,
                skipped_entries,
                uses_ntfs_usn: row.get::<_, i64>(4)? != 0,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(SearchError::from)
    }

    pub fn remove_root(&self, root: impl AsRef<Path>) -> Result<bool> {
        let root = root_for_lookup(root.as_ref())?;
        let root_display = path_to_storage(&root);
        let connection = self.connection()?;
        let removed = connection.execute("DELETE FROM roots WHERE root = ?1", [&root_display])?;
        connection.execute("DELETE FROM entries WHERE root = ?1", [&root_display])?;
        Ok(removed > 0)
    }

    /// 保存一次 MFT 快照。少量孤立记录不会让数百万条已解析路径整体不可用；
    /// 但不完整快照不会建立 USN 增量游标，避免在缺失父链上猜测后续变更。
    #[cfg(windows)]
    fn persist_ntfs_records(
        &self,
        root: String,
        records: Vec<ntfs::UsnRecord>,
        journal_id: u64,
        next_usn: i64,
        started: Instant,
    ) -> Result<ScanReport> {
        let paths = ntfs::resolve_relative_paths(&records);
        let resolved_records = records
            .iter()
            .filter(|record| {
                paths
                    .get(&record.file_reference_number)
                    .is_some_and(|path| valid_ntfs_relative_path(path))
            })
            .count();
        let skipped_entries = records.len().saturating_sub(resolved_records) as u64;
        let complete = skipped_entries == 0;
        let indexed_directories = records
            .iter()
            .filter(|record| {
                ntfs::is_directory(record)
                    && paths
                        .get(&record.file_reference_number)
                        .is_some_and(|path| valid_ntfs_relative_path(path))
            })
            .count() as u64;
        let indexed_files = resolved_records as u64 - indexed_directories;
        let generation = next_generation();

        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        transaction.execute(
            "INSERT INTO roots (root, last_scan_unix_ms, last_scan_complete, skipped_entries)
             VALUES (?1, ?2, 0, 0)
             ON CONFLICT(root) DO NOTHING",
            params![&root, current_unix_ms()],
        )?;

        transaction.execute("DELETE FROM entries WHERE root = ?1", [&root])?;
        transaction.execute("DELETE FROM ntfs_nodes WHERE root = ?1", [&root])?;
        if complete && journal_id != 0 {
            for record in &records {
                if let Some(path) = paths
                    .get(&record.file_reference_number)
                    .filter(|path| valid_ntfs_relative_path(path))
                {
                    let node = ntfs_node_from_record(record, path.clone());
                    Self::upsert_ntfs_node(&transaction, &root, &node)?;
                }
            }
            Self::update_ntfs_cursor_in(&transaction, &root, journal_id, next_usn)?;
        } else {
            // 路径树不完整或卷没有活动日志时都只保留可搜索快照，不建立
            // 增量游标；后者仍可由用户再次点击“更新本机索引”刷新。
            transaction.execute("DELETE FROM ntfs_volume_state WHERE root = ?1", [&root])?;
        }
        for record in &records {
            if let Some(path) = paths
                .get(&record.file_reference_number)
                .filter(|path| valid_ntfs_relative_path(path))
            {
                let node = ntfs_node_from_record(record, path.clone());
                Self::upsert_ntfs_entry(&transaction, &root, &node, generation)?;
            }
        }

        transaction.execute(
            "UPDATE roots
             SET last_scan_unix_ms = ?2,
                 last_scan_complete = ?3,
                 skipped_entries = ?4
             WHERE root = ?1",
            params![
                &root,
                current_unix_ms(),
                i64::from(complete),
                skipped_entries.min(i64::MAX as u64) as i64,
            ],
        )?;
        transaction.commit()?;

        Ok(ScanReport {
            root,
            indexed_files,
            indexed_directories,
            skipped_entries,
            complete,
            elapsed_ms: started.elapsed().as_millis(),
        })
    }

    /// 在一个事务内按 USN 顺序写入变更。若父节点、路径或重命名目标不能可靠
    /// 判定，返回 `false`，由上层退回一次完整 MFT 快照。
    #[cfg(windows)]
    fn apply_ntfs_delta(
        &self,
        root: &str,
        journal: ntfs::JournalInfo,
        next_usn: i64,
        mut records: Vec<ntfs::UsnRecord>,
    ) -> Result<bool> {
        records.sort_by_key(|record| record.usn);
        let generation = next_generation();
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;

        for record in &records {
            if record.file_reference_number == ntfs::ROOT_FILE_REFERENCE {
                continue;
            }
            if !Self::apply_ntfs_delta_record(&transaction, root, record, generation)? {
                // 未提交的事务会在离开作用域时回滚，避免部分变更污染索引。
                return Ok(false);
            }
        }

        Self::update_ntfs_cursor_in(&transaction, root, journal.journal_id, next_usn)?;
        transaction.execute(
            "UPDATE roots
             SET last_scan_unix_ms = ?2, last_scan_complete = 1, skipped_entries = 0
             WHERE root = ?1",
            params![root, current_unix_ms()],
        )?;
        transaction.commit()?;
        Ok(true)
    }

    #[cfg(windows)]
    fn apply_ntfs_delta_record(
        transaction: &Transaction<'_>,
        root: &str,
        record: &ntfs::UsnRecord,
        generation: i64,
    ) -> Result<bool> {
        let old_node =
            Self::ntfs_node_by_reference(transaction, root, record.file_reference_number)?;
        if ntfs::is_deleted(record) {
            if let Some(old_node) = old_node {
                Self::delete_ntfs_subtree(transaction, root, &old_node.relative_path)?;
            }
            return Ok(true);
        }
        if !valid_ntfs_component(&record.name) {
            return Ok(false);
        }

        let parent_path = if record.parent_file_reference_number == ntfs::ROOT_FILE_REFERENCE {
            String::new()
        } else {
            let Some(parent) = Self::ntfs_node_by_reference(
                transaction,
                root,
                record.parent_file_reference_number,
            )?
            else {
                return Ok(false);
            };
            if !parent.is_directory || !valid_ntfs_relative_path(&parent.relative_path) {
                return Ok(false);
            }
            parent.relative_path
        };
        let relative_path = if parent_path.is_empty() {
            record.name.clone()
        } else {
            format!("{parent_path}/{}", record.name)
        };
        if !valid_ntfs_relative_path(&relative_path) {
            return Ok(false);
        }

        let record_reference = record.file_reference_number.to_string();
        let conflict = transaction
            .query_row(
                "SELECT file_reference_number
                 FROM ntfs_nodes
                 WHERE root = ?1 AND relative_path = ?2 AND file_reference_number <> ?3
                 LIMIT 1",
                params![root, &relative_path, &record_reference],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if conflict.is_some() {
            return Ok(false);
        }

        let node = ntfs_node_from_record(record, relative_path.clone());
        match old_node {
            Some(old_node) if old_node.relative_path != relative_path => {
                // 先按旧 FRN 集合删除条目，再改节点路径；这样同名目标的稍后
                // 删除记录不会误删刚重命名进来的新条目。
                Self::delete_ntfs_entries_for_subtree(transaction, root, &old_node.relative_path)?;
                let suffix_start = old_node.relative_path.chars().count() as i64 + 1;
                transaction.execute(
                    "UPDATE ntfs_nodes
                     SET relative_path = ?1 || substr(relative_path, ?2)
                     WHERE root = ?3
                       AND (relative_path = ?4 OR relative_path LIKE ?5 ESCAPE '\\')",
                    params![
                        &relative_path,
                        suffix_start,
                        root,
                        &old_node.relative_path,
                        ntfs_path_prefix_pattern(&old_node.relative_path),
                    ],
                )?;
                Self::upsert_ntfs_node(transaction, root, &node)?;
                for changed_node in
                    Self::ntfs_nodes_for_path_prefix(transaction, root, &relative_path)?
                {
                    Self::upsert_ntfs_entry(transaction, root, &changed_node, generation)?;
                }
            }
            _ => {
                Self::upsert_ntfs_node(transaction, root, &node)?;
                Self::upsert_ntfs_entry(transaction, root, &node, generation)?;
            }
        }
        Ok(true)
    }

    #[cfg(windows)]
    fn ntfs_node_by_reference(
        transaction: &Transaction<'_>,
        root: &str,
        file_reference_number: u64,
    ) -> Result<Option<NtfsNode>> {
        transaction
            .query_row(
                "SELECT file_reference_number, parent_file_reference_number, relative_path,
                        name, is_directory, modified_unix_ms
                 FROM ntfs_nodes
                 WHERE root = ?1 AND file_reference_number = ?2",
                params![root, file_reference_number.to_string()],
                ntfs_node_from_row,
            )
            .optional()
            .map_err(SearchError::from)
    }

    #[cfg(windows)]
    fn ntfs_nodes_for_path_prefix(
        transaction: &Transaction<'_>,
        root: &str,
        path: &str,
    ) -> Result<Vec<NtfsNode>> {
        let mut statement = transaction.prepare(
            "SELECT file_reference_number, parent_file_reference_number, relative_path,
                    name, is_directory, modified_unix_ms
             FROM ntfs_nodes
             WHERE root = ?1
               AND (relative_path = ?2 OR relative_path LIKE ?3 ESCAPE '\\')
             ORDER BY length(relative_path), relative_path",
        )?;
        let rows = statement.query_map(
            params![root, path, ntfs_path_prefix_pattern(path)],
            ntfs_node_from_row,
        )?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(SearchError::from)
    }

    #[cfg(windows)]
    fn delete_ntfs_entries_for_subtree(
        transaction: &Transaction<'_>,
        root: &str,
        path: &str,
    ) -> Result<()> {
        transaction.execute(
            "DELETE FROM entries
             WHERE root = ?1
               AND file_reference_number IN (
                    SELECT file_reference_number
                    FROM ntfs_nodes
                    WHERE root = ?1
                      AND (relative_path = ?2 OR relative_path LIKE ?3 ESCAPE '\\')
               )",
            params![root, path, ntfs_path_prefix_pattern(path)],
        )?;
        Ok(())
    }

    #[cfg(windows)]
    fn delete_ntfs_subtree(transaction: &Transaction<'_>, root: &str, path: &str) -> Result<()> {
        Self::delete_ntfs_entries_for_subtree(transaction, root, path)?;
        transaction.execute(
            "DELETE FROM ntfs_nodes
             WHERE root = ?1
               AND (relative_path = ?2 OR relative_path LIKE ?3 ESCAPE '\\')",
            params![root, path, ntfs_path_prefix_pattern(path)],
        )?;
        Ok(())
    }

    #[cfg(windows)]
    fn upsert_ntfs_node(transaction: &Transaction<'_>, root: &str, node: &NtfsNode) -> Result<()> {
        transaction
            .prepare_cached(
                "INSERT INTO ntfs_nodes (
                 root, file_reference_number, parent_file_reference_number, relative_path,
                 name, is_directory, modified_unix_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(root, file_reference_number) DO UPDATE SET
                 parent_file_reference_number = excluded.parent_file_reference_number,
                 relative_path = excluded.relative_path,
                 name = excluded.name,
                 is_directory = excluded.is_directory,
                 modified_unix_ms = excluded.modified_unix_ms",
            )?
            .execute(params![
                root,
                &node.file_reference_number,
                &node.parent_file_reference_number,
                &node.relative_path,
                &node.name,
                i64::from(node.is_directory),
                node.modified_unix_ms,
            ])?;
        Ok(())
    }

    #[cfg(windows)]
    fn upsert_ntfs_entry(
        transaction: &Transaction<'_>,
        root: &str,
        node: &NtfsNode,
        generation: i64,
    ) -> Result<()> {
        transaction
            .prepare_cached(
                "INSERT INTO entries (
                 root, relative_path, name, normalized_name, normalized_path,
                 is_directory, size_bytes, modified_unix_ms, generation, file_reference_number
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7, ?8, ?9)
             ON CONFLICT(root, relative_path) DO UPDATE SET
                 name = excluded.name,
                 normalized_name = excluded.normalized_name,
                 normalized_path = excluded.normalized_path,
                 is_directory = excluded.is_directory,
                 size_bytes = excluded.size_bytes,
                 modified_unix_ms = excluded.modified_unix_ms,
                 generation = excluded.generation,
                 file_reference_number = excluded.file_reference_number",
            )?
            .execute(params![
                root,
                &node.relative_path,
                &node.name,
                normalize_for_search(&node.name),
                // 人工入口按文件名、扩展名和名称关键词搜索；完整相对路径已在
                // relative_path 中保存，无需再复制一份大写折叠路径。
                normalize_for_search(&node.name),
                i64::from(node.is_directory),
                node.modified_unix_ms,
                generation,
                &node.file_reference_number,
            ])?;
        Ok(())
    }

    #[cfg(windows)]
    fn update_ntfs_cursor(&self, root: &str, journal_id: u64, next_usn: i64) -> Result<()> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        Self::update_ntfs_cursor_in(&transaction, root, journal_id, next_usn)?;
        transaction.commit()?;
        Ok(())
    }

    #[cfg(windows)]
    fn update_ntfs_cursor_in(
        transaction: &Transaction<'_>,
        root: &str,
        journal_id: u64,
        next_usn: i64,
    ) -> Result<()> {
        transaction.execute(
            "INSERT INTO ntfs_volume_state (root, journal_id, next_usn)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(root) DO UPDATE SET
                 journal_id = excluded.journal_id,
                 next_usn = excluded.next_usn",
            params![root, journal_id.to_string(), next_usn.to_string()],
        )?;
        Ok(())
    }

    #[cfg(windows)]
    fn mark_ntfs_requires_snapshot(&self, root: &str) -> Result<()> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        transaction.execute("DELETE FROM ntfs_volume_state WHERE root = ?1", [root])?;
        transaction.execute(
            "UPDATE roots
             SET last_scan_complete = 0,
                 skipped_entries = CASE WHEN skipped_entries < 1 THEN 1 ELSE skipped_entries END,
                 last_scan_unix_ms = ?2
             WHERE root = ?1",
            params![root, current_unix_ms()],
        )?;
        transaction.commit()?;
        Ok(())
    }

    #[cfg(windows)]
    fn report_for_root(&self, root: &str, started: Instant) -> Result<ScanReport> {
        let connection = self.connection()?;
        let (complete, skipped_entries) = connection
            .query_row(
                "SELECT last_scan_complete, skipped_entries FROM roots WHERE root = ?1",
                [root],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?
            .ok_or_else(|| SearchError::Io {
                path: PathBuf::from(root),
                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "NTFS 索引根不存在，无法更新 USN 游标",
                ),
            })?;
        let (indexed_files, indexed_directories) = connection.query_row(
            "SELECT
                 SUM(CASE WHEN is_directory = 0 THEN 1 ELSE 0 END),
                 SUM(CASE WHEN is_directory = 1 THEN 1 ELSE 0 END)
             FROM entries WHERE root = ?1",
            [root],
            |row| {
                Ok((
                    row.get::<_, Option<i64>>(0)?.unwrap_or(0).max(0) as u64,
                    row.get::<_, Option<i64>>(1)?.unwrap_or(0).max(0) as u64,
                ))
            },
        )?;
        Ok(ScanReport {
            root: root.to_owned(),
            indexed_files,
            indexed_directories,
            skipped_entries: skipped_entries.max(0) as u64,
            complete: complete != 0,
            elapsed_ms: started.elapsed().as_millis(),
        })
    }

    fn initialize(&self) -> Result<()> {
        let connection = self.connection()?;
        connection.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;

             CREATE TABLE IF NOT EXISTS roots (
                 root TEXT PRIMARY KEY NOT NULL,
                 last_scan_unix_ms INTEGER NOT NULL,
                 last_scan_complete INTEGER NOT NULL,
                 skipped_entries INTEGER NOT NULL
             );

             CREATE TABLE IF NOT EXISTS entries (
                 root TEXT NOT NULL,
                 relative_path TEXT NOT NULL,
                 name TEXT NOT NULL,
                 normalized_name TEXT NOT NULL,
                 normalized_path TEXT NOT NULL,
                 is_directory INTEGER NOT NULL,
                 size_bytes INTEGER NOT NULL,
                 modified_unix_ms INTEGER,
                 generation INTEGER NOT NULL,
                 file_reference_number TEXT,
                 PRIMARY KEY (root, relative_path),
                 FOREIGN KEY (root) REFERENCES roots(root) ON DELETE CASCADE
             );

             DROP INDEX IF EXISTS entries_name_idx;
             DROP INDEX IF EXISTS entries_path_idx;
             CREATE TABLE IF NOT EXISTS ntfs_volume_state (
                 root TEXT PRIMARY KEY NOT NULL,
                 journal_id TEXT NOT NULL,
                 next_usn TEXT NOT NULL,
                 FOREIGN KEY (root) REFERENCES roots(root) ON DELETE CASCADE
             );

             CREATE TABLE IF NOT EXISTS ntfs_nodes (
                 root TEXT NOT NULL,
                 file_reference_number TEXT NOT NULL,
                 parent_file_reference_number TEXT NOT NULL,
                 relative_path TEXT NOT NULL,
                 name TEXT NOT NULL,
                 is_directory INTEGER NOT NULL,
                 modified_unix_ms INTEGER,
                 PRIMARY KEY (root, file_reference_number),
                 FOREIGN KEY (root) REFERENCES roots(root) ON DELETE CASCADE
             );",
        )?;
        ensure_column(&connection, "entries", "file_reference_number", "TEXT")?;
        ensure_column(
            &connection,
            "ntfs_nodes",
            "relative_path",
            "TEXT NOT NULL DEFAULT ''",
        )?;
        connection.execute_batch(
            "CREATE INDEX IF NOT EXISTS entries_ntfs_reference_idx
             ON entries(root, file_reference_number);",
        )?;
        Ok(())
    }

    fn connection(&self) -> Result<Connection> {
        let connection = Connection::open(&self.database_path)?;
        // foreign_keys 是 SQLite 的每连接开关；所有短连接都必须显式启用，
        // 否则删除根目录时可能留下孤立节点或 USN 游标。
        connection.execute_batch("PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;")?;
        Ok(connection)
    }
}

/// 枚举 Windows 当前挂载的本机固定磁盘。可移动盘和网络盘不会被后台误扫；
/// 用户仍可通过 CLI 的 `index <目录>` 明确加入它们。
#[cfg(windows)]
pub fn local_fixed_volumes() -> Vec<char> {
    use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives};

    const DRIVE_FIXED: u32 = 3;

    let mask = unsafe { GetLogicalDrives() };
    (0..26)
        .filter(|offset| mask & (1_u32 << offset) != 0)
        .map(|offset| (b'A' + offset as u8) as char)
        .filter(|volume| {
            let root = [*volume as u16, b':' as u16, b'\\' as u16, 0];
            unsafe { GetDriveTypeW(root.as_ptr()) == DRIVE_FIXED }
        })
        .collect()
}

fn ensure_column(
    connection: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> rusqlite::Result<()> {
    // 参数只来自本模块的固定模式名和列定义，不接受外部输入。
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if !columns.iter().any(|existing| existing == column) {
        connection.execute_batch(&format!(
            "ALTER TABLE {table} ADD COLUMN {column} {definition}"
        ))?;
    }
    Ok(())
}

#[cfg(windows)]
fn ntfs_node_from_record(record: &ntfs::UsnRecord, relative_path: String) -> NtfsNode {
    NtfsNode {
        file_reference_number: record.file_reference_number.to_string(),
        parent_file_reference_number: record.parent_file_reference_number.to_string(),
        relative_path,
        name: record.name.clone(),
        is_directory: ntfs::is_directory(record),
        modified_unix_ms: filetime_to_unix_ms(record.timestamp_filetime),
    }
}

#[cfg(windows)]
fn ntfs_node_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<NtfsNode> {
    Ok(NtfsNode {
        file_reference_number: row.get(0)?,
        parent_file_reference_number: row.get(1)?,
        relative_path: row.get(2)?,
        name: row.get(3)?,
        is_directory: row.get::<_, i64>(4)? != 0,
        modified_unix_ms: row.get(5)?,
    })
}

#[cfg(windows)]
fn valid_ntfs_component(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0'])
}

#[cfg(windows)]
fn valid_ntfs_relative_path(path: &str) -> bool {
    !path.is_empty() && path.split('/').all(valid_ntfs_component)
}

#[cfg(windows)]
fn ntfs_path_prefix_pattern(path: &str) -> String {
    format!("{}/%", escape_like(path))
}

#[cfg(windows)]
fn volume_from_ntfs_root(root: &str) -> Option<char> {
    let bytes = root.as_bytes();
    match bytes {
        [letter, b':', b'/'] if letter.is_ascii_alphabetic() => {
            Some((*letter as char).to_ascii_uppercase())
        }
        _ => None,
    }
}

#[derive(Debug, Deserialize)]
struct RpcSearchParams {
    query: String,
    limit: Option<usize>,
    root: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct RpcRootParams {
    root: PathBuf,
}

#[cfg(windows)]
#[derive(Debug, Deserialize)]
struct RpcVolumeParams {
    volume: char,
}

/// 处理单个 JSON-RPC 2.0 请求。此函数不读取文件内容；搜索结果只有元数据。
pub fn handle_rpc(index: &SearchIndex, request: Value) -> Value {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    if request.get("jsonrpc") != Some(&Value::String("2.0".to_owned())) {
        return rpc_error(id, -32600, "仅支持 JSON-RPC 2.0");
    }
    let Some(method) = request.get("method").and_then(Value::as_str) else {
        return rpc_error(id, -32600, "缺少 method");
    };
    let params = request.get("params").cloned().unwrap_or(Value::Null);

    let result = match method {
        "status.get" => index.status().and_then(to_json),
        "index.list_roots" => index
            .indexed_roots()
            .and_then(|roots| to_json(json!({"roots": roots}))),
        "search.query" => serde_json::from_value::<RpcSearchParams>(params)
            .map_err(|error| SearchError::Io {
                path: PathBuf::from("<rpc params>"),
                source: std::io::Error::new(std::io::ErrorKind::InvalidInput, error),
            })
            .and_then(|params| {
                let mut request = SearchRequest::new(params.query);
                request.limit = params.limit.unwrap_or(50);
                request.root = params.root;
                index.search(&request)
            })
            .and_then(|items| to_json(json!({"items": items}))),
        "index.scan_root" => serde_json::from_value::<RpcRootParams>(params)
            .map_err(|error| SearchError::Io {
                path: PathBuf::from("<rpc params>"),
                source: std::io::Error::new(std::io::ErrorKind::InvalidInput, error),
            })
            .and_then(|params| index.scan_root(params.root))
            .and_then(to_json),
        "index.remove_root" => serde_json::from_value::<RpcRootParams>(params)
            .map_err(|error| SearchError::Io {
                path: PathBuf::from("<rpc params>"),
                source: std::io::Error::new(std::io::ErrorKind::InvalidInput, error),
            })
            .and_then(|params| index.remove_root(params.root))
            .and_then(|removed| to_json(json!({"removed": removed}))),
        #[cfg(windows)]
        "index.scan_volume" => serde_json::from_value::<RpcVolumeParams>(params)
            .map_err(|error| SearchError::Io {
                path: PathBuf::from("<rpc params>"),
                source: std::io::Error::new(std::io::ErrorKind::InvalidInput, error),
            })
            .and_then(|params| index.scan_ntfs_volume(params.volume))
            .and_then(to_json),
        #[cfg(windows)]
        "index.sync_volume" => serde_json::from_value::<RpcVolumeParams>(params)
            .map_err(|error| SearchError::Io {
                path: PathBuf::from("<rpc params>"),
                source: std::io::Error::new(std::io::ErrorKind::InvalidInput, error),
            })
            .and_then(|params| index.sync_ntfs_volume(params.volume))
            .and_then(to_json),
        #[cfg(windows)]
        "index.sync_all_volumes" => index
            .sync_indexed_ntfs_volumes()
            .and_then(|reports| to_json(json!({"reports": reports}))),
        _ => return rpc_error(id, -32601, "未实现的方法"),
    };

    match result {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(error) => rpc_error(id, -32000, &error.to_string()),
    }
}

fn to_json<T: Serialize>(value: T) -> Result<Value> {
    serde_json::to_value(value).map_err(|error| SearchError::Io {
        path: PathBuf::from("<json response>"),
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, error),
    })
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message},
    })
}

fn absolute_root(root: &Path) -> Result<PathBuf> {
    let metadata = fs::metadata(root).map_err(|source| SearchError::Io {
        path: root.to_path_buf(),
        source,
    })?;
    if !metadata.is_dir() {
        return Err(SearchError::NotDirectory(root.to_path_buf()));
    }
    if root.is_absolute() {
        Ok(root.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|current| current.join(root))
            .map_err(|source| SearchError::Io {
                path: root.to_path_buf(),
                source,
            })
    }
}

/// 删除索引只需要可重复的根键，不要求原磁盘或移动盘此刻仍可访问。
fn root_for_lookup(root: &Path) -> Result<PathBuf> {
    if root.is_absolute() {
        Ok(root.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|current| current.join(root))
            .map_err(|source| SearchError::Io {
                path: root.to_path_buf(),
                source,
            })
    }
}

fn normalize_for_search(value: &str) -> String {
    value.nfc().flat_map(char::to_lowercase).collect()
}

fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn path_to_storage(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn current_unix_ms() -> i64 {
    system_time_to_ms(SystemTime::now()).unwrap_or(0)
}

fn next_generation() -> i64 {
    let now = current_unix_ms();
    let mut observed = LAST_GENERATION.load(Ordering::Relaxed);
    loop {
        let candidate = now.max(observed.saturating_add(1));
        match LAST_GENERATION.compare_exchange_weak(
            observed,
            candidate,
            Ordering::SeqCst,
            Ordering::Relaxed,
        ) {
            Ok(_) => return candidate,
            Err(actual) => observed = actual,
        }
    }
}

fn system_time_to_ms(value: SystemTime) -> Option<i64> {
    value
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
}

#[cfg(windows)]
fn filetime_to_unix_ms(filetime: i64) -> Option<i64> {
    const WINDOWS_TO_UNIX_EPOCH_100NS: i128 = 116_444_736_000_000_000;
    let milliseconds = (i128::from(filetime) - WINDOWS_TO_UNIX_EPOCH_100NS) / 10_000;
    i64::try_from(milliseconds).ok()
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    fn record(
        file_reference_number: u64,
        parent_file_reference_number: u64,
        usn: i64,
        is_directory: bool,
        name: &str,
    ) -> ntfs::UsnRecord {
        ntfs::UsnRecord {
            file_reference_number,
            parent_file_reference_number,
            usn,
            timestamp_filetime: 116_444_736_000_000_000,
            reason: 0,
            file_attributes: if is_directory { 0x10 } else { 0 },
            name: name.to_owned(),
        }
    }

    #[test]
    fn ntfs_delta_renames_descendants_and_deletes_a_subtree() {
        let temporary = tempfile::tempdir().expect("创建临时目录");
        let index = SearchIndex::open(temporary.path().join("index.db")).expect("打开索引");
        let root = "C:/".to_owned();
        index
            .persist_ntfs_records(
                root.clone(),
                vec![
                    record(10, ntfs::ROOT_FILE_REFERENCE, 1, true, "资料"),
                    record(11, 10, 2, false, "同步验证.txt"),
                ],
                7,
                3,
                Instant::now(),
            )
            .expect("持久化初始快照");

        let applied = index
            .apply_ntfs_delta(
                &root,
                ntfs::JournalInfo {
                    journal_id: 7,
                    first_usn: 0,
                    next_usn: 4,
                },
                4,
                vec![record(10, ntfs::ROOT_FILE_REFERENCE, 3, true, "归档")],
            )
            .expect("应用重命名变更");
        assert!(applied, "父节点完整时不应退回全量快照");

        let matches = index
            .search(&SearchRequest::new("归档/同步验证"))
            .expect("搜索重命名后的路径");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].path, "C:/归档/同步验证.txt");

        let applied = index
            .apply_ntfs_delta(
                &root,
                ntfs::JournalInfo {
                    journal_id: 7,
                    first_usn: 0,
                    next_usn: 5,
                },
                5,
                vec![ntfs::UsnRecord {
                    reason: windows_sys::Win32::System::Ioctl::USN_REASON_FILE_DELETE,
                    ..record(10, ntfs::ROOT_FILE_REFERENCE, 4, true, "归档")
                }],
            )
            .expect("应用删除变更");
        assert!(applied);
        assert!(
            index
                .search(&SearchRequest::new("同步验证"))
                .expect("搜索删除后的路径")
                .is_empty()
        );
    }

    #[test]
    fn incomplete_ntfs_snapshot_still_exposes_resolved_files() {
        let temporary = tempfile::tempdir().expect("创建临时目录");
        let index = SearchIndex::open(temporary.path().join("index.db")).expect("打开索引");
        let report = index
            .persist_ntfs_records(
                "C:/".to_owned(),
                vec![
                    record(10, ntfs::ROOT_FILE_REFERENCE, 1, true, "资料"),
                    record(11, 10, 2, false, "金福验收.pdf"),
                    record(99, 98, 3, false, "孤立记录.tmp"),
                ],
                7,
                4,
                Instant::now(),
            )
            .expect("持久化不完整快照");

        assert!(!report.complete);
        assert_eq!(report.skipped_entries, 1);
        let matches = index.search(&SearchRequest::new("pdf 金福")).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].path, "C:/资料/金福验收.pdf");
    }

    #[test]
    fn snapshot_without_an_active_journal_stays_searchable_without_a_sync_cursor() {
        let temporary = tempfile::tempdir().expect("创建临时目录");
        let index = SearchIndex::open(temporary.path().join("index.db")).expect("打开索引");
        let report = index
            .persist_ntfs_records(
                "E:/".to_owned(),
                vec![record(
                    11,
                    ntfs::ROOT_FILE_REFERENCE,
                    0,
                    false,
                    "无日志快照.pdf",
                )],
                0,
                0,
                Instant::now(),
            )
            .expect("持久化无日志快照");

        assert!(report.complete);
        let roots = index.indexed_roots().unwrap();
        assert_eq!(roots.len(), 1);
        assert!(roots[0].complete);
        assert!(!roots[0].uses_ntfs_usn);
        assert_eq!(
            index.search(&SearchRequest::new("pdf")).unwrap()[0].path,
            "E:/无日志快照.pdf"
        );
    }
}
