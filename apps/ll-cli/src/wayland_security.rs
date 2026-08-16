use std::fs;
use std::os::fd::AsFd;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
use wayland_protocols::wp::security_context::v1::client::wp_security_context_manager_v1::WpSecurityContextManagerV1;
use wayland_protocols::wp::security_context::v1::client::wp_security_context_v1::WpSecurityContextV1;

struct State;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(State: ignore WpSecurityContextManagerV1);
delegate_noop!(State: ignore WpSecurityContextV1);

pub struct WaylandSecurityContext {
    _connection: Connection,
    _manager: WpSecurityContextManagerV1,
    _context: WpSecurityContextV1,
    _listener: UnixListener,
    _close_writer: std::os::fd::OwnedFd,
    socket_path: PathBuf,
}

impl WaylandSecurityContext {
    pub fn create(bundle: &Path, app_id: &str, instance_id: &str) -> Result<Self, String> {
        let connection = Connection::connect_to_env()
            .map_err(|error| format!("failed to connect to wayland display: {error}"))?;
        Self::create_with_connection(connection, bundle, app_id, instance_id)
    }

    fn create_with_connection(
        connection: Connection,
        bundle: &Path,
        app_id: &str,
        instance_id: &str,
    ) -> Result<Self, String> {
        let (globals, mut event_queue) = registry_queue_init::<State>(&connection)
            .map_err(|error| format!("failed to get wayland registry: {error}"))?;
        let queue_handle = event_queue.handle();
        let manager = globals
            .bind::<WpSecurityContextManagerV1, _, _>(&queue_handle, 1..=1, ())
            .map_err(|error| {
                format!(
                    "failed to get wp_security_context_manager_v1, maybe compositor doesn't support this protocol: {error}"
                )
            })?;

        fs::create_dir_all(bundle)
            .map_err(|error| format!("failed to create {}: {error}", bundle.display()))?;
        let socket_path = bundle.join("wayland-socket");
        match fs::remove_file(&socket_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "failed to remove {}: {error}",
                    socket_path.display()
                ));
            }
        }
        let listener = UnixListener::bind(&socket_path)
            .map_err(|error| format!("failed to bind {}: {error}", socket_path.display()))?;
        let (close_reader, close_writer) =
            rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)
                .map_err(|error| format!("failed to create close pipe: {error}"))?;
        let context =
            manager.create_listener(listener.as_fd(), close_reader.as_fd(), &queue_handle, ());
        context.set_app_id(app_id.to_string());
        context.set_sandbox_engine("cn.org.linyaps".to_string());
        context.set_instance_id(instance_id.to_string());
        context.commit();
        event_queue
            .roundtrip(&mut State)
            .map_err(|error| format!("failed to commit wayland security context: {error}"))?;

        Ok(Self {
            _connection: connection,
            _manager: manager,
            _context: context,
            _listener: listener,
            _close_writer: close_writer,
            socket_path,
        })
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

