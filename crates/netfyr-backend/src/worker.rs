use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use netfyr_state::Match;
use netfyr_state::plan::{NamespaceId, Observation, StateDiff};
use tokio::sync::{mpsc, oneshot};

use crate::executor::{apply_plan, failed_report, preview_plan, preview_report};
use crate::linux::LinuxTransport;
use crate::{
    ApplyReport, BackendError, BackendErrorKind, BackendFuture, DryRunReport, ETHERNET,
    NetworkBackend,
};

enum Request {
    Query(Match, oneshot::Sender<Result<Observation, BackendError>>),
    Apply(StateDiff, oneshot::Sender<ApplyReport>),
    Preview(StateDiff, oneshot::Sender<DryRunReport>),
}

/// A handle to a worker whose sockets and ioctls belong to one retained netns.
#[derive(Clone)]
pub struct NetlinkBackend {
    namespace: NamespaceId,
    sender: mpsc::Sender<Request>,
}

fn disconnected() -> BackendError {
    BackendError::new(
        BackendErrorKind::Internal,
        "namespace worker stopped before accepting the request",
    )
}

fn unreported() -> BackendError {
    BackendError::new(
        BackendErrorKind::OutcomeUnknown,
        "namespace worker stopped before reporting; writes may have applied",
    )
}

fn context(error: BackendError, what: &str) -> BackendError {
    BackendError {
        message: format!("{what}: {}", error.message),
        ..error
    }
}

fn open_namespace(path: &Path) -> Result<File, BackendError> {
    let describe = |error| {
        context(
            BackendError::from_io(BackendErrorKind::NotFound, error),
            &format!("open namespace {}", path.display()),
        )
    };
    // O_NONBLOCK keeps a FIFO from blocking; O_NOCTTY keeps a terminal from
    // becoming the controlling one.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)
        .map_err(describe)?;
    // SAFETY: statfs is plain data; fstatfs fills it for the live descriptor.
    let mut filesystem: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatfs(file.as_raw_fd(), &mut filesystem) } != 0 {
        return Err(describe(io::Error::last_os_error()));
    }
    // The width of f_type and NSFS_MAGIC differs between libc targets.
    #[allow(clippy::unnecessary_cast)]
    let nsfs = filesystem.f_type as i64 == libc::NSFS_MAGIC as i64;
    if !nsfs {
        return Err(BackendError::new(
            BackendErrorKind::UnsupportedOperation,
            format!("{} is not a namespace file", path.display()),
        ));
    }
    Ok(file)
}

// Driver and PCI path are known only after enrichment, so ignore them here.
fn may_match(selector: &Match, spec: &Match) -> bool {
    Match {
        driver: None,
        pci_path: None,
        ..selector.clone()
    }
    .matches(spec)
}

impl NetlinkBackend {
    /// Open a network namespace (for example `/run/netns/test`). This never
    /// changes the calling thread's namespace. Opening the current namespace
    /// does not require the privilege needed to enter a different namespace.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, BackendError> {
        let namespace_file = open_namespace(path.as_ref())?;
        let metadata = namespace_file.metadata().map_err(|error| {
            context(
                BackendError::from_io(BackendErrorKind::Internal, error),
                "namespace metadata",
            )
        })?;
        let namespace = NamespaceId {
            device: metadata.dev(),
            inode: metadata.ino(),
        };
        let (sender, mut receiver) = mpsc::channel(16);
        let (ready_tx, ready_rx) = std_mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("netfyr-netns".into())
            .spawn(move || {
                let setup = (|| {
                    let current =
                        std::fs::metadata("/proc/thread-self/ns/net").map_err(|error| {
                            BackendError::from_io(BackendErrorKind::Internal, error)
                        })?;
                    if current.dev() != namespace.device || current.ino() != namespace.inode {
                        // The FD is held for the entire worker lifetime. Only this
                        // dedicated thread enters it, before any socket is opened.
                        // SAFETY: setns takes a live descriptor and a namespace type.
                        if unsafe { libc::setns(namespace_file.as_raw_fd(), libc::CLONE_NEWNET) }
                            != 0
                        {
                            // EINVAL: an nsfs file for another namespace type.
                            return Err(context(
                                BackendError::from_io(
                                    BackendErrorKind::UnsupportedOperation,
                                    io::Error::last_os_error(),
                                ),
                                "enter namespace",
                            ));
                        }
                    }
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|err| {
                            BackendError::new(BackendErrorKind::Internal, err.to_string())
                        })
                })();
                let runtime = match setup {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                        return;
                    }
                };
                runtime.block_on(async move {
                    let mut transport = match LinuxTransport::new(namespace).await {
                        Ok(transport) => transport,
                        Err(error) => {
                            let _ = ready_tx.send(Err(error));
                            return;
                        }
                    };
                    if ready_tx.send(Ok(())).is_err() {
                        return;
                    }
                    loop {
                        let request = tokio::select! {
                            request = receiver.recv() => request,
                            () = transport.next_notification() => continue,
                        };
                        let Some(request) = request else {
                            break;
                        };
                        match request {
                            Request::Query(selector, response) => {
                                let result = transport
                                    .inventory(|spec| may_match(&selector, spec))
                                    .await
                                    .and_then(|mut observation| {
                                        observation.devices.retain(|device| {
                                            selector.matches(&device.state.match_spec)
                                        });
                                        if observation.devices.is_empty() && selector.name.is_some()
                                        {
                                            return Err(BackendError::new(
                                                BackendErrorKind::NotFound,
                                                format!("no interface matches {selector:?}"),
                                            ));
                                        }
                                        Ok(observation)
                                    });
                                let _ = response.send(result);
                            }
                            Request::Apply(diff, response) => {
                                let report =
                                    apply_plan(&mut transport, &diff, &|| response.is_closed())
                                        .await;
                                let _ = response.send(report);
                            }
                            Request::Preview(diff, response) => {
                                let report = preview_plan(&mut transport, &diff).await;
                                let _ = response.send(report);
                            }
                        }
                    }
                    drop(namespace_file);
                });
            })
            .map_err(|err| {
                BackendError::new(
                    BackendErrorKind::Internal,
                    format!("start namespace worker: {err}"),
                )
            })?;
        ready_rx
            .recv_timeout(Duration::from_secs(30))
            .map_err(|_| disconnected())??;
        Ok(Self { namespace, sender })
    }

    /// Open the calling thread's network namespace.
    pub fn current() -> Result<Self, BackendError> {
        Self::open("/proc/thread-self/ns/net")
    }
}

