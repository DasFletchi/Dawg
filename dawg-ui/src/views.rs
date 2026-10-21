//! Desktop views. Application state and effects stay in `main`.

use super::*;
use iced::{
    Alignment, Border, Color, Element, Font, Length,
    widget::{
        Column, Space, button, checkbox, column, combo_box, container, pick_list, row, rule,
        scrollable, text, text_editor, text_input, tooltip,
    },
};
use style::{ACCENT, AMBER, BG, ButtonKind, CONSOLE, INK, MUTED, PANEL, RED, SIDE, SUCCESS};

impl Dawg {
    pub(super) fn view(&self) -> Element<'_, Message> {
        let mut workspace = Column::new().spacing(14).height(Length::Fill);
        if !self.connected && !self.connecting {
            workspace = workspace.push(self.connection_banner());
        }
        if let Some((message, true)) = &self.notice {
            workspace = workspace.push(
                row![
                    text(message).size(13).color(RED).width(Length::Fill),
                    action("Dismiss", Some(Message::Dismiss), ButtonKind::Ghost)
                ]
                .spacing(12)
                .align_y(Alignment::Center),
            );
        }
        workspace = workspace.push(match self.page {
            Page::Overview => self.overview(),
            Page::Create => self.create_view(),
            Page::Server => self.server_view(),
        });
        let body = row![
            self.sidebar(),
            rule::vertical(1),
            container(workspace)
                .padding([16, 20])
                .width(Length::Fill)
                .height(Length::Fill)
        ]
        .height(Length::Fill);
        container(column![body, rule::horizontal(1), self.status_bar()])
            .width(Length::Fill)
            .height(Length::Fill)
            .style(|_| style::plain(BG))
            .into()
    }

    fn sidebar(&self) -> Element<'_, Message> {
        let mut servers = Column::new().spacing(2).width(Length::Fill);
        if let Some(snapshot) = &self.snapshot {
            for server in &snapshot.servers {
                let selected =
                    self.page == Page::Server && self.selected.as_ref() == Some(&server.config.id);
                let entry = button(
                    row![
                        status_dot(server.status),
                        column![
                            text(shorten(&server.config.name, 20)).size(14),
                            text(server.status.to_string()).size(12).color(MUTED)
                        ]
                        .spacing(2)
                        .width(Length::Fill)
                    ]
                    .spacing(9)
                    .align_y(Alignment::Center),
                )
                .on_press(Message::Select(server.config.id.clone()))
                .padding([9, 10])
                .width(Length::Fill)
                .style(style::action(if selected {
                    ButtonKind::Selected
                } else {
                    ButtonKind::Ghost
                }));
                servers = servers.push(tooltip(
                    entry,
                    text(&server.config.name).size(13),
                    tooltip::Position::Right,
                ));
            }
        }
        if self.snapshot.as_ref().is_none_or(|s| s.servers.is_empty()) {
            servers = servers.push(
                container(
                    text(if self.connecting {
                        "Connecting…"
                    } else {
                        "No servers"
                    })
                    .size(13)
                    .color(MUTED),
                )
                .padding([8, 10]),
            );
        }
        container(
            column![
                container(text("Dawg").size(20).font(semibold())).padding([6, 10]),
                button(text("All servers").size(14))
                    .padding([8, 10])
                    .width(Length::Fill)
                    .on_press(Message::Navigate(Page::Overview))
                    .style(style::action(if self.page == Page::Overview {
                        ButtonKind::Selected
                    } else {
                        ButtonKind::Ghost
                    })),
                container(text("Servers").size(12).color(MUTED)).padding([8, 10]),
                scrollable(servers).height(Length::Fill)
            ]
            .spacing(8),
        )
        .padding(10)
        .width(190)
        .height(Length::Fill)
        .style(|_| style::plain(SIDE))
        .into()
    }

    fn status_bar(&self) -> Element<'_, Message> {
        let host_memory = self.snapshot.as_ref().map_or_else(String::new, |snapshot| {
            format!(
                "Host memory: {} / {}",
                bytes(snapshot.host.memory_used_bytes),
                bytes(snapshot.host.memory_total_bytes)
            )
        });
        let feedback: Element<'_, Message> = if let Some((message, false)) = &self.notice {
            tooltip(
                text(shorten(message, 64))
                    .size(12)
                    .color(MUTED)
                    .width(Length::Fill),
                text(message).size(13),
                tooltip::Position::Top,
            )
            .into()
        } else {
            Space::new().width(Length::Fill).into()
        };
        container(
            row![feedback, text(host_memory).size(12).color(MUTED)]
                .spacing(8)
                .align_y(Alignment::Center),
        )
        .padding([5, 14])
        .width(Length::Fill)
        .style(|_| style::plain(SIDE))
        .into()
    }

    fn connection_banner(&self) -> Element<'_, Message> {
        container(
            row![
                column![
                    text("Cannot reach the local manager")
                        .size(14)
                        .font(semibold()),
                    text(
                        self.connection_error
                            .as_deref()
                            .unwrap_or("Reconnect to load your servers and send commands.")
                    )
                    .size(13)
                    .color(MUTED)
                ]
                .spacing(4)
                .width(Length::Fill),
                action("Reconnect", Some(Message::Retry), ButtonKind::Secondary)
            ]
            .spacing(14)
            .align_y(Alignment::Center),
        )
        .padding(12)
        .width(Length::Fill)
        .style(|_| style::surface(PANEL, 4.0))
        .into()
    }

    fn overview(&self) -> Element<'_, Message> {
        let content = column![
            row![
                text("Servers")
                    .size(22)
                    .font(semibold())
                    .width(Length::Fill),
                action(
                    "New server",
                    (self.connected && !self.busy).then_some(Message::Navigate(Page::Create)),
                    ButtonKind::Primary
                )
            ]
            .align_y(Alignment::Center),
        ]
        .spacing(18);
        let Some(snapshot) = &self.snapshot else {
            return content
                .push(
                    text(if self.connecting {
                        "Connecting to your server manager…"
                    } else {
                        "Reconnect to load your servers."
                    })
                    .size(14)
                    .color(MUTED),
                )
                .into();
        };
        if snapshot.servers.is_empty() {
            return content
                .push(
                    column![
                        text("No servers yet").size(16).font(semibold()),
                        text("Choose New server to install Fabric, Vanilla, Paper or Forge, or import a server JAR.")
                            .size(14)
                            .color(MUTED)
                    ]
                    .spacing(8),
                )
                .into();
        }
        let mut table = column![
            container(
                row![
                    table_label("Name").width(Length::Fill),
                    table_label("Software").width(155),
                    table_label("Address").width(135),
                    table_label("Status").width(90),
                    table_label("Memory").width(82)
                ]
                .spacing(12),
            )
            .padding([8, 12]),
            rule::horizontal(1)
        ]
        .spacing(0);
        for server in &snapshot.servers {
            table = table
                .push(self.server_row(server))
                .push(rule::horizontal(1));
        }
        let running = snapshot
            .servers
            .iter()
            .filter(|server| server.status == ServerStatus::Running)
            .count();
        content
            .push(scrollable(table).height(Length::Fill))
            .push(
                text(format!(
                    "{} registered, {running} running",
                    snapshot.servers.len()
                ))
                .size(12)
                .color(MUTED),
            )
            .height(Length::Fill)
            .into()
    }

    fn server_row<'a>(&'a self, server: &'a ServerSnapshot) -> Element<'a, Message> {
        button(
            row![
                text(shorten(&server.config.name, 38))
                    .size(14)
                    .width(Length::Fill),
                text(format!(
                    "{} {}",
                    server.config.kind, server.config.minecraft_version
                ))
                .size(13)
                .color(MUTED)
                .width(155),
                text(
                    server
                        .network
                        .join_address
                        .as_deref()
                        .unwrap_or("Public access not configured")
                )
                .size(13)
                .color(MUTED)
                .width(135),
                container(status_label(server.status)).width(90),
                text(if server.pid.is_some() {
                    bytes(server.memory_bytes)
                } else {
                    "—".into()
                })
                .size(13)
                .color(MUTED)
                .width(82)
            ]
            .spacing(12)
            .align_y(Alignment::Center),
        )
        .padding([13, 12])
        .width(Length::Fill)
        .on_press(Message::Select(server.config.id.clone()))
        .style(style::action(ButtonKind::Ghost))
        .into()
    }

    fn create_view(&self) -> Element<'_, Message> {
        let draft = &self.draft;
        let enabled = self.connected && !self.busy;
        let validation = draft.validate(true);
        let mut form = column![
            text("New server").size(22).font(semibold()),
            field("Server name", "Name", &draft.name, enabled, |value| Message::Draft(Field::Name, value)),
            row![
                column![
                    label("Software"),
                    pick_list(&ServerKind::ALL[..], Some(draft.kind), Message::Kind)
                        .padding(8)
                        .text_size(14)
                        .width(Length::Fill)
                ]
                .spacing(5)
                .width(Length::Fill),
                column![
                    label("Minecraft version"),
                    combo_box(&self.version_options, "Search or enter an exact version", (!draft.version.is_empty()).then_some(&draft.version), |v| Message::Draft(Field::Version, v))
                        .on_input(|v| Message::Draft(Field::Version, v)).padding(8).size(14),
                    checkbox(self.show_snapshots).label("Include snapshots and pre-releases")
                        .on_toggle(Message::ShowSnapshots).size(14).text_size(12),
                ].spacing(5).width(Length::Fill)
            ]
            .spacing(16),
            text(match draft.kind {
                ServerKind::Vanilla => "Vanilla uses Mojang's official server software.",
                ServerKind::Paper => "Paper supports Bukkit and Spigot server plugins.",
                ServerKind::Fabric => "Fabric supports server-side Fabric mods.",
                ServerKind::Forge => "Forge supports server-side Forge mods on supported Minecraft versions.",
            })
            .size(12)
            .color(MUTED),
            section_title("Runtime"),
            row![
                field("Memory (MiB)", "2048", &draft.memory, enabled, |value| Message::Draft(Field::Memory, value)),
                field("Port", "25565", &draft.port, enabled, |value| Message::Draft(Field::Port, value))
            ]
            .spacing(16),
            field("Java executable", "auto or /path/to/java", &draft.java, enabled, |value| Message::Draft(Field::Java, value)),
            text("Auto chooses the Java version required by official game metadata and installs a verified runtime if needed. A custom Java executable remains available.")
                .size(12)
                .color(MUTED),
            self.network_fields(draft, enabled, false),
            field("Existing server JAR (optional)", "/path/to/server.jar", &draft.jar, enabled && draft.kind != ServerKind::Forge, |value| Message::Draft(Field::Jar, value)),
            text(if draft.kind == ServerKind::Forge { "Forge uses its official installer. Upload mods after installation." } else { "Leave empty to download server software after creating the server." })
                .size(12)
                .color(MUTED),
            row![
                checkbox(draft.eula)
                    .label("I accept the Minecraft EULA")
                    .on_toggle_maybe(enabled.then_some(Message::AcceptEula))
                    .size(16)
                    .text_size(13),
                action("Read EULA", Some(Message::OpenFolder("https://www.minecraft.net/eula".into())), ButtonKind::Ghost)
            ]
            .spacing(12)
            .align_y(Alignment::Center)
        ]
        .spacing(12);
        if self.catalog_loading {
            form = form.push(
                text("Loading versions from the official provider…")
                    .size(12)
                    .color(MUTED),
            );
        }
        if let Some(error) = &self.catalog_error {
            form = form.push(
                text(format!(
                    "Version lookup failed: {error}. You can enter an exact version above."
                ))
                .size(12)
                .color(AMBER),
            );
        }
        if let Err(reason) = &validation {
            form = form.push(text(reason.clone()).size(13).color(MUTED));
        }
        form = form.push(
            row![
                action(
                    if self.busy {
                        "Creating…"
                    } else {
                        "Create server"
                    },
                    (enabled && validation.is_ok()).then_some(Message::Create),
                    ButtonKind::Primary
                ),
                action(
                    "Cancel",
                    Some(
                        self.selected
                            .as_ref()
                            .map_or(Message::Navigate(Page::Overview), |id| Message::Select(
                                id.clone()
                            ),)
                    ),
                    ButtonKind::Secondary
                )
            ]
            .spacing(8),
        );
        scrollable(container(form).max_width(700).padding([0, 0]))
            .height(Length::Fill)
            .into()
    }

    fn server_view(&self) -> Element<'_, Message> {
        let Some(server) = self.selected_server() else {
            return column![
                text("Server unavailable").size(20).font(semibold()),
                text("Choose a server from the list, or open All servers.")
                    .size(14)
                    .color(MUTED)
            ]
            .spacing(8)
            .into();
        };
        let config = &server.config;
        let controls_enabled = self.connected && !self.busy;
        let stopped = !server.status.is_active() && server.status != ServerStatus::Installing;
        let mut controls = row![action(
            "Folder",
            Some(Message::OpenFolder(config.directory.clone())),
            ButtonKind::Secondary
        )]
        .spacing(8);
        if server.status.is_active() {
            controls = controls
                .push(action(
                    "Restart",
                    (controls_enabled && server.status == ServerStatus::Running).then_some(
                        Message::Run(Request::RestartServer(config.id.clone()), Action::Mutate),
                    ),
                    ButtonKind::Secondary,
                ))
                .push(action(
                    "Stop",
                    (controls_enabled
                        && matches!(
                            server.status,
                            ServerStatus::Running | ServerStatus::Starting
                        ))
                    .then_some(Message::Run(
                        Request::StopServer(config.id.clone()),
                        Action::Mutate,
                    )),
                    ButtonKind::Danger,
                ));
        } else if server.installed {
            controls = controls.push(action(
                "Start",
                (controls_enabled && stopped).then_some(Message::Run(
                    Request::StartServer(config.id.clone()),
                    Action::Mutate,
                )),
                ButtonKind::Primary,
            ));
        } else {
            controls = controls.push(action(
                if server.status == ServerStatus::Installing {
                    "Installing…"
                } else {
                    "Install"
                },
                (controls_enabled && stopped).then_some(Message::Run(
                    Request::InstallServer(config.id.clone()),
                    Action::Mutate,
                )),
                ButtonKind::Primary,
            ));
        }
        let header = row![
            column![
                row![
                    text(shorten(&config.name, 50)).size(22).font(semibold()),
                    status_label(server.status)
                ]
                .spacing(12)
                .align_y(Alignment::Center),
                row![
                    text(format!("{} {}", config.kind, config.minecraft_version))
                        .size(13)
                        .color(MUTED),
                    text(
                        server
                            .network
                            .join_address
                            .as_deref()
                            .unwrap_or("Public access not configured")
                    )
                    .size(13)
                    .color(MUTED)
                ]
                .spacing(18)
            ]
            .spacing(5)
            .width(Length::Fill),
            controls
        ]
        .spacing(16)
        .align_y(Alignment::Center);
        let resources = row![
            resource(
                "CPU",
                if server.pid.is_some() {
                    format!("{:.1}%", server.cpu_percent)
                } else {
                    "—".into()
                }
            ),
            resource(
                "Memory",
                if server.pid.is_some() {
                    format!("{} / {} MiB", bytes(server.memory_bytes), config.memory_mb)
                } else {
                    format!("{} MiB configured", config.memory_mb)
                }
            ),
            resource("Uptime", uptime(server.uptime_seconds)),
            Space::new().width(Length::Fill),
            text(if self.busy { "Working…" } else { "" })
                .size(12)
                .color(MUTED)
        ]
        .spacing(24)
        .align_y(Alignment::Center);
        let tabs = row![
            tab_button("Console", Tab::Console, self.tab),
            tab_button("Settings", Tab::Settings, self.tab),
            tab_button("Properties", Tab::Properties, self.tab),
            tab_button("Network", Tab::Network, self.tab)
        ]
        .spacing(16);
        let mut content = column![header, resources, tabs].spacing(12);
        if let Some(error) = &server.last_error {
            content = content.push(text(error).size(13).color(RED));
        }
        content
            .push(match self.tab {
                Tab::Console => self.console_view(server),
                Tab::Settings => self.settings_view(server),
                Tab::Properties => self.properties_view(server),
                Tab::Network => self.network_view(server),
            })
            .height(Length::Fill)
            .into()
    }

    fn console_view<'a>(&'a self, server: &'a ServerSnapshot) -> Element<'a, Message> {
        let mut lines = Column::new().spacing(3).padding(12).width(Length::Fill);
        if self.logs.is_empty() {
            lines = lines.push(
                text(if !server.installed {
                    "Install server software before starting this server."
                } else if server.status.is_active() {
                    "Waiting for server output…"
                } else {
                    "Start the server to see its output here."
                })
                .size(13)
                .color(MUTED),
            );
        }
        for entry in &self.logs {
            let color = if entry.stream == "error"
                || entry.line.contains("ERROR")
                || entry.line.contains("Exception")
            {
                RED
            } else if entry.line.contains("WARN") {
                AMBER
            } else if entry.stream == "command" {
                ACCENT
            } else {
                INK
            };
            lines = lines.push(
                row![
                    text(format_time(entry.timestamp_ms))
                        .size(12)
                        .font(Font::MONOSPACE)
                        .color(MUTED)
                        .width(72),
                    text(entry.line.replace('\t', "    "))
                        .size(13)
                        .font(Font::MONOSPACE)
                        .color(color)
                        .width(Length::Fill)
                ]
                .spacing(10),
            );
        }
        let enabled = self.connected && !self.busy && server.status == ServerStatus::Running;
        let mut command = text_input(
            if enabled {
                "Command, e.g. list or say Hello"
            } else {
                "Start the server to send commands"
            },
            &self.command,
        )
        .padding(8)
        .font(Font::MONOSPACE)
        .size(13)
        .style(style::input)
        .on_input_maybe(enabled.then_some(Message::Command));
        if enabled && !self.command.trim().is_empty() {
            command = command.on_submit(Message::SendCommand);
        }
        let toolbar = row![
            text("Times in UTC").size(12).color(MUTED),
            Space::new().width(Length::Fill),
            checkbox(self.follow)
                .label("Follow output")
                .size(15)
                .text_size(12)
                .on_toggle(Message::Follow)
        ]
        .align_y(Alignment::Center);
        column![
            toolbar,
            container(scrollable(lines).id("console-output").height(Length::Fill))
                .width(Length::Fill)
                .height(Length::Fill)
                .style(|_| style::plain(CONSOLE)),
            row![
                command,
                action(
                    "Send",
                    (enabled && !self.command.trim().is_empty()).then_some(Message::SendCommand),
                    ButtonKind::Secondary
                )
            ]
            .spacing(8)
            .align_y(Alignment::Center)
        ]
        .spacing(8)
        .height(Length::Fill)
        .into()
    }

    fn settings_view<'a>(&'a self, server: &'a ServerSnapshot) -> Element<'a, Message> {
        let editable = self.connected
            && !self.busy
            && !server.status.is_active()
            && server.status != ServerStatus::Installing;
        let config = &server.config;
        let mut content = column![
            text(if server.status.is_active() {
                "Stop the server before editing its settings."
            } else if server.status == ServerStatus::Installing {
                "Settings are read only while installation is in progress."
            } else {
                "Changes apply the next time the server starts."
            })
            .size(13)
            .color(MUTED),
            field("Server name", "Name", &self.settings.name, editable, |value| Message::Settings(Field::Name, value)),
            row![
                field("Memory (MiB)", "2048", &self.settings.memory, editable, |value| Message::Settings(Field::Memory, value)),
                field("Port", "25565", &self.settings.port, editable, |value| Message::Settings(Field::Port, value))
            ]
            .spacing(16),
            field("Java executable", "auto or /path/to/java", &self.settings.java, editable, |value| Message::Settings(Field::Java, value)),
            label("Server directory"),
            text(&config.directory).size(12).font(Font::MONOSPACE).color(MUTED),
            action("Save settings", (editable && self.settings.validate(false).is_ok()).then_some(Message::SaveSettings), ButtonKind::Primary),
            section_title("Backup"),
            text("Create a compressed archive while the server is stopped.")
                .size(13)
                .color(MUTED),
            action("Create backup", (editable && server.installed).then_some(Message::Run(Request::BackupServer(config.id.clone()), Action::Backup)), ButtonKind::Secondary),
            section_title("Server software"),
            text("Reinstall replaces server.jar. Back up first; world files and settings stay in place.")
                .size(13)
                .color(MUTED),
            action("Reinstall software", editable.then_some(Message::Run(Request::InstallServer(config.id.clone()), Action::Mutate)), ButtonKind::Secondary),
            section_title("Remove server")
        ]
        .spacing(12);
        if self.confirm_remove {
            content = content
                .push(
                    text("Remove this server from Dawg? Its files and backups will stay on disk.")
                        .size(13)
                        .color(MUTED),
                )
                .push(
                    row![
                        action(
                            "Remove server",
                            editable.then_some(Message::Remove),
                            ButtonKind::Danger
                        ),
                        action(
                            "Cancel",
                            Some(Message::ConfirmRemove(false)),
                            ButtonKind::Secondary
                        )
                    ]
                    .spacing(8),
                );
        } else {
            content = content
                .push(
                    text("Unregister the server without deleting its files.")
                        .size(13)
                        .color(MUTED),
                )
                .push(action(
                    "Remove server…",
                    editable.then_some(Message::ConfirmRemove(true)),
                    ButtonKind::Danger,
                ));
        }
        scrollable(container(content).max_width(700))
            .height(Length::Fill)
            .into()
    }

    fn network_fields<'a>(
        &self,
        draft: &'a Draft,
        editable: bool,
        settings: bool,
    ) -> Element<'a, Message> {
        let edit_host = move |v| {
            if settings {
                Message::Settings(Field::PublicHost, v)
            } else {
                Message::Draft(Field::PublicHost, v)
            }
        };
        let edit_port = move |v| {
            if settings {
                Message::Settings(Field::PublicPort, v)
            } else {
                Message::Draft(Field::PublicPort, v)
            }
        };
        column![
            section_title("Public access"),
            checkbox(draft.tunnel).label("Free stable address with Playit")
                .on_toggle_maybe(editable.then_some(move |v| Message::Tunnel(settings, v))).size(16).text_size(13),
            text("Link a free provider account after saving. The assigned address survives restarts and changing home IPs.").size(12).color(MUTED),
            checkbox(draft.upnp).label("Forward the TCP port through my router (UPnP)")
                .on_toggle_maybe(editable.then_some(move |v| Message::Upnp(settings, v))).size(16).text_size(13),
            text("UPnP needs a compatible router and public Internet connection. VPS users can enter their assigned public IP or DNS hostname below.").size(12).color(MUTED),
            field("Public hostname or IP (manual / DDNS)", "server.example.net", &draft.public_host, editable && !draft.tunnel, edit_host),
            field("Public TCP port (optional)", "Same as server port", &draft.public_port, editable && !draft.tunnel, edit_port),
        ].spacing(10).into()
    }

    fn network_view<'a>(&'a self, server: &'a ServerSnapshot) -> Element<'a, Message> {
        let editable = self.connected
            && !self.busy
            && !server.status.is_active()
            && server.status != ServerStatus::Installing;
        let mut content = column![
            label("Join address"),
            text(
                server
                    .network
                    .join_address
                    .as_deref()
                    .unwrap_or("Not configured")
            )
            .size(18)
            .font(semibold()),
            text(
                server
                    .network
                    .detail
                    .as_deref()
                    .unwrap_or("Choose how players connect, then save your settings.")
            )
            .size(13)
            .color(MUTED),
        ]
        .spacing(12);
        if let Some(lan) = &server.network.lan_address {
            content = content.push(text(format!("Local network: {lan}")).size(12).color(MUTED));
        }
        if let Some(url) = &server.network.setup_url {
            content = content.push(action(
                "Link free tunnel account",
                Some(Message::OpenFolder(url.clone())),
                ButtonKind::Primary,
            ));
        }
        if server.status.is_active() || server.status == ServerStatus::Installing {
            content = content.push(text("Stop the server and wait for installation to finish before changing public access.").size(13).color(MUTED));
        }
        content = content
            .push(self.network_fields(&self.settings, editable, true))
            .push(action(
                "Save network settings",
                (editable && self.settings.validate(false).is_ok())
                    .then_some(Message::SaveSettings),
                ButtonKind::Primary,
            ));
        scrollable(container(content).max_width(700))
            .height(Length::Fill)
            .into()
    }

    fn properties_view<'a>(&'a self, server: &'a ServerSnapshot) -> Element<'a, Message> {
        let editable = self.connected
            && !self.busy
            && !server.status.is_active()
            && server.status != ServerStatus::Installing;
        let mut editor = text_editor(&self.properties)
            .placeholder("Loading server.properties…")
            .font(Font::MONOSPACE)
            .size(13)
            .padding(12)
            .height(Length::Fill)
            .style(style::editor);
        if self.properties_loaded {
            editor = editor.on_action(Message::EditProperties);
        }
        let mut heading = row![
            text("server.properties").size(15).font(semibold()),
            Space::new().width(Length::Fill)
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        if self.properties_dirty {
            heading = heading.push(text("Unsaved changes").size(12).color(AMBER));
        }
        heading = heading
            .push(action(
                "Reload",
                (!self.busy && self.connected && !self.properties_dirty).then_some(Message::Run(
                    Request::GetProperties(server.config.id.clone()),
                    Action::LoadProperties,
                )),
                ButtonKind::Secondary,
            ))
            .push(action(
                "Save",
                (editable && self.properties_loaded && self.properties_dirty)
                    .then_some(Message::SaveProperties),
                ButtonKind::Primary,
            ));
        column![
            heading,
            text(if editable {
                "Changes take effect the next time the server starts."
            } else {
                "Read only. Stop the server to edit its properties."
            })
            .size(12)
            .color(MUTED),
            editor
        ]
        .spacing(8)
        .height(Length::Fill)
        .into()
    }
}

