mod app;
mod badge;
mod socket;
mod tray;
mod uri;

use badge::IconState;
use canopee_node::Node;
use canopee_protocol::{NodeCommand, NodeResponse};
use canopee_storage::ObjectType;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{
    class, define_class, msg_send, DeclaredClass, MainThreadMarker, MainThreadOnly,
};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate,
};
#[allow(deprecated)]
use objc2_foundation::{
    NSArray, NSObject, NSObjectProtocol, NSString, NSTimer, NSURL, NSUserNotification,
    NSUserNotificationCenter,
};
use std::cell::Cell;
use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;
use tray::{Events, HomeEntryUi, MenuSnapshot, build_menu, plural};
use trayicon::{Icon, TrayIcon, TrayIconBuilder};

const ICON_B: &[u8] = include_bytes!("./icons/constellation_24_b.png");

enum Report {
    Error(String),
    Opened,
}

type NotifQueue = Arc<Mutex<VecDeque<(String, String)>>>;

struct NodeState {
    root_path: PathBuf,
    node: Option<Node>,
    node_task: Option<JoinHandle<anyhow::Result<()>>>,
    identity: Option<String>,
    running: bool,
    error: Option<String>,
    notif_queue: NotifQueue,
    report_tx: tokio::sync::mpsc::Sender<Report>,
    quit_requested: Arc<AtomicBool>,
    last_objects: usize,
    last_peers: usize,
    last_error: Option<String>,
    primed: bool,
}

impl NodeState {
    async fn new(
        report_tx: tokio::sync::mpsc::Sender<Report>,
        notif_queue: NotifQueue,
        quit_requested: Arc<AtomicBool>,
    ) -> Self {
        let root_path = dirs::home_dir().unwrap().join(".canopee");
        match Node::open().await {
            Ok(node) => Self {
                root_path,
                node: Some(node),
                node_task: None,
                identity: None,
                running: false,
                error: None,
                notif_queue,
                report_tx,
                quit_requested,
                last_objects: 0,
                last_peers: 0,
                last_error: None,
                primed: false,
            },
            Err(e) => Self {
                root_path,
                node: None,
                node_task: None,
                identity: None,
                running: false,
                error: Some(format!("failed to open node: {e}")),
                notif_queue,
                report_tx,
                quit_requested,
                last_objects: 0,
                last_peers: 0,
                last_error: Some(format!("failed to open node: {e}")),
                primed: false,
            },
        }
    }

    fn socket_path(&self) -> PathBuf {
        self.root_path.join("node.sock")
    }

    async fn start(&mut self) {
        if self.running {
            return;
        }
        let socket = self.socket_path();
        if socket::socket_connectable(&socket).await {
            self.running = true;
            return;
        }
        let Some(node) = self.node.as_ref().cloned() else {
            self.error = Some("cannot start node — failed to open at launch".to_string());
            return;
        };
        self.error = None;
        let task = tokio::spawn(async move { node.run().await });
        self.node_task = Some(task);
        self.running = true;
    }

    async fn stop(&mut self) {
        if !self.running {
            self.node_task = None;
            return;
        }
        self.running = false;
        self.last_peers = 0;
        if let Some(task) = self.node_task.take() {
            self.shutdown_embedded().await;
            let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
        }
    }

