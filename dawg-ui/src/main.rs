mod style;
mod transport;
mod views;

use dawg_core::{
    CreateServer, DashboardSnapshot, LogEntry, NetworkConfig, Request, Response, ServerConfig,
    ServerKind, ServerSnapshot, ServerStatus, VersionCatalog, VersionChannel,
};
use iced::{
    Font, Size, Subscription, Task, Theme,
    widget::{combo_box, text_editor},
};
use std::time::Duration;
use style::{ACCENT, AMBER, BG, INK, RED};

pub fn main() -> iced::Result {
    iced::application(Dawg::new, Dawg::update, Dawg::view)
        .title("Dawg")
        .font(include_bytes!("../../assets/fonts/source-sans-3/SourceSans3-Regular.ttf").as_slice())
        .font(
            include_bytes!("../../assets/fonts/source-sans-3/SourceSans3-Semibold.ttf").as_slice(),
        )
        .default_font(Font::with_name("Source Sans 3"))
        .theme(Theme::custom(
            "Dawg",
            iced::theme::Palette {
                background: BG,
                text: INK,
                primary: ACCENT,
                success: style::SUCCESS,
                warning: AMBER,
                danger: RED,
            },
        ))
        .subscription(Dawg::subscription)
        .window(iced::window::Settings {
            size: Size::new(1240.0, 820.0),
            min_size: Some(Size::new(1050.0, 700.0)),
            ..Default::default()
        })
        .centered()
        .run()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Overview,
    Create,
    Server,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Console,
    Settings,
    Properties,
    Network,
}
#[derive(Debug, Clone, Copy)]
enum Field {
    Name,
    Version,
    Memory,
    Port,
    Java,
    Jar,
    PublicHost,
    PublicPort,
}
#[derive(Debug, Clone, Copy)]
enum Action {
    Create,
    Mutate,
    Command,
    LoadProperties,
    SaveProperties,
    Backup,
    Remove,
}

#[derive(Debug, Clone)]
enum Message {
    Connected(Result<Response, String>),
    Retry,
    Tick,
    Keyboard(iced::keyboard::Event),
    Polled(PollResult),
    Navigate(Page),
    Select(String),
    Tab(Tab),
    Draft(Field, String),
    Kind(ServerKind),
    Catalog(ServerKind, Result<Response, String>),
    ShowSnapshots(bool),
    Tunnel(bool, bool),
    Upnp(bool, bool),
    AcceptEula(bool),
    Create,
    Settings(Field, String),
    SaveSettings,
    Run(Request, Action),
    Finished(Action, Result<Response, String>),
    Command(String),
    SendCommand,
    EditProperties(text_editor::Action),
    SaveProperties,
    Follow(bool),
    ConfirmRemove(bool),
    Remove,
    OpenFolder(String),
    Opened(Result<(), String>),
    Dismiss,
}

#[derive(Debug, Clone)]
struct PollResult {
    snapshot: Result<DashboardSnapshot, String>,
    logs: Option<(String, Result<Vec<LogEntry>, String>)>,
}

