use canopee_identity::IdentityId;
use canopee_storage::{Contact, ContactList, ObjectId, ObjectType};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSApplication, NSAutoresizingMaskOptions,
    NSBackingStoreType, NSButton, NSColor, NSFont, NSModalResponseOK, NSOpenPanel, NSSavePanel,
    NSScrollView, NSSearchField, NSTabView, NSTabViewItem, NSTableColumn,
    NSTableColumnResizingOptions, NSTableView, NSTableViewDataSource, NSTextAlignment, NSTextField,
    NSView, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::CStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::mpsc::Sender;

#[derive(Clone)]
pub(crate) struct DataWindowState {
    pub open_requested: Arc<AtomicBool>,
    pub visible: Arc<AtomicBool>,
    pub dirty: Arc<AtomicBool>,
    pub snapshot: Arc<Mutex<DataSnapshot>>,
}

impl DataWindowState {
    pub(crate) fn new() -> Self {
        Self {
            open_requested: Arc::new(AtomicBool::new(false)),
            visible: Arc::new(AtomicBool::new(false)),
            dirty: Arc::new(AtomicBool::new(true)),
            snapshot: Arc::new(Mutex::new(DataSnapshot::default())),
        }
    }

    pub(crate) fn request_open(&self) {
        self.open_requested.store(true, Ordering::SeqCst);
    }
}

