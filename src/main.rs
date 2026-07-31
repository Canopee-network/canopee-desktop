use canopee_identity::IdentityId;
use canopee_node::Node;
use canopee_protocol::{NodeCommand, NodeResponse};
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use objc2_foundation::MainThreadMarker;
use std::sync::mpsc;
use trayicon::*;

mod tray;
use tray::{Events, build_menu};

#[allow(unused)]
struct AppState {
    node: Option<Node>,
    identity: Option<IdentityId>,
}

impl AppState {
    fn new(node: Option<Node>, identity: Option<IdentityId>) -> Self {
        Self {
            node,
            identity: identity.clone(),
        }
    }
}

#[tokio::main]
async fn main() {
    let (node, identity) = match Node::open().await {
        Ok(node) => match node.handle(NodeCommand::Identity).await {
            NodeResponse::Identity { identity_id } => (Some(node), Some(identity_id)),
            _ => (Some(node), None),
        },
        Err(_) => (None, None),
    };

    let state = AppState::new(node, identity);

    let icon = include_bytes!("./icons/constellation_24_b.png");
    // let icon2 = include_bytes!("./icons/constellation_24_w.png");

    // let first_icon = Icon::from_buffer(icon, None, None).unwrap();
    // let second_icon = Icon::from_buffer(icon2, None, None).unwrap();

    // Initialize NSApplication for macOS
    let mtm = MainThreadMarker::new().unwrap();
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let (s, r) = mpsc::channel::<Events>();
    // Needlessly complicated tray icon with all the whistles and bells
    let mut tray_icon = TrayIconBuilder::new()
        .sender(move |e: &Events| {
            let _ = s.send(*e);
        })
        .icon_from_buffer(icon)
        .tooltip("Canopee runtime")
        .on_right_click(Events::RightClickTrayIcon)
        .on_click(Events::LeftClickTrayIcon)
        .menu(build_menu())
        .build()
        .unwrap();

    std::thread::spawn(move || {
        r.iter().for_each(|m| match m {
            Events::RightClickTrayIcon => {
                tray_icon.show_menu().unwrap();
            }
            Events::LeftClickTrayIcon => {
                tray_icon.show_menu().unwrap();
            }
            Events::Exit => {
                std::process::exit(0);
            }
            Events::Status => match &state.identity {
                Some(id) => println!("{id}"),
                None => println!("Nope"),
            },
        })
    });
    app.run();
}

// async fn stop_node(node: Node) -> Result<(), String> {
//     match node.handle(NodeCommand::Shutdown).await {
//         NodeResponse::ShutdownAccepted => Ok(()),
//         _ => Err("".to_string()),
//     }
// }
