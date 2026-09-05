use trayicon::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Events {
    Noop,
    CopyIdentity,
    OpenApp { name: String },
    OpenUrl { url: String },
    ToggleShareHomeEntry { name: String, shared: bool },
    CopyObjectId { id: String },
    CopyPeerId { id: String },
    StartNode,
    StopNode,
    RestartNode,
    Quit,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct HomeEntryUi {
    pub name: String,
    pub object: String,
    pub shared: bool,
    pub is_app: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct MenuSnapshot {
    pub identity: Option<String>,
    pub objects: usize,
    pub peers: usize,
    pub relays: usize,
    pub running: bool,
    pub error: Option<String>,
    pub home_entries: Vec<HomeEntryUi>,
    pub peer_ids: Vec<String>,
}

fn disabled_item(name: &str) -> MenuItem<Events> {
    MenuItem::Item {
        id: Events::Noop,
        name: name.to_string(),
        disabled: true,
        icon: None,
    }
}

fn truncate(id: &str, max: usize) -> String {
    if id.chars().count() <= max {
        id.to_string()
    } else {
        let cut: String = id.chars().take(max).collect();
        format!("{cut}…")
    }
}

pub fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

fn state_line(s: &MenuSnapshot) -> String {
    if let Some(err) = &s.error {
        return format!("Canopee — error: {err}");
    }
    if s.running {
        format!(
            "Canopee — running · {} · {} · {}",
            plural(s.objects, "object", "objects"),
            plural(s.peers, "peer", "peers"),
            plural(s.relays, "relay", "relays"),
        )
    } else {
        "Canopee — stopped".to_string()
    }
}

pub fn build_menu(s: &MenuSnapshot) -> MenuBuilder<Events> {
    let mut menu = MenuBuilder::new()
        .with(disabled_item(&state_line(s)))
        .with(disabled_item(&format!("Identity: {}", truncated_identity(s))));

    menu = menu
        .item("Copy identity", Events::CopyIdentity)
        .with(disabled_item(&format!("Objects: {}", s.objects)))
        .separator();

    menu = match (s.running, s.error.is_some()) {
        (false, false) => menu
            .item("Start node", Events::StartNode)
            .item("Restart node", Events::RestartNode),
        (true, _) => menu
            .item("Stop node", Events::StopNode)
            .item("Restart node", Events::RestartNode),
        (false, true) => menu
            .item("Start node", Events::StartNode)
            .item("Restart node", Events::RestartNode),
    };

    menu = menu.separator().with(MenuItem::Submenu {
        id: None,
        name: "Connected peers".to_string(),
        children: peers_submenu(s),
        disabled: !s.running,
        icon: None,
    });

    menu = menu.separator().with(MenuItem::Submenu {
        id: None,
        name: "Home".to_string(),
        children: home_submenu(s),
        disabled: false,
        icon: None,
    });

    menu.separator().item("Quit", Events::Quit)
}

fn truncated_identity(s: &MenuSnapshot) -> String {
    match &s.identity {
        Some(id) => truncate(id, 42),
        None => "unknown".to_string(),
    }
}

fn peers_submenu(s: &MenuSnapshot) -> MenuBuilder<Events> {
    let mut sub = MenuBuilder::new();
    if s.peer_ids.is_empty() {
        return sub.with(disabled_item("No connected peers"));
    }
    for id in &s.peer_ids {
        sub = sub.item(&truncate(id, 40), Events::CopyPeerId { id: id.clone() });
    }
    sub
}

fn home_submenu(s: &MenuSnapshot) -> MenuBuilder<Events> {
    let mut sub = MenuBuilder::new();
    if s.home_entries.is_empty() {
        return sub.with(disabled_item("Nothing shared yet"));
    }
    for entry in &s.home_entries {
        if entry.is_app {
            sub = sub.item(
                &format!("Open \"{}\"", entry.name),
                Events::OpenApp { name: entry.name.clone() },
            );
        } else {
            sub = sub.with(disabled_item(&entry.name));
        }
        sub = sub.with(MenuItem::Checkable {
            id: Events::ToggleShareHomeEntry {
                name: entry.name.clone(),
                shared: entry.shared,
            },
            name: format!("Shared: {}", entry.name),
            is_checked: entry.shared,
            disabled: !s.running,
            icon: None,
        });
        sub = sub.item(
            &format!("Copy id: {}", truncate(&entry.object, 16)),
            Events::CopyObjectId { id: entry.object.clone() },
        );
    }
    sub
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> MenuSnapshot {
        MenuSnapshot {
            identity: Some("canopee://identity/12D3KooWabcdefghijklmnopqrstuvwxyzABCDEF".into()),
            objects: 7,
            peers: 2,
            relays: 1,
            running: true,
            error: None,
            home_entries: vec![
                HomeEntryUi {
                    name: "portfolio".into(),
                    object: "abc123".into(),
                    shared: true,
                    is_app: true,
                },
                HomeEntryUi {
                    name: "penguin.png".into(),
                    object: "def456".into(),
                    shared: false,
                    is_app: false,
                },
            ],
            peer_ids: vec!["12D3KooWpeerOne".into(), "12D3KooWpeerTwo".into()],
        }
    }

    fn expected_menu(s: &MenuSnapshot) -> MenuBuilder<Events> {
        let mut menu = MenuBuilder::new()
            .with(disabled_item(&state_line(s)))
            .with(disabled_item(&format!("Identity: {}", truncated_identity(s))))
            .item("Copy identity", Events::CopyIdentity)
            .with(disabled_item(&format!("Objects: {}", s.objects)))
            .separator();
        menu = if s.running {
            menu.item("Stop node", Events::StopNode)
                .item("Restart node", Events::RestartNode)
        } else {
            menu.item("Start node", Events::StartNode)
                .item("Restart node", Events::RestartNode)
        };
        menu.separator()
            .with(MenuItem::Submenu {
                id: None,
                name: "Connected peers".to_string(),
                children: peers_submenu(s),
                disabled: !s.running,
                icon: None,
            })
            .separator()
            .with(MenuItem::Submenu {
                id: None,
                name: "Home".to_string(),
                children: home_submenu(s),
                disabled: false,
                icon: None,
            })
            .separator()
            .item("Quit", Events::Quit)
    }

    #[test]
    fn menu_matches_expected_layout() {
        assert_eq!(build_menu(&sample()), expected_menu(&sample()));
    }

    #[test]
    fn stopped_menu_uses_start() {
        let mut s = sample();
        s.running = false;
        assert_eq!(build_menu(&s), expected_menu(&s));
    }

    #[test]
    fn empty_home_notes_nothing_shared() {
        let s = MenuSnapshot::default();
        assert_eq!(build_menu(&s), expected_menu(&s));
    }
}