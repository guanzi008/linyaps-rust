use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::future::Future;
use std::os::fd::OwnedFd as StdOwnedFd;
use std::pin::Pin;
use std::sync::Arc;

use futures_lite::StreamExt;
use linyaps_api::{
    CommonOptions, CommonResult, PackageInfoV2, PackageManagerInstallParameters,
    PackageManagerJobInfo, PackageManagerPruneResult, PackageManagerSearchParameters,
    PackageManagerTaskResult, PackageManagerUninstallParameters, PackageManagerUpdateParameters,
    Repo, RepoConfigV2,
};
use serde::{Serialize, de::DeserializeOwned};
use thiserror::Error;
use zbus::object_server::SignalEmitter;
use zvariant::{DeserializeDict, Fd, OwnedFd, OwnedValue, SerializeDict, Type, Value};

use crate::task::{
    Task1Proxy, TaskCompletion, TaskContext, TaskJob, TaskQueue, TaskService, VariantMap,
    common_result, new_task_id, owned_string, owned_strings,
};

type InteractionHandler<'a> = dyn Fn(i32, &VariantMap) -> bool + Send + Sync + 'a;

pub const PACKAGE_MANAGER_SERVICE: &str = "org.deepin.linglong.PackageManager1";
pub const PACKAGE_MANAGER_PATH: &str = "/org/deepin/linglong/PackageManager1";
pub const PACKAGE_MANAGER_INTERFACE: &str = "org.deepin.linglong.PackageManager1";

#[derive(Clone, Debug, Default, DeserializeDict, SerializeDict, Type, Value, OwnedValue)]
#[zvariant(signature = "a{sv}")]
struct RepoDict {
    alias: Option<String>,
    mirror_enabled: Option<bool>,
    name: String,
    priority: i64,
    url: String,
}

#[derive(Clone, Debug, Default, DeserializeDict, SerializeDict, Type, Value, OwnedValue)]
#[zvariant(signature = "a{sv}")]
struct RepoConfigDict {
    #[zvariant(rename = "defaultRepo")]
    default_repo: String,
    repos: Vec<RepoDict>,
    version: i64,
}

impl From<Repo> for RepoDict {
    fn from(repo: Repo) -> Self {
        Self {
            alias: repo.alias,
            mirror_enabled: repo.mirror_enabled,
            name: repo.name,
            priority: repo.priority,
            url: repo.url,
        }
    }
}

impl From<RepoDict> for Repo {
    fn from(repo: RepoDict) -> Self {
        Self {
            alias: repo.alias,
            mirror_enabled: repo.mirror_enabled,
            name: repo.name,
            priority: repo.priority,
            url: repo.url,
        }
    }
}

impl From<RepoConfigV2> for RepoConfigDict {
    fn from(config: RepoConfigV2) -> Self {
        Self {
            default_repo: config.default_repo,
            repos: config.repos.into_iter().map(RepoDict::from).collect(),
            version: config.version,
        }
    }
}

impl From<RepoConfigDict> for RepoConfigV2 {
    fn from(config: RepoConfigDict) -> Self {
        Self {
            default_repo: config.default_repo,
            repos: config.repos.into_iter().map(Repo::from).collect(),
            version: config.version,
        }
    }
}

#[zbus::proxy(
    interface = "org.deepin.linglong.PackageManager1",
    default_service = "org.deepin.linglong.PackageManager1",
    default_path = "/org/deepin/linglong/PackageManager1"
)]
trait PackageManager {
    #[zbus(property)]
    fn configuration(&self) -> zbus::Result<RepoConfigDict>;

    #[zbus(name = "SetConfiguration")]
    fn set_configuration(&self, parameters: RepoConfigDict) -> zbus::Result<()>;

    #[zbus(name = "Search")]
    fn search(&self, parameters: VariantMap) -> zbus::Result<VariantMap>;

    #[zbus(name = "Install")]
    fn install(&self, parameters: VariantMap) -> zbus::Result<VariantMap>;

    #[zbus(name = "InstallFromFile")]
    fn install_from_file(
        &self,
        fd: Fd<'_>,
        file_type: &str,
        options: VariantMap,
    ) -> zbus::Result<VariantMap>;

    #[zbus(name = "Uninstall")]
    fn uninstall(&self, parameters: VariantMap) -> zbus::Result<VariantMap>;

    #[zbus(name = "Update")]
    fn update(&self, parameters: VariantMap) -> zbus::Result<VariantMap>;

    #[zbus(name = "Prune")]
    fn prune(&self) -> zbus::Result<VariantMap>;

    #[zbus(name = "InitRunContext")]
    fn init_run_context(&self, config: &str, container_id: &str) -> zbus::Result<VariantMap>;

    #[zbus(signal, name = "PruneFinished")]
    fn prune_finished(&self, task_id: &str, result: VariantMap) -> zbus::Result<()>;

    #[zbus(signal, name = "InitRunContextFinished")]
    fn init_run_context_finished(&self, task_id: &str, success: bool) -> zbus::Result<()>;
}

#[derive(Debug, Error)]
pub enum PackageManagerError {
    #[error("failed to connect to package manager: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to communicate with package manager: {0}")]
    Bus(#[from] zbus::Error),
    #[error("invalid package manager response: {0}")]
    Protocol(String),
    #[error("package task failed with code {code}: {message}")]
    Task { code: i64, message: String },
}

#[derive(Debug)]
pub struct PackageManagerClient {
    connection: zbus::blocking::Connection,
}

#[derive(Clone, Debug)]
pub struct PackageManagerAsyncClient {
    connection: zbus::Connection,
}

impl PackageManagerAsyncClient {
    pub async fn system() -> Result<Self, PackageManagerError> {
        Ok(Self {
            connection: zbus::Connection::system().await?,
        })
    }

    #[cfg(unix)]
    pub async fn peer(path: impl AsRef<std::path::Path>) -> Result<Self, PackageManagerError> {
        let stream = tokio::net::UnixStream::connect(path).await?;
        let connection = zbus::connection::Builder::unix_stream(stream)
            .p2p()
            .build()
            .await?;
        Ok(Self { connection })
    }

    pub fn from_connection(connection: zbus::Connection) -> Self {
        Self { connection }
    }

    pub async fn configuration(&self) -> Result<RepoConfigV2, PackageManagerError> {
        let proxy = PackageManagerProxy::new(&self.connection).await?;
        Ok(proxy.configuration().await?.into())
    }

    pub async fn set_configuration(&self, config: RepoConfigV2) -> Result<(), PackageManagerError> {
        let proxy = PackageManagerProxy::new(&self.connection).await?;
        proxy.set_configuration(config.into()).await?;
        Ok(())
    }

