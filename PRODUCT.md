# Dawg

<!-- impeccable:product-schema 1 -->

## Platform

Linux server manager with a native Rust/Iced desktop and an authenticated browser interface served by the same daemon. Home PCs and VPS deployments share server operations and visual conventions.

## Users

The user hosts Minecraft on a home PC or VPS. Their stated expectation is that most people have one server; selecting among several servers is secondary.

## Product Purpose

Open the application into the server console and lifecycle controls. Help the user install, configure, run, and diagnose their server without having to orchestrate Java processes manually.

## Operating Context

A desktop application connects to a local daemon through a Unix socket. The daemon owns the Java processes. Closing the window leaves the servers running. Runtime details and console output come from the daemon.

## Capabilities and Constraints

Server creation and local JAR import; official Vanilla, Paper, Fabric and modern Forge installers; start, stop and restart; live stdout/stderr and commands; resource measurements; automatic verified Java runtime selection; configuration and server.properties; offline backups and registration removal preserving files. All existing functions must remain available through visual changes. The web interface adds authenticated remote administration and a capability-scoped file manager. Network settings support explicit public endpoints, managed router forwarding and a provider-assigned free tunnel hostname. A mod marketplace is outside the implemented scope.

## Brand Commitments

Keep the name Dawg. The user cited Squid Servers and Crafty as functional references and rejected the initial UI as looking generated. They explicitly requested console-first startup and a secondary server list. They did not specify a binding palette or logo.

## Evidence on Hand

The repository, existing Rust tests and Java integration fixture establish functional behavior. The UI is runnable under Xvfb with Mesa software rendering. A checksum-verified official Minecraft 1.21.11 server has now passed actual startup, Minecraft status/pong, console commands, world save and restart checks. Public Internet joining through a provider allocation requires separate verification; a Java fixture must still be identified as a fixture.

## Product Principles

- The console is the primary workspace, immediately visible after connection.
- The common one-server workflow takes precedence over a multi-server dashboard.
- Real state and actionable failures take precedence over decorative content.
- Editing and destructive operations make their prerequisites and consequences clear.

- Fabric is the default software choice; version choices come from official provider catalogs, including supported snapshots.
- Uninstallation must remove Dawg-owned files, credentials and integrations, including game data when purge is explicitly selected.
