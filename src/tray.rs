use trayicon::*;

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum Events {
    RightClickTrayIcon,
    LeftClickTrayIcon,
    Exit,
    Status,
    // CheckItem1,
}

pub fn build_menu() -> MenuBuilder<Events> {
    MenuBuilder::new()
        .item("Status", Events::Status)
        .submenu(
            "My apps",
            MenuBuilder::new(),
            //     .item("Sub Item 1", Events::SubItem1)
            //     .item("Sub Item 2", Events::SubItem2)
            //     .item("Sub Item 3", Events::SubItem3),
        )
        .separator()
        .item("Quit", Events::Exit)
}
