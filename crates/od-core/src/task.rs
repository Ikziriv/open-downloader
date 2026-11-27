//! Persistent, concurrent [`TaskManager`] with Gopeed-style lifecycle controls
//! (`resolve`, `create`, `pause`, `resume`, `retry`, `remove`), moving-average
//! speed/ETA calculation, and pub/sub [`TaskEvent`] broadcasts.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::{RwLock, Semaphore, broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::fetcher::{FetchContext, FetcherRegistry, ResolveRequest, Resource};
use crate::{DownloadOptions, DownloadOutcome, Error, Event};

/// Monotonic task identifier.
pub type TaskId = u64;

/// Scheduling priority for queued download tasks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskPriority {
    Low,
    #[default]
    Normal,
    High,
}

/// Lifecycle state of a managed download task.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", content = "detail", rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    Running,
    Paused,
    Completed,
    Failed(String),
}

impl TaskStatus {
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Pending | Self::Running)
    }
}

/// Live transfer metrics for a task.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct TaskProgress {
    pub downloaded: u64,
    pub total: Option<u64>,
    /// Smoothed transfer speed in bytes/sec.
    pub speed_bps: u64,
    /// Estimated remaining seconds, if total size and speed are known.
    pub eta_secs: Option<u64>,
}

/// Persistent snapshot of a task.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TaskRecord {
    pub id: TaskId,
    pub request: ResolveRequest,
    pub resource: Resource,
    /// Destination directory or explicit file path.
    pub dest: PathBuf,
    pub explicit_path: bool,
    /// Final output path once completed.
    pub output_path: Option<PathBuf>,
    pub status: TaskStatus,
    #[serde(default)]
    pub priority: TaskPriority,
    pub progress: TaskProgress,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

/// Request to create and enqueue a new download task.
#[derive(Debug, Clone)]
pub struct CreateTaskRequest {
    pub request: ResolveRequest,
    /// Optional pre-resolved [`Resource`]. When `None`, [`TaskManager::create`]
    /// resolves the URL first.
    pub resource: Option<Resource>,
    pub dest: PathBuf,
    pub explicit_path: bool,
    pub options: DownloadOptions,
    pub priority: TaskPriority,
    /// Start downloading immediately (default: `true`). Set `false` to add paused.
    pub auto_start: bool,
}

impl CreateTaskRequest {
    pub fn new(url: impl Into<String>, dest_dir: impl Into<PathBuf>) -> Self {
        Self {
            request: ResolveRequest::new(url),
            resource: None,
            dest: dest_dir.into(),
            explicit_path: false,
            options: DownloadOptions::default(),
            priority: TaskPriority::Normal,
            auto_start: true,
        }
    }
}

/// Events broadcast to UI, CLI, SSE, and MCP subscribers.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TaskEvent {
    Created { task: Box<TaskRecord> },
    Progress { id: TaskId, progress: TaskProgress },
    StatusChanged {
        id: TaskId,
        status: TaskStatus,
        output_path: Option<PathBuf>,
    },
    Removed { id: TaskId },
}

/// Pluggable persistence backend for [`TaskRecord`]s (atomic JSON file by default;
/// can be swapped for SQLite without changing [`TaskManager`]).
pub trait TaskStore: Send + Sync {
    fn load_all(&self) -> Result<Vec<TaskRecord>, Error>;
    fn save_all(&self, tasks: &[TaskRecord]) -> Result<(), Error>;
}

/// In-memory no-op store (useful for ephemeral CLI runs and tests).
#[derive(Default)]
pub struct MemoryTaskStore;

impl TaskStore for MemoryTaskStore {
    fn load_all(&self) -> Result<Vec<TaskRecord>, Error> {
        Ok(Vec::new())
    }

    fn save_all(&self, _tasks: &[TaskRecord]) -> Result<(), Error> {
        Ok(())
    }
}

/// Atomic JSON-file [`TaskStore`] using write-temp-then-rename.
pub struct FileTaskStore {
    path: PathBuf,
}

impl FileTaskStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl TaskStore for FileTaskStore {
    fn load_all(&self) -> Result<Vec<TaskRecord>, Error> {
        match std::fs::read(&self.path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(Error::Io(e)),
        }
    }

    fn save_all(&self, tasks: &[TaskRecord]) -> Result<(), Error> {
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let mut tmp = self.path.as_os_str().to_os_string();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        let bytes = serde_json::to_vec_pretty(tasks)?;
        std::fs::write(&tmp, bytes)?;
        if std::fs::rename(&tmp, &self.path).is_err() {
            let _ = std::fs::remove_file(&self.path);
            std::fs::rename(&tmp, &self.path)?;
        }
        Ok(())
    }
}

