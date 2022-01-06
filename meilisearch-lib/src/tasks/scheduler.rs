use std::{
    cmp::Ordering,
    collections::{hash_map::Entry, BinaryHeap, HashMap, VecDeque},
    ops::{Deref, DerefMut},
    sync::Arc,
    time::Duration,
};

use atomic_refcell::AtomicRefCell;
use chrono::Utc;
use milli::update::IndexDocumentsMethod;
use tokio::sync::{watch, RwLock};

use crate::options::SchedulerConfig;

use super::{
    batch::Batch,
    error::Result,
    task::{Job, Task, TaskContent, TaskEvent, TaskId},
    update_loop::UpdateLoop,
    TaskFilter, TaskPerformer, TaskStore,
};

#[derive(Eq, Debug, Clone, Copy)]
enum TaskType {
    DocumentAddition { number: usize },
    DocumentsUpdate { number: usize },
    Other,
}

/// Two task types are equal if they hace the same type
impl PartialEq for TaskType {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::DocumentAddition { .. }, Self::DocumentAddition { .. })
            | (Self::DocumentsUpdate { .. }, Self::DocumentsUpdate { .. }) => true,
            _ => false,
        }
    }
}

#[derive(Eq, Debug, Clone, Copy)]
struct PendingTask {
    kind: TaskType,
    id: TaskId,
}

impl PartialEq for PendingTask {
    fn eq(&self, other: &Self) -> bool {
        self.id.eq(&other.id)
    }
}

impl PartialOrd for PendingTask {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.id.partial_cmp(&other.id).map(Ordering::reverse)
    }
}

impl Ord for PendingTask {
    fn cmp(&self, other: &Self) -> Ordering {
        self.partial_cmp(other).unwrap()
    }
}

#[derive(Debug)]
struct TaskList {
    index: String,
    tasks: BinaryHeap<PendingTask>,
}

impl Deref for TaskList {
    type Target = BinaryHeap<PendingTask>;

    fn deref(&self) -> &Self::Target {
        &self.tasks
    }
}

impl DerefMut for TaskList {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.tasks
    }
}

impl TaskList {
    fn new(index: String) -> Self {
        Self {
            index,
            tasks: Default::default(),
        }
    }
}

impl PartialEq for TaskList {
    fn eq(&self, other: &Self) -> bool {
        self.index == other.index
    }
}

impl Eq for TaskList {}
impl Ord for TaskList {
    fn cmp(&self, other: &Self) -> Ordering {
        self.partial_cmp(other).unwrap()
    }
}

impl PartialOrd for TaskList {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        match (self.peek(), other.peek()) {
            (None, None) => Some(Ordering::Equal),
            (None, Some(_)) => Some(Ordering::Less),
            (Some(_), None) => Some(Ordering::Greater),
            (Some(lhs), Some(rhs)) => Some(lhs.cmp(&rhs)),
        }
    }
}

#[derive(Default)]
struct TaskQueue {
    /// maps index uids to their TaskList, for quick access
    index_tasks: HashMap<String, Arc<AtomicRefCell<TaskList>>>,
    /// A queue that orders TaskList by the priority of their fist update
    queue: BinaryHeap<Arc<AtomicRefCell<TaskList>>>,
}

impl TaskQueue {
    fn insert(&mut self, task: Task) {
        let uid = task.index_uid.into_inner();
        let id = task.id;
        let kind = match task.content {
            TaskContent::DocumentAddition {
                documents_count,
                merge_strategy: IndexDocumentsMethod::ReplaceDocuments,
                ..
            } => TaskType::DocumentAddition {
                number: documents_count,
            },
            TaskContent::DocumentAddition {
                documents_count,
                merge_strategy: IndexDocumentsMethod::UpdateDocuments,
                ..
            } => TaskType::DocumentsUpdate {
                number: documents_count,
            },
            _ => TaskType::Other,
        };
        let task = PendingTask { kind, id };

        match self.index_tasks.entry(uid) {
            Entry::Occupied(entry) => {
                // task list already exists for this index, all we have to to is to push the new
                // update to the end of the list. This won't change the order since ids are
                // monotically increasing.
                let mut list = entry.get().borrow_mut();

                // in reality, we only need the first element to be lower than the one we want to
                // insert to preserve the order in the queue.
                assert!(list.peek().map(|old_id| id > old_id.id).unwrap_or(true));

                list.push(task);
            }
            Entry::Vacant(entry) => {
                let mut task_list = TaskList::new(entry.key().to_owned());
                task_list.push(task);
                let task_list = Arc::new(AtomicRefCell::new(task_list));
                entry.insert(task_list.clone());
                self.queue.push(task_list);
            }
        }
    }

