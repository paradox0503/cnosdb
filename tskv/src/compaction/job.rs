use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{atomic, Arc};
use std::time::{Duration, Instant};

use metrics::metric_register::MetricsRegister;
use snafu::ResultExt;
use tokio::runtime::Runtime;
use tokio::sync::mpsc::Receiver;
use tokio::sync::oneshot::Receiver as OneshotReceiver;
use tokio::sync::{oneshot, Mutex, Notify, RwLock, RwLockWriteGuard, Semaphore};
use trace::{error, info, warn};

use crate::compaction::metrics::{CompactionType, VnodeCompactionMetrics};
use crate::compaction::{flush, pick_compaction, CompactTask, FlushReq};
use crate::error::{CommonSnafu, IndexErrSnafu};
use crate::mem_cache::memcache::MemCache;
use crate::summary::SummaryTask;
use crate::{TsKvContext, TskvResult, VersionEdit, VnodeId};

const COMPACT_BATCH_CHECKING_SECONDS: u64 = 1;

struct CompactProcessor {
    compact_tasks: Vec<CompactTask>,
    vnode_compaction_limit: HashMap<VnodeId, Arc<Mutex<()>>>,
}

impl CompactProcessor {
    fn insert(&mut self, task: CompactTask) {
        let vnode_id = task.vnode_id();
        if !self.compact_tasks.contains(&task) {
            self.compact_tasks.push(task);
            self.vnode_compaction_limit
                .entry(vnode_id)
                .or_insert_with(|| Arc::new(Mutex::new(())));
        }
    }

    fn take(&mut self) -> TskvResult<Vec<(CompactTask, Arc<Mutex<()>>)>> {
        let compact_tasks: Vec<CompactTask> =
            std::mem::replace(&mut self.compact_tasks, Vec::with_capacity(32));

        compact_tasks
            .into_iter()
            .map(|task| {
                let vnode_id = task.vnode_id();
                Ok((
                    task,
                    self.vnode_compaction_limit
                        .get(&vnode_id)
                        .ok_or_else(|| {
                            CommonSnafu {
                                reason: format!(
                                    "vnode_id {} not found in vnode_compaction_limit",
                                    vnode_id
                                ),
                            }
                            .build()
                        })?
                        .clone(),
                ))
            })
            .collect::<TskvResult<Vec<(CompactTask, Arc<Mutex<()>>)>>>()
    }
}

impl Default for CompactProcessor {
    fn default() -> Self {
        Self {
            compact_tasks: Vec::with_capacity(32),
            vnode_compaction_limit: HashMap::with_capacity(32),
        }
    }
}

pub struct CompactJob {
    inner: Arc<RwLock<CompactJobInner>>,
}

impl CompactJob {
    pub fn new(
        runtime: Arc<Runtime>,
        ctx: Arc<TsKvContext>,
        metrics_register: Arc<MetricsRegister>,
    ) -> Self {
        Self {
            inner: Arc::new(RwLock::new(CompactJobInner::new(
                runtime,
                ctx,
                metrics_register,
            ))),
        }
    }

    pub async fn start_merge_compact_task_job(&self, compact_task_receiver: Receiver<CompactTask>) {
        self.inner
            .read()
            .await
            .start_merge_compact_task_job(compact_task_receiver);
    }

    pub async fn start_vnode_compaction_job(&self) {
        self.inner.read().await.start_vnode_compaction_job();
    }
}

impl std::fmt::Debug for CompactJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompactJob").finish()
    }
}

struct CompactJobInner {
    ctx: Arc<TsKvContext>,
    runtime: Arc<Runtime>,
    metrics_register: Arc<MetricsRegister>,
    compact_processor: Arc<RwLock<CompactProcessor>>,
    enable_compaction: Arc<AtomicBool>,
    running_compactions: Arc<AtomicUsize>,
}