    pub async fn search(
        &self,
        parameters: PackageManagerSearchParameters,
    ) -> Result<BTreeMap<String, Vec<PackageInfoV2>>, PackageManagerError> {
        let proxy = PackageManagerProxy::new(&self.connection).await?;
        let result = self
            .run_task(proxy.search(search_parameters_map(parameters)).await?, None)
            .await?;
        let result_type = result
            .get("type")
            .and_then(|value| <&str>::try_from(value).ok())
            .unwrap_or_default();
        if result_type != "PackageManager1SearchResult" {
            return Err(PackageManagerError::Protocol(format!(
                "unexpected task result type: {result_type}"
            )));
        }
        let packages = result.get("packages").ok_or_else(|| {
            PackageManagerError::Protocol("search result has no packages".to_string())
        })?;
        let packages = value_to_json(packages).map_err(PackageManagerError::Protocol)?;
        serde_json::from_value(packages)
            .map_err(|error| PackageManagerError::Protocol(error.to_string()))
    }

    pub async fn install(
        &self,
        parameters: PackageManagerInstallParameters,
    ) -> Result<CommonResult, PackageManagerError> {
        self.install_with_interaction(parameters, |_, _| false)
            .await
    }

    pub async fn install_with_interaction<F>(
        &self,
        parameters: PackageManagerInstallParameters,
        interaction: F,
    ) -> Result<CommonResult, PackageManagerError>
    where
        F: Fn(i32, &VariantMap) -> bool + Send + Sync,
    {
        let proxy = PackageManagerProxy::new(&self.connection).await?;
        let parameters = parameters_map(parameters).map_err(PackageManagerError::Protocol)?;
        let result = self
            .run_task(proxy.install(parameters).await?, Some(&interaction))
            .await?;
        parse_common_result(&result)
    }

    pub async fn install_file_with_interaction<F>(
        &self,
        path: impl AsRef<std::path::Path>,
        file_type: &str,
        options: CommonOptions,
        interaction: F,
    ) -> Result<CommonResult, PackageManagerError>
    where
        F: Fn(i32, &VariantMap) -> bool + Send + Sync,
    {
        let file = File::open(path).map_err(|error| {
            PackageManagerError::Protocol(format!("failed to open package file: {error}"))
        })?;
        let proxy = PackageManagerProxy::new(&self.connection).await?;
        let options = parameters_map(options).map_err(PackageManagerError::Protocol)?;
        let result = self
            .run_task(
                proxy
                    .install_from_file(Fd::from(&file), file_type, options)
                    .await?,
                Some(&interaction),
            )
            .await?;
        parse_common_result(&result)
    }

    pub async fn uninstall(
        &self,
        parameters: PackageManagerUninstallParameters,
    ) -> Result<CommonResult, PackageManagerError> {
        let proxy = PackageManagerProxy::new(&self.connection).await?;
        let parameters = parameters_map(parameters).map_err(PackageManagerError::Protocol)?;
        let result = self
            .run_task(proxy.uninstall(parameters).await?, None)
            .await?;
        parse_common_result(&result)
    }

    pub async fn update(
        &self,
        parameters: PackageManagerUpdateParameters,
    ) -> Result<CommonResult, PackageManagerError> {
        let proxy = PackageManagerProxy::new(&self.connection).await?;
        let parameters = parameters_map(parameters).map_err(PackageManagerError::Protocol)?;
        let result = self.run_task(proxy.update(parameters).await?, None).await?;
        parse_common_result(&result)
    }

    pub async fn prune(&self) -> Result<PackageManagerPruneResult, PackageManagerError> {
        let proxy = PackageManagerProxy::new(&self.connection).await?;
        let mut finished = proxy.receive_prune_finished().await?;
        let job = parse_job_info(proxy.prune().await?);
        if job.code != 0 {
            return Err(PackageManagerError::Task {
                code: job.code,
                message: job.message,
            });
        }
        loop {
            let signal = finished.next().await.ok_or_else(|| {
                PackageManagerError::Protocol(
                    "package manager disappeared before prune completed".to_string(),
                )
            })?;
            let args = signal
                .args()
                .map_err(|error| PackageManagerError::Protocol(error.to_string()))?;
            if *args.task_id() != job.id {
                continue;
            }
            let result = parse_prune_result(args.result())?;
            if result.code != 0 {
                return Err(PackageManagerError::Task {
                    code: result.code,
                    message: result.message,
                });
            }
            return Ok(result);
        }
    }

    pub async fn init_run_context(
        &self,
        config: &str,
        container_id: &str,
    ) -> Result<(), PackageManagerError> {
        let proxy = PackageManagerProxy::new(&self.connection).await?;
        let mut finished = proxy.receive_init_run_context_finished().await?;
        let job = parse_job_info(proxy.init_run_context(config, container_id).await?);
        if job.code != 0 {
            return Err(PackageManagerError::Task {
                code: job.code,
                message: job.message,
            });
        }
        loop {
            let signal = finished.next().await.ok_or_else(|| {
                PackageManagerError::Protocol(
                    "package manager disappeared before run context initialization completed"
                        .to_string(),
                )
            })?;
            let args = signal
                .args()
                .map_err(|error| PackageManagerError::Protocol(error.to_string()))?;
            if *args.task_id() != job.id {
                continue;
            }
            if *args.success() {
                return Ok(());
            }
            return Err(PackageManagerError::Task {
                code: -1,
                message: "InitRunContext failed".to_string(),
            });
        }
    }

    async fn run_task(
        &self,
        created: VariantMap,
        interaction: Option<&InteractionHandler<'_>>,
    ) -> Result<VariantMap, PackageManagerError> {
        let created = parse_task_result(created);
        if created.code != 0 {
            return Err(PackageManagerError::Task {
                code: created.code,
                message: created.message,
            });
        }
        let task_path = created.task_object_path.ok_or_else(|| {
            PackageManagerError::Protocol("task response has no taskObjectPath".to_string())
        })?;
        let task = Task1Proxy::builder(&self.connection)
            .destination(PACKAGE_MANAGER_SERVICE)?
            .path(task_path)?
            .build()
            .await?;
        let mut finished = task.receive_task_finished().await?;
        let mut interactions = task.receive_request_interaction().await?;
        task.start().await?;
        loop {
            enum Event {
                Finished(Option<Result<VariantMap, String>>),
                Interaction(Option<Result<(String, i32, VariantMap), String>>),
            }
            let event = futures_lite::future::race(
                async {
                    Event::Finished(finished.next().await.map(|signal| {
                        signal
                            .args()
                            .map(|args| args.result().clone())
                            .map_err(|error| error.to_string())
                    }))
                },
                async {
                    Event::Interaction(interactions.next().await.map(|signal| {
                        signal
                            .args()
                            .map(|args| {
                                (
                                    args.interaction_id().to_string(),
                                    *args.message_id(),
                                    args.additional_message().clone(),
                                )
                            })
                            .map_err(|error| error.to_string())
                    }))
                },
            )
            .await;
            match event {
                Event::Finished(Some(Ok(result))) => {
                    let common = parse_common_result_unchecked(&result);
                    if common.code != 0 {
                        return Err(PackageManagerError::Task {
                            code: common.code,
                            message: common.message,
                        });
                    }
                    return Ok(result);
                }
                Event::Finished(Some(Err(error))) | Event::Interaction(Some(Err(error))) => {
                    return Err(PackageManagerError::Protocol(error));
                }
                Event::Finished(None) => {
                    return Err(PackageManagerError::Protocol(
                        "task disappeared before completion".to_string(),
                    ));
                }
                Event::Interaction(Some(Ok((interaction_id, message_id, additional)))) => {
                    let accepted = interaction
                        .map(|handler| handler(message_id, &additional))
                        .unwrap_or(false);
                    let mut reply = VariantMap::new();
                    reply.insert(
                        "action".to_string(),
                        owned_string(if accepted { "yes" } else { "no" }),
                    );
                    task.reply_interaction(&interaction_id, reply).await?;
                }
                Event::Interaction(None) => {}
            }
        }
    }
}

impl PackageManagerClient {
    pub fn system() -> Result<Self, PackageManagerError> {
        Self::from_connection(zbus::blocking::Connection::system()?)
    }

