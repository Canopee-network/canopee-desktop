use trayicon::*;

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum Events {
    RightClickTrayIcon,
    LeftClickTrayIcon,
    Exit,
}

pub fn build_menu() -> MenuBuilder<Events> {
    MenuBuilder::new()
        .submenu("My apps", MenuBuilder::new())
        .separator()
        .item("Quit", Events::Exit)
}
