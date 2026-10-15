//! Application navigation and shared recording resources.
use super::*;

/// What all windows share: the audio sources, the socket for the bar widget
/// and the keybindings, and the list of open windows. Every window can show
/// its own meeting; only one records at a time.
pub(super) struct Hub {
    pub(super) mic: Source,
    pub(super) system: Source,
    pub(super) statuses: ipc::Statuses,
    /// The open windows, oldest first. Held here: the window's own callbacks
    /// only hold weak references, so this is what keeps a window's recorder
    /// alive until the window is gone.
    windows: RefCell<Vec<Rc<Recorder>>>,
    /// The microphones and outputs for the menus, `None` until first listed.
    pub(super) devices: RefCell<Option<Devices>>,
    pub(super) home: std::cell::OnceCell<Rc<home::Home>>,
}

impl Hub {
    pub(super) fn new(app: &adw::Application) -> Rc<Self> {
        let hub = Rc::new(Hub {
            mic: Source::spawn(settings::device_to_record(Role::Mic)),
            system: Source::spawn(settings::device_to_record(Role::System)),
            statuses: ipc::Statuses::default(),
            windows: RefCell::default(),
            devices: RefCell::default(),
            home: std::cell::OnceCell::new(),
        });
        hub.watch_devices();
        let (commands_tx, commands_rx) = async_channel::unbounded();
        ipc::serve(
            hub.statuses.clone(),
            hub.mic.clone(),
            hub.system.clone(),
            commands_tx,
        );
        let weak = Rc::downgrade(&hub);
        let app_for_commands = app.clone();
        glib::spawn_future_local(async move {
            while let Ok(command) = commands_rx.recv().await {
                let Some(hub) = weak.upgrade() else {
                    break;
                };
                hub.command(&app_for_commands, command.0, &command.1);
            }
        });

        let new_window = gio::SimpleAction::new("new-window", None);
        let weak = Rc::downgrade(&hub);
        let app_for_new = app.clone();
        new_window.connect_activate(move |_, _| {
            if let Some(hub) = weak.upgrade() {
                hub.add_window(&app_for_new).window.present();
            }
        });
        app.add_action(&new_window);

        // Quitting closes every window; one in the middle of a meeting asks first.
        let quit = gio::SimpleAction::new("quit", None);
        let weak = Rc::downgrade(&hub);
        quit.connect_activate(move |_, _| {
            if let Some(hub) = weak.upgrade() {
                for recorder in hub.recorders() {
                    recorder.window.close();
                }
                if let Some(home) = hub.home.get() {
                    home.window.close();
                }
            }
        });
        app.add_action(&quit);
        let home = home::Home::new(app, &hub);
        let _ = hub.home.set(home);
        let weak = Rc::downgrade(&hub);
        app.connect_window_removed(move |_, window| {
            let Some(hub) = weak.upgrade() else {
                return;
            };
            let removed = {
                let mut windows = hub.windows.borrow_mut();
                windows
                    .iter()
                    .position(|r| r.window.upcast_ref::<gtk::Window>() == window)
                    .map(|index| windows.remove(index))
            };
            if let Some(recorder) = removed {
                hub.statuses
                    .lock()
                    .unwrap()
                    .retain(|s| !Arc::ptr_eq(s, &recorder.shared));
            }
            hub.settle_windows();
        });
        let weak = Rc::downgrade(&hub);
        let app_record = app.clone();
        let record = gio::SimpleAction::new("record", None);
        record.connect_activate(move |_, _| {
            if let Some(hub) = weak.upgrade() {
                if let Some(r) = hub.recording() {
                    r.stop();
                } else {
                    let r = hub.free_window(&app_record);
                    r.ready();
                    hub.present_recorder(&r);
                    r.start();
                }
            }
        });
        app.add_action(&record);
        let action = gio::SimpleAction::new("home", None);
        let weak = Rc::downgrade(&hub);
        action.connect_activate(move |_, _| {
            if let Some(hub) = weak.upgrade() {
                hub.show_home();
            }
        });
        app.add_action(&action);
        let action = gio::SimpleAction::new("open-meeting", Some(glib::VariantTy::STRING));
        let weak = Rc::downgrade(&hub);
        action.connect_activate(move |_, value| {
            if let Some(hub) = weak.upgrade()
                && let Some(path) = value.and_then(|v| v.str())
            {
                hub.open_saved(std::path::Path::new(path), None);
            }
        });
        app.add_action(&action);
        let weak = Rc::downgrade(&hub);
        glib::timeout_add_local(Duration::from_millis(500), move || {
            let Some(hub) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if let Some(home) = hub.home.get() {
                home.update_jobs();
            }
            glib::ControlFlow::Continue
        });
        hub
    }