    pub fn from_connection(
        connection: zbus::blocking::Connection,
    ) -> Result<Self, PackageManagerError> {
        Ok(Self { connection })
    }

    pub fn configuration(&self) -> Result<RepoConfigV2, PackageManagerError> {
        let proxy = PackageManagerProxyBlocking::new(&self.connection)?;
        Ok(proxy.configuration()?.into())
    }

    pub fn set_configuration(&self, config: RepoConfigV2) -> Result<(), PackageManagerError> {
        let proxy = PackageManagerProxyBlocking::new(&self.connection)?;
        proxy.set_configuration(config.into())?;
        Ok(())
    }

    pub fn search_task(
        &self,
        parameters: PackageManagerSearchParameters,
    ) -> Result<PackageManagerTaskResult, PackageManagerError> {
        let proxy = PackageManagerProxyBlocking::new(&self.connection)?;
        Ok(parse_task_result(
            proxy.search(search_parameters_map(parameters))?,
        ))
    }

    pub fn install_task(
        &self,
        parameters: PackageManagerInstallParameters,
    ) -> Result<PackageManagerTaskResult, PackageManagerError> {
        let proxy = PackageManagerProxyBlocking::new(&self.connection)?;
        let parameters = parameters_map(parameters).map_err(PackageManagerError::Protocol)?;
        Ok(parse_task_result(proxy.install(parameters)?))
    }

    pub fn install_file_task(
        &self,
        file: &File,
        file_type: &str,
        options: CommonOptions,
    ) -> Result<PackageManagerTaskResult, PackageManagerError> {
        let proxy = PackageManagerProxyBlocking::new(&self.connection)?;
        let options = parameters_map(options).map_err(PackageManagerError::Protocol)?;
        Ok(parse_task_result(proxy.install_from_file(
            Fd::from(file),
            file_type,
            options,
        )?))
    }

    pub fn uninstall_task(
        &self,
        parameters: PackageManagerUninstallParameters,
    ) -> Result<PackageManagerTaskResult, PackageManagerError> {
        let proxy = PackageManagerProxyBlocking::new(&self.connection)?;
        let parameters = parameters_map(parameters).map_err(PackageManagerError::Protocol)?;
        Ok(parse_task_result(proxy.uninstall(parameters)?))
    }

    pub fn update_task(
        &self,
        parameters: PackageManagerUpdateParameters,
    ) -> Result<PackageManagerTaskResult, PackageManagerError> {
        let proxy = PackageManagerProxyBlocking::new(&self.connection)?;
        let parameters = parameters_map(parameters).map_err(PackageManagerError::Protocol)?;
        Ok(parse_task_result(proxy.update(parameters)?))
    }

    pub fn prune_job(&self) -> Result<PackageManagerJobInfo, PackageManagerError> {
        let proxy = PackageManagerProxyBlocking::new(&self.connection)?;
        Ok(parse_job_info(proxy.prune()?))
    }
}

type GetConfigurationFuture =
    Pin<Box<dyn Future<Output = Result<RepoConfigV2, String>> + Send + 'static>>;
type GetConfiguration = dyn Fn() -> GetConfigurationFuture + Send + Sync + 'static;
type SetConfigurationFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'static>>;
type SetConfiguration = dyn Fn(RepoConfigV2) -> SetConfigurationFuture + Send + Sync + 'static;
type SearchFuture = Pin<
    Box<dyn Future<Output = Result<BTreeMap<String, Vec<PackageInfoV2>>, String>> + Send + 'static>,
>;
type Search =
    dyn Fn(PackageManagerSearchParameters, TaskContext) -> SearchFuture + Send + Sync + 'static;
type OperationFuture =
    Pin<Box<dyn Future<Output = Result<TaskCompletion, String>> + Send + 'static>>;
type Install =
    dyn Fn(PackageManagerInstallParameters, TaskContext) -> OperationFuture + Send + Sync + 'static;
type InstallFile =
    dyn Fn(File, String, CommonOptions, TaskContext) -> OperationFuture + Send + Sync + 'static;
type Uninstall = dyn Fn(PackageManagerUninstallParameters, TaskContext) -> OperationFuture
    + Send
    + Sync
    + 'static;
type Update =
    dyn Fn(PackageManagerUpdateParameters, TaskContext) -> OperationFuture + Send + Sync + 'static;
type PruneFuture =
    Pin<Box<dyn Future<Output = Result<Vec<PackageInfoV2>, String>> + Send + 'static>>;
type Prune = dyn Fn() -> PruneFuture + Send + Sync + 'static;
type InitRunContextFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'static>>;
type InitRunContext = dyn Fn(String, String) -> InitRunContextFuture + Send + Sync + 'static;
type AuthorizeFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'static>>;
type Authorize = dyn Fn(String, String) -> AuthorizeFuture + Send + Sync + 'static;

pub struct PackageManagerService {
    get_configuration: Arc<GetConfiguration>,
    set_configuration: Arc<SetConfiguration>,
    search: Option<Arc<Search>>,
    install: Option<Arc<Install>>,
    install_file: Option<Arc<InstallFile>>,
    uninstall: Option<Arc<Uninstall>>,
    update: Option<Arc<Update>>,
    prune: Option<Arc<Prune>>,
    init_run_context: Option<Arc<InitRunContext>>,
    authorize: Option<Arc<Authorize>>,
    tasks: TaskQueue,
    search_tasks: TaskQueue,
    init_run_context_tasks: TaskQueue,
}

impl PackageManagerService {
    pub fn new<GetConfigurationFn, GetConfigurationFut, SetConfigurationFn, SetConfigurationFut>(
        get_configuration: GetConfigurationFn,
        set_configuration: SetConfigurationFn,
    ) -> Self
    where
        GetConfigurationFn: Fn() -> GetConfigurationFut + Send + Sync + 'static,
        GetConfigurationFut: Future<Output = Result<RepoConfigV2, String>> + Send + 'static,
        SetConfigurationFn: Fn(RepoConfigV2) -> SetConfigurationFut + Send + Sync + 'static,
        SetConfigurationFut: Future<Output = Result<(), String>> + Send + 'static,
    {
        Self {
            get_configuration: Arc::new(move || Box::pin(get_configuration())),
            set_configuration: Arc::new(move |config| Box::pin(set_configuration(config))),
            search: None,
            install: None,
            install_file: None,
            uninstall: None,
            update: None,
            prune: None,
            init_run_context: None,
            authorize: None,
            tasks: TaskQueue::default(),
            search_tasks: TaskQueue::default(),
            init_run_context_tasks: TaskQueue::default(),
        }
    }