#[derive(Debug, Clone)]
struct Draft {
    name: String,
    kind: ServerKind,
    version: String,
    memory: String,
    port: String,
    java: String,
    jar: String,
    eula: bool,
    public_host: String,
    public_port: String,
    tunnel: bool,
    upnp: bool,
}
impl Default for Draft {
    fn default() -> Self {
        Self {
            name: String::new(),
            kind: ServerKind::Fabric,
            version: String::new(),
            memory: "2048".into(),
            port: "25565".into(),
            java: "auto".into(),
            jar: String::new(),
            eula: false,
            public_host: String::new(),
            public_port: String::new(),
            tunnel: false,
            upnp: false,
        }
    }
}
impl Draft {
    fn from_config(config: &ServerConfig) -> Self {
        Self {
            name: config.name.clone(),
            kind: config.kind,
            version: config.minecraft_version.clone(),
            memory: config.memory_mb.to_string(),
            port: config.port.to_string(),
            java: config.java_path.clone(),
            jar: String::new(),
            eula: config.eula_accepted,
            public_host: config.network.public_host.clone().unwrap_or_default(),
            public_port: config
                .network
                .external_port
                .map(|p| p.to_string())
                .unwrap_or_default(),
            tunnel: config.network.tunnel_enabled,
            upnp: config.network.upnp_enabled,
        }
    }
    fn edit(&mut self, field: Field, value: String) {
        match field {
            Field::Name => self.name = value,
            Field::Version => self.version = value,
            Field::Memory => self.memory = value,
            Field::Port => self.port = value,
            Field::Java => self.java = value,
            Field::Jar => self.jar = value,
            Field::PublicHost => self.public_host = value,
            Field::PublicPort => self.public_port = value,
        }
    }
    fn network(&self) -> Result<NetworkConfig, String> {
        let config = NetworkConfig {
            public_host: (!self.public_host.trim().is_empty())
                .then(|| self.public_host.trim().to_owned()),
            external_port: if self.public_port.trim().is_empty() {
                None
            } else {
                Some(
                    self.public_port
                        .parse()
                        .map_err(|_| "Enter a public port between 1 and 65535.")?,
                )
            },
            tunnel_enabled: self.tunnel,
            upnp_enabled: self.upnp,
        };
        config.validate()?;
        Ok(config)
    }
    fn validate(&self, require_eula: bool) -> Result<(u32, u16), String> {
        if self.name.trim().is_empty() || self.name.chars().count() > 64 {
            return Err("Give your server a name of 1–64 characters.".into());
        }
        let memory = self
            .memory
            .parse::<u32>()
            .map_err(|_| "Memory must be a number of MiB.".to_string())?;
        if !(512..=131_072).contains(&memory) {
            return Err("Choose between 512 and 131072 MiB of memory.".into());
        }
        let port = self
            .port
            .parse::<u16>()
            .map_err(|_| "Choose a port between 1024 and 65535.".to_string())?;
        if port < 1024 {
            return Err("Choose a port between 1024 and 65535.".into());
        }
        if self.java.trim().is_empty() {
            return Err("Enter a Java executable or its full path.".into());
        }
        if self.version.trim().is_empty() {
            return Err("Enter a Minecraft version, such as 1.21.11.".into());
        }
        if require_eula && !self.eula {
            return Err("Read and accept the Minecraft EULA before creating a server.".into());
        }
        self.network()?;
        Ok((memory, port))
    }
}

struct Dawg {
    snapshot: Option<DashboardSnapshot>,
    selected: Option<String>,
    page: Page,
    tab: Tab,
    connected: bool,
    connecting: bool,
    polling: bool,
    busy: bool,
    connection_error: Option<String>,
    notice: Option<(String, bool)>,
    draft: Draft,
    settings: Draft,
    logs: Vec<LogEntry>,
    cursor: u64,
    command: String,
    follow: bool,
    properties: text_editor::Content,
    properties_dirty: bool,
    properties_loaded: bool,
    confirm_remove: bool,
    catalog: Option<VersionCatalog>,
    version_options: combo_box::State<String>,
    catalog_error: Option<String>,
    catalog_loading: bool,
    show_snapshots: bool,
}