    /// passes a context with a view to the task list of the next index to schedule. It is
    /// guaranteed that the first id from task list will be the lowest pending task id.
    fn head_mut<R>(&mut self, mut f: impl FnMut(&mut TaskList) -> R) -> Option<R> {
        let head = self.queue.pop()?;
        let result = {
            let mut ref_head = head.borrow_mut();
            f(&mut *ref_head)
        };
        if !head.borrow().tasks.is_empty() {
            // After being mutated, the head is reinserted to the correct position.
            self.queue.push(head);
        } else {
            self.index_tasks.remove(&head.borrow().index);
        }

        Some(result)
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty() && self.index_tasks.is_empty()
    }
}

pub struct Scheduler {
    jobs: VecDeque<Job>,
    tasks: TaskQueue,

    store: TaskStore,
    processing: Vec<TaskId>,
    next_fetched_task_id: TaskId,
    config: SchedulerConfig,
    /// notify the update loop that a new task was received
    notifier: watch::Sender<()>,
}

impl Scheduler {
    pub fn new<P>(
        store: TaskStore,
        performer: Arc<P>,
        config: SchedulerConfig,
    ) -> Result<Arc<RwLock<Self>>>
    where
        P: TaskPerformer,
    {
        let (notifier, rcv) = watch::channel(());

        let debounce_time = config.debounce_duration_sec;

        let this = Self {
            jobs: VecDeque::new(),
            tasks: TaskQueue::default(),

            store,
            processing: Vec::new(),
            next_fetched_task_id: 0,
            config,
            notifier,
        };

        let this = Arc::new(RwLock::new(this));

        let update_loop = UpdateLoop::new(
            this.clone(),
            performer,
            debounce_time.filter(|&v| v > 0).map(Duration::from_secs),
            rcv,
        );

        tokio::task::spawn_local(update_loop.run());

        Ok(this)
    }

    fn register_task(&mut self, task: Task) {
        assert!(!task.is_finished());
        self.tasks.insert(task);
    }

    /// Clears the processing list, this method should be called when the processing of a batch is
    /// finished.
    pub fn finish(&mut self) {
        self.processing.clear();
    }

    pub fn notify(&self) {
        let _ = self.notifier.send(());
    }

    fn notifiy_if_not_empty(&self) {
        if !!self.jobs.is_empty() || !self.tasks.is_empty() {
            self.notify();
        }
    }

    pub async fn update_tasks(&self, tasks: Vec<Task>) -> Result<Vec<Task>> {
        self.store.update_tasks(tasks).await
    }

    pub async fn get_task(&self, id: TaskId, filter: Option<TaskFilter>) -> Result<Task> {
        self.store.get_task(id, filter).await
    }

    pub async fn list_tasks(
        &self,
        offset: Option<TaskId>,
        filter: Option<TaskFilter>,
        limit: Option<usize>,
    ) -> Result<Vec<Task>> {
        self.store.list_tasks(offset, filter, limit).await
    }

    pub async fn get_processing_tasks(&self) -> Result<Vec<Task>> {
        let mut tasks = Vec::new();

        for id in self.processing.iter() {
            let task = self.store.get_task(*id, None).await?;
            tasks.push(task);
        }

        Ok(tasks)
    }

    pub async fn schedule_job(&mut self, job: Job) {
        self.jobs.push_back(job);
    }

    async fn fetch_pending_tasks(&mut self) -> Result<()> {
        // We must NEVER re-enqueue an already porocessed task! it's content uuid would point to an
        // an unextisting file.
        let mut filter = TaskFilter::default();
        filter.filter_fn(|task| !task.is_finished());

        self.store
            .list_tasks(Some(self.next_fetched_task_id), Some(filter), None)
            .await?
            .into_iter()
            // the tasks arrive in reverse order, and we need to insert them in order.
            .rev()
            .for_each(|t| {
                self.next_fetched_task_id = t.id + 1;
                self.register_task(t);
            });

        Ok(())
    }

    /// Prepares the next batch, and set `processing` to the ids in that batch.
    pub async fn prepare(&mut self) -> Result<Pending> {
        // If there is a job to process, do it first.
        if let Some(job) = self.jobs.pop_front() {
            // There is more work to do, notify the update loop
            self.notifiy_if_not_empty();
            return Ok(Pending::Job(job));
        }
        // try to fill the queue with pending tasks.
        self.fetch_pending_tasks().await?;

        self.processing.clear();
        make_batch(&mut self.tasks, &mut self.processing, &self.config);

        dbg!(&self.processing);
        log::debug!("prepared batch with {} tasks", self.processing.len());

        if !self.processing.is_empty() {
            let ids = std::mem::take(&mut self.processing);

            let (ids, mut tasks) = self.store.get_pending_tasks(ids).await?;

            // The batch id is the id of the first update it contains
            let id = match tasks.first() {
                Some(Task { id, .. }) => *id,
                _ => panic!("invalid batch"),
            };

            tasks.iter_mut().for_each(|t| {
                t.events.push(TaskEvent::Batched {
                    batch_id: id,
                    timestamp: Utc::now(),
                })
            });

            self.processing = ids;

            let batch = Batch {
                id,
                created_at: Utc::now(),
                tasks,
            };

            // There is more work to do, notify the update loop
            self.notifiy_if_not_empty();

            Ok(Pending::Batch(batch))
        } else {
            Ok(Pending::Nothing)
        }
    }
}

