use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use futures_lite::StreamExt;
use linyaps_api::TaskStateName;
use zbus::object_server::SignalEmitter;
use zvariant::{OwnedValue, Value};

pub const TASK_INTERFACE: &str = "org.deepin.linglong.Task1";
pub const TASK_PATH_PREFIX: &str = "/org/deepin/linglong/Task1";

pub type VariantMap = HashMap<String, OwnedValue>;
pub type TaskFuture = Pin<Box<dyn Future<Output = Result<TaskCompletion, String>> + Send>>;
pub type TaskJob = Box<dyn FnOnce(TaskContext) -> TaskFuture + Send>;
type QueueFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

#[derive(Clone, Default)]
pub struct TaskQueue {
    inner: Arc<Mutex<TaskQueueInner>>,
}

#[derive(Default)]
struct TaskQueueInner {
    pending: VecDeque<QueueFuture>,
    running: bool,
}

impl TaskQueue {
    pub(crate) fn enqueue(&self, emitter: &SignalEmitter<'_>, future: QueueFuture) {
        let start_worker = {
            let mut inner = self.inner.lock().unwrap();
            inner.pending.push_back(future);
            if inner.running {
                false
            } else {
                inner.running = true;
                true
            }
        };
        if !start_worker {
            return;
        }
        emitter
            .connection()
            .executor()
            .spawn(self.clone().run(), "linglong package task queue")
            .detach();
    }

    async fn run(self) {
        loop {
            let future = {
                let mut inner = self.inner.lock().unwrap();
                match inner.pending.pop_front() {
                    Some(future) => future,
                    None => {
                        inner.running = false;
                        return;
                    }
                }
            };
            future.await;
        }
    }
}

#[derive(Debug)]
pub struct TaskCompletion {
    pub message: String,
    pub result: VariantMap,
    terminal_state: TaskStateName,
}

impl TaskCompletion {
    pub fn new(message: impl Into<String>, result: VariantMap) -> Self {
        Self {
            message: message.into(),
            result,
            terminal_state: TaskStateName::Succeed,
        }
    }

    pub fn failed(code: i64, message: impl Into<String>) -> Self {
        let message = message.into();
        Self {
            result: common_result(code, &message),
            message,
            terminal_state: TaskStateName::Failed,
        }
    }

    pub fn canceled(message: impl Into<String>) -> Self {
        let message = message.into();
        Self {
            result: common_result(1, &message),
            message,
            terminal_state: TaskStateName::Canceled,
        }
    }
}

#[derive(Clone)]
pub struct TaskContext {
    inner: Arc<TaskInner>,
    emitter: SignalEmitter<'static>,
}

impl TaskContext {
    pub fn is_canceled(&self) -> bool {
        self.inner.canceled.load(Ordering::Acquire)
    }

    pub async fn update_progress(
        &self,
        progress: f64,
        message: impl Into<String>,
    ) -> zbus::Result<()> {
        if !(0.0..=100.0).contains(&progress) || self.is_canceled() {
            return Ok(());
        }
        let snapshot = {
            let mut state = self.inner.state.lock().unwrap();
            if progress <= state.progress || state.state.is_terminal() {
                return Ok(());
            }
            state.progress = progress;
            state.message = message.into();
            state.clone()
        };
        TaskService::task_event(&self.emitter, "state", &snapshot.as_variant_map()).await
    }

    pub async fn reset_progress(&self, message: impl Into<String>) -> zbus::Result<()> {
        if self.is_canceled() {
            return Ok(());
        }
        let snapshot = {
            let mut state = self.inner.state.lock().unwrap();
            if state.state.is_terminal() {
                return Ok(());
            }
            state.progress = 0.0;
            state.message = message.into();
            state.clone()
        };
        TaskService::task_event(&self.emitter, "state", &snapshot.as_variant_map()).await
    }

