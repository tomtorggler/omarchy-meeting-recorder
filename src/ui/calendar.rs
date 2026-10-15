//! Home's calendar section. All provider/keyring work runs off the GTK thread.
use super::*;
use crate::calendar::{Config, Meeting, Service};
use std::sync::atomic::AtomicBool;

pub(super) struct Calendar {
    pub(super) widget: gtk::Box,
    status: gtk::Label,
    list: gtk::ListBox,
    scroll: gtk::ScrolledWindow,
    heading: gtk::Label,
    manage: gtk::Button,
    service: Arc<Mutex<Service>>,
    pub(super) config: RefCell<Config>,
    pub(super) busy: Cell<bool>,
    pub(super) connecting: Cell<bool>,
    cancel: Arc<AtomicBool>,
    events: RefCell<Vec<Meeting>>,
    preferences: RefCell<Option<Rc<super::calendar_settings::Preferences>>>,
    hub: std::rc::Weak<Hub>,
}

pub(super) enum Operation {
    Refresh,
    Connect(Config),
    Disconnect,
}

impl Calendar {
    pub(super) fn new(hub: &Rc<Hub>) -> Rc<Self> {
        let config = Config::load();
        let widget = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .build();
        let header = gtk::Box::builder().spacing(12).build();
        let heading = gtk::Label::builder()
            .label("Upcoming · next 7 days")
            .xalign(0.0)
            .hexpand(true)
            .css_classes(["heading"])
            .build();
        let manage = gtk::Button::builder().label("Calendar settings…").build();
        header.append(&heading);
        header.append(&manage);
        widget.append(&header);
        let status = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["dim-label"])
            .build();
        widget.append(&status);
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        let scroll = gtk::ScrolledWindow::builder()
            .child(&list)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(230)
            .build();
        widget.append(&scroll);
        let this = Rc::new(Self {
            widget,
            status,
            list,
            scroll,
            heading,
            manage,
            service: Arc::new(Mutex::new(Service::new(config.clone()))),
            config: RefCell::new(config),
            busy: Cell::new(false),
            connecting: Cell::new(false),
            cancel: Arc::new(AtomicBool::new(false)),
            events: RefCell::default(),
            preferences: RefCell::default(),
            hub: Rc::downgrade(hub),
        });
        let weak = Rc::downgrade(&this);
        this.manage.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.preferences();
            }
        });
        this.render();
        let weak = Rc::downgrade(&this);
        glib::timeout_add_local(Duration::from_secs(300), move || {
            let Some(this) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if this.parent().is_some_and(|w| w.is_visible()) {
                this.refresh();
            }
            glib::ControlFlow::Continue
        });
        let weak = Rc::downgrade(&this);
        glib::timeout_add_local(Duration::from_secs(60), move || {
            let Some(this) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            this.render();
            glib::ControlFlow::Continue
        });
        this
    }

    pub(super) fn parent(&self) -> Option<adw::ApplicationWindow> {
        self.hub
            .upgrade()
            .and_then(|h| h.home.get().map(|home| home.window.clone()))
    }

    pub(super) fn preferences(self: &Rc<Self>) {
        let existing = self.preferences.borrow().clone();
        let prefs = existing.unwrap_or_else(|| {
            let prefs = super::calendar_settings::Preferences::new(self);
            *self.preferences.borrow_mut() = Some(prefs.clone());
            prefs
        });
        prefs.update(self);
        prefs.dialog.present(self.parent().as_ref());
    }

    pub(super) fn status_text(&self) -> glib::GString {
        self.status.label()
    }

    pub(super) fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.set_message("Cancelling sign-in…");
    }

    pub(super) fn refresh(self: &Rc<Self>) {
        if self.config.borrow().enabled {
            self.run(Operation::Refresh);
        }
    }

    pub(super) fn run(self: &Rc<Self>, operation: Operation) {
        if self.busy.replace(true) {
            return;
        }
        self.cancel.store(false, Ordering::Relaxed);
        let connecting = matches!(operation, Operation::Connect(_));
        self.connecting.set(connecting);
        self.set_message(match &operation {
            Operation::Refresh => "Refreshing Microsoft Calendar…",
            Operation::Connect(_) => "Discovering your organization and opening Microsoft sign-in…",
            Operation::Disconnect => "Disconnecting Microsoft Calendar…",
        });
        self.render();
        let (browser_tx, browser_rx) = async_channel::bounded::<String>(1);
        if connecting {
            let weak = Rc::downgrade(self);
            glib::spawn_future_local(async move {
                if let Ok(uri) = browser_rx.recv().await
                    && let Some(this) = weak.upgrade()
                {
                    this.set_message(
                        "Finish signing in through your browser. You can cancel here.",
                    );
                    let weak = Rc::downgrade(&this);
                    gtk::UriLauncher::new(&uri).launch(
                        this.parent().as_ref(),
                        gio::Cancellable::NONE,
                        move |result| {
                            if result.is_err()
                                && let Some(this) = weak.upgrade()
                            {
                                this.cancel();
                            }
                        },
                    );
                }
            });
        }
        let service = self.service.clone();
        let cancel = self.cancel.clone();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(move || {
                let mut service = service.lock().unwrap();
                let changed = match operation {
                    Operation::Connect(config) => service.connect(config, &cancel, |uri| {
                        browser_tx
                            .send_blocking(uri)
                            .map_err(|_| "Sign-in window closed.".into())
                    }),
                    Operation::Disconnect => service.disconnect(),
                    Operation::Refresh => Ok(()),
                };
                let events = changed.and_then(|_| service.upcoming(&cancel));
                (service.config.clone(), events)
            })
            .await;
            let Some(this) = weak.upgrade() else {
                return;
            };
            this.busy.set(false);
            match result {
                Ok((config, events)) => {
                    let changed_account = this.config.borrow().email != config.email
                        || this.config.borrow().client_id != config.client_id;
                    *this.config.borrow_mut() = config;
                    match events {
                        Ok(events) => {
                            *this.events.borrow_mut() = events;
                            let message = if this.config.borrow().enabled {
                                format!(
                                    "{} · updated {}",
                                    this.config.borrow().email,
                                    glib::DateTime::now_local()
                                        .and_then(|d| d.format("%H:%M"))
                                        .unwrap_or_default()
                                )
                            } else {
                                "Calendar is off. Connect to see upcoming meetings here.".into()
                            };
                            this.set_message(&message);
                        }
                        Err(message) => {
                            // Never offer stale/cancelled events after a failed refresh.
                            this.events.borrow_mut().clear();
                            this.set_message(&message);
                        }
                    }
                    if changed_account && let Some(prefs) = this.preferences.borrow().as_ref() {
                        prefs.load_config(&this.config.borrow());
                    }
                }
                Err(_) => this.set_message("Calendar operation stopped unexpectedly. Try again."),
            }
            this.render();
        });
    }

    fn set_message(&self, message: &str) {
        self.status.set_label(message);
        if let Some(prefs) = self.preferences.borrow().as_ref() {
            prefs.update(self);
        }
    }

    fn render(&self) {
        let enabled = self.config.borrow().enabled;
        self.heading.set_visible(enabled);
        self.manage.set_label(if enabled {
            "Calendar settings…"
        } else {
            "Connect Microsoft Calendar…"
        });
        if self.status.label().is_empty() {
            self.set_message("Calendar is off. Connect to see upcoming meetings here.");
        }
        self.list.remove_all();
        if enabled {
            let now = ipc::now();
            for meeting in self.events.borrow().iter().filter(|m| m.ends_at > now) {
                let date = glib::DateTime::from_unix_local(meeting.starts_at)
                    .and_then(|d| d.format("%a %e %b · %H:%M"))
                    .unwrap_or_default();
                let end = glib::DateTime::from_unix_local(meeting.ends_at)
                    .and_then(|d| d.format("%H:%M"))
                    .unwrap_or_default();
                let names = meeting
                    .participants
                    .iter()
                    .map(|p| p.label())
                    .collect::<Vec<_>>()
                    .join(", ");
                let detail = format!(
                    "{date}–{end}{}\n{names}",
                    if meeting.starts_at <= now {
                        " · In progress"
                    } else {
                        ""
                    }
                );
                let row = adw::ActionRow::builder()
                    .title_lines(1)
                    .subtitle_lines(2)
                    .build();
                row.set_use_markup(false);
                row.set_title(&meeting.title);
                row.set_subtitle(&detail);
                let record = gtk::Button::builder()
                    .label("Record")
                    .valign(gtk::Align::Center)
                    .tooltip_text("Start recording with this meeting's title and invitees")
                    .build();
                let hub = self.hub.clone();
                let meeting = meeting.clone();
                record.connect_clicked(move |_| {
                    if let Some(hub) = hub.upgrade() {
                        hub.record_calendar_meeting(&meeting);
                    }
                });
                row.add_suffix(&record);
                self.list.append(&row);
            }
            if self.list.first_child().is_none() && !self.busy.get() {
                self.list.append(
                    &adw::ActionRow::builder()
                        .title("No upcoming meetings loaded")
                        .subtitle("Timed events in your primary calendar appear here.")
                        .build(),
                );
            }
        }
        self.scroll.set_visible(enabled);
        if let Some(prefs) = self.preferences.borrow().as_ref() {
            prefs.update(self);
        }
    }
}