impl NetworkBackend for NetlinkBackend {
    fn namespace(&self) -> NamespaceId {
        self.namespace
    }

    fn supported_entities(&self) -> &'static [&'static str] {
        &[ETHERNET]
    }

    fn query<'a>(
        &'a self,
        selector: &'a Match,
    ) -> BackendFuture<'a, Result<Observation, BackendError>> {
        Box::pin(async move {
            let (tx, rx) = oneshot::channel();
            self.sender
                .send(Request::Query(selector.clone(), tx))
                .await
                .map_err(|_| disconnected())?;
            rx.await.map_err(|_| disconnected())?
        })
    }

    fn apply<'a>(&'a self, diff: &'a StateDiff) -> BackendFuture<'a, ApplyReport> {
        Box::pin(async move {
            let (tx, rx) = oneshot::channel();
            if self
                .sender
                .send(Request::Apply(diff.clone(), tx))
                .await
                .is_err()
            {
                return failed_report(diff, disconnected());
            }
            rx.await
                .unwrap_or_else(|_| failed_report(diff, unreported()))
        })
    }

    fn dry_run<'a>(&'a self, diff: &'a StateDiff) -> BackendFuture<'a, DryRunReport> {
        Box::pin(async move {
            let (tx, rx) = oneshot::channel();
            if self
                .sender
                .send(Request::Preview(diff.clone(), tx))
                .await
                .is_err()
            {
                return preview_report(diff, failed_report(diff, disconnected()));
            }
            rx.await
                .unwrap_or_else(|_| preview_report(diff, failed_report(diff, disconnected())))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("netfyr-{}-{name}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn only_namespace_files_are_accepted() {
        let missing = NetlinkBackend::open("/nonexistent/netfyr/ns")
            .err()
            .unwrap();
        assert_eq!(missing.kind, BackendErrorKind::NotFound);
        assert_eq!(missing.errno, Some(libc::ENOENT));

        let regular = scratch("regular");
        std::fs::write(&regular, b"").unwrap();
        let error = NetlinkBackend::open(&regular).err().unwrap();
        std::fs::remove_file(&regular).unwrap();
        assert_eq!(error.kind, BackendErrorKind::UnsupportedOperation);

        let fifo = scratch("fifo");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: c_path is a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let error = NetlinkBackend::open(&fifo).err().unwrap();
        std::fs::remove_file(&fifo).unwrap();
        assert_eq!(error.kind, BackendErrorKind::UnsupportedOperation);
    }

    #[test]
    fn enrichment_ignores_criteria_known_only_after_enrichment() {
        let spec = Match {
            name: Some("eth0".into()),
            r#type: Some(ETHERNET.into()),
            ..Match::default()
        };
        let by_driver = Match {
            name: Some("eth0".into()),
            driver: Some("veth".into()),
            pci_path: Some("0000:00:1f.6".into()),
            ..Match::default()
        };
        assert!(may_match(&by_driver, &spec));
        let other = Match {
            name: Some("eth1".into()),
            ..by_driver
        };
        assert!(!may_match(&other, &spec));
        assert!(may_match(&Match::default(), &spec));
    }
}
