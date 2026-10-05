use eframe::egui::{self, Color32, RichText};
use optchat::{
    memory::{Kind, Memory, Part},
    model::Provider,
    runtime::{self, Command, Event, Settings},
};
use std::{path::PathBuf, time::Duration};
use tokio::sync::mpsc;

fn main() -> eframe::Result {
    let provider = match Provider::parse(
        &std::env::var("OPTCHAT_PROVIDER").unwrap_or_else(|_| "anthropic".into()),
    ) {
        Ok(provider) => provider,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    };
    let directory = std::env::var_os("OPTCHAT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("chat"));
    let master = std::env::var("OPTCHAT_MODEL").unwrap_or_else(|_| provider.default_model().into());
    let compactor =
        std::env::var("OPTCHAT_COMPACTOR").unwrap_or_else(|_| provider.default_model().into());
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
    let rt = tokio::runtime::Runtime::new().expect("Tokio runtime");
    let (key, errors) = match rt.block_on(optchat::credentials::load(provider)) {
        Ok(key) => (key, vec![]),
        Err(error) => (
            std::env::var(provider.key_variable()).unwrap_or_default(),
            vec![error.to_string()],
        ),
    };
    let connected = !key.trim().is_empty();
    let settings = Settings {
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
            Ok(Box::new(App {
                tx,
                rx: event_rx,
                memory: Memory::default(),
                draft: String::new(),
                status: "Opening memory…".into(),
                errors,
                api_key: String::new(),
                saving_settings: false,
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
}
impl App {
    fn send(&mut self) {
        if self.saving_settings {
            return;
        }
        if self.draft.trim().is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.draft);
        if self.tx.send(Command::Send(text)).is_err() {
            self.errors
                .push("Memory worker is unavailable; restart the app.".into());
        } else {
            self.active = true;
        }
    }
    fn receive(&mut self) {
        while let Ok(event) = self.rx.try_recv() {
            match event {
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
                    self.errors.push(s);
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
                            !self.draft.trim().is_empty(),
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
                            ui.add_space(4.0); ui.add(egui::Label::new(&m.text).wrap().selectable(true)); ui.add_space(12.0); ui.separator();
                        }
                    });
                }
                if !self.thoughts.is_empty() { egui::CollapsingHeader::new("Reasoning · not saved to memory").show(ui,|ui| { ui.add(egui::Label::new(&self.thoughts).wrap()); }); }
                if !self.streaming.is_empty() { ui.add_space(12.0); ui.add(egui::Label::new(&self.streaming).wrap()); }
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
                });
        }
        match self.tab {
            Tab::Chat => self.chat(ui),
            Tab::Memory => self.memory(ui),
        }
    }
}