impl Default for DataWindowState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct DataSnapshot {
    pub running: bool,
    pub error: Option<String>,
    pub current_device: Option<String>,
    pub identity: Option<String>,
    pub display_name: Option<String>,
    pub username: Option<String>,
    pub objects: Vec<StorageObjectUi>,
    pub contacts: Vec<ContactUi>,
    pub contacts_version: u64,
    pub devices: Vec<DeviceUi>,
    pub peers: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StorageObjectUi {
    pub id: String,
    pub name: String,
    pub object_type: String,
    pub size: u64,
    pub shared: bool,
    pub home_name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ContactUi {
    pub name: String,
    pub peer_id: String,
    pub dh_public_key: [u8; 32],
    pub note: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DeviceUi {
    pub id: String,
    pub name: String,
    pub current: bool,
    pub online: bool,
}

pub(crate) enum DataAction {
    AddFiles(Vec<PathBuf>),
    ImportBundle(PathBuf),
    Rename {
        id: ObjectId,
        name: String,
        /// The home entry the object currently sits under, if any. The
        /// visible name of a shared object is its home entry, so a rename has
        /// to move that entry too or the old label sticks.
        previous_home_name: Option<String>,
        /// Whether the entry is currently network-visible, so a rename keeps
        /// it shared (and republishes the pointer) instead of silently
        /// dropping it off the network.
        was_shared: bool,
    },
    Share { id: ObjectId, name: String },
    Unshare { name: String },
    Export { id: ObjectId, destination: PathBuf },
    /// Writes an object's original bytes to a user-chosen path, so a stored
    /// file can be recovered without the CLI.
    SaveCopy { id: ObjectId, destination: PathBuf },
    /// Writes an object's original bytes to a temp file and hands it to the
    /// system's default application.
    OpenObject { id: ObjectId, name: String },
    Delete { id: ObjectId },
    SaveContacts(ContactList),
    ResolveProfile(IdentityId),
    SyncDevices,
    RemoveDevice(String),
    SaveProfile { display_name: String },
    ClaimUsername(String),
}

pub(crate) enum DataFeedback {
    Success(String),
    Error(String),
    ProfileResolved { owner: IdentityId, summary: String },
}

#[derive(Default)]
struct DataUiState {
    snapshot: DataSnapshot,
    storage_rows: Vec<StorageObjectUi>,
    visible_storage: Vec<StorageObjectUi>,
    contact_rows: Vec<ContactUi>,
    visible_contacts: Vec<ContactUi>,
    device_rows: Vec<DeviceUi>,
    visible_devices: Vec<DeviceUi>,
    profiles: HashMap<String, String>,
}

pub struct DataWindowIvars {
    state: DataWindowState,
    action_tx: Sender<DataAction>,
    window: Retained<NSWindow>,
    identity_name: Retained<NSTextField>,
    identity_meta: Retained<NSTextField>,
    tab_views: [Retained<NSView>; 3],
    search_fields: [Retained<NSSearchField>; 3],
    tables: [Retained<NSTableView>; 3],
    scroll_views: [Retained<NSScrollView>; 3],
    empty_labels: [Retained<NSTextField>; 3],
    status_label: Retained<NSTextField>,
    ui: RefCell<DataUiState>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = DataWindowIvars]
    pub struct DataWindowController;

    unsafe impl NSObjectProtocol for DataWindowController {}

    impl DataWindowController {
        #[unsafe(method(searchChanged:))]
        fn search_changed(&self, _sender: &AnyObject) {
            self.refresh_rows();
        }

        #[unsafe(method(addFiles:))]
        fn add_files(&self, _sender: &AnyObject) {
            let Some(paths) = self.choose_files("Add files", true) else {
                return;
            };
            if paths.is_empty() {
                return;
            }
            self.send_action(DataAction::AddFiles(paths), "Adding files…");
        }

        #[unsafe(method(importBundle:))]
        fn import_bundle(&self, _sender: &AnyObject) {
            let Some(paths) = self.choose_files("Import Canopee bundle", false) else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            self.send_action(DataAction::ImportBundle(path), "Importing bundle…");
        }

        #[unsafe(method(renameObject:))]
        fn rename_object(&self, _sender: &AnyObject) {
            let Some(object) = self.selected_storage() else {
                self.require_selection("Select an object to rename.");
                return;
            };
            let Some(name) = self.prompt_text("Rename object", "Object name", &object.name) else {
                return;
            };
            let name = name.trim();
            if name.is_empty() {
                self.show_error("The object name cannot be empty.");
                return;
            }
            self.send_action(
                DataAction::Rename {
                    id: ObjectId::new(&object.id),
                    name: name.to_string(),
                    previous_home_name: object.home_name.clone(),
                    was_shared: object.shared,
                },
                "Renaming object…",
            );
        }

        #[unsafe(method(toggleObjectSharing:))]
        fn toggle_object_sharing(&self, _sender: &AnyObject) {
            let Some(object) = self.selected_storage() else {
                self.require_selection("Select an object to share or unshare.");
                return;
            };
            if object.shared {
                let name = object
                    .home_name
                    .clone()
                    .unwrap_or_else(|| object.name.clone());
                self.send_action(DataAction::Unshare { name }, "Unsharing object…");
                return;
            }
            let default_name = object.home_name.clone().unwrap_or_else(|| {
                Path::new(&object.name)
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .unwrap_or(&object.name)
                    .to_string()
            });
            let Some(name) = self.prompt_text("Share object", "Home name", &default_name) else {
                return;
            };
            let name = name.trim();
            if name.is_empty() {
                self.show_error("The home name cannot be empty.");
                return;
            }
            self.send_action(
                DataAction::Share {
                    id: ObjectId::new(&object.id),
                    name: name.to_string(),
                },
                "Sharing object…",
            );
        }

        #[unsafe(method(exportObject:))]
        fn export_object(&self, _sender: &AnyObject) {
            let Some(object) = self.selected_storage() else {
                self.require_selection("Select an object to export.");
                return;
            };
            let Some(path) = self.choose_export_path(&object.name) else {
                return;
            };
            self.send_action(
                DataAction::Export {
                    id: ObjectId::new(&object.id),
                    destination: path,
                },
                "Exporting object…",
            );
        }

        #[unsafe(method(saveCopy:))]
        fn save_copy(&self, _sender: &AnyObject) {
            let Some(object) = self.selected_storage() else {
                self.require_selection("Select an object to save a copy of.");
                return;
            };
            let Some(path) = self.choose_save_copy_path(&object.name, &object.id) else {
                return;
            };
            self.send_action(
                DataAction::SaveCopy {
                    id: ObjectId::new(&object.id),
                    destination: path,
                },
                "Saving a copy…",
            );
        }

        #[unsafe(method(openObject:))]
        fn open_object(&self, _sender: &AnyObject) {
            self.open_selected_object();
        }

        /// Double-clicking a row is the standard "open it" gesture, so route
        /// it to the same action as the Open button.
        #[unsafe(method(activateRow:))]
        fn activate_row(&self, sender: &AnyObject) {
            let table: &NSTableView =
                unsafe { &*(sender as *const AnyObject as *const NSTableView) };
            if self.table_index(table) == Some(0) {
                self.open_selected_object();
            }
        }

        #[unsafe(method(editProfile:))]
        fn edit_profile(&self, _sender: &AnyObject) {
            let current = self.snapshot().display_name.unwrap_or_default();
            let Some(name) = self.prompt_text("Your display name", "Name shown to contacts", &current)
            else {
                return;
            };
            let name = name.trim();
            if name.is_empty() {
                self.show_error("The display name cannot be empty.");
                return;
            }
            self.send_action(
                DataAction::SaveProfile {
                    display_name: name.to_string(),
                },
                "Saving profile…",
            );
        }

        #[unsafe(method(claimUsername:))]
        fn claim_username(&self, _sender: &AnyObject) {
            if !self.snapshot().running {
                self.show_error("Start Canopee before claiming a username.");
                return;
            }
            let current = self.snapshot().username.unwrap_or_default();
            let Some(name) =
                self.prompt_text("Claim a username", "Username", &current)
            else {
                return;
            };
            let name = name.trim();
            if name.is_empty() {
                self.show_error("The username cannot be empty.");
                return;
            }
            self.send_action(DataAction::ClaimUsername(name.to_string()), "Claiming username…");
        }

        #[unsafe(method(copyObjectId:))]
        fn copy_object_id(&self, _sender: &AnyObject) {
            let Some(object) = self.selected_storage() else {
                self.require_selection("Select an object before copying its ID.");
                return;
            };
            crate::copy_to_clipboard(&object.id);
            self.set_status("Object ID copied.");
        }

        #[unsafe(method(deleteObject:))]
        fn delete_object(&self, _sender: &AnyObject) {
            let Some(object) = self.selected_storage() else {
                self.require_selection("Select an object to delete.");
                return;
            };
            if !self.confirm(
                "Delete object?",
                &format!(
                    "“{}” and its home entry will be removed from this device.",
                    object.name
                ),
                "Delete",
            ) {
                return;
            }
            self.send_action(
                DataAction::Delete {
                    id: ObjectId::new(&object.id),
                },
                "Deleting object…",
            );
        }

        #[unsafe(method(addContact:))]
        fn add_contact(&self, _sender: &AnyObject) {
            if let Some(contact) = self.contact_dialog(None) {
                self.save_contacts(contact, None);
            }
        }

        #[unsafe(method(editContact:))]
        fn edit_contact(&self, _sender: &AnyObject) {
            let Some(contact) = self.selected_contact() else {
                self.require_selection("Select a contact to edit.");
                return;
            };
            if let Some(updated) = self.contact_dialog(Some(&contact)) {
                self.save_contacts(updated, Some(contact.peer_id.clone()));
            }
        }

        #[unsafe(method(removeContact:))]
        fn remove_contact(&self, _sender: &AnyObject) {
            let Some(contact) = self.selected_contact() else {
                self.require_selection("Select a contact to remove.");
                return;
            };
            if !self.confirm(
                "Remove contact?",
                &format!("Remove “{}” from this device?", contact.name),
                "Remove",
            ) {
                return;
            }
            let remaining: Vec<Contact> = self
                .snapshot()
                .contacts
                .iter()
                .filter(|item| item.peer_id != contact.peer_id)
                .map(contact_from_ui)
                .collect();
            self.send_contacts(remaining, "Contact removed.");
        }

        #[unsafe(method(copyContactId:))]
        fn copy_contact_id(&self, _sender: &AnyObject) {
            let Some(contact) = self.selected_contact() else {
                self.require_selection("Select a contact before copying its ID.");
                return;
            };
            crate::copy_to_clipboard(&contact.peer_id);
            self.set_status("Contact ID copied.");
        }

        #[unsafe(method(resolveContactProfile:))]
        fn resolve_contact_profile(&self, _sender: &AnyObject) {
            let Some(contact) = self.selected_contact() else {
                self.require_selection("Select a contact to resolve.");
                return;
            };
            if !self.snapshot().running {
                self.show_error("Start Canopee before resolving a contact profile.");
                return;
            }
            self.send_action(
                DataAction::ResolveProfile(IdentityId::new(contact.peer_id.clone())),
                "Resolving contact profile…",
            );
        }

        #[unsafe(method(syncDevices:))]
        fn sync_devices(&self, _sender: &AnyObject) {
            if !self.snapshot().running {
                self.show_error("Start Canopee before syncing devices.");
                return;
            }
            self.send_action(DataAction::SyncDevices, "Syncing devices…");
        }

        #[unsafe(method(removeDevice:))]
        fn remove_device(&self, _sender: &AnyObject) {
            let Some(device) = self.selected_device() else {
                self.require_selection("Select a device to remove.");
                return;
            };
            if device.current {
                self.show_error("The current device cannot be removed.");
                return;
            }
            if !self.confirm(
                "Remove device?",
                &format!("Remove “{}” from the device list?", device.name),
                "Remove",
            ) {
                return;
            }
            self.send_action(
                DataAction::RemoveDevice(device.id),
                "Removing device…",
            );
        }

        #[unsafe(method(copyDeviceId:))]
        fn copy_device_id(&self, _sender: &AnyObject) {
            let Some(device) = self.selected_device() else {
                self.require_selection("Select a device before copying its ID.");
                return;
            };
            crate::copy_to_clipboard(&device.id);
            self.set_status("Device ID copied.");
        }
    }

    // Must live inside `define_class!` so objc2 registers these as real
    // selectors and records the protocol conformance on the class. Declaring
    // it outside leaves the tables with no rows at runtime.
    unsafe impl NSTableViewDataSource for DataWindowController {
        // Selector names are dictated by the protocol, hence the casing.
        #[allow(non_snake_case)]
        #[unsafe(method(numberOfRowsInTableView:))]
        fn numberOfRowsInTableView(&self, table_view: &NSTableView) -> isize {
            self.table_index(table_view)
                .map(|index| self.row_count(index))
                .unwrap_or(0) as isize
        }

        #[allow(non_snake_case)]
        #[unsafe(method_id(tableView:objectValueForTableColumn:row:))]
        fn tableView_objectValueForTableColumn_row(
            &self,
            table_view: &NSTableView,
            table_column: Option<&NSTableColumn>,
            row: isize,
        ) -> Retained<AnyObject> {
            // AppKit renders an empty string as a blank cell, so an unknown
            // column/row degrades to empty rather than raising.
            let text = if row < 0 {
                String::new()
            } else {
                table_column
                    .and_then(|column| {
                        self.table_index(table_view)
                            .map(|index| self.value_for_row(index, row as usize, column.title().to_string()))
                    })
                    .unwrap_or_default()
            };
            let value: Retained<AnyObject> =
                Retained::into_super(NSString::from_str(&text)).into();
            value
        }
    }
);

struct TabParts {
    item: Retained<NSTabViewItem>,
    view: Retained<NSView>,
    search: Retained<NSSearchField>,
    table: Retained<NSTableView>,
    scroll: Retained<NSScrollView>,
    empty: Retained<NSTextField>,
}

impl DataWindowController {
    pub(crate) fn new(
        mtm: MainThreadMarker,
        state: DataWindowState,
        action_tx: Sender<DataAction>,
    ) -> Retained<Self> {
        let content_rect = rect(0.0, 0.0, 920.0, 640.0);
        let mask = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable
            | NSWindowStyleMask::Resizable;
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                mtm.alloc(),
                content_rect,
                mask,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        let title = NSString::from_str("Canopee Data");
        window.setTitle(&title);
        window.setContentMinSize(NSSize::new(820.0, 600.0));
        unsafe { window.setReleasedWhenClosed(false) };

        let root = NSView::initWithFrame(mtm.alloc(), content_rect);
        let tab_view = NSTabView::initWithFrame(mtm.alloc(), rect(18.0, 48.0, 884.0, 490.0));
        tab_view.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMaxYMargin,
        );
        let storage = make_tab(
            mtm,
            "Storage",
            &[
                ("Name", 220.0),
                ("Type", 125.0),
                ("Size", 90.0),
                ("Sharing", 95.0),
                ("Object ID", 345.0),
            ],
            "Search stored objects",
            "No stored objects. Add a file or import a bundle.",
        );
        let contacts = make_tab(
            mtm,
            "Contacts",
            &[
                ("Name", 160.0),
                ("Profile", 150.0),
                ("Identity", 330.0),
                ("Note", 235.0),
            ],
            "Search contacts",
            "No contacts yet. Add someone you know to get started.",
        );
        let devices = make_tab(
            mtm,
            "Devices",
            &[("Name", 230.0), ("Status", 120.0), ("Device ID", 525.0)],
            "Search devices",
            "No devices are listed for this identity.",
        );
        tab_view.addTabViewItem(&storage.item);
        tab_view.addTabViewItem(&contacts.item);
        tab_view.addTabViewItem(&devices.item);
        root.addSubview(&tab_view);

        let status_label =
            NSTextField::labelWithString(&NSString::from_str("Loading local data…"), mtm);
        status_label.setFrame(rect(18.0, 16.0, 884.0, 24.0));
        status_label.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMinYMargin,
        );
        root.addSubview(&status_label);

        // Identity header: the window answers "who am I" without the user
        // having to read a peer id out of the tray menu.
        let identity_name = NSTextField::labelWithString(&NSString::from_str(""), mtm);
        identity_name.setFrame(rect(18.0, 590.0, 560.0, 24.0));
        let header_font = NSFont::boldSystemFontOfSize(15.0);
        identity_name.setFont(Some(&header_font));
        identity_name.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMaxYMargin);
        root.addSubview(&identity_name);