fn action<'a>(
    label: &'a str,
    message: Option<Message>,
    kind: ButtonKind,
) -> iced::widget::Button<'a, Message> {
    button(text(label).size(13))
        .padding([7, 12])
        .style(style::action(kind))
        .on_press_maybe(message)
}

fn label(value: &str) -> iced::widget::Text<'_> {
    text(value).size(13).color(INK)
}

fn table_label(value: &str) -> iced::widget::Text<'_> {
    text(value).size(12).font(semibold()).color(MUTED)
}

fn section_title(value: &str) -> Element<'_, Message> {
    container(text(value).size(15).font(semibold()))
        .padding(iced::Padding {
            top: 10.0,
            ..Default::default()
        })
        .into()
}

fn field<'a>(
    name: &'a str,
    placeholder: &'a str,
    value: &'a str,
    enabled: bool,
    on_input: impl Fn(String) -> Message + 'a,
) -> Element<'a, Message> {
    column![
        label(name),
        text_input(placeholder, value)
            .padding(8)
            .size(14)
            .style(style::input)
            .on_input_maybe(enabled.then_some(on_input))
    ]
    .spacing(5)
    .width(Length::Fill)
    .into()
}

fn tab_button(label: &str, tab: Tab, selected: Tab) -> Element<'_, Message> {
    let is_selected = tab == selected;
    let underline = container(Space::new().height(2).width(Length::Fill)).style(move |_| {
        style::plain(if is_selected {
            ACCENT
        } else {
            Color::TRANSPARENT
        })
    });
    column![
        button(
            text(label)
                .size(14)
                .color(if is_selected { INK } else { MUTED })
        )
        .padding([7, 0])
        .on_press(Message::Tab(tab))
        .style(style::action(ButtonKind::Ghost)),
        underline
    ]
    .width(Length::Shrink)
    .spacing(2)
    .into()
}