    pub fn with_search<F, Fut>(mut self, search: F) -> Self
    where
        F: Fn(PackageManagerSearchParameters, TaskContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<BTreeMap<String, Vec<PackageInfoV2>>, String>> + Send + 'static,
    {
        self.search = Some(Arc::new(move |parameters, context| {
            Box::pin(search(parameters, context))
        }));
        self
    }

    pub fn with_install<F, Fut>(mut self, install: F) -> Self
    where
        F: Fn(PackageManagerInstallParameters, TaskContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<TaskCompletion, String>> + Send + 'static,
    {
        self.install = Some(Arc::new(move |parameters, context| {
            Box::pin(install(parameters, context))
        }));
        self
    }

    pub fn with_install_file<F, Fut>(mut self, install_file: F) -> Self
    where
        F: Fn(File, String, CommonOptions, TaskContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<TaskCompletion, String>> + Send + 'static,
    {
        self.install_file = Some(Arc::new(move |file, file_type, options, context| {
            Box::pin(install_file(file, file_type, options, context))
        }));
        self
    }

    pub fn with_uninstall<F, Fut>(mut self, uninstall: F) -> Self
    where
        F: Fn(PackageManagerUninstallParameters, TaskContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<TaskCompletion, String>> + Send + 'static,
    {
        self.uninstall = Some(Arc::new(move |parameters, context| {
            Box::pin(uninstall(parameters, context))
        }));
        self
    }

    pub fn with_update<F, Fut>(mut self, update: F) -> Self
    where
        F: Fn(PackageManagerUpdateParameters, TaskContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<TaskCompletion, String>> + Send + 'static,
    {
        self.update = Some(Arc::new(move |parameters, context| {
            Box::pin(update(parameters, context))
        }));
        self
    }

    pub fn with_prune<F, Fut>(mut self, prune: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Vec<PackageInfoV2>, String>> + Send + 'static,
    {
        self.prune = Some(Arc::new(move || Box::pin(prune())));
        self
    }

    pub fn with_init_run_context<F, Fut>(mut self, init_run_context: F) -> Self
    where
        F: Fn(String, String) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), String>> + Send + 'static,
    {
        self.init_run_context = Some(Arc::new(move |config, container_id| {
            Box::pin(init_run_context(config, container_id))
        }));
        self
    }

    pub fn with_authorizer<F, Fut>(mut self, authorize: F) -> Self
    where
        F: Fn(String, String) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), String>> + Send + 'static,
    {
        self.authorize = Some(Arc::new(move |action, sender| {
            Box::pin(authorize(action, sender))
        }));
        self
    }

    async fn authorize(
        &self,
        action: &str,
        header: &zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<()> {
        let Some(authorize) = self.authorize.clone() else {
            return Ok(());
        };
        let sender = header
            .sender()
            .ok_or_else(|| zbus::fdo::Error::AccessDenied("caller has no bus name".to_string()))?
            .to_string();
        authorize(action.to_string(), sender)
            .await
            .map_err(zbus::fdo::Error::AccessDenied)
    }
}

#[zbus::interface(name = "org.deepin.linglong.PackageManager1")]
impl PackageManagerService {
    #[zbus(property)]
    async fn configuration(&self) -> zbus::fdo::Result<RepoConfigDict> {
        (self.get_configuration)()
            .await
            .map(RepoConfigDict::from)
            .map_err(zbus::fdo::Error::Failed)
    }

    async fn set_configuration(
        &self,
        parameters: RepoConfigDict,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<()> {
        self.authorize(
            "org.deepin.linglong.PackageManager1.set-configuration",
            &header,
        )
        .await?;
        (self.set_configuration)(parameters.into())
            .await
            .map_err(zbus::fdo::Error::Failed)
    }

    async fn search(
        &self,
        parameters: VariantMap,
        #[zbus(object_server)] object_server: &zbus::ObjectServer,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<VariantMap> {
        let parameters = match parse_search_parameters(parameters) {
            Ok(parameters) => parameters,
            Err(message) => return Ok(common_result(-1, message)),
        };
        let Some(search) = self.search.clone() else {
            return Ok(common_result(-1, "search is unavailable"));
        };

        let search_id = parameters.id.clone();
        let task_search_id = search_id.clone();
        let job: TaskJob = Box::new(move |context| {
            Box::pin(async move {
                context
                    .update_state_message(format!("searching {task_search_id}"))
                    .await
                    .map_err(|error| error.to_string())?;
                let packages = search(parameters, context).await?;
                Ok(TaskCompletion::new(
                    "search completed",
                    search_result_map(packages),
                ))
            })
        });
        let task = TaskService::new_in_queue(
            format!("waiting to search {search_id}"),
            job,
            self.search_tasks.clone(),
            task_owner(&header),
        );
        let task_path = task.path().to_string();
        object_server
            .at(task_path.clone(), task)
            .await
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        watch_registered_task(object_server, &task_path).await?;
        enqueue_registered_task(object_server, &task_path).await?;
        Ok(task_result_map(
            task_path,
            format!("{search_id} is waiting to be searched"),
        ))
    }

    async fn install(
        &self,
        parameters: VariantMap,
        #[zbus(object_server)] object_server: &zbus::ObjectServer,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<VariantMap> {
        self.authorize("org.deepin.linglong.PackageManager1.install", &header)
            .await?;
        let parameters = match parse_parameters::<PackageManagerInstallParameters>(parameters) {
            Ok(parameters) => parameters,
            Err(message) => return Ok(common_result(2001, message)),
        };
        let Some(install) = self.install.clone() else {
            return Ok(common_result(-1, "install is unavailable"));
        };
        let task_name = format!("Install {}", install_fuzzy_display(&parameters));
        register_operation_task(
            object_server,
            task_name.clone(),
            format!("{task_name} is queued"),
            Box::new(move |context| install(parameters, context)),
            self.tasks.clone(),
            task_owner(&header),
        )
        .await
    }

    async fn install_from_file(
        &self,
        fd: OwnedFd,
        file_type: &str,
        options: VariantMap,
        #[zbus(object_server)] object_server: &zbus::ObjectServer,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<VariantMap> {
        self.authorize(
            "org.deepin.linglong.PackageManager1.install-from-file",
            &header,
        )
        .await?;
        let options = match parse_parameters::<CommonOptions>(options) {
            Ok(options) => options,
            Err(message) => return Ok(common_result(-1, message)),
        };
        let Some(install_file) = self.install_file.clone() else {
            return Ok(common_result(-1, "install from file is unavailable"));
        };
        let fd = StdOwnedFd::from(fd);
        let file = File::from(fd);
        register_operation_task(
            object_server,
            "waiting to install from file".to_string(),
            format!("{file_type} file is now installing"),
            Box::new({
                let file_type = file_type.to_string();
                move |context| install_file(file, file_type, options, context)
            }),
            self.tasks.clone(),
            task_owner(&header),
        )
        .await
    }

    async fn uninstall(
        &self,
        parameters: VariantMap,
        #[zbus(object_server)] object_server: &zbus::ObjectServer,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<VariantMap> {
        self.authorize("org.deepin.linglong.PackageManager1.uninstall", &header)
            .await?;
        let parameters = match parse_parameters::<PackageManagerUninstallParameters>(parameters) {
            Ok(parameters) => parameters,
            Err(message) => return Ok(common_result(2101, message)),
        };
        let Some(uninstall) = self.uninstall.clone() else {
            return Ok(common_result(-1, "uninstall is unavailable"));
        };
        let module = parameters.package.module.as_deref().unwrap_or("binary");
        let summary = format!(
            "{}/{}/{}/{} is now uninstalling",
            parameters.package.id,
            parameters.package.version.as_deref().unwrap_or("unknown"),
            "unknown",
            module
        );
        register_operation_task(
            object_server,
            "waiting to uninstall".to_string(),
            summary,
            Box::new(move |context| uninstall(parameters, context)),
            self.tasks.clone(),
            task_owner(&header),
        )
        .await
    }

    async fn update(
        &self,
        parameters: VariantMap,
        #[zbus(object_server)] object_server: &zbus::ObjectServer,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<VariantMap> {
        self.authorize("org.deepin.linglong.PackageManager1.update", &header)
            .await?;
        let parameters = match parse_parameters::<PackageManagerUpdateParameters>(parameters) {
            Ok(parameters) => parameters,
            Err(message) => return Ok(common_result(2201, message)),
        };
        let Some(update) = self.update.clone() else {
            return Ok(common_result(-1, "update is unavailable"));
        };
        register_operation_task(
            object_server,
            "update apps".to_string(),
            "update apps is queued".to_string(),
            Box::new(move |context| update(parameters, context)),
            self.tasks.clone(),
            task_owner(&header),
        )
        .await
    }

    async fn prune(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<VariantMap> {
        self.authorize("org.deepin.linglong.PackageManager1.prune", &header)
            .await?;
        let Some(prune) = self.prune.clone() else {
            return Ok(common_result(-1, "prune is unavailable"));
        };
        let task_id = new_task_id();
        let emitted_task_id = task_id.clone();
        let emitter = emitter.to_owned();
        let queue_emitter = emitter.clone();
        self.tasks.enqueue(
            &queue_emitter,
            Box::pin(async move {
                let result = match prune().await {
                    Ok(packages) => prune_result_map(0, "", packages),
                    Err(message) => prune_result_map(-1, message, Vec::new()),
                };
                let _ = Self::prune_finished(&emitter, &emitted_task_id, &result).await;
            }),
        );
        Ok(job_info_map(task_id, ""))
    }

    async fn init_run_context(
        &self,
        config: &str,
        container_id: &str,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<VariantMap> {
        let Some(initialize) = self.init_run_context.clone() else {
            return Ok(common_result(
                -1,
                "run context initialization is unavailable",
            ));
        };
        let task_id = new_task_id();
        let emitted_task_id = task_id.clone();
        let config = config.to_string();
        let container_id = container_id.to_string();
        let emitter = emitter.to_owned();
        let queue_emitter = emitter.clone();
        self.init_run_context_tasks.enqueue(
            &queue_emitter,
            Box::pin(async move {
                let success = initialize(config, container_id).await.is_ok();
                let _ = Self::init_run_context_finished(&emitter, &emitted_task_id, success).await;
            }),
        );
        Ok(job_info_map(task_id, "InitRunContext queued"))
    }

    #[zbus(signal, name = "PruneFinished")]
    async fn prune_finished(
        emitter: &SignalEmitter<'_>,
        task_id: &str,
        result: &VariantMap,
    ) -> zbus::Result<()>;

    #[zbus(signal, name = "InitRunContextFinished")]
    async fn init_run_context_finished(
        emitter: &SignalEmitter<'_>,
        task_id: &str,
        success: bool,
    ) -> zbus::Result<()>;
}

async fn register_operation_task(
    object_server: &zbus::ObjectServer,
    initial_message: String,
    response_message: String,
    job: TaskJob,
    queue: TaskQueue,
    owner: Option<String>,
) -> zbus::fdo::Result<VariantMap> {
    let task = TaskService::new_pending_in_queue(initial_message, job, queue, owner);
    let task_path = task.path().to_string();
    object_server
        .at(task_path.clone(), task)
        .await
        .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
    watch_registered_task(object_server, &task_path).await?;
    Ok(task_result_map(task_path, response_message))
}

async fn watch_registered_task(
    object_server: &zbus::ObjectServer,
    task_path: &str,
) -> zbus::fdo::Result<()> {
    let task = object_server
        .interface::<_, TaskService>(task_path)
        .await
        .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
    let emitter = task.signal_emitter().clone();
    let service = task.get().await.clone();
    service
        .watch_owner(emitter)
        .await
        .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
}

async fn enqueue_registered_task(
    object_server: &zbus::ObjectServer,
    task_path: &str,
) -> zbus::fdo::Result<()> {
    let task = object_server
        .interface::<_, TaskService>(task_path)
        .await
        .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
    let emitter = task.signal_emitter().clone();
    let executor = emitter.connection().executor().clone();
    executor
        .spawn(
            async move {
                async_io::Timer::after(std::time::Duration::from_millis(25)).await;
                task.get().await.enqueue_queued(emitter);
            },
            "linglong queued task activation",
        )
        .detach();
    Ok(())
}

fn task_owner(header: &zbus::message::Header<'_>) -> Option<String> {
    header.sender().map(ToString::to_string)
}

fn parse_parameters<T: DeserializeOwned>(parameters: VariantMap) -> Result<T, String> {
    let value = variant_map_to_json(&parameters)?;
    serde_json::from_value(value).map_err(|error| error.to_string())
}

fn variant_map_to_json(parameters: &VariantMap) -> Result<serde_json::Value, String> {
    let mut object = serde_json::Map::new();
    for (key, value) in parameters {
        object.insert(key.clone(), value_to_json(value)?);
    }
    Ok(serde_json::Value::Object(object))
}

fn install_fuzzy_display(parameters: &PackageManagerInstallParameters) -> String {
    format!(
        "{}:{}/{}/unknown",
        parameters.package.channel.as_deref().unwrap_or("unknown"),
        parameters.package.id,
        parameters.package.version.as_deref().unwrap_or("unknown")
    )
}

fn parameters_map<T: Serialize>(parameters: T) -> Result<VariantMap, String> {
    let serde_json::Value::Object(parameters) =
        serde_json::to_value(parameters).map_err(|error| error.to_string())?
    else {
        return Err("package manager parameters must be an object".to_string());
    };
    Ok(parameters
        .into_iter()
        .map(|(key, value)| (key, json_owned_value(value)))
        .collect())
}

fn search_parameters_map(parameters: PackageManagerSearchParameters) -> VariantMap {
    let mut result = VariantMap::new();
    result.insert("id".to_string(), owned_string(parameters.id));
    result.insert("repos".to_string(), owned_strings(parameters.repos));
    result
}

fn parse_search_parameters(
    mut parameters: VariantMap,
) -> Result<PackageManagerSearchParameters, String> {
    let id = parameters
        .remove("id")
        .ok_or_else(|| "missing search parameter: id".to_string())
        .and_then(|value| {
            String::try_from(value).map_err(|_| "search parameter id must be a string".to_string())
        })?;
    let repos = parameters
        .remove("repos")
        .ok_or_else(|| "missing search parameter: repos".to_string())
        .and_then(|value| {
            Vec::<String>::try_from(value)
                .map_err(|_| "search parameter repos must be a string array".to_string())
        })?;
    Ok(PackageManagerSearchParameters { id, repos })
}

fn task_result_map(task_path: String, message: String) -> VariantMap {
    let mut result = common_result(0, message);
    result.insert("taskObjectPath".to_string(), owned_string(task_path));
    result
}

fn job_info_map(id: String, message: impl Into<String>) -> VariantMap {
    let mut result = common_result(0, message);
    result.insert("id".to_string(), owned_string(id));
    result
}

fn parse_job_info(mut result: VariantMap) -> PackageManagerJobInfo {
    PackageManagerJobInfo {
        code: result
            .remove("code")
            .and_then(|value| i64::try_from(value).ok())
            .unwrap_or(-1),
        id: result
            .remove("id")
            .and_then(|value| String::try_from(value).ok())
            .unwrap_or_default(),
        message: result
            .remove("message")
            .and_then(|value| String::try_from(value).ok())
            .unwrap_or_default(),
        result_type: result
            .remove("type")
            .and_then(|value| String::try_from(value).ok())
            .unwrap_or_default(),
    }
}

fn parse_task_result(mut result: VariantMap) -> PackageManagerTaskResult {
    PackageManagerTaskResult {
        code: result
            .remove("code")
            .and_then(|value| i64::try_from(value).ok())
            .unwrap_or(-1),
        message: result
            .remove("message")
            .and_then(|value| String::try_from(value).ok())
            .unwrap_or_default(),
        task_object_path: result
            .remove("taskObjectPath")
            .and_then(|value| String::try_from(value).ok()),
        result_type: result
            .remove("type")
            .and_then(|value| String::try_from(value).ok())
            .unwrap_or_default(),
    }
}

fn parse_common_result(result: &VariantMap) -> Result<CommonResult, PackageManagerError> {
    let result = parse_common_result_unchecked(result);
    if result.code != 0 {
        return Err(PackageManagerError::Task {
            code: result.code,
            message: result.message,
        });
    }
    Ok(result)
}

fn parse_common_result_unchecked(result: &VariantMap) -> CommonResult {
    CommonResult {
        code: result
            .get("code")
            .and_then(|value| i64::try_from(value).ok())
            .unwrap_or(-1),
        message: result
            .get("message")
            .and_then(|value| <&str>::try_from(value).ok())
            .unwrap_or("package task failed")
            .to_string(),
        result_type: result
            .get("type")
            .and_then(|value| <&str>::try_from(value).ok())
            .unwrap_or_default()
            .to_string(),
    }
}

fn search_result_map(packages: BTreeMap<String, Vec<PackageInfoV2>>) -> VariantMap {
    let mut groups = VariantMap::new();
    for (alias, packages) in packages {
        let packages = packages
            .into_iter()
            .map(package_info_map)
            .map(OwnedValue::from)
            .collect::<Vec<_>>();
        groups.insert(
            alias,
            Value::from(packages)
                .try_into()
                .expect("valid package array"),
        );
    }

    let mut result = common_result(0, "");
    result.insert(
        "type".to_string(),
        owned_string("PackageManager1SearchResult"),
    );
    result.insert("packages".to_string(), OwnedValue::from(groups));
    result
}

fn prune_result_map(
    code: i64,
    message: impl Into<String>,
    packages: Vec<PackageInfoV2>,
) -> VariantMap {
    let mut result = common_result(code, message);
    result.insert(
        "type".to_string(),
        owned_string("PackageManager1PruneResult"),
    );
    let packages = packages
        .into_iter()
        .map(package_info_map)
        .map(OwnedValue::from)
        .collect::<Vec<_>>();
    result.insert(
        "packages".to_string(),
        Value::from(packages)
            .try_into()
            .expect("valid package array"),
    );
    result
}

fn parse_prune_result(
    result: &VariantMap,
) -> Result<PackageManagerPruneResult, PackageManagerError> {
    serde_json::from_value(variant_map_to_json(result).map_err(PackageManagerError::Protocol)?)
        .map_err(|error| PackageManagerError::Protocol(error.to_string()))
}

fn package_info_map(package: PackageInfoV2) -> VariantMap {
    let mut result = VariantMap::new();
    result.insert("arch".to_string(), owned_strings(package.arch));
    result.insert("base".to_string(), owned_string(package.base));
    result.insert("channel".to_string(), owned_string(package.channel));
    insert_optional_strings(&mut result, "command", package.command);
    insert_optional_string(
        &mut result,
        "compatible_version",
        package.compatible_version,
    );
    insert_optional_string(&mut result, "description", package.description);
    insert_optional_json(&mut result, "ext_impl", package.extension_implementation);
    if let Some(extensions) = package.extensions {
        let values = extensions
            .into_iter()
            .map(|value| {
                json_owned_value(
                    serde_json::to_value(value).expect("extension definition is serializable"),
                )
            })
            .collect::<Vec<_>>();
        result.insert(
            "extensions".to_string(),
            Value::from(values)
                .try_into()
                .expect("valid extension array"),
        );
    }
    result.insert("id".to_string(), owned_string(package.id));
    result.insert("kind".to_string(), owned_string(package.kind));
    result.insert("module".to_string(), owned_string(package.module));
    result.insert("name".to_string(), owned_string(package.name));
    insert_optional_json(&mut result, "permissions", package.permissions);
    insert_optional_string(&mut result, "runtime", package.runtime);
    result.insert(
        "schema_version".to_string(),
        owned_string(package.schema_version),
    );
    result.insert("size".to_string(), package.size.into());
    insert_optional_string(&mut result, "uuid", package.uuid);
    result.insert("version".to_string(), owned_string(package.version));
    result
}

fn insert_optional_string(result: &mut VariantMap, key: &str, value: Option<String>) {
    if let Some(value) = value {
        result.insert(key.to_string(), owned_string(value));
    }
}

fn insert_optional_strings(result: &mut VariantMap, key: &str, value: Option<Vec<String>>) {
    if let Some(value) = value {
        result.insert(key.to_string(), owned_strings(value));
    }
}

fn insert_optional_json<T: serde::Serialize>(result: &mut VariantMap, key: &str, value: Option<T>) {
    if let Some(value) = value {
        result.insert(
            key.to_string(),
            json_owned_value(serde_json::to_value(value).expect("API value is serializable")),
        );
    }
}

fn json_owned_value(value: serde_json::Value) -> OwnedValue {
    match value {
        serde_json::Value::Null => owned_string(""),
        serde_json::Value::Bool(value) => value.into(),
        serde_json::Value::Number(value) => {
            if let Some(value) = value.as_i64() {
                value.into()
            } else if let Some(value) = value.as_u64() {
                value.into()
            } else {
                value.as_f64().unwrap_or_default().into()
            }
        }
        serde_json::Value::String(value) => owned_string(value),
        serde_json::Value::Array(values) => {
            let values = values.into_iter().map(json_owned_value).collect::<Vec<_>>();
            Value::from(values).try_into().expect("valid JSON array")
        }
        serde_json::Value::Object(values) => {
            let values = values
                .into_iter()
                .map(|(key, value)| (key, json_owned_value(value)))
                .collect::<HashMap<_, _>>();
            OwnedValue::from(values)
        }
    }
}

fn value_to_json(value: &OwnedValue) -> Result<serde_json::Value, String> {
    borrowed_value_to_json(value)
}

fn borrowed_value_to_json(value: &Value<'_>) -> Result<serde_json::Value, String> {
    Ok(match value {
        Value::U8(value) => serde_json::Value::from(*value),
        Value::Bool(value) => serde_json::Value::from(*value),
        Value::I16(value) => serde_json::Value::from(*value),
        Value::U16(value) => serde_json::Value::from(*value),
        Value::I32(value) => serde_json::Value::from(*value),
        Value::U32(value) => serde_json::Value::from(*value),
        Value::I64(value) => serde_json::Value::from(*value),
        Value::U64(value) => serde_json::Value::from(*value),
        Value::F64(value) => serde_json::Number::from_f64(*value)
            .map(serde_json::Value::Number)
            .ok_or_else(|| "non-finite floating point value".to_string())?,
        Value::Str(value) => serde_json::Value::String(value.to_string()),
        Value::Signature(value) => serde_json::Value::String(value.to_string()),
        Value::ObjectPath(value) => serde_json::Value::String(value.to_string()),
        Value::Value(value) => borrowed_value_to_json(value)?,
        Value::Array(value) => serde_json::Value::Array(
            value
                .inner()
                .iter()
                .map(borrowed_value_to_json)
                .collect::<Result<_, _>>()?,
        ),
        Value::Dict(value) => {
            let mut object = serde_json::Map::new();
            for (key, value) in value.iter() {
                let key = <&str>::try_from(key)
                    .map_err(|_| "dictionary key is not a string".to_string())?;
                object.insert(key.to_string(), borrowed_value_to_json(value)?);
            }
            serde_json::Value::Object(object)
        }
        Value::Structure(value) => serde_json::Value::Array(
            value
                .fields()
                .iter()
                .map(borrowed_value_to_json)
                .collect::<Result<_, _>>()?,
        ),
        #[cfg(unix)]
        Value::Fd(_) => return Err("file descriptors cannot be represented as JSON".to_string()),
    })
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::sync::Mutex;

    use futures_lite::StreamExt;

    use super::*;
    use crate::Task1Proxy;

    #[test]
    fn api_config_round_trips_through_dbus_shape() {
        let config = RepoConfigV2 {
            default_repo: "stable".to_string(),
            repos: vec![Repo {
                alias: Some("stable".to_string()),
                mirror_enabled: Some(true),
                name: "main".to_string(),
                priority: 100,
                url: "https://example.invalid".to_string(),
            }],
            version: 2,
        };
        assert_eq!(
            RepoConfigV2::from(RepoConfigDict::from(config.clone())),
            config
        );
        assert_eq!(RepoConfigDict::SIGNATURE, "a{sv}");
        assert_eq!(RepoDict::SIGNATURE, "a{sv}");
    }

    #[tokio::test]
    async fn configuration_waits_for_async_source() {
        let expected = RepoConfigV2 {
            default_repo: "stable".to_string(),
            repos: vec![Repo {
                alias: None,
                mirror_enabled: None,
                name: "stable".to_string(),
                priority: 0,
                url: "https://stable.example".to_string(),
            }],
            version: 2,
        };
        let (started_sender, started_receiver) = async_channel::bounded(1);
        let (release_sender, release_receiver) = async_channel::bounded(1);
        let getter_expected = expected.clone();
        let service = Arc::new(PackageManagerService::new(
            move || {
                let started_sender = started_sender.clone();
                let release_receiver = release_receiver.clone();
                let expected = getter_expected.clone();
                async move {
                    started_sender
                        .send(())
                        .await
                        .map_err(|error| error.to_string())?;
                    release_receiver
                        .recv()
                        .await
                        .map_err(|error| error.to_string())?;
                    Ok(expected)
                }
            },
            |_config| async { Ok(()) },
        ));
        let request_service = service.clone();
        let request = tokio::spawn(async move { request_service.configuration().await });

        started_receiver.recv().await.unwrap();
        tokio::task::yield_now().await;
        assert!(!request.is_finished());
        release_sender.send(()).await.unwrap();

        let observed = tokio::time::timeout(std::time::Duration::from_secs(1), request)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(RepoConfigV2::from(observed), expected);
    }

    #[tokio::test]
    async fn client_and_service_exchange_configuration_on_dbus() {
        let initial = RepoConfigV2 {
            default_repo: "stable".to_string(),
            repos: vec![Repo {
                alias: None,
                mirror_enabled: None,
                name: "stable".to_string(),
                priority: 0,
                url: "https://stable.example".to_string(),
            }],
            version: 2,
        };
        let state = Arc::new(Mutex::new(initial.clone()));
        let getter_state = state.clone();
        let setter_state = state.clone();
        let installed_file = Arc::new(Mutex::new(None));
        let installed_file_state = installed_file.clone();
        let authorizations = Arc::new(Mutex::new(Vec::new()));
        let authorization_state = authorizations.clone();
        let service = PackageManagerService::new(
            move || {
                let getter_state = getter_state.clone();
                async move { Ok(getter_state.lock().unwrap().clone()) }
            },
            move |config| {
                let setter_state = setter_state.clone();
                async move {
                    *setter_state.lock().unwrap() = config;
                    Ok(())
                }
            },
        )
        .with_search(|parameters, _| async move {
            let mut packages = BTreeMap::new();
            packages.insert(
                parameters.repos[0].clone(),
                vec![PackageInfoV2 {
                    arch: vec!["x86_64".to_string()],
                    base: "main:org.deepin.base/23.1.0/x86_64".to_string(),
                    channel: "main".to_string(),
                    command: None,
                    compatible_version: None,
                    description: Some("Demo".to_string()),
                    extension_implementation: None,
                    extensions: None,
                    id: parameters.id,
                    kind: "app".to_string(),
                    module: "binary".to_string(),
                    name: "Demo".to_string(),
                    permissions: None,
                    runtime: None,
                    schema_version: "1.0".to_string(),
                    size: 42,
                    uuid: None,
                    version: "1.0.0.0".to_string(),
                }],
            );
            Ok(packages)
        })
        .with_install_file(move |mut file, file_type, options, _| {
            let installed_file_state = installed_file_state.clone();
            async move {
                let mut contents = Vec::new();
                file.read_to_end(&mut contents)
                    .map_err(|error| error.to_string())?;
                *installed_file_state.lock().unwrap() = Some((file_type, options, contents));
                Ok(TaskCompletion::new(
                    "install layer successfully",
                    common_result(0, "install layer successfully"),
                ))
            }
        })
        .with_prune(|| async {
            Ok(vec![PackageInfoV2 {
                arch: vec!["x86_64".to_string()],
                base: String::new(),
                channel: "main".to_string(),
                command: None,
                compatible_version: None,
                description: None,
                extension_implementation: None,
                extensions: None,
                id: "org.example.unused".to_string(),
                kind: "runtime".to_string(),
                module: "binary".to_string(),
                name: "Unused runtime".to_string(),
                permissions: None,
                runtime: None,
                schema_version: "1.0".to_string(),
                size: 8,
                uuid: None,
                version: "1.0.0.0".to_string(),
            }])
        })
        .with_init_run_context(|config, container_id| async move {
            if config != "{\"version\":\"1\"}" || container_id != "container" {
                return Err("unexpected run context".to_string());
            }
            Ok(())
        })
        .with_authorizer(move |action, sender| {
            let authorization_state = authorization_state.clone();
            async move {
                if sender.is_empty() {
                    return Err("missing sender".to_string());
                }
                authorization_state.lock().unwrap().push(action);
                Ok(())
            }
        });
        let _service_connection = zbus::connection::Builder::session()
            .unwrap()
            .name(PACKAGE_MANAGER_SERVICE)
            .unwrap()
            .serve_at(PACKAGE_MANAGER_PATH, service)
            .unwrap()
            .build()
            .await
            .unwrap();

        let updated = RepoConfigV2 {
            default_repo: "testing".to_string(),
            repos: vec![Repo {
                alias: Some("testing".to_string()),
                mirror_enabled: Some(true),
                name: "testing-origin".to_string(),
                priority: 100,
                url: "https://testing.example".to_string(),
            }],
            version: 2,
        };
        let client_updated = updated.clone();
        let observed = tokio::task::spawn_blocking(move || {
            let connection = zbus::blocking::Connection::session().unwrap();
            let client = PackageManagerClient::from_connection(connection).unwrap();
            assert_eq!(client.configuration().unwrap(), initial);
            client.set_configuration(client_updated).unwrap();
            client.configuration().unwrap()
        })
        .await
        .unwrap();

        assert_eq!(observed, updated);
        assert_eq!(*state.lock().unwrap(), updated);

        let connection = zbus::Connection::session().await.unwrap();
        let package_manager = PackageManagerProxy::new(&connection).await.unwrap();
        let created = package_manager
            .search(search_parameters_map(PackageManagerSearchParameters {
                id: "org.example.demo".to_string(),
                repos: vec!["stable".to_string()],
            }))
            .await
            .unwrap();
        let created = parse_task_result(created);
        assert_eq!(created.code, 0);
        let task = Task1Proxy::builder(&connection)
            .destination(PACKAGE_MANAGER_SERVICE)
            .unwrap()
            .path(created.task_object_path.unwrap())
            .unwrap()
            .build()
            .await
            .unwrap();
        let mut events = task.receive_task_event().await.unwrap();
        let mut finished = task.receive_task_finished().await.unwrap();
        task.start().await.unwrap();

        let mut saw_processing = false;
        while !saw_processing {
            let signal = events.next().await.unwrap();
            let args = signal.args().unwrap();
            if *args.event() == "state"
                && <&str>::try_from(&args.data()["state"]).unwrap() == "Processing"
            {
                saw_processing = true;
            }
        }
        let signal = finished.next().await.unwrap();
        let args = signal.args().unwrap();
        assert_eq!(
            <&str>::try_from(&args.result()["type"]).unwrap(),
            "PackageManager1SearchResult"
        );
        let groups = VariantMap::try_from(args.result()["packages"].clone()).unwrap();
        let packages = Vec::<OwnedValue>::try_from(groups["stable"].clone()).unwrap();
        let package = VariantMap::try_from(packages.into_iter().next().unwrap()).unwrap();
        assert_eq!(
            <&str>::try_from(&package["id"]).unwrap(),
            "org.example.demo"
        );

        let client = PackageManagerAsyncClient::from_connection(connection);
        let packages = client
            .search(PackageManagerSearchParameters {
                id: "org.example.second".to_string(),
                repos: vec!["stable".to_string()],
            })
            .await
            .unwrap();
        assert_eq!(packages["stable"][0].id, "org.example.second");
        assert_eq!(packages["stable"][0].size, 42);

        let package_file =
            std::env::temp_dir().join(format!("linyaps-dbus-install-file-{}", std::process::id()));
        std::fs::write(&package_file, b"layer payload").unwrap();
        let result = client
            .install_file_with_interaction(
                &package_file,
                "layer",
                CommonOptions {
                    force: true,
                    no_auto_prune: Some(true),
                    skip_interaction: false,
                },
                |_, _| false,
            )
            .await
            .unwrap();
        std::fs::remove_file(package_file).unwrap();
        assert_eq!(result.code, 0);
        let (file_type, options, contents) = installed_file.lock().unwrap().take().unwrap();
        assert_eq!(file_type, "layer");
        assert!(options.force);
        assert_eq!(contents, b"layer payload");

        let pruned = client.prune().await.unwrap();
        assert_eq!(pruned.code, 0);
        assert_eq!(pruned.packages.unwrap()[0].id, "org.example.unused");
        client
            .init_run_context("{\"version\":\"1\"}", "container")
            .await
            .unwrap();
        assert_eq!(
            *authorizations.lock().unwrap(),
            [
                "org.deepin.linglong.PackageManager1.set-configuration",
                "org.deepin.linglong.PackageManager1.install-from-file",
                "org.deepin.linglong.PackageManager1.prune",
            ]
        );
    }
}