    /// Lists the devices now and again whenever one comes or goes, so a
    /// headset plugged in shows up in the menus.
    pub(super) fn watch_devices(self: &Rc<Self>) {
        let (changed_tx, changed) = async_channel::bounded(1);
        let _ = changed_tx.try_send(());
        audio::watch_devices(move || {
            !matches!(
                changed_tx.try_send(()),
                Err(async_channel::TrySendError::Closed(_))
            )
        });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while changed.recv().await.is_ok() {
                // Changes come in bursts: list once they have settled.
                glib::timeout_future(Duration::from_millis(150)).await;
                let _ = changed.try_recv();
                let Ok(devices) = gio::spawn_blocking(audio::devices).await else {
                    continue;
                };
                let Some(hub) = weak.upgrade() else {
                    break;
                };
                *hub.devices.borrow_mut() = Some(devices);
                hub.update_pickers();
            }
        });
    }

    /// Shows the devices, and the ones being recorded, in every window.
    pub(super) fn update_pickers(&self) {
        let devices = self.devices.borrow();
        for recorder in self.recorders() {
            recorder.update_pickers(devices.as_ref());
        }
    }

    pub(super) fn add_window(self: &Rc<Self>, app: &adw::Application) -> Rc<Recorder> {
        let recorder = Recorder::new(app, self);
        self.windows.borrow_mut().push(recorder.clone());
        recorder.learn_hypr_address();
        recorder
    }

    /// Reconcile liveness after a whole UI transition, including any window being presented.
    pub(super) fn settle_windows(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            if let Some(hub) = weak.upgrade() {
                hub.close_hidden_views_if_idle();
            }
        });
    }

    /// Hidden views support navigation, but must not keep an otherwise closed app alive.
    fn close_hidden_views_if_idle(&self) {
        let Some(home) = self.home.get() else {
            return;
        };
        let recorders = self.recorders();
        if home.window.is_visible()
            || recorders
                .iter()
                .any(|r| r.window.is_visible() || r.state.get().busy() || r.acting())
        {
            return;
        }
        if let Some(app) = home.window.application() {
            app.quit();
        }
    }

    pub(super) fn recoveries(&self) -> Vec<PathBuf> {
        let active: Vec<_> = self
            .recorders()
            .iter()
            .filter(|r| r.state.get().busy())
            .filter_map(|r| r.staging.borrow().clone())
            .collect();
        unfinished_recordings()
            .into_iter()
            .filter(|path| !active.contains(path))
            .collect()
    }

    /// The open windows, oldest first.
    pub(super) fn recorders(&self) -> Vec<Rc<Recorder>> {
        self.windows.borrow().clone()
    }

    /// The window that is recording, if any.
    pub(super) fn recording(&self) -> Option<Rc<Recorder>> {
        self.recorders()
            .into_iter()
            .find(|r| r.state.get() == State::Recording)
    }

    /// Where the bar widget and the keybindings act: the window recording,
    /// else the one transcribing, else the one in front, else a new one.
    pub(super) fn target(self: &Rc<Self>, app: &adw::Application) -> Rc<Recorder> {
        let recorders = self.recorders();
        let active = app.active_window();
        self.recording()
            .or_else(|| {
                recorders
                    .iter()
                    .find(|r| r.state.get() == State::Transcribing)
                    .cloned()
            })
            .or_else(|| {
                recorders
                    .iter()
                    .find(|r| active.as_ref() == Some(r.window.upcast_ref()))
                    .cloned()
            })
            .or_else(|| recorders.last().cloned())
            .unwrap_or_else(|| self.add_window(app))
    }

    /// A window free to show a meeting: the one in front when it is not busy,
    /// else another free one, else a new one.
    pub(super) fn free_window(self: &Rc<Self>, app: &adw::Application) -> Rc<Recorder> {
        let free = |r: &Rc<Recorder>| matches!(r.state.get(), State::Idle | State::Done);
        let recorders = self.recorders();
        let active = app.active_window();
        recorders
            .iter()
            .find(|r| active.as_ref() == Some(r.window.upcast_ref()) && free(r))
            .or_else(|| recorders.iter().rev().find(|r| free(r)))
            .cloned()
            .unwrap_or_else(|| self.add_window(app))
    }

    /// A command from the socket: `omarchy-meeting-recorder start` and friends.
    /// `title` is what came with it: the name for `start`, the notes file for
    /// `notes`, "" without one.
    pub(super) fn command(self: &Rc<Self>, app: &adw::Application, command: &str, title: &str) {
        if command == "new-window" {
            self.add_window(app).window.present();
            return;
        }
        if command == "notes" || command == "name" {
            let r = self.recording().unwrap_or_else(|| self.free_window(app));
            // Notes are for reading while recording: a window showing a meeting
            // goes back to the recording page first.
            if r.state.get() == State::Done {
                r.ready();
            }
            if command == "notes" {
                r.open_notes(std::path::Path::new(title));
            } else if r.state.get() == State::Idle {
                set_name(&r.title_row, title);
            }
            self.present_recorder(&r);
            return;
        }
        let r = match command {
            // Starting takes a window that is free, so a meeting being
            // transcribed stays on screen.
            "start" => self.recording().unwrap_or_else(|| self.free_window(app)),
            _ => self.target(app),
        };
        match command {
            // A name typed on the ready page is kept unless the command
            // brings one; from the done page it starts fresh.
            "start" if matches!(r.state.get(), State::Idle | State::Done) => {
                if r.state.get() == State::Done {
                    r.ready();
                }
                if !title.is_empty() {
                    set_name(&r.title_row, title);
                }
                r.start();
                self.present_recorder(&r);
            }
            "stop" => r.stop(),
            "compact" => r.set_compact(!r.compact.get()),
            "pause" => r.toggle_pause(),
            _ => {}
        }
    }
}