    async fn shutdown_embedded(&mut self) {
        let socket = self.socket_path();
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut ready = socket::socket_connectable(&socket).await;
        while !ready && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
            ready = socket::socket_connectable(&socket).await;
        }
        if ready {
            let _ = socket::send_command(&socket, NodeCommand::Shutdown).await;
        }
    }

    async fn check_node(&mut self) {
        if let Some(task) = self.node_task.take() {
            if task.is_finished() {
                match task.await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => self.error = Some(e.to_string()),
                    Err(_) => self.error = Some("node task panicked".to_string()),
                }
                self.running = false;
            } else {
                self.node_task = Some(task);
            }
        }
    }

    async fn refresh(&mut self, snapshot: &Arc<Mutex<MenuSnapshot>>, dirty: &Arc<AtomicBool>) {
        self.check_node().await;

        let mut snap = MenuSnapshot {
            running: self.running,
            error: self.error.clone(),
            identity: self.identity.clone(),
            ..Default::default()
        };

        if let Some(node) = self.node.as_ref() {
            if let Some(NodeResponse::Identity { identity_id }) =
                query(node, NodeCommand::Identity).await
            {
                self.identity = Some(identity_id.to_string());
                snap.identity = self.identity.clone();
            }
            if let Some(NodeResponse::Objects { objects }) = query(node, NodeCommand::List).await {
                snap.objects = objects.len();
            }

            if self.running {
                if let Some(NodeResponse::Peers { peers }) = query(node, NodeCommand::Peers).await
                {
                    snap.peers = peers.len();
                    snap.peer_ids = peers.iter().map(|p| p.peer_id.clone()).collect();
                }
                if let Some(NodeResponse::RelayReservations { reservations }) =
                    query(node, NodeCommand::RelayReservations).await
                {
                    snap.relays = reservations.len();
                }
                if let Some(NodeResponse::HomeIndex {
                    index: Some(home),
                }) = query(node, NodeCommand::LoadHomeIndex).await
                {
                    snap.home_entries = home
                        .entries
                        .into_iter()
                        .map(|e| HomeEntryUi {
                            name: e.name.clone(),
                            object: e.object.to_string(),
                            shared: e.shared,
                            is_app: matches!(e.object_type, ObjectType::AppManifest),
                        })
                        .collect();
                }
            }
        }

        self.notify_transitions(&snap);
        let mut shared = snapshot.lock().unwrap();
        if *shared != snap {
            *shared = snap;
            dirty.store(true, Ordering::SeqCst);
        }
    }

    fn notify_transitions(&mut self, snap: &MenuSnapshot) {
        let notify = |title: &str, message: &str| {
            self.notif_queue
                .lock()
                .unwrap()
                .push_back((title.to_string(), message.to_string()));
        };
        if self.primed {
            if self.running {
                if snap.objects > self.last_objects {
                    notify(
                        "Canopee",
                        &format!("New object stored — {} total", snap.objects),
                    );
                }
                if snap.peers > self.last_peers {
                    notify("Canopee", &format!("{} connected", snap.peers));
                }
            }
            if snap.error.is_some() && self.last_error.is_none() {
                notify(
                    "Canopee error",
                    snap.error.as_deref().unwrap_or("unknown error"),
                );
            }
        }
        self.last_objects = snap.objects;
        self.last_peers = snap.peers;
        self.last_error = snap.error.clone();
        self.primed = true;
    }

    async fn handle(&mut self, event: &Events) {
        match event {
            Events::Noop => {}
            Events::CopyIdentity => {
                if let Some(id) = &self.identity {
                    copy_to_clipboard(id);
                }
            }
            Events::OpenApp { name } => {
                let Some(identity) = self.identity.clone() else {
                    self.error = Some("identity unknown — cannot open app".to_string());
                    return;
                };
                self.spawn_open(identity.as_str(), name.as_str()).await;
            }
            Events::OpenUrl { url } => {
                eprintln!("canopee: received URL {url}");
                match uri::parse(url) {
                    Ok((owner, name)) => match uri::resolve_owner(&owner) {
                        Ok(owner) => self.spawn_open(&owner, &name).await,
                        Err(e) => self.error = Some(e.to_string()),
                    },
                    Err(e) => self.error = Some(format!("bad link: {e}")),
                }
            }
            Events::ToggleShareHomeEntry { name, shared } => {
                let target_shared = !shared;
                let Some(node) = self.node.as_ref().cloned() else {
                    return;
                };
                let response = node
                    .handle(NodeCommand::SetHomeEntryShared {
                        name: name.clone(),
                        shared: target_shared,
                    })
                    .await;
                self.error = match response {
                    NodeResponse::Error { message } => Some(message),
                    _ => None,
                };
            }
            Events::CopyObjectId { id } => copy_to_clipboard(id),
            Events::CopyPeerId { id } => copy_to_clipboard(id),
            Events::StartNode => self.start().await,
            Events::StopNode => self.stop().await,
            Events::RestartNode => {
                self.stop().await;
                self.start().await;
            }
            Events::Quit => {
                self.stop().await;
                self.quit_requested.store(true, Ordering::SeqCst);
            }
        }
    }
    /// Opens a canopee app (`owner`/`name`) on a detached task so a slow DHT
    /// resolve or network fetch never stalls the tray's refresh loop.
    async fn spawn_open(&mut self, owner: &str, name: &str) {
        if !self.running {
            self.start().await;
        }
        let Some(node) = self.node.as_ref().cloned() else {
            self.error = Some("node unavailable".to_string());
            return;
        };
        let self_identity = self.identity.clone().unwrap_or_default();
        let owner = owner.to_string();
        let name = name.to_string();
        let report_tx = self.report_tx.clone();
        tokio::spawn(async move {
            let result = tokio::time::timeout(
                Duration::from_secs(60),
                app::open_app(&node, &owner, &name, &self_identity),
            )
            .await;
            match result {
                Ok(Ok(url)) => {
                    eprintln!("canopee: serving {owner}/{name} at {url}");
                    let _ = Command::new("open").arg(&url).spawn();
                    let _ = report_tx.send(Report::Opened).await;
                }
                Ok(Err(e)) => {
                    eprintln!("canopee: failed to open {owner}/{name}: {e}");
                    let _ = report_tx
                        .send(Report::Error(format!("could not open {owner}/{name}: {e}")))
                        .await;
                }
                Err(_) => {
                    eprintln!("canopee: timed out opening {owner}/{name}");
                    let _ = report_tx
                        .send(Report::Error(format!("timed out opening {owner}/{name}")))
                        .await;
                }
            }
        });
    }
}