fn status_dot(status: ServerStatus) -> Element<'static, Message> {
    dot(status_color(status))
}

fn dot(color: Color) -> Element<'static, Message> {
    container(Space::new().width(6).height(6))
        .style(move |_| {
            style::plain(color).border(Border {
                radius: 3.0.into(),
                ..Default::default()
            })
        })
        .into()
}

fn status_label(status: ServerStatus) -> Element<'static, Message> {
    row![
        status_dot(status),
        text(status.to_string())
            .size(12)
            .color(status_color(status))
    ]
    .spacing(6)
    .align_y(Alignment::Center)
    .into()
}

fn resource(label: &str, value: String) -> Element<'static, Message> {
    row![
        text(label.to_string()).size(13).color(MUTED),
        text(value).size(13).color(INK)
    ]
    .spacing(7)
    .into()
}

fn status_color(status: ServerStatus) -> Color {
    match status {
        ServerStatus::Running => SUCCESS,
        ServerStatus::Crashed => RED,
        ServerStatus::Installing | ServerStatus::Starting | ServerStatus::Stopping => AMBER,
        ServerStatus::Stopped => MUTED,
    }
}

fn semibold() -> Font {
    Font {
        weight: iced::font::Weight::Semibold,
        ..Font::with_name("Source Sans 3")
    }
}

fn bytes(value: u64) -> String {
    if value >= 1024 * 1024 * 1024 {
        format!("{:.1} GiB", value as f64 / 1_073_741_824.0)
    } else {
        format!("{} MiB", value / 1_048_576)
    }
}

fn uptime(value: Option<u64>) -> String {
    match value {
        Some(s) if s >= 3600 => format!("{}h {}m", s / 3600, s % 3600 / 60),
        Some(s) if s >= 60 => format!("{}m {}s", s / 60, s % 60),
        Some(s) => format!("{s}s"),
        None => "—".into(),
    }
}

fn format_time(ms: u64) -> String {
    let seconds = ms / 1000 % 86400;
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}

fn shorten(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }
    let mut text: String = value.chars().take(max_chars.saturating_sub(1)).collect();
    text.push('…');
    text
}
