mod tray;
use canopee_node::Node;
use canopee_protocol::{NodeCommand, NodeResponse};
use objc2::rc::Retained;
use objc2_app_kit::{
    NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSApplication, NSApplicationActivationPolicy,
};
use objc2_foundation::{MainThreadMarker, NSArray};
use std::{path::PathBuf, sync::mpsc};
use tokio::net::UnixStream;
use tray::{Events, build_menu};
use trayicon::*;

#[derive(Clone)]
struct NodeState {
    root_path: PathBuf,
    node: Node,
    identity: Option<String>,
    running: bool,
    error: Option<String>,
}

impl NodeState {
    async fn new() -> Self {
        Self {
            root_path: dirs::home_dir().unwrap().join(".canopee"),
            node: Node::open().await.unwrap(),
            identity: None,
            running: false,
            error: None,
        }
    }
    fn set_running(&mut self, running: bool) {
        self.running = running;
    }
    fn set_error(&mut self, error: String) {
        self.error = Some(error);
    }
    fn set_identity(&mut self, identity: String) {
        self.identity = Some(identity);
    }
    async fn identity(&mut self) {
        match self.node.handle(NodeCommand::Identity).await {
            NodeResponse::Identity { identity_id } => self.set_identity(identity_id.to_string()),
            NodeResponse::Error { message } => self.set_error(message),
            _ => {}
        }
    }
    async fn is_running(&mut self) -> bool {
        let socket_path = self.root_path.join("node.sock");
        let running = UnixStream::connect(socket_path).await.is_ok();
        self.running = running;
        running
    }
    async fn run_node(&mut self) {
        if !self.is_running().await {
            let mut s = self.clone();
            tokio::spawn(async move {
                if let Err(e) = s.node.run().await {
                    s.set_error(e.to_string());
                } else {
                    s.set_running(true);
                    s.identity().await;
                }
            });
        }
    }
}

fn is_dark_mode(app: &Retained<NSApplication>) -> bool {
    let appearance = app.effectiveAppearance();
    let dark = unsafe { NSAppearanceNameDarkAqua };
    let light = unsafe { NSAppearanceNameAqua };
    let names = NSArray::from_slice(&[dark, light]);
    appearance
        .bestMatchFromAppearancesWithNames(&names)
        .map(|matched| matched.to_string() == dark.to_string())
        .unwrap_or(false)
}

#[tokio::main]
async fn main() {
    let mtm = MainThreadMarker::new().expect("Must run on main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let mut node_state = NodeState::new().await;
    node_state.run_node().await;

    let dark_mode_icon = include_bytes!("./icons/constellation_24_w.png");
    let light_mode_icon = include_bytes!("./icons/constellation_24_b.png");
    let icon: &[u8] = if is_dark_mode(&app) {
        dark_mode_icon
    } else {
        light_mode_icon
    };

    let (sender, receiver) = mpsc::channel::<Events>();

    let mut tray_icon = TrayIconBuilder::new()
        .sender(move |event: &Events| {
            let _ = sender.send(*event);
        })
        .icon_from_buffer(icon)
        .tooltip("Canopee runtime")
        .on_right_click(Events::RightClickTrayIcon)
        .on_click(Events::LeftClickTrayIcon)
        .menu(build_menu())
        .build()
        .expect("Failed to create tray icon");

    std::thread::spawn(move || {
        for event in receiver {
            match event {
                Events::RightClickTrayIcon | Events::LeftClickTrayIcon => {
                    tray_icon.show_menu().unwrap();
                }
                Events::Exit => {
                    std::process::exit(0);
                }
            }
        }
    });

    app.run();
}