impl Drop for WaylandSecurityContext {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket_path);
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::OwnedFd;
    use std::os::unix::net::{UnixStream, UnixStream as ClientStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, mpsc};
    use std::thread;
    use std::time::Duration;

    use wayland_client::Connection;
    use wayland_protocols::wp::security_context::v1::server::wp_security_context_manager_v1::{
        Request as ManagerRequest, WpSecurityContextManagerV1 as ServerManager,
    };
    use wayland_protocols::wp::security_context::v1::server::wp_security_context_v1::{
        Request as ContextRequest, WpSecurityContextV1 as ServerContext,
    };
    use wayland_server::{Client, DataInit, Dispatch, Display, DisplayHandle, GlobalDispatch, New};

    use super::WaylandSecurityContext;

    #[derive(Debug, Eq, PartialEq)]
    struct Metadata {
        app_id: String,
        sandbox_engine: String,
        instance_id: String,
        received_listen_fd: bool,
        received_close_fd: bool,
    }

    struct ServerState {
        app_id: String,
        sandbox_engine: String,
        instance_id: String,
        listen_fd: Option<OwnedFd>,
        close_fd: Option<OwnedFd>,
        committed: Option<mpsc::Sender<Metadata>>,
    }

    impl GlobalDispatch<ServerManager, ()> for ServerState {
        fn bind(
            _: &mut Self,
            _: &DisplayHandle,
            _: &Client,
            resource: New<ServerManager>,
            _: &(),
            data_init: &mut DataInit<'_, Self>,
        ) {
            data_init.init(resource, ());
        }
    }

    impl Dispatch<ServerManager, ()> for ServerState {
        fn request(
            state: &mut Self,
            _: &Client,
            _: &ServerManager,
            request: ManagerRequest,
            _: &(),
            _: &DisplayHandle,
            data_init: &mut DataInit<'_, Self>,
        ) {
            if let ManagerRequest::CreateListener {
                id,
                listen_fd,
                close_fd,
            } = request
            {
                state.listen_fd = Some(listen_fd);
                state.close_fd = Some(close_fd);
                data_init.init(id, ());
            }
        }
    }

    impl Dispatch<ServerContext, ()> for ServerState {
        fn request(
            state: &mut Self,
            _: &Client,
            _: &ServerContext,
            request: ContextRequest,
            _: &(),
            _: &DisplayHandle,
            _: &mut DataInit<'_, Self>,
        ) {
            match request {
                ContextRequest::SetAppId { app_id } => state.app_id = app_id,
                ContextRequest::SetSandboxEngine { name } => state.sandbox_engine = name,
                ContextRequest::SetInstanceId { instance_id } => {
                    state.instance_id = instance_id;
                }
                ContextRequest::Commit => {
                    if let Some(sender) = state.committed.take() {
                        sender
                            .send(Metadata {
                                app_id: state.app_id.clone(),
                                sandbox_engine: state.sandbox_engine.clone(),
                                instance_id: state.instance_id.clone(),
                                received_listen_fd: state.listen_fd.is_some(),
                                received_close_fd: state.close_fd.is_some(),
                            })
                            .unwrap();
                    }
                }
                _ => {}
            }
        }
    }

    #[test]
    fn registers_security_metadata_and_listener_fds() {
        let (client_socket, server_socket) = UnixStream::pair().unwrap();
        let (sender, receiver) = mpsc::channel();
        let stopped = Arc::new(AtomicBool::new(false));
        let server_stopped = Arc::clone(&stopped);
        let server = thread::spawn(move || {
            let mut display = Display::<ServerState>::new().unwrap();
            let mut handle = display.handle();
            handle.create_global::<ServerState, ServerManager, _>(1, ());
            handle.insert_client(server_socket, Arc::new(())).unwrap();
            let mut state = ServerState {
                app_id: String::new(),
                sandbox_engine: String::new(),
                instance_id: String::new(),
                listen_fd: None,
                close_fd: None,
                committed: Some(sender),
            };
            while !server_stopped.load(Ordering::Acquire) {
                display.dispatch_clients(&mut state).unwrap();
                display.flush_clients().unwrap();
                thread::sleep(Duration::from_millis(1));
            }
        });

        let temporary = tempfile::tempdir().unwrap();
        let connection = Connection::from_socket(client_socket).unwrap();
        let context = WaylandSecurityContext::create_with_connection(
            connection,
            temporary.path(),
            "org.deepin.demo",
            "instance-123",
        )
        .unwrap();
        assert!(ClientStream::connect(context.socket_path()).is_ok());
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(2)).unwrap(),
            Metadata {
                app_id: "org.deepin.demo".to_string(),
                sandbox_engine: "cn.org.linyaps".to_string(),
                instance_id: "instance-123".to_string(),
                received_listen_fd: true,
                received_close_fd: true,
            }
        );
        let socket_path = context.socket_path().to_path_buf();
        drop(context);
        assert!(!socket_path.exists());
        stopped.store(true, Ordering::Release);
        server.join().unwrap();
    }
}