impl CompactJobInner {
    fn new(
        runtime: Arc<Runtime>,
        ctx: Arc<TsKvContext>,
        metrics_register: Arc<MetricsRegister>,
    ) -> Self {
        let compact_processor = Arc::new(RwLock::new(CompactProcessor::default()));

        Self {
            ctx,
            runtime,
            metrics_register,
            compact_processor,
            enable_compaction: Arc::new(AtomicBool::new(false)),
            running_compactions: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn start_merge_compact_task_job(&self, mut compact_task_receiver: Receiver<CompactTask>) {
        info!("Compaction: start merge compact task job");
        let compact_processor = self.compact_processor.clone();
        self.runtime.spawn(async move {
            while let Some(compact_task) = compact_task_receiver.recv().await {
                compact_processor.write().await.insert(compact_task);
            }
        });
    }

    fn start_vnode_compaction_job(&self) {
        info!("Compaction: start vnode compaction job");
        if self
            .enable_compaction
            .compare_exchange(
                false,
                true,
                atomic::Ordering::Acquire,
                atomic::Ordering::Relaxed,
            )
            .is_err()
        {
            info!("Compaction: failed to change enable_compaction from false to true, compaction is already started");
            return;
        }

        let runtime_inner = self.runtime.clone();
        let compact_processor = self.compact_processor.clone();
        let enable_compaction = self.enable_compaction.clone();
        let running_compaction = self.running_compactions.clone();
        let ctx = self.ctx.clone();
        let metrics_registry = self.metrics_register.clone();

        self.runtime.spawn(async move {
            // TODO: Concurrent compactions should not over argument $cpu.
            let compaction_limit = Arc::new(Semaphore::new(
                ctx.options.storage.max_concurrent_compaction as usize,
            ));
            let mut check_interval =
                tokio::time::interval(Duration::from_secs(COMPACT_BATCH_CHECKING_SECONDS));

            loop {
                check_interval.tick().await;
                if !enable_compaction.load(atomic::Ordering::SeqCst) {
                    break;
                }
                if compact_processor.read().await.compact_tasks.is_empty() {
                    continue;
                }
                let vnode_ids = match compact_processor.write().await.take() {
                    Ok(vnode_ids) => vnode_ids,
                    Err(e) => {
                        error!("Failed to take vnode_ids from compact_processor: {:?}", e);
                        break;
                    }
                };
                let now = Instant::now();
                for (task, limit) in vnode_ids {
                    let vnode_id = task.vnode_id();
                    let ts_family = ctx
                        .version_set
                        .read()
                        .await
                        .get_tsfamily_by_tf_id(vnode_id)
                        .await;
                    if let Some(tsf) = ts_family {
                        info!("Starting compaction on ts_family {}", vnode_id);
                        if !tsf.read().await.can_compaction() {
                            info!("forbidden compaction on moving vnode {}", vnode_id);
                            return;
                        }
                        let version = tsf.read().await.version();
                        let compact_req = pick_compaction(task, version).await;
                        if let Some(req) = compact_req {
                            // Method acquire_owned() will return AcquireError if the semaphore has been closed.
                            let permit = compaction_limit.clone().acquire_owned().await.unwrap();
                            let enable_compaction = enable_compaction.clone();
                            let running_compaction = running_compaction.clone();

                            let ctx = ctx.clone();
                            let metrics_registry = metrics_registry.clone();
                            runtime_inner.spawn(async move {
                                let _guard = limit.lock().await;
                                // Check enable compaction
                                if !enable_compaction.load(atomic::Ordering::SeqCst) {
                                    return;
                                }
                                // Edit running_compaction
                                running_compaction.fetch_add(1, atomic::Ordering::SeqCst);
                                let _sub_running_compaction_guard = DeferGuard(Some(|| {
                                    running_compaction.fetch_sub(1, atomic::Ordering::SeqCst);
                                }));

                                let vnode_compaction_metrics = VnodeCompactionMetrics::new(
                                    &metrics_registry,
                                    ctx.options.storage.node_id,
                                    vnode_id,
                                    CompactionType::Normal,
                                    ctx.options.storage.collect_compaction_metrics,
                                );
                                match super::run_compaction_job(
                                    req,
                                    ctx.global_ctx.clone(),
                                    vnode_compaction_metrics,
                                )
                                .await
                                {
                                    Ok(Some((version_edit, file_metas))) => {
                                        let (summary_tx, _summary_rx) = oneshot::channel();
                                        let _ = ctx
                                            .summary_task_sender
                                            .send(SummaryTask::new(
                                                tsf.clone(),
                                                version_edit,
                                                Some(file_metas),
                                                None,
                                                summary_tx,
                                            ))
                                            .await;

                                        // TODO Handle summary result using summary_rx.
                                    }
                                    Ok(None) => {
                                        info!("There is nothing to compact.");
                                    }
                                    Err(e) => {
                                        error!("Compaction job failed: {:?}", e);
                                    }
                                }
                                drop(permit);
                            });
                        } else {
                            info!("There is no need to compact.");
                        }
                    }
                }
                info!(
                    "Compacting on vnode(job start): costs {} sec",
                    now.elapsed().as_secs()
                );
            }
        });
    }
}

pub struct StartVnodeCompactionGuard<'a> {
    inner: RwLockWriteGuard<'a, CompactJobInner>,
}

impl<'a> Drop for StartVnodeCompactionGuard<'a> {
    fn drop(&mut self) {
        info!("StopCompactionGuard(drop): start vnode compaction job");
        self.inner.start_vnode_compaction_job();
    }
}

pub struct DeferGuard<F: FnOnce()>(Option<F>);

impl<F: FnOnce()> Drop for DeferGuard<F> {
    fn drop(&mut self) {
        if let Some(f) = self.0.take() {
            f()
        }
    }
}

pub struct FlushJob {
    ctx: Arc<TsKvContext>,

