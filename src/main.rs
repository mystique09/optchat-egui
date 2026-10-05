use eframe::egui::{self, Color32, RichText};
use optchat::{
    memory::{Kind, Memory, Part},
    model::Provider,
    runtime::{self, Command, Event, Settings},
};
use std::{path::PathBuf, time::Duration};
use tokio::sync::mpsc;

fn main() -> eframe::Result {
    let directory = std::env::var_os("OPTCHAT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("chat"));
    let rt = tokio::runtime::Runtime::new().expect("Tokio runtime");
    let (saved, mut errors) = match rt.block_on(runtime::ModelSelection::load(&directory)) {
        Ok(saved) => (saved, vec![]),
        Err(error) => (
            None,
            vec![format!("Could not load model settings: {error}")],
        ),
    };
    let provider = match Provider::parse(&std::env::var("OPTCHAT_PROVIDER").unwrap_or_else(|_| {
        saved
            .as_ref()
            .map_or(Provider::Anthropic, |s| s.provider)
            .label()
            .to_lowercase()
    })) {
        Ok(provider) => provider,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    };
    let saved = saved.filter(|s| s.provider == provider);
    let master = std::env::var("OPTCHAT_MODEL").unwrap_or_else(|_| {
        saved
            .as_ref()
            .map_or_else(|| provider.default_model().into(), |s| s.master.clone())
    });
    let compactor = std::env::var("OPTCHAT_COMPACTOR").unwrap_or_else(|_| {
        saved
            .as_ref()
            .map_or_else(|| provider.default_model().into(), |s| s.compactor.clone())
    });
    let instructions_path = std::env::var_os("OPTCHAT_INSTRUCTIONS")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("AGENTS.md"));
    let instructions = match std::fs::read_to_string(&instructions_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            eprintln!("{}: {e}", instructions_path.display());
            return Ok(());
        }
    };
    let key = match rt.block_on(optchat::credentials::load(provider)) {
        Ok(key) => key,
        Err(error) => {
            errors.push(error.to_string());
            std::env::var(provider.key_variable()).unwrap_or_default()
        }
    };
    let connected = !key.trim().is_empty();
    let settings = Settings {
        integrations_path: Some(directory.join("integrations.json")),
        provider,
        directory: directory.clone(),
        master: master.clone(),
        compactor: compactor.clone(),
        instructions,
        key,
        endpoint: std::env::var("OPTCHAT_ENDPOINT").unwrap_or_else(|_| provider.endpoint().into()),
    };
    let (tx, rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let worker = rt.spawn(runtime::run(settings, rx, event_tx));
    let shutdown = tx.clone();
    let result = eframe::run_native(
        "OptChat",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([1140.0, 800.0])
                .with_min_inner_size([720.0, 520.0]),
            ..Default::default()
        },
        Box::new(move |cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            cc.egui_ctx.style_mut_of(egui::Theme::Dark, |style| {
                style
                    .text_styles
                    .insert(egui::TextStyle::Body, egui::FontId::proportional(18.0));
                style
                    .text_styles
                    .insert(egui::TextStyle::Button, egui::FontId::proportional(16.0));
                style
                    .text_styles
                    .insert(egui::TextStyle::Monospace, egui::FontId::monospace(16.0));
                style
                    .text_styles
                    .insert(egui::TextStyle::Heading, egui::FontId::proportional(26.0));
            });
            Ok(Box::new(App {
                tx,
                rx: event_rx,
                memory: Memory::default(),
                draft: String::new(),
                attachments: Vec::new(),
                markdown_cache: egui_commonmark::CommonMarkCache::default(),
                status: "Opening memory…".into(),
                errors,
                api_key: String::new(),
                saving_settings: false,
                permissions: runtime::Permissions::Ask,
                permissions_open: false,
                integration_config: optchat::integrations::Config::default(),
                integration_base: optchat::integrations::Config::default(),
                integration_saving: false,
                new_server: String::new(),
                new_skill_directory: String::new(),
                integration_status: vec!["Discovering integrations…".into()],
                approval: None,
                oauth_busy: false,
                oauth_url: None,
                streaming: String::new(),
                thoughts: String::new(),
                usage: String::new(),
                active: false,
                connected,
                provider,
                selected_provider: provider,
                tab: Tab::Chat,
                selected: None,
                master,
                compactor,
                settings: false,
                directory,
                import_path: String::new(),
            }))
        }),
    );
    let _ = shutdown.send(Command::Shutdown);
    rt.block_on(async {
        let _ = worker.await;
    });
    result
}
#[derive(PartialEq)]
enum Tab {
    Chat,
    Memory,
}
struct App {
    tx: mpsc::UnboundedSender<Command>,
    rx: mpsc::UnboundedReceiver<Event>,
    memory: Memory,
    draft: String,
    attachments: Vec<PathBuf>,
    markdown_cache: egui_commonmark::CommonMarkCache,
    status: String,
    errors: Vec<String>,
    streaming: String,
    thoughts: String,
    usage: String,
    active: bool,
    connected: bool,
    provider: Provider,
    selected_provider: Provider,
    tab: Tab,
    selected: Option<Part>,
    master: String,
    compactor: String,
    settings: bool,
    directory: PathBuf,
    import_path: String,
    api_key: String,
    saving_settings: bool,
    permissions: runtime::Permissions,
    permissions_open: bool,
    integration_config: optchat::integrations::Config,
    integration_base: optchat::integrations::Config,
    integration_saving: bool,
    new_server: String,
    new_skill_directory: String,
    integration_status: Vec<String>,
    approval: Option<optchat::integrations::Approval>,
    oauth_busy: bool,
    oauth_url: Option<String>,
}
impl App {
    fn send(&mut self) {
        if self.draft.trim() == "/permissions" {
            self.draft.clear();
            self.permissions_open = true;
            return;
        }
        if self.saving_settings {
            return;
        }
        if self.draft.trim().is_empty() && self.attachments.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.draft);
        let command = if self.attachments.is_empty() {
            Command::Send(text)
        } else {
            Command::SendFiles {
                text,
                paths: std::mem::take(&mut self.attachments),
            }
        };
        if self.tx.send(command).is_err() {
            self.errors
                .push("Memory worker is unavailable; restart the app.".into());
        } else {
            self.active = true;
        }
    }
    fn receive(&mut self) {
        while let Ok(event) = self.rx.try_recv() {
            match event {
                Event::AttachmentRejected {
                    text,
                    paths,
                    error,
                    active,
                } => {
                    if !self.draft.is_empty() {
                        self.draft.push_str("\n\n");
                    }
                    self.draft.push_str(&text);
                    self.attachments.extend(paths);
                    self.errors.push(error);
                    self.active = active;
                }
                Event::OAuthBusy(busy) => self.oauth_busy = busy,
                Event::OAuthUrl(url) => self.oauth_url = Some(url),
                Event::SummaryRecovered(part) => {
                    let prefix = format!("Summary {}:", part.address());
                    self.errors.retain(|error| !error.starts_with(&prefix));
                }
                Event::Permissions(mode) => {
                    self.permissions = mode;
                    if mode == runtime::Permissions::FullAccess
                        && let Some(approval) = self.approval.take()
                    {
                        let _ = approval.answer.send(true);
                    }
                }
                Event::Integrations { config, status } => {
                    if let Ok(config) = optchat::integrations::Config::parse(&config)
                        && (self.integration_saving
                            || serde_json::to_value(&self.integration_config).ok()
                                == serde_json::to_value(&self.integration_base).ok())
                    {
                        self.integration_config = config.clone();
                        self.integration_base = config;
                    }
                    self.integration_status = status;
                    self.integration_saving = false;
                }
                Event::Approval(approval) => {
                    if !approval.answer.is_closed() {
                        if self.permissions == runtime::Permissions::FullAccess {
                            let _ = approval.answer.send(true);
                        } else {
                            self.approval = Some(approval);
                        }
                    }
                }
                Event::Snapshot(memory) => {
                    if memory.root.len() > self.memory.root.len() {
                        self.streaming.clear();
                    }
                    self.memory = memory;
                }
                Event::Status(s) => {
                    if s == "Thinking" {
                        self.active = true;
                        self.thoughts.clear();
                        self.streaming.clear();
                    }
                    self.status = s;
                }
                Event::Error(s) => {
                    self.integration_saving = false;
                    if !self.errors.contains(&s) {
                        self.errors.push(s);
                    }
                    if self.errors.len() > 20 {
                        self.errors.remove(0);
                    }
                }
                Event::Text(s) => self.streaming.push_str(&s),
                Event::Thought(s) => self.thoughts.push_str(&s),
                Event::Usage(u) => {
                    self.usage = u.display();
                }
                Event::ProviderApplied(provider, connected) => {
                    self.provider = provider;
                    self.connected = connected;
                    self.usage.clear();
                }
                Event::SettingsApplied => {
                    self.api_key.clear();
                    self.saving_settings = false;
                }
                Event::SettingsRejected => self.saving_settings = false,
                Event::Idle => {
                    self.approval = None;
                    self.active = false;
                    self.streaming.clear();
                }
            }
        }
    }
    fn chat(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("composer")
            .resizable(false)
            .show(ui, |ui| {
                ui.add_space(10.0);
                ui.weak("Drop images or text/log files here to attach them (up to 8 files).");
                let mut remove = None;
                for (index, path) in self.attachments.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(path.file_name().unwrap_or_default().to_string_lossy());
                        if ui.small_button("Remove").clicked() {
                            remove = Some(index);
                        }
                    });
                }
                if let Some(index) = remove {
                    self.attachments.remove(index);
                }
                let response = ui.add(
                    egui::TextEdit::multiline(&mut self.draft)
                        .hint_text("Write a message. Your history stays here.")
                        .desired_rows(3)
                        .desired_width(f32::INFINITY),
                );
                let submit = response.has_focus()
                    && ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::Enter));
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            !self.draft.trim().is_empty() || !self.attachments.is_empty(),
                            egui::Button::new(if self.active {
                                "Queue message"
                            } else {
                                "Send message"
                            }),
                        )
                        .clicked()
                        || submit
                    {
                        self.send();
                    }
                    ui.weak("⌘ Enter to send");
                    if self.active && ui.button("Stop").clicked() {
                        let _ = self.tx.send(Command::Cancel);
                    }
                });
                ui.add_space(6.0);
            });
        egui::CentralPanel::default().show(ui,|ui| {
            egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false,false]).show(ui,|ui| {
                if self.memory.root.is_empty() {
                    ui.add_space(90.0); ui.heading("One chat. A lasting memory."); ui.add_space(12.0);
                    ui.label("Start a conversation. OptChat keeps the original messages and builds a compact, browsable memory as you go.");
                    ui.add_space(12.0); ui.weak("Open Memory to see exactly what the model will remember next turn.");
                    if !self.connected { ui.add_space(24.0); ui.label(RichText::new("Connect a model to begin").color(Color32::from_rgb(240,192,110))); ui.label("Open Settings, select a provider, and save your API key. You can browse and import history without a key."); }
                }
                for m in &self.memory.root {
                    ui.push_id(m.i,|ui| {
                        if matches!(m.kind,Kind::Tool|Kind::Echo|Kind::Note) {
                            egui::CollapsingHeader::new(format!("{} · #{}",m.kind.label(),m.i)).show(ui,|ui| { ui.add(egui::Label::new(&m.text).wrap()); });
                        } else {
                            ui.add_space(16.0);
                            ui.horizontal(|ui| { ui.label(RichText::new(if m.kind==Kind::User { "YOU" } else { "OPTCHAT" }).small().strong().color(if m.kind==Kind::User { Color32::from_rgb(126,204,186) } else { Color32::from_rgb(148,177,227) })); ui.weak(m.date.format("%b %d · %H:%M").to_string()); });
                            ui.add_space(4.0);
                            egui_commonmark::CommonMarkViewer::new().show(ui, &mut self.markdown_cache, &m.text);
                            ui.add_space(12.0); ui.separator();
                        }
                    });
                }
                if !self.thoughts.is_empty() { egui::CollapsingHeader::new("Reasoning · not saved to memory").show(ui,|ui| { ui.add(egui::Label::new(&self.thoughts).wrap()); }); }
                if !self.streaming.is_empty() { ui.add_space(12.0); ui.push_id("streaming", |ui| { egui_commonmark::CommonMarkViewer::new().show(ui, &mut self.markdown_cache, &self.streaming); }); }
            });
        });
    }
    fn memory(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show(ui,|ui| {
            ui.heading("The view"); ui.label("This is the memory supplied to the next turn. Open a range to explore its children and original messages.");
            ui.add_space(8.0);
            if let Some(p)=self.selected {
                if ui.button("← Back to the view").clicked() { self.selected=None; }
                ui.heading(p.address()); ui.weak(format!("{} — {}",self.memory.date(p.start()),self.memory.date(p.end()-1)));
                egui::ScrollArea::vertical().show(ui,|ui| {
                    if p.l==0 { ui.add(egui::Label::new(self.memory.root[p.i].source()).wrap().selectable(true)); }
                    else { for child in p.children() { if ui.button(child.address()).clicked() { self.selected=Some(child); } ui.add(egui::Label::new(self.memory.text(child)).wrap()); ui.separator(); } }
                });
            } else {
                egui::ScrollArea::vertical().auto_shrink([false,false]).show(ui,|ui| {
                    for p in &self.memory.view { ui.horizontal_wrapped(|ui| { if ui.button(RichText::new(p.address()).monospace()).clicked() { self.selected=Some(*p); } ui.label(self.memory.text(*p)); }); ui.separator(); }
                    if self.memory.view.is_empty() { ui.weak("Your first message will start the tree."); }
                });
            }
        });
    }
}
impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.receive();
        for file in ui.input(|input| input.raw.dropped_files.clone()) {
            let path = file.path().to_path_buf();
            if self.attachments.contains(&path) {
                continue;
            }
            if self.attachments.len() == 8 {
                let error = "Attach at most 8 files per message.".to_owned();
                if !self.errors.contains(&error) {
                    self.errors.push(error);
                }
                break;
            }
            self.attachments.push(path);
            self.tab = Tab::Chat;
        }
        if let Some(url) = self.oauth_url.take() {
            ui.ctx().open_url(egui::OpenUrl::new_tab(url));
        }
        if self.permissions_open {
            egui::Window::new("Permissions")
                .open(&mut self.permissions_open)
                .collapsible(false)
                .show(ui.ctx(), |ui| {
                    ui.label("Choose how OptChat runs local and MCP tools. This choice is saved for future sessions.");
                    let mut mode = self.permissions;
                    ui.radio_value(&mut mode, runtime::Permissions::Ask, "Ask for approval");
                    ui.radio_value(&mut mode, runtime::Permissions::FullAccess, "Full access");
                    ui.weak("Full access allows shell commands, local file access, and all configured MCP tools without approval, including changes to files and external services.");
                    if mode != self.permissions { let _ = self.tx.send(Command::Permissions(mode)); }
                });
        }
        if self.approval.as_ref().is_some_and(|a| a.answer.is_closed()) {
            self.approval = None;
        }
        if let Some(approval) = &self.approval {
            let mut answer = None;
            egui::Window::new("Allow tool call?")
                .collapsible(false)
                .resizable(true)
                .show(ui.ctx(), |ui| {
                    ui.label(format!(
                        "Server: {} · Tool: {}",
                        approval.server, approval.tool
                    ));
                    egui::ScrollArea::vertical()
                        .max_height(240.0)
                        .show(ui, |ui| {
                            ui.add(
                                egui::Label::new(
                                    serde_json::to_string_pretty(&approval.arguments)
                                        .unwrap_or_default(),
                                )
                                .wrap(),
                            );
                        });
                    ui.horizontal(|ui| {
                        if ui.button("Allow once").clicked() {
                            answer = Some(true);
                        }
                        if ui.button("Deny").clicked() {
                            answer = Some(false);
                        }
                        if ui.button("Permissions…").clicked() {
                            self.permissions_open = true;
                        }
                    });
                });
            if let Some(answer) = answer
                && let Some(approval) = self.approval.take()
            {
                let _ = approval.answer.send(answer);
            }
        }
        ui.ctx().request_repaint_after(Duration::from_millis(100));
        egui::Panel::top("header").show(ui, |ui| {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.heading("OptChat");
                ui.weak("/ endless conversation");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Settings").clicked() {
                        self.settings = !self.settings;
                    }
                    if ui.button("Permissions").clicked() {
                        self.permissions_open = true;
                    }
                    ui.selectable_value(&mut self.tab, Tab::Memory, "Memory");
                    ui.selectable_value(&mut self.tab, Tab::Chat, "Chat");
                });
            });
            ui.add_space(8.0);
        });
        egui::Panel::bottom("status").show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                if self.active {
                    ui.spinner();
                }
                ui.label(&self.status);
                ui.separator();
                ui.weak(format!(
                    "{} messages · {} summaries · {:.1} / 128 KB view",
                    self.memory.root.len(),
                    self.memory.tree.len(),
                    self.memory.bytes() as f64 / 1000.0
                ));
            });
            if !self.usage.is_empty() {
                ui.small(&self.usage);
            }
            if let Some(error) = self.errors.last() {
                ui.horizontal_wrapped(|ui| {
                    ui.colored_label(Color32::from_rgb(245, 153, 139), error);
                });
                if ui.small_button("Dismiss").clicked() {
                    self.errors.pop();
                }
            }
        });
        if self.settings {
            egui::Window::new("Settings")
                .resizable(true)
                .vscroll(true)
                .show(ui.ctx(), |ui| {
                    ui.label(format!("Active provider: {}", self.provider.label()));
                    let previous = self.selected_provider;
                    ui.add_enabled_ui(!self.active && !self.saving_settings, |ui| {
                        egui::ComboBox::from_label("Provider")
                            .selected_text(self.selected_provider.label())
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut self.selected_provider,
                                    Provider::Anthropic,
                                    "Anthropic",
                                );
                                ui.selectable_value(
                                    &mut self.selected_provider,
                                    Provider::DeepSeek,
                                    "DeepSeek",
                                );
                            });
                    });
                    if previous != self.selected_provider {
                        self.api_key.clear();
                        self.master = self.selected_provider.default_model().into();
                        self.compactor = self.selected_provider.default_model().into();
                    }
                    ui.label("Master model");
                    ui.text_edit_singleline(&mut self.master);
                    ui.label("Compactor model");
                    ui.text_edit_singleline(&mut self.compactor);
                    ui.label(format!("{} API key", self.selected_provider.label()));
                    ui.add_enabled(
                        !self.active && !self.saving_settings,
                        egui::TextEdit::singleline(&mut self.api_key)
                            .password(true)
                            .hint_text("Enter a new key; leave blank to keep the existing key"),
                    );
                    ui.weak(
                        "Saved securely in your OS credential store. Applies without restarting.",
                    );
                    if ui
                        .add_enabled(
                            !self.active && !self.saving_settings,
                            egui::Button::new("Save and apply"),
                        )
                        .clicked()
                    {
                        self.saving_settings = self
                            .tx
                            .send(Command::Models {
                                provider: self.selected_provider,
                                master: self.master.clone(),
                                compactor: self.compactor.clone(),
                                api_key: if self.api_key.is_empty() {
                                    None
                                } else {
                                    Some(self.api_key.clone())
                                },
                            })
                            .is_ok();
                        if !self.saving_settings {
                            self.errors
                                .push("Memory worker is unavailable; restart the app.".into());
                        }
                    }
                    ui.separator();
                    ui.label(format!("History: {}", self.directory.display()));
                    ui.weak(if self.connected {
                        format!("{} API key configured", self.provider.label())
                    } else {
                        format!("{} API key is not configured", self.provider.label())
                    });
                    if ui.button("Export memory as HTML").clicked() {
                        let _ = self
                            .tx
                            .send(Command::Export(self.directory.join("memory.html")));
                    }
                    ui.separator();
                    ui.label("Import UTF-8 notes · one note per non-empty line");
                    ui.text_edit_singleline(&mut self.import_path);
                    if ui
                        .add_enabled(
                            !self.import_path.is_empty() && !self.active,
                            egui::Button::new("Import notes"),
                        )
                        .clicked()
                    {
                        let _ = self
                            .tx
                            .send(Command::Import(PathBuf::from(&self.import_path)));
                    }
                    ui.weak("Imports append permanently. Reimporting adds another copy.");
                    ui.separator();
                    egui::CollapsingHeader::new("MCP and skills").show(ui, |ui| {
                        let (valid, oauth_action) = ui.add_enabled_ui(!self.oauth_busy && !self.active && !self.integration_saving, |ui| {
                            integration_editor(ui, &mut self.integration_config, &mut self.new_server, &mut self.new_skill_directory)
                        }).inner;
                        if let Some((server, sign_out)) = oauth_action {
                            match serde_json::to_string(&self.integration_config) {
                                Ok(config) => {
                                    let base = serde_json::to_string(&self.integration_base).expect("config serializes");
                                    self.oauth_busy = self.tx.send(Command::OAuth { config, base, server, sign_out }).is_ok();
                                    self.integration_saving = self.oauth_busy;
                                }
                                Err(error) => self.errors.push(error.to_string()),
                            }
                        }
                        if self.oauth_busy {
                            ui.label("Waiting for OAuth sign-in / connection… Complete authorization in your browser.");
                            if ui.button("Cancel sign-in").clicked() { let _ = self.tx.send(Command::CancelOAuth); }
                        }
                        if ui
                            .add_enabled(
                                !self.active && !self.oauth_busy && !self.integration_saving && valid,
                                egui::Button::new("Save and reload integrations"),
                            )
                            .clicked()
                        {
                            match serde_json::to_string(&self.integration_config) {
                                Ok(config) => {
                                    let base = serde_json::to_string(&self.integration_base).expect("config serializes");
                                    let _ = self.tx.send(Command::ReloadIntegrations { config, base });
                                    self.integration_saving = true;
                                }
                                Err(error) => self.errors.push(error.to_string()),
                            }
                        }
                        for status in &self.integration_status {
                            ui.label(status);
                        }
                        if ui.add_enabled(!self.active && !self.oauth_busy, egui::Button::new("Reload from disk (discard Settings edits)")).clicked() {
                            self.integration_config = self.integration_base.clone();
                            let _ = self.tx.send(Command::ReloadFromDisk);
                        }
                    });
                });
        }
        match self.tab {
            Tab::Chat => self.chat(ui),
            Tab::Memory => self.memory(ui),
        }
    }
}