        let identity_meta = NSTextField::labelWithString(&NSString::from_str(""), mtm);
        identity_meta.setFrame(rect(18.0, 570.0, 660.0, 18.0));
        identity_meta.setTextColor(Some(&NSColor::secondaryLabelColor()));
        identity_meta.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMaxYMargin);
        root.addSubview(&identity_meta);
        window.setContentView(Some(&root));

        let this = mtm.alloc().set_ivars(DataWindowIvars {
            state: state.clone(),
            action_tx,
            window,
            identity_name,
            identity_meta,
            tab_views: [storage.view, contacts.view, devices.view],
            search_fields: [storage.search, contacts.search, devices.search],
            tables: [storage.table, contacts.table, devices.table],
            scroll_views: [storage.scroll, contacts.scroll, devices.scroll],
            empty_labels: [storage.empty, contacts.empty, devices.empty],
            status_label,
            ui: RefCell::new(DataUiState::default()),
        });
        let this: Retained<Self> = unsafe { msg_send![super(this), init] };

        for table in this.ivars().tables.iter() {
            unsafe {
                table.setDataSource(Some(ProtocolObject::from_ref(&*this)));
                table.setTarget(Some(&*this));
                table.setAction(Some(Sel::register(c"activateRow:")));
            }
        }
        for (title, width, x, selector) in [
            ("Edit Profile…", 112.0, 678.0, c"editProfile:" as &CStr),
            ("Username…", 104.0, 798.0, c"claimUsername:" as &CStr),
        ] {
            let button = make_button(
                mtm,
                &this,
                title,
                rect(x, 588.0, width, 28.0),
                selector,
            );
            button.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinXMargin);
            root.addSubview(&button);
        }
        this.install_controls(mtm);
        this.apply_snapshot();
        this
    }

    pub(crate) fn sync(&self, feedback: Vec<DataFeedback>) {
        for item in feedback {
            self.apply_feedback(item);
        }

        let visible = self.ivars().window.isVisible();
        self.ivars().state.visible.store(visible, Ordering::SeqCst);

        if self
            .ivars()
            .state
            .open_requested
            .swap(false, Ordering::SeqCst)
        {
            self.apply_snapshot();
            self.show();
            return;
        }

        if self.ivars().state.dirty.swap(false, Ordering::SeqCst) {
            self.apply_snapshot();
        }
    }

    fn install_controls(&self, mtm: MainThreadMarker) {
        for search in self.ivars().search_fields.iter() {
            unsafe {
                search.setTarget(Some(self));
                search.setAction(Some(Sel::register(c"searchChanged:")));
            }
        }

        // Ordered by how often each is reached for, so the common flow sits
        // on the left where the eye lands first.
        let storage_buttons: &[(&str, f64, &CStr)] = &[
            ("Add Files…", 92.0, c"addFiles:"),
            ("Save a Copy…", 104.0, c"saveCopy:"),
            ("Open", 62.0, c"openObject:"),
            ("Share / Unshare", 122.0, c"toggleObjectSharing:"),
            ("Rename…", 84.0, c"renameObject:"),
            ("Import…", 78.0, c"importBundle:"),
            ("Export…", 74.0, c"exportObject:"),
            ("Copy ID", 74.0, c"copyObjectId:"),
            ("Delete…", 80.0, c"deleteObject:"),
        ];
        let contact_buttons: &[(&str, f64, &CStr)] = &[
            ("Add…", 65.0, c"addContact:"),
            ("Edit…", 65.0, c"editContact:"),
            ("Remove…", 85.0, c"removeContact:"),
            ("Copy ID", 75.0, c"copyContactId:"),
            ("Resolve Profile", 125.0, c"resolveContactProfile:"),
        ];
        let device_buttons: &[(&str, f64, &CStr)] = &[
            ("Sync All", 85.0, c"syncDevices:"),
            ("Remove…", 85.0, c"removeDevice:"),
            ("Copy ID", 75.0, c"copyDeviceId:"),
        ];
        for (tab, specs) in [storage_buttons, contact_buttons, device_buttons]
            .into_iter()
            .enumerate()
        {
            let mut x = 10.0;
            for (title, width, selector) in specs {
                let button = make_button(
                    mtm,
                    self,
                    title,
                    rect(x, 370.0, *width, 30.0),
                    selector,
                );
                self.ivars().tab_views[tab].addSubview(&button);
                x += width + 8.0;
            }
        }
    }

    fn open_selected_object(&self) {
        let Some(object) = self.selected_storage() else {
            self.require_selection("Select an object to open.");
            return;
        };
        self.send_action(
            DataAction::OpenObject {
                id: ObjectId::new(&object.id),
                name: download_file_name(&object.name, &object.id),
            },
            "Opening…",
        );
    }

    fn show(&self) {
        let mtm = MainThreadMarker::new().expect("data window must run on main thread");
        let app = NSApplication::sharedApplication(mtm);
        app.activate();
        self.ivars().window.makeKeyAndOrderFront(None);
        self.ivars().state.visible.store(true, Ordering::SeqCst);
    }

    fn apply_snapshot(&self) {
        let snapshot = self.snapshot();
        self.apply_identity(&snapshot);
        let (object_count, contact_count) = {
            let mut ui = self.ivars().ui.borrow_mut();
            ui.snapshot = snapshot.clone();
            ui.storage_rows = snapshot.objects.clone();
            ui.contact_rows = snapshot.contacts.clone();
            ui.device_rows = snapshot.devices.clone();
            (ui.storage_rows.len(), ui.contact_rows.len())
        };
        if let Some(error) = snapshot.error {
            self.set_status(&format!("Node error: {error}"));
        } else if snapshot.running {
            self.set_status(&format!(
                "Connected · {object_count} objects · {contact_count} contacts · {} peers",
                snapshot.peers.len()
            ));
        } else {
            self.set_status("Node stopped · local data remains available");
        }
        self.refresh_rows();
    }

    /// Renders the "who am I" header. Falls back through display name →
    /// username → a prompt to set one up, so the window is never blank.
    fn apply_identity(&self, snapshot: &DataSnapshot) {
        let name = match (&snapshot.display_name, &snapshot.username) {
            (Some(name), _) if !name.trim().is_empty() => name.clone(),
            (None, Some(username)) => format!("@{username}"),
            (Some(name), Some(username)) => format!("{name} · @{username}"),
            (Some(name), None) if !name.trim().is_empty() => name.clone(),
            _ => "Set up your profile".to_string(),
        };
        self.ivars()
            .identity_name
            .setStringValue(&NSString::from_str(&name));

        let mut parts: Vec<String> = Vec::new();
        match &snapshot.username {
            Some(username) => parts.push(format!("@{username}")),
            None => parts.push("no username claimed".to_string()),
        }
        match &snapshot.identity {
            Some(identity) => parts.push(format!(
                "canopee://identity/{}",
                short_id(identity_peer_id(identity))
            )),
            None => parts.push("no identity yet".to_string()),
        }
        self.ivars()
            .identity_meta
            .setStringValue(&NSString::from_str(&parts.join(" · ")));
    }

    fn apply_feedback(&self, feedback: DataFeedback) {
        match feedback {
            DataFeedback::Success(message) => self.set_status(&message),
            DataFeedback::Error(message) => {
                self.set_status(&message);
                self.show_error(&message);
            }
            DataFeedback::ProfileResolved { owner, summary } => {
                self.ivars()
                    .ui
                    .borrow_mut()
                    .profiles
                    .insert(owner.to_string(), summary.clone());
                self.set_status(&format!("Profile resolved: {summary}"));
                self.refresh_rows();
            }
        }
    }

    fn refresh_rows(&self) {
        let ivars = self.ivars();
        let storage_query = field_text(&ivars.search_fields[0]);
        let contact_query = field_text(&ivars.search_fields[1]);
        let device_query = field_text(&ivars.search_fields[2]);
        let empty_states;
        let visible_counts;

        {
            let mut ui = ivars.ui.borrow_mut();
            ui.visible_storage = ui
                .storage_rows
                .iter()
                .filter(|item| {
                    matches_query(
                        &storage_query,
                        [&item.name, &item.object_type, &item.id, ""],
                    )
                })
                .cloned()
                .collect();
            ui.visible_contacts = ui
                .contact_rows
                .iter()
                .filter(|item| {
                    let profile = ui
                        .profiles
                        .get(&item.peer_id)
                        .map(String::as_str)
                        .unwrap_or("");
                    matches_query(
                        &contact_query,
                        [&item.name, &item.peer_id, &item.note, profile],
                    )
                })
                .cloned()
                .collect();
            ui.visible_devices = ui
                .device_rows
                .iter()
                .filter(|item| {
                    let status = if item.current {
                        "current device"
                    } else if item.online {
                        "online"
                    } else {
                        "offline"
                    };
                    matches_query(&device_query, [&item.name, &item.id, status, ""])
                })
                .cloned()
                .collect();
            empty_states = [
                (
                    ui.visible_storage.is_empty(),
                    ui.storage_rows.is_empty(),
                    "No stored objects. Add a file or import a bundle.",
                ),
                (
                    ui.visible_contacts.is_empty(),
                    ui.contact_rows.is_empty(),
                    "No contacts yet. Add someone you know to get started.",
                ),
                (
                    ui.visible_devices.is_empty(),
                    ui.device_rows.is_empty(),
                    "No devices are listed for this identity.",
                ),
            ];
            visible_counts = [
                ui.visible_storage.len(),
                ui.visible_contacts.len(),
                ui.visible_devices.len(),
            ];
        }

        for index in 0..3 {
            let has_rows = visible_counts[index] > 0;
            ivars.tables[index].reloadData();
            // The table is shown only when it has rows; otherwise the empty
            // state takes its place.
            ivars.scroll_views[index].setHidden(!has_rows);
            ivars.empty_labels[index].setHidden(has_rows);
            let (visible_empty, all_empty, default_message) = empty_states[index];
            let message = if visible_empty && !all_empty {
                "No matching items."
            } else {
                default_message
            };
            ivars.empty_labels[index].setStringValue(&NSString::from_str(message));
        }
    }

    fn table_index(&self, table: &NSTableView) -> Option<usize> {
        self.ivars().tables.iter().position(|candidate| {
            std::ptr::eq(
                candidate.as_ref() as *const NSTableView,
                table as *const NSTableView,
            )
        })
    }

    fn row_count(&self, index: usize) -> usize {
        let ui = self.ivars().ui.borrow();
        match index {
            0 => ui.visible_storage.len(),
            1 => ui.visible_contacts.len(),
            2 => ui.visible_devices.len(),
            _ => 0,
        }
    }

    fn value_for_row(&self, table: usize, row: usize, column: String) -> String {
        let ui = self.ivars().ui.borrow();
        match table {
            0 => ui
                .visible_storage
                .get(row)
                .map(|item| match column.as_str() {
                    "Name" => item.name.clone(),
                    "Type" => item.object_type.clone(),
                    "Size" => format_size(item.size),
                    "Sharing" => {
                        if item.shared {
                            "Shared".to_string()
                        } else {
                            "Private".to_string()
                        }
                    }
                    _ => item.id.clone(),
                })
                .unwrap_or_default(),
            1 => ui
                .visible_contacts
                .get(row)
                .map(|item| match column.as_str() {
                    "Name" => item.name.clone(),
                    "Profile" => ui
                        .profiles
                        .get(&item.peer_id)
                        .cloned()
                        .unwrap_or_else(|| "Not resolved".to_string()),
                    "Identity" => item.peer_id.clone(),
                    _ => item.note.clone(),
                })
                .unwrap_or_default(),
            2 => ui
                .visible_devices
                .get(row)
                .map(|item| match column.as_str() {
                    "Name" => item.name.clone(),
                    "Status" => {
                        if item.current {
                            "Current device".to_string()
                        } else if item.online {
                            "Online".to_string()
                        } else {
                            "Offline".to_string()
                        }
                    }
                    _ => item.id.clone(),
                })
                .unwrap_or_default(),
            _ => String::new(),
        }
    }

    fn selected_storage(&self) -> Option<StorageObjectUi> {
        let row = self.ivars().tables[0].selectedRow();
        (row >= 0)
            .then(|| {
                self.ivars()
                    .ui
                    .borrow()
                    .visible_storage
                    .get(row as usize)
                    .cloned()
            })
            .flatten()
    }

    fn selected_contact(&self) -> Option<ContactUi> {
        let row = self.ivars().tables[1].selectedRow();
        (row >= 0)
            .then(|| {
                self.ivars()
                    .ui
                    .borrow()
                    .visible_contacts
                    .get(row as usize)
                    .cloned()
            })
            .flatten()
    }

    fn selected_device(&self) -> Option<DeviceUi> {
        let row = self.ivars().tables[2].selectedRow();
        (row >= 0)
            .then(|| {
                self.ivars()
                    .ui
                    .borrow()
                    .visible_devices
                    .get(row as usize)
                    .cloned()
            })
            .flatten()
    }

    fn snapshot(&self) -> DataSnapshot {
        lock_or_poisoned(&self.ivars().state.snapshot).clone()
    }

    fn send_action(&self, action: DataAction, status: &str) {
        if self.ivars().action_tx.try_send(action).is_err() {
            self.show_error("The data action queue is unavailable. Please try again.");
            return;
        }
        self.set_status(status);
    }

    fn save_contacts(&self, updated: ContactUi, replaced_peer_id: Option<String>) {
        let snapshot = self.snapshot();
        if let Some(peer_id) = &replaced_peer_id {
            if peer_id != &updated.peer_id
                && snapshot
                    .contacts
                    .iter()
                    .any(|contact| contact.peer_id == updated.peer_id)
            {
                self.show_error("A contact with this identity already exists.");
                return;
            }
        } else if snapshot
            .contacts
            .iter()
            .any(|contact| contact.peer_id == updated.peer_id)
        {
            self.show_error("A contact with this identity already exists.");
            return;
        }

        let mut contacts: Vec<Contact> = snapshot
            .contacts
            .iter()
            .filter(|contact| Some(contact.peer_id.clone()) != replaced_peer_id)
            .map(contact_from_ui)
            .collect();
        contacts.push(contact_from_ui(&updated));
        contacts.sort_by_key(|contact| contact.name.to_lowercase());
        self.send_contacts(
            contacts,
            if replaced_peer_id.is_some() {
                "Contact updated."
            } else {
                "Contact added."
            },
        );
    }

    fn send_contacts(&self, contacts: Vec<Contact>, status: &str) {
        let snapshot = self.snapshot();
        self.send_action(
            DataAction::SaveContacts(ContactList {
                contacts,
                version: snapshot.contacts_version.saturating_add(1),
            }),
            status,
        );
    }

    fn contact_dialog(&self, existing: Option<&ContactUi>) -> Option<ContactUi> {
        let mtm = MainThreadMarker::new().expect("contact dialog must run on main thread");
        let alert = NSAlert::new(mtm);
        alert.setMessageText(&NSString::from_str(if existing.is_some() {
            "Edit contact"
        } else {
            "Add contact"
        }));
        alert.setInformativeText(&NSString::from_str(
            "Paste the contact’s Canopee identity and displayed DH public key.",
        ));

        let view = NSView::initWithFrame(mtm.alloc(), rect(0.0, 0.0, 500.0, 170.0));
        let name = form_field(
            mtm,
            &view,
            "Name",
            0.0,
            existing.map(|item| item.name.as_str()).unwrap_or(""),
        );
        let peer_id = form_field(
            mtm,
            &view,
            "Identity",
            46.0,
            existing.map(|item| item.peer_id.as_str()).unwrap_or(""),
        );
        let key = form_field(
            mtm,
            &view,
            "DH key",
            92.0,
            existing
                .map(|item| encode_hex(&item.dh_public_key))
                .unwrap_or_default()
                .as_str(),
        );
        let note = form_field(
            mtm,
            &view,
            "Note",
            138.0,
            existing.map(|item| item.note.as_str()).unwrap_or(""),
        );
        key.setPlaceholderString(Some(&NSString::from_str("64 hexadecimal characters")));
        peer_id.setPlaceholderString(Some(&NSString::from_str("Identity peer ID")));
        alert.setAccessoryView(Some(&view));
        alert.addButtonWithTitle(&NSString::from_str("Save"));
        alert.addButtonWithTitle(&NSString::from_str("Cancel"));
        if alert.runModal() != NSAlertFirstButtonReturn {
            return None;
        }

        let name = field_text(&name);
        let peer_id = field_text(&peer_id);
        let key = field_text(&key);
        let note = field_text(&note);
        let name = name.trim().to_string();
        let peer_id = peer_id.trim().to_string();
        if name.is_empty() {
            self.show_error("The contact name cannot be empty.");
            return None;
        }
        if peer_id.is_empty() {
            self.show_error("The contact identity cannot be empty.");
            return None;
        }
        let Some(dh_public_key) = decode_hex_32(&key) else {
            self.show_error("The DH key must contain exactly 64 hexadecimal characters.");
            return None;
        };
        Some(ContactUi {
            name,
            peer_id,
            dh_public_key,
            note: note.trim().to_string(),
        })
    }

    fn prompt_text(&self, title: &str, label: &str, value: &str) -> Option<String> {
        let mtm = MainThreadMarker::new().expect("prompt must run on main thread");
        let alert = NSAlert::new(mtm);
        alert.setMessageText(&NSString::from_str(title));
        let field = NSTextField::initWithFrame(mtm.alloc(), rect(0.0, 0.0, 360.0, 26.0));
        field.setStringValue(&NSString::from_str(value));
        let view = NSView::initWithFrame(mtm.alloc(), rect(0.0, 0.0, 360.0, 58.0));
        let caption = NSTextField::labelWithString(&NSString::from_str(label), mtm);
        caption.setFrame(rect(0.0, 34.0, 360.0, 20.0));
        view.addSubview(&caption);
        view.addSubview(&field);
        alert.setAccessoryView(Some(&view));
        alert.addButtonWithTitle(&NSString::from_str("Save"));
        alert.addButtonWithTitle(&NSString::from_str("Cancel"));
        if alert.runModal() != NSAlertFirstButtonReturn {
            return None;
        }
        Some(field_text(&field))
    }

    fn choose_files(&self, message: &str, multiple: bool) -> Option<Vec<PathBuf>> {
        let mtm = MainThreadMarker::new().expect("open panel must run on main thread");
        let panel = NSOpenPanel::openPanel(mtm);
        panel.setCanChooseFiles(true);
        panel.setCanChooseDirectories(false);
        panel.setAllowsMultipleSelection(multiple);
        panel.setMessage(Some(&NSString::from_str(message)));
        if panel.runModal() != NSModalResponseOK {
            return None;
        }
        let paths = panel
            .URLs()
            .iter()
            .filter_map(|url| url.path().map(|path| PathBuf::from(path.to_string())))
            .collect::<Vec<_>>();
        (!paths.is_empty()).then_some(paths)
    }

    fn choose_export_path(&self, name: &str) -> Option<PathBuf> {
        let mtm = MainThreadMarker::new().expect("save panel must run on main thread");
        let panel = NSSavePanel::savePanel(mtm);
        panel.setCanCreateDirectories(true);
        panel.setMessage(Some(&NSString::from_str(
            "Choose where to export this object bundle",
        )));
        let stem = Path::new(name)
            .file_stem()
            .and_then(|value| value.to_str())
            .filter(|value| !value.is_empty())
            .unwrap_or("canopee-object");
        panel.setNameFieldStringValue(&NSString::from_str(&format!("{stem}.canopee")));
        if panel.runModal() != NSModalResponseOK {
            return None;
        }
        panel
            .URL()
            .and_then(|url| url.path().map(|path| PathBuf::from(path.to_string())))
    }

    /// Save panel for "Save a Copy…": writes the object's original bytes, so
    /// the suggested name is the object's own name rather than a bundle name.
    fn choose_save_copy_path(&self, name: &str, id: &str) -> Option<PathBuf> {
        let mtm = MainThreadMarker::new().expect("save panel must run on main thread");
        let panel = NSSavePanel::savePanel(mtm);
        panel.setCanCreateDirectories(true);
        panel.setMessage(Some(&NSString::from_str(
            "Choose where to save a copy of this object",
        )));
        panel.setNameFieldStringValue(&NSString::from_str(&download_file_name(name, id)));
        if panel.runModal() != NSModalResponseOK {
            return None;
        }
        panel
            .URL()
            .and_then(|url| url.path().map(|path| PathBuf::from(path.to_string())))
    }

    fn confirm(&self, title: &str, message: &str, action: &str) -> bool {
        let mtm = MainThreadMarker::new().expect("confirmation must run on main thread");
        let alert = NSAlert::new(mtm);
        alert.setMessageText(&NSString::from_str(title));
        alert.setInformativeText(&NSString::from_str(message));
        alert.addButtonWithTitle(&NSString::from_str(action));
        alert.addButtonWithTitle(&NSString::from_str("Cancel"));
        alert.runModal() == NSAlertFirstButtonReturn
    }

    fn show_error(&self, message: &str) {
        let mtm = MainThreadMarker::new().expect("alert must run on main thread");
        let alert = NSAlert::new(mtm);
        alert.setMessageText(&NSString::from_str("Canopee"));
        alert.setInformativeText(&NSString::from_str(message));
        alert.addButtonWithTitle(&NSString::from_str("OK"));
        alert.runModal();
    }

    fn require_selection(&self, message: &str) {
        self.set_status(message);
        self.show_error(message);
    }

    fn set_status(&self, message: &str) {
        self.ivars()
            .status_label
            .setStringValue(&NSString::from_str(message));
    }
}