#[derive(Debug)]
pub enum Pending {
    Batch(Batch),
    Job(Job),
    Nothing,
}

fn make_batch(tasks: &mut TaskQueue, processing: &mut Vec<TaskId>, config: &SchedulerConfig) {
    // the processing list MUST be empty when it is handed to us.
    assert!(processing.is_empty());

    let mut doc_count = 0;
    tasks.head_mut(|list| match list.peek().copied() {
        Some(PendingTask {
            kind: TaskType::Other,
            id,
        }) => {
            processing.push(id);
            list.pop();
        }
        Some(PendingTask { kind, .. }) => loop {
            match list.peek() {
                Some(pending) if pending.kind == kind => {
                    if processing.len() >= config.max_batch_size.max(1) {
                        break;
                    }
                    let pending = list.pop().unwrap();
                    processing.push(pending.id);

                    // add the number of documents to count if we are scheduling document additions and
                    // stop adding if we already have enough. We check that bound only
                    // after adding the task to the batch, so a single update is always
                    // processed even if it has to any documents in it.
                    match pending.kind {
                        TaskType::DocumentsUpdate { number }
                        | TaskType::DocumentAddition { number } => {
                            doc_count += number;

                            if doc_count >= config.max_documents_per_batch.unwrap_or(usize::MAX) {
                                break;
                            }
                        }
                        _ => (),
                    }
                }
                _ => break,
            }
        },
        None => (),
    });
}

#[cfg(test)]
mod test {
    use milli::update::IndexDocumentsMethod;
    use uuid::Uuid;

    use crate::{index_resolver::IndexUid, tasks::task::TaskContent};

    use super::*;

    fn gen_task(id: TaskId, index_uid: &str, content: TaskContent) -> Task {
        Task {
            id,
            index_uid: IndexUid::new_unchecked(index_uid.to_owned()),
            content,
            events: vec![],
        }
    }

    #[test]
    fn register_updates_multiples_indexes() {
        let mut queue = TaskQueue::default();
        queue.insert(gen_task(0, "test1", TaskContent::IndexDeletion));
        queue.insert(gen_task(1, "test2", TaskContent::IndexDeletion));
        queue.insert(gen_task(2, "test2", TaskContent::IndexDeletion));
        queue.insert(gen_task(3, "test2", TaskContent::IndexDeletion));
        queue.insert(gen_task(4, "test1", TaskContent::IndexDeletion));
        queue.insert(gen_task(5, "test1", TaskContent::IndexDeletion));
        queue.insert(gen_task(6, "test2", TaskContent::IndexDeletion));

        let test1_tasks = queue
            .head_mut(|tasks| tasks.drain().map(|t| t.id).collect::<Vec<_>>())
            .unwrap();

        assert_eq!(test1_tasks, &[0, 4, 5]);

        let test2_tasks = queue
            .head_mut(|tasks| tasks.drain().map(|t| t.id).collect::<Vec<_>>())
            .unwrap();

        assert_eq!(test2_tasks, &[1, 2, 3, 6]);

        assert!(queue.index_tasks.is_empty());
        assert!(queue.queue.is_empty());
    }

    #[test]
    fn test_make_batch() {
        let mut queue = TaskQueue::default();
        let content = TaskContent::DocumentAddition {
            content_uuid: Uuid::new_v4(),
            merge_strategy: IndexDocumentsMethod::ReplaceDocuments,
            primary_key: Some("test".to_string()),
            documents_count: 0,
        };
        queue.insert(gen_task(0, "test1", content.clone()));
        queue.insert(gen_task(1, "test2", content.clone()));
        queue.insert(gen_task(2, "test2", TaskContent::IndexDeletion));
        queue.insert(gen_task(3, "test2", content.clone()));
        queue.insert(gen_task(4, "test1", content.clone()));
        queue.insert(gen_task(5, "test1", TaskContent::IndexDeletion));
        queue.insert(gen_task(6, "test2", content.clone()));
        queue.insert(gen_task(7, "test1", content.clone()));

        let mut batch = Vec::new();

        let config = SchedulerConfig::default();
        make_batch(&mut queue, &mut batch, &config);
        assert_eq!(batch, &[0, 4]);

        batch.clear();
        make_batch(&mut queue, &mut batch, &config);
        assert_eq!(batch, &[1]);

        batch.clear();
        make_batch(&mut queue, &mut batch, &config);
        assert_eq!(batch, &[2]);

        batch.clear();
        make_batch(&mut queue, &mut batch, &config);
        assert_eq!(batch, &[3, 6]);

        batch.clear();
        make_batch(&mut queue, &mut batch, &config);
        assert_eq!(batch, &[5]);

        batch.clear();
        make_batch(&mut queue, &mut batch, &config);
        assert_eq!(batch, &[7]);

        assert!(queue.is_empty());
    }
}
