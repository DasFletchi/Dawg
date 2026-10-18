use iced::{
    Element, Length,
    widget::{button, column, container, row, scrollable, text},
};

#[derive(Debug, Clone)]
pub enum ServerListMessage {
    StartServer(String),
    StopServer(String),
    OpenFolder(String),
}

pub struct ServerList {
    _selected_server: Option<String>,
}

impl ServerList {
    pub fn new() -> Self {
        Self {
            _selected_server: None,
        }
    }

    pub fn view<'a>(&'a self) -> Element<'a, ServerListMessage> {
        let server_item = row![
            text("My Fabric Server").width(Length::Fill),
            button("Start").on_press(ServerListMessage::StartServer("1".to_string())),
            button("Stop").on_press(ServerListMessage::StopServer("1".to_string())),
            button("Folder").on_press(ServerListMessage::OpenFolder("1".to_string())),
        ]
        .spacing(10)
        .padding(10);

        let list = scrollable(column![server_item].spacing(5));

        container(list)
            .width(Length::Fixed(300.0))
            .height(Length::Fill)
            .into()
    }
}
