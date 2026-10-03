mod app;
mod badge;
mod data_window;
mod socket;
mod tray;
mod uri;

use badge::IconState;
use data_window::{
    ContactUi, DataAction, DataFeedback, DataSnapshot, DataWindowController, DataWindowState,
    DeviceUi, StorageObjectUi, object_type_label,
};
use canopee_node::Node;
use canopee_protocol::{NodeCommand, NodeResponse};
use canopee_storage::{ExportBundle, HomeEntry, ObjectId, ObjectInfo, ObjectType};
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
use std::sync::{Arc, Mutex, MutexGuard};
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
    data_feedback_tx: tokio::sync::mpsc::Sender<DataFeedback>,
    data_state: DataWindowState,
    quit_requested: Arc<AtomicBool>,
    last_objects: usize,
    last_peers: usize,
    last_error: Option<String>,
    primed: bool,
}

impl NodeState {
    async fn new(
        report_tx: tokio::sync::mpsc::Sender<Report>,
        data_feedback_tx: tokio::sync::mpsc::Sender<DataFeedback>,
        data_state: DataWindowState,
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
                data_feedback_tx,
                data_state,
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
                data_feedback_tx,
                data_state,
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
        let mut objects: Vec<ObjectInfo> = Vec::new();
        let mut home_entries: Vec<HomeEntry> = Vec::new();

        if let Some(node) = self.node.as_ref() {
            if let Some(NodeResponse::Identity { identity_id }) =
                query(node, NodeCommand::Identity).await
            {
                self.identity = Some(identity_id.to_string());
                snap.identity = self.identity.clone();
            }
            if let Some(NodeResponse::Objects {
                objects: stored_objects,
            }) = query(node, NodeCommand::List).await
            {
                snap.objects = stored_objects.len();
                objects = stored_objects;
            }
            if let Some(NodeResponse::HomeIndex {
                index: Some(home),
            }) = query(node, NodeCommand::LoadHomeIndex).await
            {
                home_entries = home.entries;
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
            }
        }

        snap.home_entries = home_entries
            .iter()
            .map(|entry| HomeEntryUi {
                name: entry.name.clone(),
                object: entry.object.to_string(),
                shared: entry.shared,
                is_app: matches!(entry.object_type, ObjectType::AppManifest),
            })
            .collect();

        let data_active = self.data_state.visible.load(Ordering::SeqCst)
            || self.data_state.open_requested.load(Ordering::SeqCst);
        if data_active {
            let mut data = DataSnapshot {
                running: self.running,
                error: self.error.clone(),
                peers: snap.peer_ids.clone(),
                identity: self.identity.clone(),
                ..Default::default()
            };
            if let Some(node) = self.node.as_ref() {
                if let Some(NodeResponse::Profile {
                    profile: Some(profile),
                }) = query(node, NodeCommand::LoadProfile).await
                {
                    data.display_name = Some(profile.display_name);
                }
                if let Some(NodeResponse::Username { username }) =
                    query(node, NodeCommand::ShowUsername).await
                {
                    data.username = username;
                }
                if let Some(NodeResponse::ContactList { list: Some(list) }) =
                    query(node, NodeCommand::LoadContactList).await
                {
                    data.contacts_version = list.version;
                    data.contacts = list
                        .contacts
                        .into_iter()
                        .map(|contact| ContactUi {
                            name: contact.name,
                            peer_id: contact.peer_id,
                            dh_public_key: contact.dh_public_key,
                            note: contact.note.unwrap_or_default(),
                        })
                        .collect();
                }
                if let Some(NodeResponse::Device { peer_id, .. }) =
                    query(node, NodeCommand::Device).await
                {
                    data.current_device = Some(peer_id);
                }
                if let Some(NodeResponse::DeviceList { devices }) =
                    query(node, NodeCommand::DeviceList).await
                {
                    data.devices = devices
                        .into_iter()
                        .map(|device| DeviceUi {
                            current: data.current_device.as_deref() == Some(device.device_id.as_str()),
                            online: snap.peer_ids.contains(&device.device_id),
                            id: device.device_id,
                            name: device.device_name,
                        })
                        .collect();
                }
            }
            data.objects = objects
                .into_iter()
                .map(|object| {
                    let home = home_entries.iter().find(|entry| entry.object == object.id);
                    let id = object.id.to_string();
                    let name = home
                        .map(|entry| entry.name.clone())
                        .or(object.name)
                        .unwrap_or_else(|| format!("Unnamed object {}", short_id(&id)));
                    StorageObjectUi {
                        name,
                        object_type: object_type_label(object.object_type).to_string(),
                        size: object.size,
                        shared: home.is_some_and(|entry| entry.shared),
                        home_name: home.map(|entry| entry.name.clone()),
                        id,
                    }
                })
                .collect();
            data.objects
                .sort_by_key(|item| item.name.to_lowercase());
            data.contacts
                .sort_by_key(|item| item.name.to_lowercase());
            data.devices.sort_by(|left, right| {
                right
                    .current
                    .cmp(&left.current)
                    .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            });

            let mut shared_data = lock_or_poisoned(&self.data_state.snapshot);
            if *shared_data != data {
                *shared_data = data;
                self.data_state.dirty.store(true, Ordering::SeqCst);
            }
        }

        self.notify_transitions(&snap);
        let mut shared = lock_or_poisoned(snapshot);
        if *shared != snap {
            *shared = snap;
            dirty.store(true, Ordering::SeqCst);
        }
    }

