//! Recent meetings, local search, and the jobs still running in this application.
use super::*;
use crate::library::{self, Entry};

pub(super) struct Home {
    pub(super) window: adw::ApplicationWindow,
    search: gtk::SearchEntry,
    calendar: Rc<super::calendar::Calendar>,
    meetings: gtk::ListBox,
    jobs: gtk::ListBox,
    message: gtk::Label,
    record: gtk::Button,
    more: gtk::Button,
    recover: gtk::Button,
    entries: RefCell<Vec<Entry>>,
    loading: Cell<bool>,
    reload: Cell<bool>,
    limit: Cell<usize>,
    job_signature: RefCell<String>,
    hub: std::rc::Weak<Hub>,
}

impl Home {
    pub(super) fn new(app: &adw::Application, hub: &Rc<Hub>) -> Rc<Self> {
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Meetings")
            .default_width(760)
            .default_height(700)
            .build();
        let toolbar = adw::ToolbarView::new();
        let header = adw::HeaderBar::new();
        let help = gtk::Button::builder()
            .icon_name("preferences-system-symbolic")
            .tooltip_text("Shortcuts and preferences")
            .build();
        header.pack_end(&help);
        let refresh = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Refresh meetings")
            .build();
        header.pack_start(&refresh);
        toolbar.add_top_bar(&header);
        let body = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .margin_start(24)
            .margin_end(24)
            .margin_top(12)
            .margin_bottom(24)
            .build();
        let heading = gtk::Label::builder()
            .label("Your meetings")
            .xalign(0.0)
            .css_classes(["title-1"])
            .build();
        body.append(&heading);
        let controls = gtk::Box::builder().spacing(12).build();
        let record = gtk::Button::builder()
            .label("Record a meeting")
            .action_name("app.record")
            .css_classes(["suggested-action", "pill"])
            .tooltip_text("Start recording (Ctrl+R)")
            .build();
        let import = gtk::Button::builder()
            .label("Import recording…")
            .css_classes(["pill"])
            .build();
        controls.append(&record);
        controls.append(&import);
        body.append(&controls);
        let recover = gtk::Button::builder()
            .label("Recover unfinished recording…")
            .visible(false)
            .halign(gtk::Align::Start)
            .build();
        body.append(&recover);
        let jobs = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .visible(false)
            .build();
        body.append(&jobs);
        let calendar = super::calendar::Calendar::new(hub);
        body.append(&calendar.widget);
        let search = gtk::SearchEntry::builder()
            .placeholder_text("Search meetings and transcripts")
            .hexpand(true)
            .build();
        body.append(&search);
        let message = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["dim-label"])
            .build();
        body.append(&message);
        let meetings = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        let scroll = gtk::ScrolledWindow::builder()
            .child(&meetings)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();
        body.append(&scroll);
        let more = gtk::Button::with_label("Show more");
        body.append(&more);
        toolbar.set_content(Some(&body));
        window.set_content(Some(&toolbar));
        let home = Rc::new(Self {
            window,
            search,
            calendar,
            meetings,
            jobs,
            message,
            record,
            more,
            recover,
            entries: RefCell::default(),
            loading: Cell::new(false),
            reload: Cell::new(false),
            limit: Cell::new(100),
            job_signature: RefCell::default(),
            hub: Rc::downgrade(hub),
        });
        let action = gio::SimpleAction::new("search", None);
        let weak = Rc::downgrade(&home);
        action.connect_activate(move |_, _| {
            if let Some(home) = weak.upgrade() {
                home.focus_search();
            }
        });
        home.window.add_action(&action);
        let weak = Rc::downgrade(&home);
        home.search.connect_search_changed(move |_| {
            if let Some(home) = weak.upgrade() {
                home.limit.set(100);
                home.render();
            }
        });
        let weak = Rc::downgrade(&home);
        home.more.connect_clicked(move |_| {
            if let Some(home) = weak.upgrade() {
                home.limit.set(home.limit.get() + 100);
                home.render();
            }
        });
        let weak = Rc::downgrade(&home);
        refresh.connect_clicked(move |_| {
            if let Some(home) = weak.upgrade() {
                home.refresh();
                home.calendar.refresh();
            }
        });
        let weak = Rc::downgrade(hub);
        import.connect_clicked(move |_| {
            if let Some(hub) = weak.upgrade()
                && let Some(app) = hub.home.get().and_then(|h| h.window.application())
            {
                let app = app.downcast::<adw::Application>().expect("adw app");
                let recorder = hub.free_window(&app);
                recorder.ready();
                hub.present_recorder(&recorder);
                recorder.import_button.emit_clicked();
            }
        });
        let weak = Rc::downgrade(hub);
        home.recover.connect_clicked(move |_| {
            if let Some(hub) = weak.upgrade()
                && let Some(app) = hub.home.get().and_then(|h| h.window.application())
            {
                let app = app.downcast::<adw::Application>().expect("adw app");
                let recorder = hub.free_window(&app);
                recorder.ready();
                recorder.offer_recovery();
            }
        });
        let weak = Rc::downgrade(&home);
        help.connect_clicked(move |_| {
            if let Some(home) = weak.upgrade() {
                home.preferences();
            }
        });
        let weak = Rc::downgrade(hub);
        home.window.connect_close_request(move |window| {
            if let Some(hub) = weak.upgrade() {
                let recorders = hub.recorders();
                if recorders.iter().any(|r| r.state.get().busy()) {
                    window.set_visible(false);
                    hub.settle_windows();
                    return glib::Propagation::Stop;
                }
                for r in recorders {
                    r.window.destroy();
                }
            }
            glib::Propagation::Proceed
        });
        home.refresh();
        home.calendar.refresh();
        home
    }

    pub(super) fn focus_search(&self) {
        self.search.grab_focus();
    }

    pub(super) fn refresh(self: &Rc<Self>) {
        if self.loading.replace(true) {
            self.reload.set(true);
            return;
        }
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(|| library::scan(&library::root())).await;
            let Some(home) = weak.upgrade() else {
                return;
            };
            home.loading.set(false);
            match result {
                Ok(Ok(entries)) => {
                    *home.entries.borrow_mut() = entries;
                    home.render();
                }
                _ => home.message.set_label(
                    "Could not read the meetings folder. Check its permissions and refresh.",
                ),
            }
            if home.reload.replace(false) {
                home.refresh();
            }
        });
    }

    fn render(&self) {
        self.recover.set_visible(
            self.hub
                .upgrade()
                .is_some_and(|hub| !hub.recoveries().is_empty()),
        );
        self.meetings.remove_all();
        let query = self.search.text();
        let mut count = 0;
        for entry in self.entries.borrow().iter() {
            for hit in entry.search(&query) {
                count += 1;
                if count > self.limit.get() {
                    continue;
                }
                let date = glib::DateTime::from_unix_local(entry.manifest.started_at)
                    .and_then(|d| d.format("%a %e %b · %H:%M"))
                    .map(|d| d.to_string())
                    .unwrap_or_default();
                let state = if self.is_processing(&entry.dir) {
                    "Processing"
                } else if entry.problem.is_some() {
                    "Needs attention · open to retry"
                } else if entry.transcript.is_empty() {
                    "Audio saved · no transcript"
                } else {
                    "Ready"
                };
                let detail = if hit.text.is_empty() {
                    format!(
                        "{date} · {} · {state}",
                        format_elapsed(entry.manifest.duration_secs)
                    )
                } else {
                    hit.text
                };
                let row = adw::ActionRow::builder()
                    .subtitle_lines(2)
                    .activatable(true)
                    .build();
                row.set_use_markup(false);
                row.set_title(&entry.manifest.title);
                row.set_subtitle(&detail);
                row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
                let weak = self.hub.clone();
                let dir = entry.dir.clone();
                row.connect_activated(move |_| {
                    if let Some(hub) = weak.upgrade() {
                        hub.open_saved(&dir, hit.at_ms);
                    }
                });
                self.meetings.append(&row);
            }
        }
        self.more.set_visible(count > self.limit.get());
        self.message.set_label(if count == 0 {
            if query.is_empty() {
                "Record or import a meeting to get started."
            } else {
                "No matching meetings or transcript passages."
            }
        } else if query.is_empty() {
            "Recent meetings"
        } else {
            "Matching meetings and passages"
        });
    }

    fn is_processing(&self, dir: &std::path::Path) -> bool {
        self.hub.upgrade().is_some_and(|hub| {
            hub.recorders().iter().any(|r| {
                matches!(
                    r.state.get(),
                    State::Stopping | State::Transcribing
                ) && r.result_dir.borrow().as_deref() == Some(dir)
            })
        })
    }

    pub(super) fn update_jobs(&self) {
        let Some(hub) = self.hub.upgrade() else {
            return;
        };
        let recording = hub.recording().is_some();
        self.record.set_label(if recording {
            "Stop recording"
        } else {
            "Record a meeting"
        });
        self.record.set_tooltip_text(Some(if recording {
            "Stop recording (Ctrl+R)"
        } else {
            "Start recording (Ctrl+R)"
        }));
        let jobs: Vec<_> = hub
            .recorders()
            .into_iter()
            .filter(|r| r.state.get().busy())
            .collect();
        let signature = jobs
            .iter()
            .map(|r| {
                let status = r.shared.lock().unwrap();
                format!(
                    "{}:{}:{}:{}:{}",
                    status.started_at,
                    status.title,
                    status.state,
                    status.stage,
                    (status.progress * 100.0) as u32
                )
            })
            .collect::<Vec<_>>()
            .join("|");
        if *self.job_signature.borrow() == signature {
            return;
        }
        *self.job_signature.borrow_mut() = signature;
        self.jobs.remove_all();
        self.jobs.set_visible(!jobs.is_empty());
        for recorder in jobs {
            let status = recorder.shared.lock().unwrap().clone();
            let subtitle = match recorder.state.get() {
                State::Recording => {
                    if recorder.paused.get() {
                        "Paused".into()
                    } else {
                        "Recording · open controls".into()
                    }
                }
                State::Stopping => "Saving audio…".into(),
                _ => format!("{} · {:.0}%", status.stage, status.progress * 100.0),
            };
            let row = adw::ActionRow::builder().activatable(true).build();
            row.set_use_markup(false);
            row.set_title(&recorder.title());
            row.set_subtitle(&subtitle);
            if recorder.state.get() == State::Transcribing {
                let cancel = gtk::Button::builder()
                    .label("Cancel")
                    .valign(gtk::Align::Center)
                    .build();
                let weak = Rc::downgrade(&recorder);
                cancel.connect_clicked(move |_| {
                    if let Some(r) = weak.upgrade()
                        && let Some(abort) = r.abort.borrow().as_ref()
                    {
                        abort.store(true, Ordering::Relaxed);
                    }
                });
                row.add_suffix(&cancel);
            }
            let weak = Rc::downgrade(&recorder);
            let hub = self.hub.clone();
            row.connect_activated(move |_| {
                if let (Some(hub), Some(r)) = (hub.upgrade(), weak.upgrade()) {
                    hub.present_recorder(&r);
                }
            });
            self.jobs.append(&row);
        }
    }

    fn preferences(self: &Rc<Self>) {
        let dialog = adw::PreferencesDialog::builder()
            .title("Preferences")
            .build();
        let page = adw::PreferencesPage::new();
        let group = adw::PreferencesGroup::builder()
            .title("Recording defaults")
            .build();
        let folder = adw::ActionRow::builder()
            .title("Meetings folder")
            .subtitle(library::root().to_string_lossy())
            .use_markup(false)
            .activatable(true)
            .build();
        let weak = Rc::downgrade(self);
        folder.connect_activated(move |_| {
            let Some(home) = weak.upgrade() else {
                return;
            };
            let chooser = gtk::FileDialog::builder().title("Meetings folder").build();
            let weak = Rc::downgrade(&home);
            chooser.select_folder(Some(&home.window), gio::Cancellable::NONE, move |result| {
                if let Ok(file) = result
                    && let Some(path) = file.path()
                {
                    settings::save_meetings_folder(&path);
                    if let Some(home) = weak.upgrade() {
                        home.refresh();
                    }
                }
            });
        });
        group.add(&folder);
        let current = crate::models::configured();
        let mut names: Vec<String> = crate::models::MODELS
            .iter()
            .map(|m| m.name.to_owned())
            .collect();
        if !names.contains(&current) {
            names.push(current.clone());
        }
        let refs: Vec<_> = names.iter().map(String::as_str).collect();
        let model = adw::ComboRow::builder()
            .title("Speech model")
            .subtitle("Runs locally; downloaded when needed")
            .model(&gtk::StringList::new(&refs))
            .selected(names.iter().position(|n| *n == current).unwrap_or(0) as u32)
            .build();
        let weak = Rc::downgrade(self);
        model.connect_selected_notify(move |row| {
            if let Some(name) = names.get(row.selected() as usize)
                && let Some(home) = weak.upgrade()
            {
                if let Err(error) = crate::models::save_configured(name) {
                    home.message
                        .set_label(&format!("Could not save speech model: {error}"));
                }
                if let Some(hub) = home.hub.upgrade() {
                    for r in hub.recorders() {
                        r.update_model_banner();
                    }
                }
            }
        });
        group.add(&model);
        page.add(&group);
        let calendars = adw::PreferencesGroup::builder().title("Calendar").build();
        let calendar_row = adw::ActionRow::builder()
            .title("Microsoft Calendar")
            .subtitle("Upcoming meetings, titles, and invitees")
            .activatable(true)
            .build();
        let weak = Rc::downgrade(&self.calendar);
        calendar_row.connect_activated(move |_| {
            if let Some(calendar) = weak.upgrade() {
                calendar.preferences();
            }
        });
        calendars.add(&calendar_row);
        page.add(&calendars);
        let actions = adw::PreferencesGroup::builder().title("Actions").build();
        let row = adw::ActionRow::builder()
            .title("Manage actions")
            .subtitle("Commands, scripts, and automatic runs for every meeting")
            .activatable(true)
            .build();
        row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        let parent = dialog.downgrade();
        let hub = self.hub.clone();
        row.connect_activated(move |_| {
            if let Some(parent) = parent.upgrade() {
                action_settings::present(&parent, hub.clone());
            }
        });
        actions.add(&row);
        page.add(&actions);
        let shortcuts = adw::PreferencesGroup::builder()
            .title("Shortcuts")
            .description("For a desktop shortcut, bind: omarchy-meeting-recorder start")
            .build();
        for (name, key) in [
            ("Start or stop recording", "Ctrl+R"),
            ("Your meetings", "Ctrl+H"),
            ("Search", "Ctrl+F"),
            ("Bookmark while recording", "Ctrl+B"),
            ("Compact view", "Ctrl+M"),
        ] {
            let row = adw::ActionRow::builder().title(name).build();
            row.add_suffix(&gtk::Label::new(Some(key)));
            shortcuts.add(&row);
        }
        page.add(&shortcuts);
        let privacy = adw::PreferencesGroup::builder().title("Your data")
            .description("Audio and transcription stay on this computer. Optional chapters send transcript text to your configured agent. Custom actions run when you choose them, or automatically when you enable that option for an action.").build();
        page.add(&privacy);
        dialog.add(&page);
        dialog.present(Some(&self.window));
    }
}

