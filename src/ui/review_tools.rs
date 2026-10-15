//! Transcript editing, bookmarks, and exporting the transcript.
use super::*;

impl Recorder {
    pub(super) fn configure_action(self: &Rc<Self>) {
        action_settings::present(&self.window, self.hub.clone());
    }

    pub(super) fn connect_review_tools(self: &Rc<Self>) {
        let action = gio::SimpleAction::new("bookmark", None);
        let weak = Rc::downgrade(self);
        action.connect_activate(move |_, _| {
            if let Some(r) = weak.upgrade() {
                r.bookmark();
            }
        });
        self.window.add_action(&action);
    }

    fn bookmark(&self) {
        if self.state.get() != State::Recording || self.paused.get() {
            return;
        }
        let Some(staging) = self.staging.borrow().clone() else {
            return;
        };
        let mut bookmarks = self.bookmarks.borrow().clone();
        let at_ms = self.elapsed() * 1000;
        bookmarks.push(crate::bookmarks::Bookmark {
            at_ms,
            label: format!("Moment {}", bookmarks.len() + 1),
        });
        match crate::bookmarks::save_bookmarks(&staging, &bookmarks) {
            Ok(()) => {
                *self.bookmarks.borrow_mut() = bookmarks;
                self.toast(&format!("Bookmarked {}", chapters::clock(at_ms)));
            }
            Err(error) => self.toast(&format!("Could not save bookmark: {error}")),
        }
    }

    pub(super) fn refresh_bookmarks(self: &Rc<Self>) {
        let bookmarks = self
            .result_dir
            .borrow()
            .as_ref()
            .map(|dir| crate::bookmarks::bookmarks(dir))
            .unwrap_or_default();
        self.bookmark_menu.set_visible(!bookmarks.is_empty());
        let list = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .build();
        let popover = gtk::Popover::builder().child(&list).build();
        for bookmark in bookmarks {
            let button = gtk::Button::with_label(&format!(
                "{} · {}",
                chapters::clock(bookmark.at_ms),
                bookmark.label
            ));
            let weak = Rc::downgrade(self);
            let popover = popover.clone();
            button.connect_clicked(move |_| {
                if let Some(r) = weak.upgrade() {
                    r.player.play_from(bookmark.at_ms);
                    r.highlight(bookmark.at_ms);
                }
                popover.popdown();
            });
            list.append(&button);
        }
        self.bookmark_menu.set_popover(Some(&popover));
    }

    pub(super) fn update_health(&self) {
        let active = self.state.get() == State::Recording;
        let problem = if active {
            self.mic
                .health()
                .map(|s| format!("Microphone: {s}"))
                .or_else(|| self.system.health().map(|s| format!("Computer: {s}")))
                .unwrap_or_default()
        } else {
            String::new()
        };
        self.health_banner.set_title(&problem);
        self.health_banner
            .set_revealed(!problem.is_empty() && !self.compact.get());
        self.compact_dot
            .set_label(if problem.is_empty() { "●" } else { "!" });
        self.compact_dot
            .set_tooltip_text((!problem.is_empty()).then_some(problem.as_str()));
        if *self.health_last.borrow() != problem {
            if !problem.is_empty()
                && let Some(app) = self.window.application()
            {
                let notification = gio::Notification::new("Check your recording");
                notification.set_body(Some(&problem));
                notification.set_default_action("app.home");
                app.send_notification(Some("recording-health"), &notification);
            } else if let Some(app) = self.window.application() {
                app.withdraw_notification("recording-health");
            }
            *self.health_last.borrow_mut() = problem;
        }
    }

    pub(super) fn export_controls(self: &Rc<Self>, parent: &gtk::Box) {
        let controls = gtk::Box::builder().spacing(8).homogeneous(true).build();
        let clean = gtk::Button::with_label("Copy text");
        let save = gtk::Button::with_label("Save notes…");
        controls.append(&clean);
        controls.append(&save);
        parent.append(&controls);
        let weak = Rc::downgrade(self);
        clean.connect_clicked(move |_| {
            if let Some(r) = weak.upgrade()
                && let Some(dir) = r.result_dir.borrow().as_ref()
            {
                match std::fs::read_to_string(dir.join("transcript.md")) {
                    Ok(text) => {
                        r.window
                            .clipboard()
                            .set_text(&crate::bookmarks::clean_text(&text));
                        r.toast("Copied text without timestamps");
                    }
                    Err(error) => r.toast(&format!("Could not read transcript: {error}")),
                }
            }
        });
        let weak = Rc::downgrade(self);
        save.connect_clicked(move |_| {
            if let Some(r) = weak.upgrade() {
                r.save_notes();
            }
        });
    }