impl Dawg {
    fn new() -> (Self, Task<Message>) {
        (
            Self {
                snapshot: None,
                selected: None,
                page: Page::Overview,
                tab: Tab::Console,
                connected: false,
                connecting: true,
                polling: false,
                busy: false,
                connection_error: None,
                notice: None,
                draft: Draft::default(),
                settings: Draft::default(),
                logs: Vec::new(),
                cursor: 0,
                command: String::new(),
                follow: true,
                properties: text_editor::Content::new(),
                properties_dirty: false,
                properties_loaded: false,
                confirm_remove: false,
                catalog: None,
                version_options: combo_box::State::new(Vec::new()),
                catalog_error: None,
                catalog_loading: false,
                show_snapshots: false,
            },
            Task::perform(transport::connect(), Message::Connected),
        )
    }
    fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            iced::time::every(Duration::from_secs(1)).map(|_| Message::Tick),
            iced::keyboard::listen().map(Message::Keyboard),
        ])
    }
    fn selected_server(&self) -> Option<&ServerSnapshot> {
        self.snapshot
            .as_ref()?
            .servers
            .iter()
            .find(|s| Some(&s.config.id) == self.selected.as_ref())
    }
    fn run(&mut self, request: Request, action: Action) -> Task<Message> {
        if self.busy || !self.connected {
            return Task::none();
        }
        self.busy = true;
        self.notice = None;
        Task::perform(transport::request(request), move |result| {
            Message::Finished(action, result)
        })
    }
    fn poll(&mut self) -> Task<Message> {
        if !self.connected || self.polling {
            return Task::none();
        }
        self.polling = true;
        let selected = self.selected.clone();
        let after = self.cursor;
        Task::perform(
            async move {
                let snapshot = async {
                    match transport::request(Request::Snapshot).await? {
                        Response::Snapshot(snapshot) => Ok(snapshot),
                        Response::Error(error) => Err(error),
                        _ => Err("Unexpected dashboard response.".into()),
                    }
                };
                let logs = async {
                    if let Some(id) = selected {
                        let result = match transport::request(Request::Logs {
                            id: id.clone(),
                            after,
                        })
                        .await
                        {
                            Ok(Response::Logs { entries, .. }) => Ok(entries),
                            Ok(Response::Error(error)) | Err(error) => Err(error),
                            _ => Err("Unexpected console response.".into()),
                        };
                        Some((id, result))
                    } else {
                        None
                    }
                };
                let (snapshot, logs) = tokio::join!(snapshot, logs);
                PollResult { snapshot, logs }
            },
            Message::Polled,
        )
    }
    fn load_catalog(&mut self) -> Task<Message> {
        self.catalog = None;
        self.catalog_error = None;
        self.catalog_loading = true;
        self.version_options = combo_box::State::new(Vec::new());
        let kind = self.draft.kind;
        Task::perform(
            transport::request(Request::VersionCatalog { kind }),
            move |result| Message::Catalog(kind, result),
        )
    }
    fn filter_versions(&mut self) {
        let choices = self
            .catalog
            .as_ref()
            .map(|catalog| {
                catalog
                    .versions
                    .iter()
                    .filter(|v| self.show_snapshots || v.channel == VersionChannel::Release)
                    .map(|v| v.id.clone())
                    .collect()
            })
            .unwrap_or_default();
        self.version_options = combo_box::State::new(choices);
    }
    fn select(&mut self, id: String) {
        if self.selected.as_ref() != Some(&id) {
            self.logs.clear();
            self.cursor = 0;
            self.command.clear();
            self.properties = text_editor::Content::new();
            self.properties_dirty = false;
            self.properties_loaded = false;
        }
        self.selected = Some(id);
        self.page = Page::Server;
        self.tab = Tab::Console;
        self.confirm_remove = false;
        if let Some(server) = self.selected_server() {
            self.settings = Draft::from_config(&server.config);
        }
    }
    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Connected(result) => {
                self.connecting = false;
                match result {
                    Ok(Response::Snapshot(snapshot)) => {
                        self.connected = true;
                        self.connection_error = None;
                        self.cursor = 0;
                        self.logs.clear();
                        self.properties_loaded = false;
                        let selected = self
                            .selected
                            .clone()
                            .filter(|id| {
                                snapshot
                                    .servers
                                    .iter()
                                    .any(|server| &server.config.id == id)
                            })
                            .or_else(|| {
                                snapshot
                                    .servers
                                    .first()
                                    .map(|server| server.config.id.clone())
                            });
                        self.snapshot = Some(snapshot);
                        if let Some(id) = selected {
                            self.select(id);
                            return self.poll();
                        }
                        self.selected = None;
                        self.page = Page::Overview;
                    }
                    Ok(Response::Error(error)) | Err(error) => {
                        self.connected = false;
                        self.connection_error = Some(error);
                    }
                    _ => {
                        self.connected = false;
                        self.connection_error =
                            Some("Unexpected response from Dawg daemon.".into());
                    }
                }
            }
            Message::Retry => {
                if !self.connecting {
                    self.connecting = true;
                    return Task::perform(transport::connect(), Message::Connected);
                }
            }
            Message::Tick => return self.poll(),
            Message::Keyboard(iced::keyboard::Event::KeyPressed {
                key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Tab),
                modifiers,
                ..
            }) => {
                return if modifiers.shift() {
                    iced::widget::operation::focus_previous()
                } else {
                    iced::widget::operation::focus_next()
                };
            }
            Message::Keyboard(_) => {}
            Message::Polled(result) => {
                self.polling = false;
                match result.snapshot {
                    Ok(snapshot) => {
                        self.snapshot = Some(snapshot);
                        self.connected = true;
                        self.connection_error = None;
                    }
                    Err(error) => {
                        self.connected = false;
                        self.connection_error = Some(error);
                    }
                }
                if let Some((id, Ok(entries))) = result.logs
                    && self.selected.as_ref() == Some(&id)
                {
                    for entry in entries {
                        if entry.seq > self.cursor {
                            self.cursor = entry.seq;
                            self.logs.push(entry);
                        }
                    }
                    if self.logs.len() > 1500 {
                        self.logs.drain(..self.logs.len() - 1500);
                    }
                    if self.follow {
                        return iced::widget::operation::snap_to_end("console-output");
                    }
                }
            }
            Message::Navigate(page) => {
                self.page = page;
                self.notice = None;
                self.confirm_remove = false;
                if page == Page::Create {
                    self.draft = Draft::default();
                    if let Some(snapshot) = &self.snapshot {
                        let used: Vec<_> = snapshot.servers.iter().map(|s| s.config.port).collect();
                        if let Some(port) = (25565u16..=65535).find(|p| !used.contains(p)) {
                            self.draft.port = port.to_string();
                        }
                    }
                    return self.load_catalog();
                }
            }
            Message::Select(id) => {
                self.select(id);
                return self.poll();
            }
            Message::Tab(tab) => {
                self.tab = tab;
                self.confirm_remove = false;
                if matches!(tab, Tab::Settings | Tab::Network)
                    && let Some(server) = self.selected_server()
                {
                    self.settings = Draft::from_config(&server.config);
                }
                if tab == Tab::Properties
                    && !self.properties_loaded
                    && let Some(id) = self.selected.clone()
                {
                    return self.run(Request::GetProperties(id), Action::LoadProperties);
                }
            }
            Message::Draft(field, value) => self.draft.edit(field, value),
            Message::Kind(kind) => {
                self.draft.kind = kind;
                if kind == ServerKind::Forge {
                    self.draft.jar.clear();
                }
                self.draft.version.clear();
                return self.load_catalog();
            }
            Message::Catalog(kind, result) => {
                if kind != self.draft.kind {
                    return Task::none();
                }
                self.catalog_loading = false;
                match result {
                    Ok(Response::VersionCatalog(catalog)) => {
                        if self.draft.version.is_empty() {
                            self.draft.version = catalog.latest_release.clone().unwrap_or_default();
                        }
                        self.catalog = Some(catalog);
                        self.filter_versions();
                    }
                    Ok(Response::Error(error)) | Err(error) => self.catalog_error = Some(error),
                    _ => {
                        self.catalog_error =
                            Some("The version provider returned an unexpected response.".into())
                    }
                }
            }
            Message::ShowSnapshots(show) => {
                self.show_snapshots = show;
                self.filter_versions();
            }
            Message::Tunnel(settings, enabled) => {
                let draft = if settings {
                    &mut self.settings
                } else {
                    &mut self.draft
                };
                draft.tunnel = enabled;
                if enabled {
                    draft.upnp = false;
                }
            }
            Message::Upnp(settings, enabled) => {
                let draft = if settings {
                    &mut self.settings
                } else {
                    &mut self.draft
                };
                draft.upnp = enabled;
                if enabled {
                    draft.tunnel = false;
                }
            }
            Message::AcceptEula(accepted) => self.draft.eula = accepted,
            Message::Create => match self.draft.validate(true) {
                Ok((memory_mb, port)) => {
                    let request = CreateServer {
                        name: self.draft.name.trim().into(),
                        kind: self.draft.kind,
                        minecraft_version: self.draft.version.trim().into(),
                        memory_mb,
                        port,
                        java_path: self.draft.java.trim().into(),
                        eula_accepted: self.draft.eula,
                        network: self.draft.network().expect("validated"),
                        import_jar: if self.draft.jar.trim().is_empty() {
                            None
                        } else {
                            Some(self.draft.jar.trim().into())
                        },
                    };
                    return self.run(Request::CreateServer(request), Action::Create);
                }
                Err(error) => self.notice = Some((error, true)),
            },
            Message::Settings(field, value) => self.settings.edit(field, value),
            Message::SaveSettings => match self.settings.validate(false) {
                Ok((memory_mb, port)) => {
                    if let Some(server) = self.selected_server() {
                        let mut config = server.config.clone();
                        config.name = self.settings.name.trim().into();
                        config.memory_mb = memory_mb;
                        config.port = port;
                        config.java_path = self.settings.java.trim().into();
                        config.network = self.settings.network().expect("validated");
                        return self.run(Request::UpdateServer(config), Action::Mutate);
                    }
                }
                Err(error) => self.notice = Some((error, true)),
            },
            Message::Run(request, action) => return self.run(request, action),
            Message::Finished(action, result) => {
                self.busy = false;
                match result {
                    Err(error) | Ok(Response::Error(error)) => self.notice = Some((error, true)),
                    Ok(response) => {
                        match response {
                            Response::Server(server) => {
                                let server = *server;
                                let id = server.config.id.clone();
                                let installed = server.installed;
                                if let Some(snapshot) = &mut self.snapshot {
                                    if let Some(existing) =
                                        snapshot.servers.iter_mut().find(|s| s.config.id == id)
                                    {
                                        *existing = server;
                                    } else {
                                        snapshot.servers.push(server);
                                    }
                                }
                                if matches!(action, Action::Create) {
                                    self.select(id);
                                    self.notice = Some((if installed {
                                        "Server imported. Start it when you are ready."
                                    } else {
                                        "Server created. Install the server software to get started."
                                    }.into(), false));
                                } else if matches!(action, Action::Mutate) {
                                    self.notice = Some(("Changes saved.".into(), false));
                                }
                            }
                            Response::Properties { id, content } => {
                                if self.selected.as_ref() == Some(&id) {
                                    self.properties = text_editor::Content::with_text(&content);
                                    self.properties_loaded = true;
                                    self.properties_dirty = false;
                                }
                            }
                            Response::BackupCreated { path } => {
                                self.notice = Some((format!("Backup saved to {path}"), false))
                            }
                            Response::Removed(id) => {
                                if let Some(snapshot) = &mut self.snapshot {
                                    snapshot.servers.retain(|s| s.config.id != id);
                                }
                                self.selected = None;
                                self.page = Page::Overview;
                                self.confirm_remove = false;
                                self.notice = Some((
                                    "Server removed from Dawg. Its files are still on disk.".into(),
                                    false,
                                ));
                            }
                            _ => {
                                match action {
                                    Action::SaveProperties => {
                                        self.properties_dirty = false;
                                        self.notice = Some(("server.properties saved. Changes apply on the next start.".into(), false));
                                    }
                                    Action::Command => {}
                                    Action::Mutate => {
                                        self.notice = Some((
                                            "Request accepted. Follow progress in the console."
                                                .into(),
                                            false,
                                        ))
                                    }
                                    _ => {}
                                }
                            }
                        }
                        return self.poll();
                    }
                }
            }
            Message::Command(command) => self.command = command,
            Message::SendCommand => {
                if let Some(id) = self.selected.clone()
                    && !self.command.trim().is_empty()
                {
                    let command = self.command.trim().to_string();
                    self.command.clear();
                    return self.run(Request::SendCommand { id, command }, Action::Command);
                }
            }
            Message::EditProperties(action) => {
                let is_edit = action.is_edit();
                if !is_edit
                    || self.selected_server().is_some_and(|s| {
                        !s.status.is_active() && s.status != ServerStatus::Installing
                    })
                {
                    self.properties.perform(action);
                    self.properties_dirty |= is_edit;
                }
            }
            Message::SaveProperties => {
                if let Some(id) = self.selected.clone() {
                    return self.run(
                        Request::SaveProperties {
                            id,
                            content: self.properties.text(),
                        },
                        Action::SaveProperties,
                    );
                }
            }
            Message::Follow(follow) => {
                self.follow = follow;
                if follow {
                    return iced::widget::operation::snap_to_end("console-output");
                }
            }
            Message::ConfirmRemove(confirm) => self.confirm_remove = confirm,
            Message::Remove => {
                if let Some(id) = self.selected.clone() {
                    return self.run(Request::RemoveServer(id), Action::Remove);
                }
            }
            Message::OpenFolder(path) => {
                return Task::perform(
                    async move {
                        let status = tokio::process::Command::new("xdg-open")
                            .arg(path)
                            .status()
                            .await
                            .map_err(|e| format!("Could not open folder: {e}"))?;
                        if status.success() {
                            Ok(())
                        } else {
                            Err("No file manager is available. The server directory is shown in Settings.".into())
                        }
                    },
                    Message::Opened,
                );
            }
            Message::Opened(Err(error)) => self.notice = Some((error, true)),
            Message::Opened(Ok(())) => {}
            Message::Dismiss => self.notice = None,
        }
        Task::none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn create_form_requires_explicit_eula_and_valid_runtime() {
        let mut draft = Draft {
            name: "World".into(),
            ..Draft::default()
        };
        assert!(draft.validate(true).is_err());
        draft.eula = true;
        draft.version = "1.21.11".into();
        assert_eq!(draft.validate(true).unwrap(), (2048, 25565));
        draft.port = "0".into();
        assert!(draft.validate(true).is_err());
        draft.port = "25565".into();
        draft.memory = "-1".into();
        assert!(draft.validate(true).is_err());
    }

    #[test]
    fn default_software_is_fabric_and_versions_are_loaded_from_the_provider() {
        let (mut app, _) = Dawg::new();
        assert_eq!(app.draft.kind, ServerKind::Fabric);
        assert!(app.draft.version.is_empty());
        let catalog = VersionCatalog {
            kind: ServerKind::Fabric,
            latest_release: Some("26.3".into()),
            latest_snapshot: Some("26.4-snapshot-5".into()),
            versions: vec![
                dawg_core::MinecraftVersion {
                    id: "26.4-snapshot-5".into(),
                    channel: VersionChannel::Snapshot,
                },
                dawg_core::MinecraftVersion {
                    id: "26.3".into(),
                    channel: VersionChannel::Release,
                },
            ],
        };
        let _ = app.update(Message::Catalog(
            ServerKind::Fabric,
            Ok(Response::VersionCatalog(catalog)),
        ));
        assert_eq!(app.draft.version, "26.3");
        assert!(!app.catalog_loading);
        let _ = app.update(Message::ShowSnapshots(true));
        assert!(app.show_snapshots);
        assert_eq!(app.catalog.unwrap().versions[0].id, "26.4-snapshot-5");
    }

    #[test]
    fn a_delayed_catalog_cannot_select_a_version_for_a_different_loader() {
        let (mut app, _) = Dawg::new();
        app.draft.kind = ServerKind::Paper;
        app.catalog_loading = true;
        let _ = app.update(Message::Catalog(
            ServerKind::Fabric,
            Ok(Response::VersionCatalog(VersionCatalog {
                kind: ServerKind::Fabric,
                latest_release: Some("26.3".into()),
                latest_snapshot: None,
                versions: Vec::new(),
            })),
        ));
        assert!(app.catalog.is_none());
        assert!(app.draft.version.is_empty());
        assert!(app.catalog_loading);
    }
    #[test]
    fn selecting_a_different_server_clears_old_console_and_editor() {
        let (mut app, _) = Dawg::new();
        app.selected = Some("old".into());
        app.cursor = 99;
        app.command = "say old".into();
        app.properties_loaded = true;
        app.properties_dirty = true;
        app.select("new".into());
        assert_eq!(app.cursor, 0);
        assert!(app.command.is_empty());
        assert!(!app.properties_loaded);
        assert!(!app.properties_dirty);
    }

    #[test]
    fn reconnect_accepts_console_sequences_from_a_restarted_daemon() {
        let (mut app, _) = Dawg::new();
        app.cursor = 42;
        app.logs.push(LogEntry {
            seq: 42,
            timestamp_ms: 0,
            stream: "stdout".into(),
            line: "Old session".into(),
        });
        let _ = app.update(Message::Connected(Ok(Response::Snapshot(
            DashboardSnapshot {
                protocol_version: dawg_core::PROTOCOL_VERSION,
                data_directory: "/test".into(),
                host: Default::default(),
                servers: Vec::new(),
            },
        ))));
        assert_eq!(app.cursor, 0);
        assert!(app.logs.is_empty());
        assert!(app.connected);
    }

    fn server_fixture(id: &str) -> ServerSnapshot {
        ServerSnapshot {
            config: ServerConfig {
                id: id.into(),
                name: format!("Server {id}"),
                directory: format!("/test/{id}"),
                kind: ServerKind::Vanilla,
                minecraft_version: "1.21.11".into(),
                memory_mb: 2048,
                port: 25565,
                java_path: "java".into(),
                eula_accepted: true,
                network: Default::default(),
            },
            status: ServerStatus::Stopped,
            installed: true,
            pid: None,
            cpu_percent: 0.0,
            memory_bytes: 0,
            uptime_seconds: None,
            last_error: None,
            network: Default::default(),
        }
    }

    fn connected_snapshot(servers: Vec<ServerSnapshot>) -> Message {
        Message::Connected(Ok(Response::Snapshot(DashboardSnapshot {
            protocol_version: dawg_core::PROTOCOL_VERSION,
            data_directory: "/test".into(),
            host: Default::default(),
            servers,
        })))
    }

    #[test]
    fn opening_existing_server_goes_directly_to_its_console() {
        let (mut app, _) = Dawg::new();
        let _ = app.update(connected_snapshot(vec![server_fixture("one")]));
        assert_eq!(app.selected.as_deref(), Some("one"));
        assert_eq!(app.page, Page::Server);
        assert_eq!(app.tab, Tab::Console);
        assert!(
            app.polling,
            "Fetch the console immediately instead of waiting for the timer"
        );
    }

    #[test]
    fn reconnect_keeps_the_selected_server_instead_of_the_first_row() {
        let (mut app, _) = Dawg::new();
        app.selected = Some("two".into());
        let _ = app.update(connected_snapshot(vec![
            server_fixture("one"),
            server_fixture("two"),
        ]));
        assert_eq!(app.selected.as_deref(), Some("two"));
        assert_eq!(app.page, Page::Server);
        assert_eq!(app.tab, Tab::Console);
    }

    #[test]
    fn empty_installation_keeps_setup_available_without_a_sample_server() {
        let (mut app, _) = Dawg::new();
        let _ = app.update(connected_snapshot(vec![]));
        assert!(app.selected.is_none());
        assert_eq!(app.page, Page::Overview);
        assert!(app.snapshot.as_ref().unwrap().servers.is_empty());
    }

    #[test]
    fn delayed_properties_response_does_not_replace_another_servers_editor() {
        let (mut app, _) = Dawg::new();
        app.selected = Some("current".into());
        app.properties = text_editor::Content::with_text("motd=Current server");
        let _ = app.update(Message::Finished(
            Action::LoadProperties,
            Ok(Response::Properties {
                id: "previous".into(),
                content: "motd=Wrong server".into(),
            }),
        ));
        assert_eq!(app.properties.text().trim(), "motd=Current server");
    }
}