impl Drop for Calendar {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Hub {
    fn record_calendar_meeting(self: &Rc<Self>, meeting: &Meeting) {
        if let Some(recorder) = self.recording() {
            self.present_recorder(&recorder);
            recorder.toast("A recording is already running. Its meeting details have been kept.");
            return;
        }
        let Some(app) = self.home.get().and_then(|home| home.window.application()) else {
            return;
        };
        let app = app.downcast::<adw::Application>().expect("adw app");
        let recorder = self.free_window(&app);
        recorder.ready();
        recorder.set_calendar_meeting(Some(meeting.clone()));
        recorder.title_row.set_text(&meeting.title);
        self.present_recorder(&recorder);
        recorder.start();
    }
}

pub(super) fn calendar_invitees_row() -> adw::ActionRow {
    adw::ActionRow::builder()
        .title("Calendar invitees")
        .subtitle_lines(2)
        .use_markup(false)
        .visible(false)
        .build()
}

impl Recorder {
    pub(super) fn set_calendar_meeting(&self, meeting: Option<Meeting>) {
        let names = meeting
            .as_ref()
            .map(|m| {
                m.participants
                    .iter()
                    .map(|p| {
                        if p.name.is_empty() || p.email.is_empty() {
                            p.label().to_owned()
                        } else {
                            format!("{} <{}>", p.name, p.email)
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        for row in &self.calendar_rows {
            row.set_subtitle(if names.is_empty() {
                "No invitees listed"
            } else {
                &names
            });
            row.set_tooltip_text(Some(&names));
            row.set_visible(meeting.is_some());
        }
        *self.calendar_meeting.borrow_mut() = meeting;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run with an isolated XDG directory, GTK Broadway, and the synthetic parec/pacat
    /// fixtures. This test records synthetic silence, never the real user's devices.
    #[test]
    #[ignore = "requires isolated GTK display and synthetic audio commands"]
    fn calendar_ui_and_recording_smoke() {
        assert_eq!(
            std::env::var("MEETING_RECORDER_SYNTHETIC_TEST").as_deref(),
            Ok("1")
        );
        adw::init().unwrap();
        let app = adw::Application::builder()
            .application_id("com.example.CalendarSmoke")
            .flags(gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(gio::Cancellable::NONE).unwrap();
        let hub = Hub::new(&app);
        let calendar = Calendar::new(&hub);
        assert!(!calendar.config.borrow().enabled);
        calendar.preferences();
        calendar.preferences(); // reopening must not overlap RefCell borrows
        calendar
            .preferences
            .borrow()
            .as_ref()
            .unwrap()
            .dialog
            .close();
        let meeting = Meeting {
            provider: "microsoft".into(),
            id: "occurrence1".into(),
            title: "Planning <&>".into(),
            starts_at: ipc::now() - 60,
            ends_at: ipc::now() + 1800,
            participants: vec![crate::calendar::Person {
                name: "Alex <&>".into(),
                email: "alex@example.com".into(),
            }],
        };
        calendar.config.borrow_mut().enabled = true;
        *calendar.events.borrow_mut() = vec![meeting.clone()];
        calendar.render();
        let row = calendar
            .list
            .row_at_index(0)
            .unwrap()
            .downcast::<adw::ActionRow>()
            .unwrap();
        assert_eq!(row.title(), "Planning <&>");
        assert!(!row.uses_markup());
        hub.record_calendar_meeting(&meeting);
        let recorder = hub.recording().expect("calendar Record starts a recording");
        assert_eq!(recorder.title(), meeting.title);
        assert_eq!(recorder.calendar_meeting.borrow().as_ref(), Some(&meeting));
        let staging = recorder.staging.borrow().clone().unwrap();
        assert_eq!(
            super::super::read_recording_note(&staging)
                .unwrap()
                .calendar,
            Some(meeting.clone())
        );
        let mut different = meeting.clone();
        different.title = "A different meeting".into();
        hub.record_calendar_meeting(&different);
        assert_eq!(recorder.title(), meeting.title);
        recorder.mic.stop_recording();
        recorder.system.stop_recording();
        recorder.ready();
        assert!(recorder.calendar_meeting.borrow().is_none());
        assert!(!recorder.calendar_rows[0].is_visible());
        for recorder in hub.recorders() {
            recorder.window.destroy();
        }
        hub.home.get().unwrap().window.destroy();
        std::fs::remove_dir_all(staging).unwrap();
    }
}