pub(crate) fn object_type_label(object_type: ObjectType) -> &'static str {
    match object_type {
        ObjectType::Blob => "File",
        ObjectType::AppManifest => "App",
        ObjectType::AppPointer => "App pointer",
        ObjectType::Profile => "Profile",
        ObjectType::ContactList => "Contacts",
        ObjectType::HomeIndex => "Home index",
        ObjectType::Capability => "Capability",
        _ => "Other",
    }
}

fn contact_from_ui(contact: &ContactUi) -> Contact {
    Contact {
        name: contact.name.clone(),
        peer_id: contact.peer_id.clone(),
        dh_public_key: contact.dh_public_key,
        note: (!contact.note.is_empty()).then(|| contact.note.clone()),
    }
}

fn make_tab(
    mtm: MainThreadMarker,
    title: &str,
    columns: &[(&str, f64)],
    placeholder: &str,
    empty_message: &str,
) -> TabParts {
    // Sized so NSTabView's content area (tab view height minus the tab bar)
    // leaves a few points of slack; the subviews below are positioned
    // relative to this frame and only stretch with it.
    let view = NSView::initWithFrame(mtm.alloc(), rect(0.0, 0.0, 871.0, 456.0));
    view.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    let search = NSSearchField::initWithFrame(mtm.alloc(), rect(10.0, 414.0, 851.0, 30.0));
    search.setPlaceholderString(Some(&NSString::from_str(placeholder)));
    search.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
    view.addSubview(&search);

    let table = NSTableView::initWithFrame(mtm.alloc(), rect(0.0, 0.0, 851.0, 348.0));
    table.setRowHeight(27.0);
    table.setUsesAlternatingRowBackgroundColors(true);
    for (heading, width) in columns {
        let column = NSTableColumn::init(mtm.alloc());
        column.setTitle(&NSString::from_str(heading));
        column.setWidth(*width);
        column.setMinWidth(70.0);
        column.setResizingMask(NSTableColumnResizingOptions::AutoresizingMask);
        table.addTableColumn(&column);
    }
    let scroll = NSScrollView::initWithFrame(mtm.alloc(), rect(10.0, 8.0, 851.0, 348.0));
    scroll.setDocumentView(Some(&table));
    scroll.setHasVerticalScroller(true);
    scroll.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    view.addSubview(&scroll);

    let empty = NSTextField::wrappingLabelWithString(&NSString::from_str(empty_message), mtm);
    empty.setFrame(rect(153.0, 157.0, 565.0, 50.0));
    empty.setAlignment(NSTextAlignment::Center);
    view.addSubview(&empty);

    let identifier: Retained<AnyObject> = Retained::into_super(NSString::from_str(title)).into();
    let item = unsafe { NSTabViewItem::initWithIdentifier(mtm.alloc(), Some(&identifier)) };
    item.setLabel(&NSString::from_str(title));
    item.setView(Some(&view));
    TabParts {
        item,
        view,
        search,
        table,
        scroll,
        empty,
    }
}