impl Hub {
    pub(super) fn show_home(&self) {
        for recorder in self.recorders() {
            recorder.player.pause();
            recorder.window.set_visible(false);
        }
        if let Some(home) = self.home.get() {
            home.refresh();
            home.update_jobs();
            home.window.present();
        }
    }

    pub(super) fn present_recorder(&self, recorder: &Recorder) {
        if let Some(home) = self.home.get() {
            home.window.set_visible(false);
        }
        recorder.quit_when_done.set(false);
        recorder.window.present();
    }

    pub(super) fn open_saved(self: &Rc<Self>, path: &std::path::Path, at_ms: Option<i64>) {
        let Some(app) = self.home.get().and_then(|home| home.window.application()) else {
            return;
        };
        let app = app.downcast::<adw::Application>().expect("adw app");
        let Some((folder, _)) = meeting::open(path) else {
            return;
        };
        let path = folder.as_path();
        let recorder = self
            .recorders()
            .into_iter()
            .find(|r| r.result_dir.borrow().as_deref() == Some(path))
            .unwrap_or_else(|| {
                let r = self.free_window(&app);
                r.open_meeting(path);
                r
            });
        if recorder.state.get() == State::Done {
            recorder.open_meeting(path);
        }
        self.present_recorder(&recorder);
        if let Some(ms) = at_ms
            && recorder.state.get() == State::Done
        {
            recorder.player.play_from(ms);
            recorder.highlight(ms);
        }
    }
}