    notify: Arc<Notify>,
    queue: Arc<RwLock<BTreeMap<u64, (FlushReq, SummaryTask)>>>,
}

impl FlushJob {
    pub fn new(ctx: Arc<TsKvContext>) -> Arc<Self> {
        let job = Self {
            ctx,
            notify: Arc::new(Notify::new()),
            queue: Arc::new(RwLock::new(BTreeMap::new())),
        };

        let job = Arc::new(job);
        job.ctx.runtime.spawn(Self::write_summary_job(job.clone()));

        job
    }

    async fn write_summary_job(job: Arc<FlushJob>) {
        loop {
            job.notify.notified().await;
            let mut queue_w = job.queue.write().await;
            while let Some((key, (flush_req, summary_task))) = queue_w.pop_first() {
                if !flush_req.completion {
                    queue_w.insert(key, (flush_req, summary_task));
                    break;
                }

                info!("Flush: completion request {} at {}", flush_req, key);
                if let Err(e) = job.ctx.summary_task_sender.send(summary_task).await {
                    warn!(
                        "Flush: failed to send summary task for tsf_id: {}: {e}",
                        flush_req.tf_id
                    );
                }

                if flush_req.trigger_compact {
                    let _ = job
                        .ctx
                        .compact_task_sender
                        .send(CompactTask::Delta(flush_req.tf_id))
                        .await;
                }
            }
        }
    }

    pub async fn run_block(job: Arc<FlushJob>, request: FlushReq) -> TskvResult<()> {
        let result = Self::run(job, &request).await;
        info!("Flush: block flush  {} result: {:?}", request, result);

        result
    }

    pub fn run_spawn(job: Arc<FlushJob>, request: FlushReq) -> TskvResult<()> {
        let runtime = job.ctx.runtime.clone();
        runtime.spawn(async move {
            let result = Self::run(job, &request).await;
            info!("Flush: spawn flush  {} result: {:?}", request, result);
        });

        Ok(())
    }

    async fn run(job: Arc<FlushJob>, request: &FlushReq) -> TskvResult<()> {
        info!("Flush: begin flush data {}", request);

        let instant = std::time::Instant::now();

        // flush index
        request
            .ts_index
            .write()
            .await
            .flush()
            .await
            .context(IndexErrSnafu)?;
        request.flush_metrics.write().await.flush_index_time = instant.elapsed().as_millis() as u64;

        let ts_family_w = request.ts_family.write().await;
        let mut mems = ts_family_w.im_cache().clone();
        mems.retain(|x| x.read().mark_flushing());
        let mut receivers = vec![];
        if mems.is_empty() {
            info!("Flush: flush data {} memcache is empty", request);
            return Ok(());
        }

        for mem in mems.iter() {
            let flush_seq = mem.read().min_seq_no();
            let (task_state_sender, task_state_receiver) = oneshot::channel();
            receivers.push(task_state_receiver);
            let summary_task = SummaryTask::new(
                request.ts_family.clone(),
                VersionEdit::default(),
                None,
                Some(vec![mem.clone()]),
                task_state_sender,
            );

            job.queue
                .write()
                .await
                .insert(flush_seq, (request.clone(), summary_task));
        }
        drop(ts_family_w);

        if let Err(err) = Self::flush_memtables(job, request, mems.clone(), receivers).await {
            for item in mems.iter_mut() {
                item.read().erase_flushing();
            }

            return Err(err);
        }

        let _ = request
            .ts_index
            .write()
            .await
            .clear_tombstone_series()
            .await;

        request.flush_metrics.write().await.flush_use_time = instant.elapsed().as_millis() as u64;

        let flush_metrics = request.flush_metrics.read().await;
        request
            .ts_family
            .read()
            .await
            .report_flush_metrics(&flush_metrics);

        Ok(())
    }