    pub async fn update_state_message(&self, message: impl Into<String>) -> zbus::Result<()> {
        if self.is_canceled() {
            return Ok(());
        }
        let snapshot = {
            let mut state = self.inner.state.lock().unwrap();
            if state.state.is_terminal() {
                return Ok(());
            }
            state.message = message.into();
            state.clone()
        };
        TaskService::task_event(&self.emitter, "state", &snapshot.as_variant_map()).await
    }

    pub async fn send_message(&self, message: impl Into<String>) -> zbus::Result<()> {
        let mut data = VariantMap::new();
        data.insert("message".to_string(), owned_string(message.into()));
        TaskService::task_event(&self.emitter, "message", &data).await
    }

    pub async fn request_interaction(
        &self,
        message_id: i32,
        additional_message: VariantMap,
    ) -> zbus::Result<bool> {
        if self.is_canceled() {
            return Ok(false);
        }
        let interaction_id = new_interaction_id();
        let (sender, receiver) = async_channel::bounded(1);
        {
            let mut interactions = self.inner.interactions.lock().unwrap();
            if !interactions.is_empty() {
                return Ok(false);
            }
            interactions.insert(interaction_id.clone(), sender);
        }
        if let Err(error) = TaskService::request_interaction(
            &self.emitter,
            &interaction_id,
            message_id,
            &additional_message,
        )
        .await
        {
            self.inner
                .interactions
                .lock()
                .unwrap()
                .remove(&interaction_id);
            return Err(error);
        }
        let accepted = futures_lite::future::race(async { receiver.recv().await.ok() }, async {
            async_io::Timer::after(std::time::Duration::from_secs(180)).await;
            None
        })
        .await
        .unwrap_or(false);
        self.inner
            .interactions
            .lock()
            .unwrap()
            .remove(&interaction_id);
        Ok(accepted)
    }
}

#[derive(Clone, Debug)]
struct TaskSnapshot {
    state: TaskStateName,
    progress: f64,
    message: String,
}

impl TaskSnapshot {
    fn as_variant_map(&self) -> VariantMap {
        let mut result = VariantMap::new();
        result.insert("message".to_string(), owned_string(&self.message));
        result.insert("progress".to_string(), self.progress.into());
        result.insert("state".to_string(), owned_string(self.state.as_str()));
        result
    }
}

trait TerminalState {
    fn is_terminal(&self) -> bool;
}

impl TerminalState for TaskStateName {
    fn is_terminal(&self) -> bool {
        matches!(self, Self::Canceled | Self::Failed | Self::Succeed)
    }
}

struct TaskInner {
    state: Mutex<TaskSnapshot>,
    job: Mutex<Option<TaskJob>>,
    interactions: Mutex<HashMap<String, async_channel::Sender<bool>>>,
    started: AtomicBool,
    running: AtomicBool,
    canceled: AtomicBool,
    finished: AtomicBool,
}

#[derive(Clone)]
pub struct TaskService {
    path: String,
    inner: Arc<TaskInner>,
    queue: TaskQueue,
    owner: Option<String>,
}

impl TaskService {
    pub fn new(initial_message: impl Into<String>, job: TaskJob) -> Self {
        Self::new_in_queue(initial_message, job, TaskQueue::default(), None)
    }

    pub fn new_pending(initial_message: impl Into<String>, job: TaskJob) -> Self {
        Self::new_pending_in_queue(initial_message, job, TaskQueue::default(), None)
    }

    pub fn new_in_queue(
        initial_message: impl Into<String>,
        job: TaskJob,
        queue: TaskQueue,
        owner: Option<String>,
    ) -> Self {
        Self::with_state(TaskStateName::Queued, initial_message, job, queue, owner)
    }

    pub fn new_pending_in_queue(
        initial_message: impl Into<String>,
        job: TaskJob,
        queue: TaskQueue,
        owner: Option<String>,
    ) -> Self {
        Self::with_state(TaskStateName::Pending, initial_message, job, queue, owner)
    }