    fn notify_transitions(&mut self, snap: &MenuSnapshot) {
        let notify = |title: &str, message: &str| {
            lock_or_poisoned(&self.notif_queue)
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
            Events::OpenDataWindow => self.data_state.request_open(),
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

    /// Runs a data action on its own task so a slow import, a multi-file add
    /// or a DHT round trip never stalls the tray refresh loop. The worker loop
    /// keeps ticking, so the table updates as soon as the action lands.
    fn spawn_data_action(&self, action: DataAction) {
        let Some(node) = self.node.clone() else {
            let feedback_tx = self.data_feedback_tx.clone();
            tokio::spawn(async move {
                let _ = feedback_tx
                    .send(DataFeedback::Error(
                        "Canopee node is unavailable.".to_string(),
                    ))
                    .await;
            });
            return;
        };
        let feedback_tx = self.data_feedback_tx.clone();
        tokio::spawn(async move {
            let feedback = match NodeState::perform_data_action(&node, action).await {
                Ok(feedback) => feedback,
                Err(message) => DataFeedback::Error(message),
            };
            let _ = feedback_tx.send(feedback).await;
        });
    }

    async fn perform_data_action(node: &Node, action: DataAction) -> Result<DataFeedback, String> {
        match action {
            DataAction::AddFiles(paths) => {
                if paths.is_empty() {
                    return Err("No files were selected.".to_string());
                }
                let mut stored = 0_usize;
                for path in paths {
                    let data = tokio::fs::read(&path)
                        .await
                        .map_err(|error| format!("Could not read {}: {error}", path.display()))?;
                    let name = path
                        .file_name()
                        .and_then(|value| value.to_str())
                        .map(str::to_string);
                    let response = data_query(
                        node,
                        NodeCommand::Put { data, name },
                    )
                    .await?;
                    ensure_data_success(response)?;
                    stored += 1;
                }
                Ok(DataFeedback::Success(format!(
                    "Added {stored} {}.",
                    if stored == 1 { "file" } else { "files" }
                )))
            }
            DataAction::ImportBundle(path) => {
                let bytes = tokio::fs::read(&path)
                    .await
                    .map_err(|error| format!("Could not read {}: {error}", path.display()))?;
                let bundle: ExportBundle = bincode::deserialize(&bytes)
                    .map_err(|error| format!("Invalid Canopee bundle: {error}"))?;
                let response = data_query(node, NodeCommand::Import { bundle }).await?;
                ensure_data_success(response)?;
                Ok(DataFeedback::Success("Bundle imported.".to_string()))
            }
            DataAction::Rename {
                id,
                name,
                previous_home_name,
                was_shared,
            } => {
                // The sidecar label always moves, so the object is named even
                // when it has no home entry.
                let response = data_query(
                    node,
                    NodeCommand::SetName {
                        id: id.clone(),
                        name: name.clone(),
                    },
                )
                .await?;
                ensure_data_success(response)?;

                if let Some(previous) = previous_home_name.filter(|value| *value != name) {
                    move_home_entry(node, &previous, &name, id, was_shared).await?;
                }
                Ok(DataFeedback::Success(format!("Renamed to “{name}”.")))
            }
            DataAction::Share { id, name } => {
                let response = data_query(
                    node,
                    NodeCommand::ShareObject {
                        name,
                        object: id,
                        app: None,
                        // Tray sharing has no recipient picker yet, so this
                        // shares to your own devices only — the object stays
                        // encrypted and keeps its id. Named recipients arrive
                        // with `ShareObject`'s `recipients` support.
                        recipients: vec![],
                    },
                )
                .await?;
                ensure_data_success(response)?;
                Ok(DataFeedback::Success("Object shared.".to_string()))
            }
            DataAction::Unshare { name } => {
                let response = data_query(
                    node,
                    NodeCommand::SetHomeEntryShared {
                        name,
                        shared: false,
                    },
                )
                .await?;
                ensure_data_success(response)?;
                Ok(DataFeedback::Success("Object unshared.".to_string()))
            }
            DataAction::Export { id, destination } => {
                let response = data_query(node, NodeCommand::Export { id }).await?;
                let NodeResponse::Exported { bundle } = response else {
                    return Err(data_response_error(response));
                };
                let bytes = bincode::serialize(&bundle)
                    .map_err(|error| format!("Could not serialize export: {error}"))?;
                tokio::fs::write(&destination, bytes)
                    .await
                    .map_err(|error| {
                        format!("Could not write {}: {error}", destination.display())
                    })?;
                Ok(DataFeedback::Success(format!(
                    "Exported to {}.",
                    destination.display()
                )))
            }
            DataAction::Delete { id } => {
                let response = data_query(node, NodeCommand::DeleteObject { id }).await?;
                ensure_data_success(response)?;
                Ok(DataFeedback::Success("Object deleted.".to_string()))
            }
            DataAction::SaveContacts(list) => {
                let response = data_query(node, NodeCommand::SaveContactList { list }).await?;
                ensure_data_success(response)?;
                Ok(DataFeedback::Success("Contacts saved.".to_string()))
            }
            DataAction::ResolveProfile(owner) => {
                let response = data_query(
                    node,
                    NodeCommand::ResolveProfile {
                        owner: owner.clone(),
                    },
                )
                .await?;
                let NodeResponse::Profile {
                    profile: Some(profile),
                } = response
                else {
                    return Err(if let NodeResponse::Error { message } = response {
                        message
                    } else {
                        "No profile was found for this contact.".to_string()
                    });
                };
                let mut summary = if profile.display_name.trim().is_empty() {
                    "Unnamed profile".to_string()
                } else {
                    profile.display_name
                };
                summary.push_str(&format!(" · v{}", profile.version));
                if profile.avatar.is_some() {
                    summary.push_str(" · avatar");
                }
                Ok(DataFeedback::ProfileResolved { owner, summary })
            }
            DataAction::SyncDevices => {
                let response = data_query(node, NodeCommand::SyncDeviceList).await?;
                ensure_data_success(response)?;
                Ok(DataFeedback::Success("Device list synced.".to_string()))
            }
            DataAction::RemoveDevice(device_id) => {
                let response = data_query(
                    node,
                    NodeCommand::RemoveDevice {
                        device_id,
                    },
                )
                .await?;
                ensure_data_success(response)?;
                Ok(DataFeedback::Success("Device removed.".to_string()))
            }
            DataAction::SaveCopy { id, destination } => {
                let bytes = fetch_object_bytes(node, id).await?;
                tokio::fs::write(&destination, &bytes)
                    .await
                    .map_err(|error| format!("Could not write {}: {error}", destination.display()))?;
                Ok(DataFeedback::Success(format!(
                    "Saved a copy to {}.",
                    destination.display()
                )))
            }
            DataAction::OpenObject { id, name } => {
                let bytes = fetch_object_bytes(node, id).await?;
                let path = std::env::temp_dir().join("canopee-open").join(&name);
                if let Some(parent) = path.parent() {
                    tokio::fs::create_dir_all(parent).await.map_err(|error| {
                        format!("Could not prepare {}: {error}", parent.display())
                    })?;
                }
                tokio::fs::write(&path, &bytes)
                    .await
                    .map_err(|error| format!("Could not write {}: {error}", path.display()))?;
                open_path(&path);
                Ok(DataFeedback::Success(format!("Opened “{name}”.")))
            }
            DataAction::SaveProfile { display_name } => {
                // The profile is replaced wholesale, so the existing key
                // material has to be carried over or contacts lose the ability
                // to derive a shared secret with this identity.
                let existing = match data_query(node, NodeCommand::LoadProfile).await? {
                    NodeResponse::Profile { profile } => profile,
                    _ => None,
                };
                let profile = canopee_storage::Profile {
                    display_name,
                    dh_public_key: existing
                        .map(|profile| profile.dh_public_key)
                        .unwrap_or([0_u8; 32]),
                    avatar: None,
                    version: 0,
                };
                let response = data_query(node, NodeCommand::SaveProfile { profile }).await?;
                ensure_data_success(response)?;
                Ok(DataFeedback::Success("Profile saved.".to_string()))
            }
            DataAction::ClaimUsername(username) => {
                let claimed = username.clone();
                let response = data_query(node, NodeCommand::ClaimUsername { username }).await?;
                ensure_data_success(response)?;
                Ok(DataFeedback::Success(format!(
                    "Claimed username “{claimed}”."
                )))
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

async fn data_query(node: &Node, command: NodeCommand) -> Result<NodeResponse, String> {
    tokio::time::timeout(Duration::from_secs(30), node.handle(command))
        .await
        .map_err(|_| "The data action timed out.".to_string())
}

/// Pulls an object's original bytes back out of the store so the window can
/// write a real file. Fetched via the node (not the disk) so signature
/// verification still runs.
async fn fetch_object_bytes(node: &Node, id: ObjectId) -> Result<Vec<u8>, String> {
    let response = data_query(node, NodeCommand::Get { id }).await?;
    match response {
        NodeResponse::Object { object } => Ok(object.payload.data),
        other => Err(data_response_error(other)),
    }
}

/// Moves a home entry from `old_name` to `new_name`, keeping the object, its
/// type and its app flag. A home entry's name is the network-visible handle
/// (`entry:<name>`), so a rename that skipped this would leave the object
/// resolvable only under its old name.
async fn move_home_entry(
    node: &Node,
    old_name: &str,
    new_name: &str,
    id: ObjectId,
    shared: bool,
) -> Result<(), String> {
    let index = match data_query(node, NodeCommand::LoadHomeIndex).await? {
        NodeResponse::HomeIndex { index } => index,
        _ => None,
    };
    let Some(mut index) = index else {
        return Ok(());
    };
    let Some(mut entry) = index.entries.iter().find(|e| e.name == old_name).cloned() else {
        return Ok(());
    };

    entry.name = new_name.to_string();
    entry.object = id.clone();
    // Drop both the old entry and anything already squatting the new name, so
    // the index cannot end up with two entries pointing at one object.
    index.entries.retain(|e| e.name != old_name && e.name != new_name);
    let app = entry.app.clone();
    index.entries.push(entry);
    let response = data_query(node, NodeCommand::SaveHomeIndex { index }).await?;
    ensure_data_success(response)?;

    if shared {
        // Republish so peers resolve the object under its new name.
        let response = data_query(
            node,
            NodeCommand::ShareObject {
                name: new_name.to_string(),
                object: id,
                app,
                recipients: vec![],
            },
        )
        .await?;
        ensure_data_success(response)?;
    }
    Ok(())
}

fn open_path(path: &std::path::Path) {
    if let Err(error) = Command::new("open").arg(path).spawn() {
        eprintln!("canopee: could not open {}: {error}", path.display());
    }
}

fn ensure_data_success(response: NodeResponse) -> Result<(), String> {
    match response {
        NodeResponse::Error { message } => Err(message),
        _ => Ok(()),
    }
}

fn data_response_error(response: NodeResponse) -> String {
    match response {
        NodeResponse::Error { message } => message,
        _ => "The node returned an unexpected response.".to_string(),
    }
}

fn short_id(id: &str) -> String {
    id.chars().take(10).collect()
}

pub(crate) fn copy_to_clipboard(text: &str) {
    if let Ok(mut child) = Command::new("pbcopy").stdin(Stdio::piped()).spawn() {
        if let Some(stdin) = child.stdin.as_mut() {
            let _ = stdin.write_all(text.as_bytes());
        }
        let _ = child.wait();
    }
}

fn lock_or_poisoned<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn install_panic_hook() {
    let log_path = dirs::home_dir().unwrap().join(".canopee/tray-crash.log");
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let location = info
            .location()
            .map(|l| l.to_string())
            .unwrap_or_else(|| "unknown location".to_string());
        let message = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            format!("{:?}", info.payload())
        };
        let report = format!("panic in {:?} at {location}: {message}", thread.name());
        eprintln!("canopee: {report}");
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
        {
            let _ = writeln!(file, "{report}");
        }
    }));
}

pub struct TrayUpdaterIvars {
    tray: Arc<Mutex<TrayIcon<Events>>>,
    snapshot: Arc<Mutex<MenuSnapshot>>,
    dirty: Arc<AtomicBool>,
    notif_queue: NotifQueue,
    quit_requested: Arc<AtomicBool>,
    data_window: Retained<DataWindowController>,
    data_feedback_rx: Mutex<tokio::sync::mpsc::Receiver<DataFeedback>>,
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
                lock_or_poisoned(&ivars.notif_queue).drain(..).collect();
            for (title, message) in pending {
                deliver_notification(&title, &message);
            }

            let mut data_feedback = Vec::new();
            while let Ok(item) = lock_or_poisoned(&ivars.data_feedback_rx).try_recv() {
                data_feedback.push(item);
            }
            ivars.data_window.sync(data_feedback);

            if ivars.dirty.swap(false, Ordering::SeqCst) {
                let snapshot = lock_or_poisoned(&ivars.snapshot).clone();
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
    let center: Option<Retained<NSUserNotificationCenter>> = unsafe {
        msg_send![class!(NSUserNotificationCenter), defaultUserNotificationCenter]
    };
    let Some(center) = center else {
        eprintln!("canopee: notification dropped — no NSUserNotificationCenter");
        return;
    };
    let alloc: objc2::rc::Allocated<NSUserNotification> =
        unsafe { msg_send![class!(NSUserNotification), alloc] };
    let notif: Retained<NSUserNotification> = NSUserNotification::init(alloc);
    let title = NSString::from_str(title);
    let message = NSString::from_str(message);
    notif.setTitle(Some(&title));
    notif.setInformativeText(Some(&message));
    center.deliverNotification(&notif);
    eprintln!("canopee: notification: {title}: {message}");
}

#[tokio::main]
async fn main() {
    install_panic_hook();
    let mtm = MainThreadMarker::new().expect("Must run on main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let snapshot = Arc::new(Mutex::new(MenuSnapshot::default()));
    let dirty = Arc::new(AtomicBool::new(true));
    let data_state = DataWindowState::new();
    let (data_action_tx, mut data_action_rx) = tokio::sync::mpsc::channel::<DataAction>(128);
    let (data_feedback_tx, data_feedback_rx) = tokio::sync::mpsc::channel::<DataFeedback>(64);
    let data_window = DataWindowController::new(mtm, data_state.clone(), data_action_tx);
    if std::env::args_os().any(|arg| arg == "--data") {
        data_state.request_open();
    }

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
            data_window,
            data_feedback_rx: Mutex::new(data_feedback_rx),
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
    let worker_data_state = data_state.clone();
    let (report_tx, mut report_rx) = tokio::sync::mpsc::channel::<Report>(16);
    tokio::spawn(async move {
        let mut node_state = NodeState::new(
            report_tx,
            data_feedback_tx,
            worker_data_state,
            notif_queue,
            quit_requested.clone(),
        )
        .await;
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
                maybe = data_action_rx.recv() => {
                    let Some(action) = maybe else {
                        break;
                    };
                    node_state.spawn_data_action(action);
                    node_state.refresh(&worker_snapshot, &worker_dirty).await;
                }
            }
        }
    });

    app.run();
}