struct ManagedTask {
    record: TaskRecord,
    options: DownloadOptions,
    cancel: Option<CancellationToken>,
}

struct Inner {
    registry: FetcherRegistry,
    store: Arc<dyn TaskStore>,
    tasks: RwLock<BTreeMap<TaskId, ManagedTask>>,
    next_id: AtomicU64,
    slots: Arc<Semaphore>,
    events: broadcast::Sender<TaskEvent>,
}

/// Central task orchestrator owning protocol dispatch, concurrency limits,
/// persistence, pause/resume, and live progress/speed broadcasts.
#[derive(Clone)]
pub struct TaskManager {
    inner: Arc<Inner>,
}

impl TaskManager {
    /// Create a [`TaskManager`] and recover any persisted tasks from `store`.
    /// Tasks that were `Running` or `Pending` during an unclean shutdown are
    /// restored as `Paused` so they can resume cleanly from `.odj`/`.segments`.
    pub fn new(
        registry: FetcherRegistry,
        store: Arc<dyn TaskStore>,
        max_concurrent: usize,
    ) -> Result<Self, Error> {
        let persisted = store.load_all()?;
        let mut map = BTreeMap::new();
        let mut max_id = 0u64;
        for mut rec in persisted {
            max_id = max_id.max(rec.id);
            if rec.status.is_active() {
                rec.status = TaskStatus::Paused;
                rec.progress.speed_bps = 0;
                rec.progress.eta_secs = None;
            }
            map.insert(
                rec.id,
                ManagedTask {
                    record: rec,
                    options: DownloadOptions::default(),
                    cancel: None,
                },
            );
        }

        let (events, _) = broadcast::channel(512);
        Ok(Self {
            inner: Arc::new(Inner {
                registry,
                store,
                tasks: RwLock::new(map),
                next_id: AtomicU64::new(max_id + 1),
                slots: Arc::new(Semaphore::new(max_concurrent.max(1))),
                events,
            }),
        })
    }

    /// Subscribe to real-time [`TaskEvent`] updates.
    pub fn subscribe(&self) -> broadcast::Receiver<TaskEvent> {
        self.inner.events.subscribe()
    }

    /// Phase 1 (`Resolve`): inspect a URL without starting a download.
    pub async fn resolve(&self, req: &ResolveRequest) -> Result<(ResolveRequest, Resource), Error> {
        self.inner.registry.resolve(req).await
    }

    /// Phase 2 (`Create`): register a task, persist it, and optionally start it.
    pub async fn create(&self, mut req: CreateTaskRequest) -> Result<TaskRecord, Error> {
        let (effective_req, resource) = match req.resource.take() {
            Some(r) => (req.request, r),
            None => self.inner.registry.resolve(&req.request).await?,
        };

        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let now = now_ms();
        let status = if req.auto_start {
            TaskStatus::Pending
        } else {
            TaskStatus::Paused
        };

        let record = TaskRecord {
            id,
            request: effective_req,
            progress: TaskProgress {
                downloaded: 0,
                total: resource.total_size,
                speed_bps: 0,
                eta_secs: None,
            },
            resource,
            dest: req.dest,
            explicit_path: req.explicit_path,
            output_path: None,
            status,
            priority: req.priority,
            created_at_ms: now,
            updated_at_ms: now,
        };

        {
            let mut tasks = self.inner.tasks.write().await;
            tasks.insert(
                id,
                ManagedTask {
                    record: record.clone(),
                    options: req.options,
                    cancel: None,
                },
            );
        }
        self.persist().await?;
        let _ = self.inner.events.send(TaskEvent::Created {
            task: Box::new(record.clone()),
        });

        if req.auto_start {
            self.spawn_task(id).await?;
        }

        Ok(record)
    }

    /// Return a snapshot of all tasks ordered by ID.
    pub async fn list(&self) -> Vec<TaskRecord> {
        let tasks = self.inner.tasks.read().await;
        tasks.values().map(|t| t.record.clone()).collect()
    }

    /// Get a single task snapshot by ID.
    pub async fn get(&self, id: TaskId) -> Option<TaskRecord> {
        let tasks = self.inner.tasks.read().await;
        tasks.get(&id).map(|t| t.record.clone())
    }

