mod ui;
mod ipc;

use iced::{Element, Task, widget::{row, container}, Length};
use ui::{console::{Console, ConsoleMessage}, server_list::{ServerList, ServerListMessage}};
use ipc::message::{DaemonRequest, DaemonResponse};
use tokio::net::UnixStream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn get_socket_path() -> String {
    let uid = unsafe { libc::getuid() };
    format!("/run/user/{}/dawg.sock", uid)
}

pub fn main() -> iced::Result {
    iced::application(
        Dawg::title,
        Dawg::update,
        Dawg::view,
    ).run()
}

struct Dawg {
    server_list: ServerList,
    console: Console,
}

impl Default for Dawg {
    fn default() -> Self {
        Self {
            server_list: ServerList::new(),
            console: Console::new(),
        }
    }
}

impl Dawg {
    fn title() -> (Dawg, Task<Message>) {
        (Dawg::default(), Task::none())
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::ServerList(msg) => {
                match msg {
                    ServerListMessage::StartServer(id) => {
                        return Task::perform(
                            send_daemon_request(DaemonRequest::StartServer(id)),
                            |res| match res {
                                Some(r) => Message::DaemonResponse(r),
                                None => Message::DaemonResponse(DaemonResponse::Error("Failed to connect to daemon".to_string())),
                            }
                        );
                    }
                    ServerListMessage::StopServer(id) => {
                         return Task::perform(
                            send_daemon_request(DaemonRequest::StopServer(id)),
                            |res| match res {
                                Some(r) => Message::DaemonResponse(r),
                                None => Message::DaemonResponse(DaemonResponse::Error("Failed to connect to daemon".to_string())),
                            }
                        );
                    }
                    ServerListMessage::OpenFolder(id) => {
                        println!("Open folder for server {}", id);
                        let _ = std::process::Command::new("xdg-open")
                            .arg(".")
                            .spawn();
                    },
                }
            }
            Message::Console(_msg) => {}
            Message::DaemonResponse(res) => {
                println!("Got response from daemon: {:?}", res);
            }
        }
        Task::none()
    }

    fn view(&self) -> Element<'_, Message> {
        let content = row![
            self.server_list.view().map(Message::ServerList),
            self.console.view().map(Message::Console),
        ]
        .width(Length::Fill)
        .height(Length::Fill);

        container(content)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }
}

#[derive(Debug, Clone)]
enum Message {
    ServerList(ServerListMessage),
    Console(ConsoleMessage),
    DaemonResponse(DaemonResponse),
}

async fn send_daemon_request(req: DaemonRequest) -> Option<DaemonResponse> {
    let socket_path = get_socket_path();
    if let Ok(mut stream) = UnixStream::connect(&socket_path).await {
        if let Ok(req_str) = serde_json::to_string(&req) {
            let _ = stream.write_all(req_str.as_bytes()).await;
            let mut buf = vec![0; 4096];
            if let Ok(n) = stream.read(&mut buf).await {
                let res_str = String::from_utf8_lossy(&buf[..n]);
                if let Ok(res) = serde_json::from_str::<DaemonResponse>(&res_str) {
                    return Some(res);
                }
            }
        }
    }
    None
}