    fn with_state(
        initial_state: TaskStateName,
        initial_message: impl Into<String>,
        job: TaskJob,
        queue: TaskQueue,
        owner: Option<String>,
    ) -> Self {
        Self {
            path: format!("{TASK_PATH_PREFIX}/{}", new_task_id()),
            inner: Arc::new(TaskInner {
                state: Mutex::new(TaskSnapshot {
                    state: initial_state,
                    progress: 0.0,
                    message: initial_message.into(),
                }),
                job: Mutex::new(Some(job)),
                interactions: Mutex::new(HashMap::new()),
                started: AtomicBool::new(false),
                running: AtomicBool::new(false),
                canceled: AtomicBool::new(false),
                finished: AtomicBool::new(false),
            }),
            queue,
            owner,
        }
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn enqueue_queued(&self, emitter: SignalEmitter<'static>) {
        if self.inner.state.lock().unwrap().state != TaskStateName::Queued {
            return;
        }
        self.enqueue(emitter);
    }

    pub async fn watch_owner(&self, emitter: SignalEmitter<'static>) -> zbus::Result<()> {
        let Some(owner) = self.owner.clone() else {
            return Ok(());
        };
        let connection = emitter.connection().clone();
        let proxy = zbus::fdo::DBusProxy::new(&connection).await?;
        let mut changes = proxy
            .receive_name_owner_changed_with_args(&[(0, owner.as_str()), (2, "")])
            .await?;
        let owner_name = owner
            .as_str()
            .try_into()
            .map_err(|error: zbus::names::Error| zbus::Error::Failure(error.to_string()))?;
        if !proxy.name_has_owner(owner_name).await? {
            self.caller_disconnected(&emitter).await;
            return Ok(());
        }
        let task = self.clone();
        connection
            .executor()
            .spawn(
                async move {
                    if changes.next().await.is_some() {
                        task.caller_disconnected(&emitter).await;
                    }
                },
                "linglong task owner watcher",
            )
            .detach();
        Ok(())
    }

    fn enqueue(&self, emitter: SignalEmitter<'static>) {
        if self.inner.started.swap(true, Ordering::AcqRel)
            || self.inner.finished.load(Ordering::Acquire)
        {
            return;
        }
        let inner = self.inner.clone();
        let path = self.path.clone();
        let queue_emitter = emitter.clone();
        self.queue.enqueue(
            &queue_emitter,
            Box::pin(async move {
                run_task(inner, emitter.clone()).await;
                let _ = emitter
                    .connection()
                    .object_server()
                    .remove::<TaskService, _>(path)
                    .await;
            }),
        );
    }

    fn authorize_caller(&self, header: &zbus::message::Header<'_>) -> zbus::fdo::Result<()> {
        let Some(owner) = &self.owner else {
            return Ok(());
        };
        if header.sender().map(ToString::to_string).as_deref() == Some(owner) {
            return Ok(());
        }
        Err(zbus::fdo::Error::AccessDenied(
            "not the task owner".to_string(),
        ))
    }

    async fn cancel_task(&self, emitter: &SignalEmitter<'_>) -> zbus::Result<()> {
        if self.inner.finished.load(Ordering::Acquire) {
            return Ok(());
        }
        self.inner.canceled.store(true, Ordering::Release);
        self.inner.interactions.lock().unwrap().clear();
        let message = format!(
            "task {} has been canceled by user",
            self.path.rsplit('/').next().unwrap()
        );
        let snapshot = set_state(&self.inner, TaskStateName::Canceled, Some(message.clone()));
        Self::task_event(emitter, "state", &snapshot.as_variant_map()).await?;
        if self.inner.running.load(Ordering::Acquire) {
            return Ok(());
        }
        if self.inner.finished.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        Self::task_finished(emitter, &common_result(1, message)).await
    }

    async fn caller_disconnected(&self, emitter: &SignalEmitter<'_>) {
        self.inner.interactions.lock().unwrap().clear();
        let should_cancel = {
            let state = self.inner.state.lock().unwrap();
            state.state == TaskStateName::Pending
        };
        if !should_cancel || self.inner.finished.swap(true, Ordering::AcqRel) {
            return;
        }
        self.inner.canceled.store(true, Ordering::Release);
        let message = "caller disconnected".to_string();
        let snapshot = set_state(&self.inner, TaskStateName::Canceled, Some(message.clone()));
        let _ = Self::task_event(emitter, "state", &snapshot.as_variant_map()).await;
        let _ = Self::task_finished(emitter, &common_result(1, message)).await;
        let _ = emitter
            .connection()
            .object_server()
            .remove::<TaskService, _>(self.path.as_str())
            .await;
    }
}

#[zbus::interface(name = "org.deepin.linglong.Task1")]
impl TaskService {
    #[zbus(name = "Start")]
    async fn start(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<()> {
        self.authorize_caller(&header)?;
        let snapshot = {
            let mut state = self.inner.state.lock().unwrap();
            match state.state {
                TaskStateName::Pending => {
                    state.state = TaskStateName::Queued;
                    Some(state.clone())
                }
                TaskStateName::Queued => None,
                _ => return Ok(()),
            }
        };
        if let Some(snapshot) = snapshot {
            Self::task_event(&emitter, "state", &snapshot.as_variant_map())
                .await
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        }
        self.enqueue(emitter.to_owned());
        Ok(())
    }

    #[zbus(name = "Cancel")]
    async fn cancel(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<()> {
        self.authorize_caller(&header)?;
        self.cancel_task(&emitter)
            .await
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
    }

    #[zbus(name = "ReplyInteraction")]
    fn reply_interaction(
        &self,
        interaction_id: &str,
        replies: VariantMap,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<()> {
        self.authorize_caller(&header)?;
        let action = replies
            .get("action")
            .and_then(|value| <&str>::try_from(value).ok());
        if !matches!(action, Some("yes" | "no")) {
            return Err(zbus::fdo::Error::InvalidArgs(
                "invalid interaction reply".to_string(),
            ));
        }
        let sender = self
            .inner
            .interactions
            .lock()
            .unwrap()
            .remove(interaction_id)
            .ok_or_else(|| {
                zbus::fdo::Error::InvalidArgs("interaction is not active".to_string())
            })?;
        sender
            .try_send(action == Some("yes"))
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
    }

    #[zbus(signal, name = "TaskEvent")]
    pub async fn task_event(
        emitter: &SignalEmitter<'_>,
        event: &str,
        data: &VariantMap,
    ) -> zbus::Result<()>;

    #[zbus(signal, name = "TaskFinished")]
    pub async fn task_finished(
        emitter: &SignalEmitter<'_>,
        result: &VariantMap,
    ) -> zbus::Result<()>;

    #[zbus(signal, name = "RequestInteraction")]
    pub async fn request_interaction(
        emitter: &SignalEmitter<'_>,
        interaction_id: &str,
        message_id: i32,
        additional_message: &VariantMap,
    ) -> zbus::Result<()>;
}

async fn run_task(inner: Arc<TaskInner>, emitter: SignalEmitter<'static>) {
    if inner.finished.load(Ordering::Acquire) {
        return;
    }
    inner.running.store(true, Ordering::Release);
    if inner.finished.load(Ordering::Acquire) {
        inner.running.store(false, Ordering::Release);
        return;
    }
    let Some(job) = inner.job.lock().unwrap().take() else {
        inner.running.store(false, Ordering::Release);
        return;
    };
    let snapshot = set_state(&inner, TaskStateName::Processing, None);
    let _ = TaskService::task_event(&emitter, "state", &snapshot.as_variant_map()).await;

    let context = TaskContext {
        inner: inner.clone(),
        emitter: emitter.clone(),
    };
    let completion = job(context).await;
    inner.running.store(false, Ordering::Release);
    if inner.finished.swap(true, Ordering::AcqRel) {
        return;
    }
    if inner.canceled.load(Ordering::Acquire) {
        let message = inner.state.lock().unwrap().message.clone();
        let _ = TaskService::task_finished(&emitter, &common_result(1, message)).await;
        return;
    }

    match completion {
        Ok(completion) => {
            let snapshot = set_state(&inner, completion.terminal_state, Some(completion.message));
            let _ = TaskService::task_event(&emitter, "state", &snapshot.as_variant_map()).await;
            let _ = TaskService::task_finished(&emitter, &completion.result).await;
        }
        Err(message) => {
            let snapshot = set_state(&inner, TaskStateName::Failed, Some(message.clone()));
            let _ = TaskService::task_event(&emitter, "state", &snapshot.as_variant_map()).await;
            let result = common_result(-1, message);
            let _ = TaskService::task_finished(&emitter, &result).await;
        }
    }
}

#[zbus::proxy(interface = "org.deepin.linglong.Task1")]
pub trait Task1 {
    #[zbus(name = "Start")]
    fn start(&self) -> zbus::Result<()>;

    #[zbus(name = "Cancel")]
    fn cancel(&self) -> zbus::Result<()>;

    #[zbus(name = "ReplyInteraction")]
    fn reply_interaction(&self, interaction_id: &str, replies: VariantMap) -> zbus::Result<()>;

    #[zbus(signal, name = "TaskEvent")]
    fn task_event(&self, event: &str, data: VariantMap) -> zbus::Result<()>;

    #[zbus(signal, name = "TaskFinished")]
    fn task_finished(&self, result: VariantMap) -> zbus::Result<()>;

    #[zbus(signal, name = "RequestInteraction")]
    fn request_interaction(
        &self,
        interaction_id: &str,
        message_id: i32,
        additional_message: VariantMap,
    ) -> zbus::Result<()>;
}

pub fn owned_string(value: impl AsRef<str>) -> OwnedValue {
    Value::from(value.as_ref()).try_into().unwrap()
}

pub fn owned_strings(values: Vec<String>) -> OwnedValue {
    Value::from(values).try_into().unwrap()
}

pub fn common_result(code: i64, message: impl Into<String>) -> VariantMap {
    let mut result = VariantMap::new();
    result.insert("code".to_string(), code.into());
    result.insert("message".to_string(), owned_string(message.into()));
    result.insert("type".to_string(), owned_string(""));
    result
}

fn set_state(
    inner: &TaskInner,
    state_name: TaskStateName,
    message: Option<String>,
) -> TaskSnapshot {
    let mut state = inner.state.lock().unwrap();
    state.state = state_name;
    if let Some(message) = message {
        state.message = message;
    }
    state.clone()
}

pub(crate) fn new_task_id() -> String {
    if let Ok(uuid) = std::fs::read_to_string("/proc/sys/kernel/random/uuid") {
        let id = uuid.trim().replace('-', "");
        if id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return id.to_ascii_lowercase();
        }
    }

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let entropy = timestamp
        ^ u128::from(std::process::id()) << 64
        ^ u128::from(COUNTER.fetch_add(1, Ordering::Relaxed));
    format!("{entropy:032x}")
}

fn new_interaction_id() -> String {
    if let Ok(uuid) = std::fs::read_to_string("/proc/sys/kernel/random/uuid") {
        let id = uuid.trim();
        if id.len() == 36
            && id.bytes().enumerate().all(|(index, byte)| {
                if matches!(index, 8 | 13 | 18 | 23) {
                    byte == b'-'
                } else {
                    byte.is_ascii_hexdigit()
                }
            })
        {
            return id.to_ascii_lowercase();
        }
    }
    let compact = new_task_id();
    format!(
        "{}-{}-{}-{}-{}",
        &compact[0..8],
        &compact[8..12],
        &compact[12..16],
        &compact[16..20],
        &compact[20..32]
    )
}

#[cfg(test)]
mod tests {
    use futures_lite::StreamExt;

    use super::*;

    #[test]
    fn task_ids_match_upstream_object_path_shape() {
        let task = TaskService::new(
            "queued",
            Box::new(|_| Box::pin(async { Ok(TaskCompletion::new("done", VariantMap::new())) })),
        );
        let id = task
            .path()
            .strip_prefix(&format!("{TASK_PATH_PREFIX}/"))
            .unwrap();
        assert_eq!(id.len(), 32);
        assert!(id.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    #[test]
    fn state_and_result_maps_have_variant_dictionary_shape() {
        let state = TaskSnapshot {
            state: TaskStateName::Processing,
            progress: 10.0,
            message: "working".to_string(),
        }
        .as_variant_map();
        assert_eq!(<&str>::try_from(&state["state"]).unwrap(), "Processing");
        assert_eq!(f64::try_from(&state["progress"]).unwrap(), 10.0);

        let result = common_result(-1, "failed");
        assert_eq!(i64::try_from(&result["code"]).unwrap(), -1);
        assert_eq!(<&str>::try_from(&result["type"]).unwrap(), "");
    }

    #[test]
    fn interaction_ids_match_qt_uuid_shape() {
        let id = new_interaction_id();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[8..9], "-");
        assert_eq!(&id[13..14], "-");
        assert_eq!(&id[18..19], "-");
        assert_eq!(&id[23..24], "-");
    }

    #[tokio::test]
    async fn queue_serializes_tasks_and_rejects_non_owner() {
        let service = zbus::Connection::session().await.unwrap();
        let destination = service.unique_name().unwrap().to_owned();
        let client = zbus::Connection::session().await.unwrap();
        let owner = client.unique_name().unwrap().to_string();
        let intruder = zbus::Connection::session().await.unwrap();
        let queue = TaskQueue::default();
        let order = Arc::new(Mutex::new(Vec::new()));

        let first_order = order.clone();
        let first = TaskService::new_pending_in_queue(
            "first",
            Box::new(move |_| {
                let order = first_order.clone();
                Box::pin(async move {
                    order.lock().unwrap().push("first-start");
                    async_io::Timer::after(std::time::Duration::from_millis(10)).await;
                    order.lock().unwrap().push("first-end");
                    Ok(TaskCompletion::new("first", common_result(0, "")))
                })
            }),
            queue.clone(),
            Some(owner.clone()),
        );
        let first_path = first.path().to_string();
        service
            .object_server()
            .at(first_path.clone(), first)
            .await
            .unwrap();

        let second_order = order.clone();
        let second = TaskService::new_pending_in_queue(
            "second",
            Box::new(move |_| {
                let order = second_order.clone();
                Box::pin(async move {
                    order.lock().unwrap().push("second-start");
                    order.lock().unwrap().push("second-end");
                    Ok(TaskCompletion::new("second", common_result(0, "")))
                })
            }),
            queue,
            Some(owner),
        );
        let second_path = second.path().to_string();
        service
            .object_server()
            .at(second_path.clone(), second)
            .await
            .unwrap();

        let intruder_task = Task1Proxy::builder(&intruder)
            .destination(destination.clone())
            .unwrap()
            .path(first_path.clone())
            .unwrap()
            .build()
            .await
            .unwrap();
        let error = intruder_task.start().await.unwrap_err();
        assert!(error.to_string().contains("not the task owner"));

        let first_task = Task1Proxy::builder(&client)
            .destination(destination.clone())
            .unwrap()
            .path(first_path)
            .unwrap()
            .build()
            .await
            .unwrap();
        let second_task = Task1Proxy::builder(&client)
            .destination(destination)
            .unwrap()
            .path(second_path)
            .unwrap()
            .build()
            .await
            .unwrap();
        let mut first_finished = first_task.receive_task_finished().await.unwrap();
        let mut second_finished = second_task.receive_task_finished().await.unwrap();
        first_task.start().await.unwrap();
        second_task.start().await.unwrap();
        first_finished.next().await.unwrap();
        second_finished.next().await.unwrap();

        assert_eq!(
            *order.lock().unwrap(),
            ["first-start", "first-end", "second-start", "second-end"]
        );
    }

    #[tokio::test]
    async fn pending_task_cancels_when_owner_disconnects() {
        let service = zbus::Connection::session().await.unwrap();
        let destination = service.unique_name().unwrap().to_owned();
        let owner = zbus::Connection::session().await.unwrap();
        let owner_name = owner.unique_name().unwrap().to_string();
        let observer = zbus::Connection::session().await.unwrap();
        let task = TaskService::new_pending_in_queue(
            "pending",
            Box::new(|_| Box::pin(async { Ok(TaskCompletion::new("done", common_result(0, ""))) })),
            TaskQueue::default(),
            Some(owner_name),
        );
        let task_path = task.path().to_string();
        service
            .object_server()
            .at(task_path.clone(), task)
            .await
            .unwrap();
        let task = service
            .object_server()
            .interface::<_, TaskService>(task_path.as_str())
            .await
            .unwrap();
        let emitter = task.signal_emitter().clone();
        let service_task = task.get().await.clone();
        service_task.watch_owner(emitter).await.unwrap();

        let observer_task = Task1Proxy::builder(&observer)
            .destination(destination)
            .unwrap()
            .path(task_path)
            .unwrap()
            .build()
            .await
            .unwrap();
        let mut finished = observer_task.receive_task_finished().await.unwrap();
        drop(owner);
        let signal = futures_lite::future::race(async { finished.next().await }, async {
            async_io::Timer::after(std::time::Duration::from_secs(1)).await;
            None
        })
        .await
        .expect("task did not finish after owner disconnected");
        let args = signal.args().unwrap();
        assert_eq!(i64::try_from(&args.result()["code"]).unwrap(), 1);
        assert_eq!(
            <&str>::try_from(&args.result()["message"]).unwrap(),
            "caller disconnected"
        );
    }

    #[tokio::test]
    async fn running_task_finishes_only_after_job_returns_from_cancel() {
        let service = zbus::Connection::session().await.unwrap();
        let destination = service.unique_name().unwrap().to_owned();
        let client = zbus::Connection::session().await.unwrap();
        let owner = client.unique_name().unwrap().to_string();
        let (started_sender, started_receiver) = async_channel::bounded(1);
        let (release_sender, release_receiver) = async_channel::bounded(1);
        let task = TaskService::new_pending_in_queue(
            "pending",
            Box::new(move |_| {
                Box::pin(async move {
                    started_sender.send(()).await.unwrap();
                    release_receiver.recv().await.unwrap();
                    let mut concrete = VariantMap::new();
                    concrete.insert("type".to_string(), owned_string("ExampleResult"));
                    concrete.insert("value".to_string(), 42_i64.into());
                    Ok(TaskCompletion::new("succeeded", concrete))
                })
            }),
            TaskQueue::default(),
            Some(owner),
        );
        let task_path = task.path().to_string();
        service
            .object_server()
            .at(task_path.clone(), task)
            .await
            .unwrap();
        let task = Task1Proxy::builder(&client)
            .destination(destination)
            .unwrap()
            .path(task_path)
            .unwrap()
            .build()
            .await
            .unwrap();
        let mut finished = task.receive_task_finished().await.unwrap();
        task.start().await.unwrap();
        started_receiver.recv().await.unwrap();
        task.cancel().await.unwrap();

        let early =
            futures_lite::future::race(async { finished.next().await.map(|_| true) }, async {
                async_io::Timer::after(std::time::Duration::from_millis(20)).await;
                None
            })
            .await;
        assert!(early.is_none());

        release_sender.send(()).await.unwrap();
        let signal = finished.next().await.unwrap();
        let args = signal.args().unwrap();
        assert_eq!(i64::try_from(&args.result()["code"]).unwrap(), 1);
        assert!(!args.result().contains_key("value"));
    }
}