    fn save_notes(self: &Rc<Self>) {
        let Some(dir) = self.result_dir.borrow().clone() else {
            return;
        };
        let dialog = gtk::FileDialog::builder()
            .title("Save meeting notes to a folder")
            .modal(true)
            .build();
        if let Some(folder) = settings::notes_folder() {
            dialog.set_initial_folder(Some(&gio::File::for_path(folder)));
        }
        let weak = Rc::downgrade(self);
        dialog.select_folder(Some(&self.window), gio::Cancellable::NONE, move |result| {
            let Some(r) = weak.upgrade() else {
                return;
            };
            let Ok(folder) = result else {
                return;
            };
            let Some(folder) = folder.path() else {
                r.toast("Choose a local folder");
                return;
            };
            settings::save_notes_folder(&folder);
            glib::spawn_future_local(async move {
                let result = gio::spawn_blocking(move || crate::bookmarks::export(&dir, &folder)).await;
                match result {
                    Ok(Ok(path)) => r.toast(&format!("Saved {}", path.display())),
                    Ok(Err(error)) => r.toast(&format!("Could not save notes: {error}")),
                    Err(_) => r.toast("Could not finish saving notes"),
                }
            });
        });
    }

    /// One paragraph of the transcript: time, speaker and text in columns, with
    /// edit, swap-speaker and delete buttons that show on hover.
    pub(super) fn paragraph_row(
        self: &Rc<Self>,
        paragraph: &Paragraph,
        speaker_index: usize,
        time_width: i32,
        speaker_width: i32,
    ) -> gtk::ListBoxRow {
        let time = gtk::Label::builder()
            .label(&paragraph.time)
            .xalign(0.0)
            .valign(gtk::Align::Start)
            .width_request(time_width)
            .css_classes(["numeric", "dim-label"])
            .build();
        let speaker = gtk::Label::builder()
            .label(&paragraph.speaker)
            .xalign(0.0)
            .valign(gtk::Align::Start)
            .width_request(speaker_width)
            .css_classes([format!("speaker-{}", speaker_index % 6)])
            .build();
        let text = gtk::Label::builder()
            .label(&paragraph.text)
            .xalign(0.0)
            .valign(gtk::Align::Start)
            .hexpand(true)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .natural_wrap_mode(gtk::NaturalWrapMode::None)
            .build();
        let action = |icon: &str, tooltip: &str| {
            gtk::Button::builder()
                .icon_name(icon)
                .tooltip_text(tooltip)
                .valign(gtk::Align::Start)
                .css_classes(["flat", "circular"])
                .build()
        };
        let edit = action("document-edit-symbolic", "Edit this line");
        let swap = action("avatar-default-symbolic", "Choose speaker");
        let delete = action("user-trash-symbolic", "Delete this line");
        let actions = gtk::Box::builder()
            .spacing(2)
            .valign(gtk::Align::Start)
            .css_classes(["row-actions"])
            .build();
        actions.append(&edit);
        actions.append(&swap);
        actions.append(&delete);

        let content = gtk::Box::builder().spacing(14).build();
        content.append(&time);
        content.append(&speaker);
        content.append(&text);
        content.append(&actions);
        let row = gtk::ListBoxRow::builder()
            .child(&content)
            .tooltip_text(format!("Play from {}", paragraph.time))
            .build();
        row.set_cursor_from_name(Some("pointer"));

        let (sources, stamp, who) = (
            paragraph.sources.clone(),
            paragraph.time.clone(),
            paragraph.speaker.clone(),
        );
        let weak = Rc::downgrade(self);
        let delete_sources = sources.clone();
        delete.connect_clicked(move |_| {
            if let Some(r) = weak.upgrade() {
                r.delete_paragraph(&delete_sources);
            }
        });
        let weak = Rc::downgrade(self);
        let swap_sources = sources.clone();
        swap.connect_clicked(move |_| {
            if let Some(r) = weak.upgrade() {
                r.choose_speaker(&swap_sources);
            }
        });
        let weak = Rc::downgrade(self);
        let original = paragraph.text.clone();
        let (edit_content, edit_speaker, edit_row) =
            (content.clone(), speaker.clone(), row.clone());
        edit.connect_clicked(move |button| {
            let Some(r) = weak.upgrade() else { return };
            r.start_editing(
                &edit_row,
                &edit_content,
                &edit_speaker,
                &text,
                button,
                &sources,
                &stamp,
                &who,
                &original,
            );
        });
        row
    }