    /// Pause a pending or running task; partial data & `.odj` journal are preserved.
    pub async fn pause(&self, id: TaskId) -> Result<(), Error> {
        let cancel = {
            let mut tasks = self.inner.tasks.write().await;
            let entry = tasks.get_mut(&id).ok_or(Error::TaskNotFound(id))?;
            if !entry.record.status.is_active() {
                return Err(Error::InvalidState(format!(
                    "cannot pause task {id} in state {:?}",
                    entry.record.status
                )));
            }
            entry.record.status = TaskStatus::Paused;
            entry.record.progress.speed_bps = 0;
            entry.record.progress.eta_secs = None;
            entry.record.updated_at_ms = now_ms();
            entry.cancel.take()
        };

        if let Some(token) = cancel {
            token.cancel();
        }
        self.persist().await?;
        let _ = self.inner.events.send(TaskEvent::StatusChanged {
            id,
            status: TaskStatus::Paused,
            output_path: None,
        });
        Ok(())
    }

    /// Resume a paused or failed task.
    pub async fn resume(&self, id: TaskId) -> Result<(), Error> {
        {
            let mut tasks = self.inner.tasks.write().await;
            let entry = tasks.get_mut(&id).ok_or(Error::TaskNotFound(id))?;
            match entry.record.status {
                TaskStatus::Paused | TaskStatus::Failed(_) => {
                    entry.record.status = TaskStatus::Pending;
                    entry.record.updated_at_ms = now_ms();
                }
                _ => {
                    return Err(Error::InvalidState(format!(
                        "cannot resume task {id} in state {:?}",
                        entry.record.status
                    )));
                }
            }
        }
        self.persist().await?;
        let _ = self.inner.events.send(TaskEvent::StatusChanged {
            id,
            status: TaskStatus::Pending,
            output_path: None,
        });
        self.spawn_task(id).await
    }

    /// Remove a task from the manager, cancelling it if active. When
    /// `delete_files` is `true`, removes the output file and sidecars (`.part`, `.odj`).
    pub async fn remove(&self, id: TaskId, delete_files: bool) -> Result<(), Error> {
        let removed = {
            let mut tasks = self.inner.tasks.write().await;
            tasks.remove(&id).ok_or(Error::TaskNotFound(id))?
        };
        if let Some(token) = removed.cancel {
            token.cancel();
        }
        if delete_files {
            let target = removed.record.output_path.unwrap_or_else(|| {
                if removed.record.explicit_path {
                    removed.record.dest.clone()
                } else {
                    removed.record.dest.join(&removed.record.resource.name)
                }
            });
            let (part, journal) = crate::engine::sidecar_paths(&target);
            let segs = crate::stream::segments_dir(&target);
            let _ = tokio::fs::remove_file(&target).await;
            let _ = tokio::fs::remove_file(&part).await;
            let _ = tokio::fs::remove_file(&journal).await;
            let _ = tokio::fs::remove_dir_all(&segs).await;
        }
        self.persist().await?;
        let _ = self.inner.events.send(TaskEvent::Removed { id });
        Ok(())
    }

    async fn spawn_task(&self, id: TaskId) -> Result<(), Error> {
        let token = CancellationToken::new();
        let (request, resource, dest, explicit_path, mut options) = {
            let mut tasks = self.inner.tasks.write().await;
            let entry = tasks.get_mut(&id).ok_or(Error::TaskNotFound(id))?;
            entry.cancel = Some(token.clone());
            (
                entry.record.request.clone(),
                entry.record.resource.clone(),
                entry.record.dest.clone(),
                entry.record.explicit_path,
                entry.options.clone(),
            )
        };
        options.cancel = Some(token.clone());

        let manager = self.clone();
        tokio::spawn(async move {
            // Wait for a concurrency slot, honouring pause/cancel while queued.
            let _permit = tokio::select! {
                _ = token.cancelled() => return,
                permit = manager.inner.slots.acquire() => match permit {
                    Ok(p) => p,
                    Err(_) => return,
                },
            };

            // Transition Pending → Running.
            {
                let mut tasks = manager.inner.tasks.write().await;
                let Some(entry) = tasks.get_mut(&id) else {
                    return;
                };
                if entry.record.status != TaskStatus::Pending {
                    return;
                }
                entry.record.status = TaskStatus::Running;
                entry.record.updated_at_ms = now_ms();
            }
            let _ = manager.persist().await;
            let _ = manager.inner.events.send(TaskEvent::StatusChanged {
                id,
                status: TaskStatus::Running,
                output_path: None,
            });

            let (tx, mut rx) = mpsc::unbounded_channel::<Event>();
            let progress_mgr = manager.clone();
            let progress_task = tokio::spawn(async move {
                let mut last_sample_time = Instant::now();
                let mut last_sample_bytes = 0u64;
                let mut ema_speed = 0.0f64;

                while let Some(ev) = rx.recv().await {
                    if let Event::Progress {
                        downloaded, total, ..
                    } = ev
                    {
                        let elapsed = last_sample_time.elapsed();
                        if elapsed >= Duration::from_millis(150) {
                            let delta = downloaded.saturating_sub(last_sample_bytes);
                            let instant_bps = delta as f64 / elapsed.as_secs_f64();
                            ema_speed = if ema_speed == 0.0 {
                                instant_bps
                            } else {
                                0.35 * instant_bps + 0.65 * ema_speed
                            };
                            last_sample_time = Instant::now();
                            last_sample_bytes = downloaded;
                        }
                        let speed_bps = ema_speed.round() as u64;
                        let eta_secs = total.and_then(|t| {
                            if speed_bps > 0 && t > downloaded {
                                Some((t - downloaded) / speed_bps)
                            } else {
                                None
                            }
                        });
                        let progress = TaskProgress {
                            downloaded,
                            total,
                            speed_bps,
                            eta_secs,
                        };
                        {
                            let mut tasks = progress_mgr.inner.tasks.write().await;
                            if let Some(entry) = tasks.get_mut(&id) {
                                entry.record.progress = progress.clone();
                            }
                        }
                        let _ = progress_mgr
                            .inner
                            .events
                            .send(TaskEvent::Progress { id, progress });
                    }
                }
            });

            let ctx = FetchContext {
                id,
                request,
                resource,
                dest,
                explicit_path,
                options,
                events: tx,
            };
            let result = manager.inner.registry.download(ctx).await;
            let _ = progress_task.await;

            manager.finish_task(id, result).await;
        });

        Ok(())
    }