fn make_button(
    mtm: MainThreadMarker,
    target: &DataWindowController,
    title: &str,
    frame: NSRect,
    selector: &CStr,
) -> Retained<NSButton> {
    let button = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str(title),
            Some(target),
            Some(Sel::register(selector)),
            mtm,
        )
    };
    button.setFrame(frame);
    button
}

fn short_id(id: &str) -> String {
    id.chars().take(10).collect()
}

/// `NodeResponse::Identity` already yields a `canopee://identity/<peer-id>`
/// URI, so strip the scheme before truncating for display.
fn identity_peer_id(identity: &str) -> &str {
    identity
        .strip_prefix("canopee://identity/")
        .unwrap_or(identity)
}

/// Turns a stored object name into something safe to hand to a save panel or
/// the filesystem. Canopee names are user-supplied, so path separators and
/// traversal segments have to be stripped rather than trusted.
fn download_file_name(name: &str, id: &str) -> String {
    let candidate = name.rsplit('/').next().unwrap_or(name).trim();
    let sanitized: String = candidate
        .chars()
        .map(|c| if c == ':' || c == '\0' { '_' } else { c })
        .collect();
    let trimmed = sanitized.trim().trim_matches('.').trim();
    if trimmed.is_empty() {
        return format!("canopee-object-{}", &id.chars().take(12).collect::<String>());
    }
    trimmed.to_string()
}