    async fn flush_memtables(
        job: Arc<FlushJob>,
        request: &FlushReq,
        mems: Vec<Arc<parking_lot::RwLock<MemCache>>>,
        receivers: Vec<OneshotReceiver<Result<(), crate::TskvError>>>,
    ) -> TskvResult<()> {
        for mem in mems {
            let flush_seq = mem.read().min_seq_no();
            match flush::flush_memtable(request, mem).await {
                Ok((ve, files_meta)) => {
                    if let Some(entry) = job.queue.write().await.get_mut(&flush_seq) {
                        entry.0.completion = true;
                        entry.1.request.version_edit = ve;
                        entry.1.request.file_metas = Some(files_meta);
                        job.notify.notify_one();
                    }
                }

                Err(err) => {
                    error!(
                        "Flush: memcache failed for tsf_id: {}, because : {:?}",
                        request.tf_id, err
                    );
                    return Err(err);
                }
            };
        }

        for task_state_receiver in receivers {
            match task_state_receiver.await {
                Ok(result) => {
                    if let Err(err) = result {
                        error!(
                            "Flush: failed to apply summary task  for tsf_id: {}, because : {:?}",
                            request.tf_id, err
                        );

                        return Err(err);
                    }
                }

                Err(err) => {
                    error!(
                    "Flush: failed to receive summary task result for tsf_id: {}, because : {:?}",
                    request.tf_id, err
                );
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod test {
    use std::sync::atomic::{self, AtomicI32};
    use std::sync::Arc;

    use super::DeferGuard;
    use crate::compaction::job::CompactProcessor;
    use crate::compaction::CompactTask;
    use crate::VnodeId;

    #[test]
    fn test_build_compact_batch() {
        let mut compact_batch_builder = CompactProcessor::default();
        compact_batch_builder.insert(CompactTask::Normal(1));
        compact_batch_builder.insert(CompactTask::Normal(2));
        compact_batch_builder.insert(CompactTask::Normal(1));
        compact_batch_builder.insert(CompactTask::Normal(3));
        assert_eq!(compact_batch_builder.compact_tasks.len(), 3);

        let mut keys: Vec<VnodeId> = compact_batch_builder
            .compact_tasks
            .iter()
            .map(|task| task.vnode_id())
            .collect();
        keys.sort();
        assert_eq!(keys, vec![1, 2, 3]);

        let vnode_ids = compact_batch_builder
            .take()
            .unwrap()
            .into_iter()
            .map(|(task, _)| task.vnode_id())
            .collect::<Vec<VnodeId>>();
        assert_eq!(vnode_ids, vec![1, 2, 3]);
    }

    #[test]
    fn test_defer_guard() {
        let a = Arc::new(AtomicI32::new(0));
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(2)
            .build()
            .unwrap();
        {
            let a = a.clone();
            let jh = runtime.spawn(async move {
                a.fetch_add(1, atomic::Ordering::SeqCst);
                let _guard = DeferGuard(Some(|| {
                    a.fetch_sub(1, atomic::Ordering::SeqCst);
                }));
                a.fetch_add(1, atomic::Ordering::SeqCst);
                a.fetch_add(1, atomic::Ordering::SeqCst);

                assert_eq!(a.load(atomic::Ordering::SeqCst), 3);
            });
            let _ = runtime.block_on(jh);
        }
        assert_eq!(a.load(atomic::Ordering::SeqCst), 2);
    }
}