fn integration_editor(
    ui: &mut egui::Ui,
    config: &mut optchat::integrations::Config,
    new_server: &mut String,
    new_skill_directory: &mut String,
) -> (bool, Option<(String, bool)>) {
    use optchat::integrations::Server;
    let mut valid = true;
    let mut oauth_action = None;
    ui.label("MCP servers");
    ui.weak("Changes apply when saved. Saving starts configured server commands.");
    let mut remove = None;
    for (name, server) in &mut config.servers {
        ui.push_id(name, |ui| {
            ui.group(|ui| {
                ui.horizontal(|ui| {
                    ui.strong(name);
                    if ui.button("Remove server").clicked() {
                        remove = Some(name.clone());
                    }
                });
                let was_http = matches!(server, Server::Http { .. });
                let mut http = was_http;
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut http, false, "Local command (stdio)");
                    ui.selectable_value(&mut http, true, "Streamable HTTP");
                });
                if http != was_http {
                    *server = if http {
                        Server::Http {
                            url: String::new(),
                            bearer_token_env: None,
                            oauth: None,
                        }
                    } else {
                        Server::Stdio {
                            command: String::new(),
                            args: vec![],
                            cwd: None,
                            env_from: Default::default(),
                        }
                    };
                }
                match server {
                    Server::Stdio {
                        command,
                        args,
                        cwd,
                        env_from,
                    } => {
                        ui.label("Command");
                        ui.text_edit_singleline(command);
                        ui.label("Arguments (one per field; no shell quoting needed)");
                        let mut removed = None;
                        for (index, arg) in args.iter_mut().enumerate() {
                            ui.push_id(index, |ui| {
                                ui.horizontal(|ui| {
                                    ui.text_edit_singleline(arg);
                                    if ui.button("Remove argument").clicked() {
                                        removed = Some(index);
                                    }
                                });
                            });
                        }
                        if let Some(index) = removed {
                            args.remove(index);
                        }
                        if ui.button("Add argument").clicked() {
                            args.push(String::new());
                        }
                        ui.label("Working directory (optional)");
                        let mut directory = cwd
                            .as_ref()
                            .map(|p| p.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        if ui.text_edit_singleline(&mut directory).changed() {
                            *cwd = if directory.is_empty() {
                                None
                            } else {
                                Some(directory.into())
                            };
                        }
                        ui.label(
                            "Environment references: CHILD_VARIABLE=APP_VARIABLE (one per line)",
                        );
                        let mut environment = env_from
                            .iter()
                            .map(|(key, value)| format!("{key}={value}"))
                            .collect::<Vec<_>>()
                            .join("\n");
                        // Retain incomplete edits until focus leaves the field.
                        let id = ui.id().with("environment");
                        environment = ui
                            .data_mut(|data| data.get_temp::<String>(id))
                            .unwrap_or(environment);
                        let response =
                            ui.add(egui::TextEdit::multiline(&mut environment).desired_rows(2));
                        if response.changed() {
                            ui.data_mut(|data| data.insert_temp(id, environment.clone()));
                        }
                        {
                            let entries: Option<std::collections::BTreeMap<String, String>> =
                                environment
                                    .lines()
                                    .filter(|line| !line.trim().is_empty())
                                    .map(|line| {
                                        let (key, value) = line.split_once('=')?;
                                        if key.trim().is_empty() || value.trim().is_empty() {
                                            return None;
                                        }
                                        Some((key.trim().into(), value.trim().into()))
                                    })
                                    .collect();
                            if let Some(entries) = entries {
                                *env_from = entries;
                                if !response.has_focus() {
                                    ui.data_mut(|data| data.remove::<String>(id));
                                }
                            } else {
                                valid = false;
                            }
                        }
                        if ui.data_mut(|data| data.get_temp::<String>(id)).is_some()
                            && !response.has_focus()
                        {
                            ui.colored_label(
                                Color32::LIGHT_RED,
                                "Use CHILD_VARIABLE=APP_VARIABLE for each environment reference.",
                            );
                        }
                    }
                    Server::Http {
                        url,
                        bearer_token_env,
                        oauth,
                    } => {
                        ui.label("Server URL");
                        ui.text_edit_singleline(url);
                        let mut use_oauth = oauth.is_some();
                        if ui.checkbox(&mut use_oauth, "Browser sign-in (OAuth)").changed() {
                            *oauth = use_oauth.then(Default::default);
                            if use_oauth { *bearer_token_env = None; }
                        }
                        if let Some(oauth) = oauth {
                            ui.label("Client ID (optional; blank uses dynamic registration)");
                            ui.text_edit_singleline(&mut oauth.client_id);
                            ui.label("Scopes (optional, separated by spaces)");
                            ui.text_edit_singleline(&mut oauth.scopes);
                            ui.horizontal(|ui| {
                                if ui.button("Save and sign in").clicked() { oauth_action = Some((name.clone(), false)); }
                                if ui.button("Sign out").clicked() { oauth_action = Some((name.clone(), true)); }
                            });
                            ui.weak("Tokens are stored in the OS credential store. Sign out removes local tokens; revoke access at the provider to revoke the grant.");
                        } else {
                        ui.label("Bearer token environment variable (optional)");
                        let mut token = bearer_token_env.clone().unwrap_or_default();
                        if ui.text_edit_singleline(&mut token).changed() {
                            *bearer_token_env = if token.is_empty() { None } else { Some(token) };
                        }
                        }
                    }
                }
            });
        });
    }
    if let Some(name) = remove {
        config.servers.remove(&name);
    }
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(new_server).hint_text("New server name"));
        let valid = !new_server.is_empty()
            && new_server.len() <= 64
            && new_server
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
            && !config.servers.contains_key(new_server);
        if ui
            .add_enabled(valid, egui::Button::new("Add MCP server"))
            .clicked()
        {
            config.servers.insert(
                std::mem::take(new_server),
                Server::Stdio {
                    command: String::new(),
                    args: vec![],
                    cwd: None,
                    env_from: Default::default(),
                },
            );
        }
    });
    ui.weak("Use a unique name containing letters, numbers, hyphens or underscores.");
    ui.separator();
    ui.label("Skills");
    ui.weak("Add a skill folder containing SKILL.md, or a directory of skills. Removing a path stops discovery there; files stay on disk.");
    let mut remove = None;
    for (index, path) in config.skill_directories.iter().enumerate() {
        ui.push_id(("skill", index), |ui| {
            ui.horizontal(|ui| {
                ui.label(path.display().to_string());
                if ui.button("Remove skill path").clicked() {
                    remove = Some(index);
                }
            });
        });
    }
    if let Some(index) = remove {
        config.skill_directories.remove(index);
    }
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(new_skill_directory)
                .hint_text("~/.agents/skills or a specific skill folder"),
        );
        let path = PathBuf::from(new_skill_directory.trim());
        if ui
            .add_enabled(
                !new_skill_directory.trim().is_empty() && !config.skill_directories.contains(&path),
                egui::Button::new("Add skill path"),
            )
            .clicked()
        {
            config.skill_directories.push(path);
            new_skill_directory.clear();
        }
    });
    ui.horizontal(|ui| {
        ui.label("Tool timeout (seconds)");
        ui.add(egui::DragValue::new(&mut config.timeout_seconds).range(1..=600));
    });
    (valid, oauth_action)
}