fn form_field(
    mtm: MainThreadMarker,
    view: &NSView,
    label: &str,
    y: f64,
    value: &str,
) -> Retained<NSTextField> {
    let caption = NSTextField::labelWithString(&NSString::from_str(label), mtm);
    caption.setFrame(rect(0.0, y, 92.0, 24.0));
    view.addSubview(&caption);
    let field = NSTextField::textFieldWithString(&NSString::from_str(value), mtm);
    field.setFrame(rect(100.0, y, 392.0, 26.0));
    view.addSubview(&field);
    field
}

fn field_text(field: &NSTextField) -> String {
    field.stringValue().to_string()
}

fn matches_query(query: &str, values: [&str; 4]) -> bool {
    let query = query.trim().to_lowercase();
    query.is_empty()
        || values
            .iter()
            .any(|value| value.to_lowercase().contains(&query))
}

fn format_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let bytes = bytes as f64;
    if bytes < KIB {
        format!("{bytes} B")
    } else if bytes < MIB {
        format!("{:.1} KB", bytes / KIB)
    } else if bytes < GIB {
        format!("{:.1} MB", bytes / MIB)
    } else {
        format!("{:.1} GB", bytes / GIB)
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(result, "{byte:02x}");
    }
    result
}

fn decode_hex_32(value: &str) -> Option<[u8; 32]> {
    let value = value.trim();
    if value.len() != 64 {
        return None;
    }
    let mut result = [0_u8; 32];
    for (index, chunk) in value.as_bytes().chunks_exact(2).enumerate() {
        let high = decode_nibble(chunk[0])?;
        let low = decode_nibble(chunk[1])?;
        result[index] = (high << 4) | low;
    }
    Some(result)
}