    /// Swaps the text of a row for an editor: Enter saves, Escape cancels,
    /// leaving the field saves too.
    #[allow(clippy::too_many_arguments)]
    fn start_editing(
        self: &Rc<Self>,
        row: &gtk::ListBoxRow,
        content: &gtk::Box,
        speaker: &gtk::Label,
        label: &gtk::Label,
        edit_button: &gtk::Button,
        sources: &[usize],
        time: &str,
        who: &str,
        original: &str,
    ) {
        if self.state.get() != State::Done {
            self.toast("Wait for actions to finish before editing the transcript");
            return;
        }
        row.set_activatable(false);
        row.add_css_class("editing");
        // Start at the size of the text it replaces: without a width the first
        // frame wraps every letter and the field flashes up very tall.
        let (width, height) = (label.width().max(120), label.height().max(1));
        label.set_visible(false);
        edit_button.set_sensitive(false);
        let editor = gtk::TextView::builder()
            .wrap_mode(gtk::WrapMode::WordChar)
            .hexpand(true)
            .valign(gtk::Align::Start)
            .width_request(width)
            .height_request(height)
            .css_classes(["transcript-editor"])
            .build();
        editor.buffer().set_text(original);
        content.insert_child_after(&editor, Some(speaker));
        editor.grab_focus();

        let done = Rc::new(Cell::new(false));
        let finish = {
            let (weak, editor, label, row, button) = (
                Rc::downgrade(self),
                editor.clone(),
                label.clone(),
                row.clone(),
                edit_button.clone(),
            );
            let (sources, time, who, original) = (
                sources.to_vec(),
                time.to_owned(),
                who.to_owned(),
                original.to_owned(),
            );
            let done = done.clone();
            move |save: bool| {
                if done.replace(true) {
                    return;
                }
                let buffer = editor.buffer();
                let text = buffer
                    .text(&buffer.start_iter(), &buffer.end_iter(), false)
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                if let Some(parent) = editor.parent().and_downcast::<gtk::Box>() {
                    parent.remove(&editor);
                }
                label.set_visible(true);
                row.set_activatable(true);
                row.remove_css_class("editing");
                button.set_sensitive(true);
                if save
                    && text != original
                    && let Some(r) = weak.upgrade()
                {
                    // Deferred: saving rebuilds the list this row lives in.
                    let (sources, time, who) = (sources.clone(), time.clone(), who.clone());
                    glib::idle_add_local_once(move || {
                        if text.is_empty() {
                            r.delete_paragraph(&sources);
                        } else {
                            r.edit_paragraph(&sources, &time, &who, &text);
                        }
                    });
                }
            }
        };
        let finish = Rc::new(finish);
        let keys = gtk::EventControllerKey::new();
        let on_key = finish.clone();
        keys.connect_key_pressed(move |_, key, _, modifiers| match key {
            gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter
                if !modifiers.contains(gtk::gdk::ModifierType::SHIFT_MASK) =>
            {
                on_key(true);
                glib::Propagation::Stop
            }
            gtk::gdk::Key::Escape => {
                on_key(false);
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        });
        editor.add_controller(keys);
        let focus = gtk::EventControllerFocus::new();
        let on_leave = finish.clone();
        focus.connect_leave(move |_| on_leave(true));
        editor.add_controller(focus);
    }

    /// Changes transcript.md line by line and shows the result, keeping the
    /// scroll position. Returns the text as it was, for undo.
    fn rewrite_transcript(
        self: &Rc<Self>,
        change: impl FnOnce(&mut Vec<String>),
    ) -> Option<String> {
        if self.state.get() != State::Done {
            self.toast("Wait for actions to finish before editing the transcript");
            return None;
        }
        let dir = self.result_dir.borrow().clone()?;
        let path = dir.join("transcript.md");
        let before = std::fs::read_to_string(&path).ok()?;
        let mut lines: Vec<String> = before.lines().map(str::to_owned).collect();
        change(&mut lines);
        // Removing lines leaves their blank separators behind; keep one.
        let mut tidy: Vec<String> = Vec::new();
        for line in lines {
            if line.trim().is_empty() && tidy.last().is_some_and(|l| l.trim().is_empty()) {
                continue;
            }
            tidy.push(line);
        }
        let after = tidy.join("\n") + "\n";
        if crate::files::atomic_write(&path, after.as_bytes()).is_err() {
            self.toast("Could not save the transcript");
            return None;
        }
        self.redraw_transcript(&after);
        Some(before)
    }

    pub(super) fn redraw_transcript(self: &Rc<Self>, markdown: &str) {
        let adjustment = self.transcript_scroll.vadjustment();
        let scroll = adjustment.value();
        self.show_transcript(Some(markdown), None);
        glib::idle_add_local_once(move || adjustment.set_value(scroll));
    }

    fn edit_paragraph(self: &Rc<Self>, sources: &[usize], time: &str, who: &str, text: &str) {
        let (first, rest) = match sources.split_first() {
            Some(split) => split,
            None => return,
        };
        let (first, rest) = (*first, rest.to_vec());
        let line = format!("**[{time}] {who}:** {text}");
        if self
            .rewrite_transcript(move |lines| {
                if let Some(slot) = lines.get_mut(first) {
                    *slot = line;
                }
                for index in rest.into_iter().rev() {
                    if index < lines.len() {
                        lines.remove(index);
                    }
                }
            })
            .is_some()
        {
            self.toast("Saved");
        }
    }

    fn delete_paragraph(self: &Rc<Self>, sources: &[usize]) {
        let mut sources = sources.to_vec();
        sources.sort_unstable();
        let Some(before) = self.rewrite_transcript(move |lines| {
            for index in sources.into_iter().rev() {
                if index < lines.len() {
                    lines.remove(index);
                }
            }
        }) else {
            return;
        };
        let toast = adw::Toast::builder()
            .title("Line deleted")
            .button_label("Undo")
            .build();
        let weak = Rc::downgrade(self);
        let generation = self.meeting_generation.get();
        toast.connect_button_clicked(move |_| {
            let Some(r) = weak.upgrade() else { return };
            if r.state.get() != State::Done || r.meeting_generation.get() != generation {
                return;
            }
            if let Some(dir) = r.result_dir.borrow().clone() {
                match crate::files::atomic_write(&dir.join("transcript.md"), before.as_bytes()) {
                    Ok(()) => r.redraw_transcript(&before),
                    Err(error) => r.toast(&format!("Could not undo deletion: {error}")),
                }
            }
        });
        self.toasts.add_toast(toast);
    }

    fn choose_speaker(self: &Rc<Self>, sources: &[usize]) {
        let speakers = self
            .manifest
            .borrow()
            .as_ref()
            .map(|m| m.people())
            .unwrap_or_default();
        let dialog = adw::AlertDialog::new(Some("Who said this?"), None);
        dialog.add_response("cancel", "Cancel");
        dialog.set_close_response("cancel");
        for (i, name) in speakers.iter().enumerate() {
            dialog.add_response(&i.to_string(), name);
        }
        let sources = sources.to_vec();
        let weak = Rc::downgrade(self);
        dialog.connect_response(None, move |_, response| {
            let Some(name) = response
                .parse::<usize>()
                .ok()
                .and_then(|i| speakers.get(i))
                .map(|name| name.to_owned())
            else {
                return;
            };
            if let Some(r) = weak.upgrade() {
                let sources = sources.clone();
                r.rewrite_transcript(move |lines| {
                    for index in sources {
                        if let Some(line) = lines.get_mut(index)
                            && let Some((time, _, text)) = parse_segment(line)
                        {
                            *line = format!("**[{time}] {name}:** {text}");
                        }
                    }
                });
            }
        });
        dialog.present(Some(&self.window));
    }
}