async fn query(node: &Node, command: NodeCommand) -> Option<NodeResponse> {
    tokio::time::timeout(Duration::from_secs(4), node.handle(command))
        .await
        .ok()
}

fn copy_to_clipboard(text: &str) {
    if let Ok(mut child) = Command::new("pbcopy").stdin(Stdio::piped()).spawn() {
        if let Some(stdin) = child.stdin.as_mut() {
            let _ = stdin.write_all(text.as_bytes());
        }
        let _ = child.wait();
    }
}

pub struct TrayUpdaterIvars {
    tray: Arc<Mutex<TrayIcon<Events>>>,
    snapshot: Arc<Mutex<MenuSnapshot>>,
    dirty: Arc<AtomicBool>,
    notif_queue: NotifQueue,
    quit_requested: Arc<AtomicBool>,
    icons: [Icon; 3],
    icon_state: Cell<IconState>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = TrayUpdaterIvars]
    #[derive(PartialEq, Eq, Hash)]
    pub struct TrayUpdater;

    impl TrayUpdater {
        #[unsafe(method(refreshTray:))]
        fn refresh_tray(&self, _timer: &NSTimer) {
            let ivars = self.ivars();
            if ivars.quit_requested.swap(false, Ordering::SeqCst) {
                let mtm = MainThreadMarker::new().unwrap();
                let app = NSApplication::sharedApplication(mtm);
                app.terminate(None::<&AnyObject>);
                return;
            }

            let pending: Vec<(String, String)> =
                ivars.notif_queue.lock().unwrap().drain(..).collect();
            for (title, message) in pending {
                deliver_notification(&title, &message);
            }

            if ivars.dirty.swap(false, Ordering::SeqCst) {
                let snapshot = ivars.snapshot.lock().unwrap().clone();
                let mut tray = ivars.tray.lock().unwrap();
                let menu = build_menu(&snapshot);
                let _ = tray.set_menu(&menu);
                let _ = tray.set_tooltip(&trayline(&snapshot));

                let state = if snapshot.error.is_some() {
                    IconState::Error
                } else if snapshot.running {
                    IconState::Running
                } else {
                    IconState::Stopped
                };
                if ivars.icon_state.get() != state {
                    ivars.icon_state.set(state);
                    let _ = tray.set_icon(&ivars.icons[state as usize]);
                }
            }
        }
    }
);

impl TrayUpdater {
    fn new(mtm: MainThreadMarker, ivars: TrayUpdaterIvars) -> Retained<Self> {
        let this = mtm.alloc().set_ivars(ivars);
        unsafe { msg_send![super(this), init] }
    }
}

pub struct AppDelegateIvars {
    tx: tokio::sync::mpsc::Sender<Events>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = AppDelegateIvars]
    #[derive(PartialEq, Eq, Hash)]
    pub struct AppDelegate;

    unsafe impl NSObjectProtocol for AppDelegate {}

    unsafe impl NSApplicationDelegate for AppDelegate {
        #[unsafe(method(application:openURLs:))]
        fn open_urls(&self, _app: &NSApplication, urls: &NSArray<NSURL>) {
            for i in 0..urls.count() {
                let url = urls.objectAtIndex(i);
                let Some(absolute) = url.absoluteString() else {
                    continue;
                };
                let tx = self.ivars().tx.clone();
                let _ = tx.try_send(Events::OpenUrl {
                    url: absolute.to_string(),
                });
            }
        }
    }
);