fn decode_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn rect(x: f64, y: f64, width: f64, height: f64) -> NSRect {
    NSRect {
        origin: NSPoint { x, y },
        size: NSSize { width, height },
    }
}

fn lock_or_poisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_storage_sizes() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(2048), "2.0 KB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn round_trips_dh_keys() {
        let bytes = [42_u8; 32];
        let encoded = encode_hex(&bytes);
        assert_eq!(encoded.len(), 64);
        assert_eq!(decode_hex_32(&encoded), Some(bytes));
        assert_eq!(decode_hex_32("not-hex"), None);
    }

    #[test]
    fn filters_all_visible_fields() {
        // Every column a row exposes must be searchable, including the
        // resolved-profile summary in the fourth slot.
        assert!(matches_query("photo", ["Photo.jpg", "File", "abc", ""]));
        assert!(matches_query("v2", ["Alice", "Alice v2", "id", ""]));
        assert!(matches_query("avatar", ["Alice", "", "id", "Alice · v2 · avatar"]));
        assert!(!matches_query("photo", ["Document", "File", "abc", ""]));
        // An empty query keeps everything, which is what clearing search does.
        assert!(matches_query("  ", ["Document", "File", "abc", ""]));
    }

    #[test]
    fn strips_scheme_from_identity_for_display() {
        let peer = "12ab34cd56ef";
        assert_eq!(identity_peer_id(&format!("canopee://identity/{peer}")), peer);
        // A bare peer id is passed through unchanged.
        assert_eq!(identity_peer_id(peer), peer);
        assert_eq!(
            format!("canopee://identity/{}", short_id(identity_peer_id(&format!("canopee://identity/{peer}")))),
            "canopee://identity/12ab34cd56"
        );
    }

    #[test]
    fn sanitizes_download_names() {
        // A stored name is user-supplied, so separators and traversal must
        // never survive into a path handed to a save panel.
        assert_eq!(download_file_name("photo.jpg", "abc123"), "photo.jpg");
        assert_eq!(download_file_name("dir/photo.jpg", "abc123"), "photo.jpg");
        assert_eq!(download_file_name("../../etc/passwd", "abc123"), "passwd");
        assert_eq!(download_file_name("  ", "abcdef123456789"), "canopee-object-abcdef123456");
        assert_eq!(download_file_name("a:b.txt", "abc"), "a_b.txt");
    }
}