    async fn finish_task(&self, id: TaskId, result: Result<DownloadOutcome, Error>) {
        let status_update = {
            let mut tasks = self.inner.tasks.write().await;
            let Some(entry) = tasks.get_mut(&id) else {
                return;
            };
            entry.cancel = None;
            entry.record.progress.speed_bps = 0;
            entry.record.progress.eta_secs = None;
            entry.record.updated_at_ms = now_ms();

            match result {
                Ok(outcome) => {
                    if let Some(total) = entry.record.progress.total {
                        entry.record.progress.downloaded = total;
                    }
                    entry.record.output_path = Some(outcome.path.clone());
                    entry.record.status = TaskStatus::Completed;
                    Some((TaskStatus::Completed, Some(outcome.path)))
                }
                Err(Error::Cancelled) => {
                    entry.record.status = TaskStatus::Paused;
                    Some((TaskStatus::Paused, None))
                }
                Err(e) => {
                    let msg = e.to_string();
                    entry.record.status = TaskStatus::Failed(msg.clone());
                    Some((TaskStatus::Failed(msg), None))
                }
            }
        };

        let _ = self.persist().await;
        if let Some((status, output_path)) = status_update {
            let _ = self.inner.events.send(TaskEvent::StatusChanged {
                id,
                status,
                output_path,
            });
        }
    }

    async fn persist(&self) -> Result<(), Error> {
        let snapshot: Vec<TaskRecord> = {
            let tasks = self.inner.tasks.read().await;
            tasks.values().map(|t| t.record.clone()).collect()
        };
        let store = Arc::clone(&self.inner.store);
        tokio::task::spawn_blocking(move || store.save_all(&snapshot))
            .await
            .map_err(|e| Error::Worker(e.to_string()))?
    }
}

/// Export a slice of [`TaskRecord`]s as RFC 4180 CSV for spreadsheets or audits.
pub fn export_tasks_csv(tasks: &[TaskRecord]) -> String {
    let mut out = String::from("id,name,url,status,priority,downloaded,total,output_path\n");
    for t in tasks {
        let st = match &t.status {
            TaskStatus::Pending => "pending",
            TaskStatus::Running => "running",
            TaskStatus::Paused => "paused",
            TaskStatus::Completed => "completed",
            TaskStatus::Failed(_) => "failed",
        };
        let prio = match t.priority {
            TaskPriority::Low => "low",
            TaskPriority::Normal => "normal",
            TaskPriority::High => "high",
        };
        let total = t
            .progress
            .total
            .map(|n| n.to_string())
            .unwrap_or_default();
        let path = t
            .output_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        out.push_str(&format!(
            "{},{},{},{},{},{},{},{}\n",
            t.id,
            csv_cell(&t.resource.name),
            csv_cell(&t.request.url),
            st,
            prio,
            t.progress.downloaded,
            total,
            csv_cell(&path),
        ));
    }
    out
}

fn csv_cell(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