impl AppDelegate {
    fn new(mtm: MainThreadMarker, ivars: AppDelegateIvars) -> Retained<Self> {
        let this = mtm.alloc().set_ivars(ivars);
        unsafe { msg_send![super(this), init] }
    }
}

fn trayline(s: &MenuSnapshot) -> String {
    if let Some(_err) = &s.error {
        return "Canopee — error".to_string();
    }
    if s.running {
        format!(
            "Canopee — {} · {}",
            plural(s.objects, "object", "objects"),
            plural(s.peers, "peer", "peers"),
        )
    } else {
        "Canopee — stopped".to_string()
    }
}

#[allow(deprecated)]
fn deliver_notification(title: &str, message: &str) {
    let alloc: objc2::rc::Allocated<NSUserNotification> =
        unsafe { msg_send![class!(NSUserNotification), alloc] };
    let notif: Retained<NSUserNotification> = NSUserNotification::init(alloc);
    let title = NSString::from_str(title);
    let message = NSString::from_str(message);
    notif.setTitle(Some(&title));
    notif.setInformativeText(Some(&message));
    let center = NSUserNotificationCenter::defaultUserNotificationCenter();
    center.deliverNotification(&notif);
    eprintln!("canopee: notification: {title}: {message}");
}

#[tokio::main]
async fn main() {
    let mtm = MainThreadMarker::new().expect("Must run on main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let snapshot = Arc::new(Mutex::new(MenuSnapshot::default()));
    let dirty = Arc::new(AtomicBool::new(true));

    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel::<Events>(256);

    let delegate = AppDelegate::new(
        mtm,
        AppDelegateIvars { tx: event_tx.clone() },
    );
    app.setDelegate(Some(objc2::runtime::ProtocolObject::from_ref(&*delegate)));

    let mut icon = Icon::from_buffer(ICON_B, None, None).expect("failed to load icon");
    icon.set_template(true);
    let tray_icon = TrayIconBuilder::new()
        .sender(move |event: &Events| {
            let _ = event_tx.try_send(event.clone());
        })
        .icon(icon)
        .tooltip("Canopee")
        .menu(build_menu(&snapshot.lock().unwrap().clone()))
        .build()
        .expect("Failed to create tray icon");

    let tray_arc = Arc::new(Mutex::new(tray_icon));

    let notif_queue: NotifQueue = Arc::new(Mutex::new(VecDeque::new()));
    let quit_requested = Arc::new(AtomicBool::new(false));

    let updater = TrayUpdater::new(
        mtm,
        TrayUpdaterIvars {
            tray: tray_arc.clone(),
            snapshot: snapshot.clone(),
            dirty: dirty.clone(),
            notif_queue: notif_queue.clone(),
            quit_requested: quit_requested.clone(),
            icons: badge::icons_for_states(ICON_B),
            icon_state: Cell::new(IconState::Stopped),
        },
    );
    let _timer: Retained<NSTimer> = unsafe {
        msg_send![
            class!(NSTimer),
            scheduledTimerWithTimeInterval: 1.0,
            target: &*updater,
            selector: Sel::register(c"refreshTray:"),
            userInfo: None::<&AnyObject>,
            repeats: true
        ]
    };
    let _delegate = delegate;

    let worker_snapshot = snapshot.clone();
    let worker_dirty = dirty.clone();
    let (report_tx, mut report_rx) = tokio::sync::mpsc::channel::<Report>(16);
    tokio::spawn(async move {
        let mut node_state =
            NodeState::new(report_tx, notif_queue, quit_requested.clone()).await;
        node_state.start().await;
        let mut ticker = tokio::time::interval(Duration::from_secs(2));
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    node_state.refresh(&worker_snapshot, &worker_dirty).await;
                }
                maybe = event_rx.recv() => {
                    match maybe {
                        Some(event) => {
                            node_state.handle(&event).await;
                            node_state.refresh(&worker_snapshot, &worker_dirty).await;
                        }
                        None => break,
                    }
                }
                maybe = report_rx.recv() => {
                    match maybe {
                        Some(Report::Error(message)) => node_state.error = Some(message),
                        Some(Report::Opened) => node_state.error = None,
                        None => break,
                    }
                    node_state.refresh(&worker_snapshot, &worker_dirty).await;
                }
            }
        }
    });

    app.run();
}