//! One action manager shared by Home preferences and every transcript.
use super::*;
use crate::actions::{self, Action};

pub(super) fn present(parent: &impl IsA<gtk::Widget>, hub: std::rc::Weak<Hub>) {
    let dialog = adw::PreferencesDialog::builder().title("Actions").build();
    let page = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::builder()
        .title("Your actions")
        .description("Available for every meeting. Automatic actions run in this order after a new transcript and optional chapters are saved, including imports and retranscription.").build();
    let list = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .build();
    group.add(&list);
    page.add(&group);
    let help = adw::PreferencesGroup::builder()
        .description("Automatic runs are off by default (auto = true in config.toml). Scripts can change files or share meeting data. Opening or editing a transcript never runs them.").build();
    help.add(&gtk::LinkButton::with_label(
        actions::DOCS,
        "Script documentation and examples",
    ));
    page.add(&help);
    dialog.add(&page);
    render(&dialog, &list, hub);
    dialog.present(Some(parent));
}

fn render(dialog: &adw::PreferencesDialog, list: &gtk::Box, hub: std::rc::Weak<Hub>) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
    let actions = match actions::read() {
        Ok(actions) => actions,
        Err(error) => {
            let label = gtk::Label::builder()
                .label(error)
                .wrap(true)
                .xalign(0.0)
                .build();
            list.append(&label);
            return;
        }
    };
    if actions.is_empty() {
        list.append(
            &gtk::Label::builder()
                .label("No actions yet. Add a command or script to get started.")
                .wrap(true)
                .xalign(0.0)
                .build(),
        );
    }
    for action in actions {
        let row = adw::ActionRow::builder()
            .activatable(true)
            .subtitle_lines(2)
            .build();
        row.set_use_markup(false);
        row.set_title(&action.name);
        row.set_subtitle(&format!(
            "{} · {}",
            if action.auto {
                "Automatic"
            } else {
                "Manual"
            },
            action.command
        ));
        row.add_suffix(&gtk::Image::from_icon_name("document-edit-symbolic"));
        let weak_dialog = dialog.downgrade();
        let weak_list = list.downgrade();
        let hub = hub.clone();
        row.connect_activated(move |_| {
            if let (Some(dialog), Some(list)) = (weak_dialog.upgrade(), weak_list.upgrade()) {
                edit(&dialog, &list, Some(action.clone()), hub.clone());
            }
        });
        let rows = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        rows.append(&row);
        list.append(&rows);
    }
    let add = gtk::Button::builder()
        .label("Add action")
        .halign(gtk::Align::Start)
        .css_classes(["suggested-action"])
        .build();
    let weak_dialog = dialog.downgrade();
    let weak_list = list.downgrade();
    add.connect_clicked(move |_| {
        if let (Some(dialog), Some(list)) = (weak_dialog.upgrade(), weak_list.upgrade()) {
            edit(&dialog, &list, None, hub.clone());
        }
    });
    list.append(&add);
}

fn edit(
    parent: &adw::PreferencesDialog,
    list: &gtk::Box,
    previous: Option<Action>,
    hub: std::rc::Weak<Hub>,
) {
    let dialog = adw::PreferencesDialog::builder()
        .title(if previous.is_some() {
            "Edit action"
        } else {
            "Add action"
        })
        .build();
    let page = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::new();
    let name = adw::EntryRow::builder().title("Action name").build();
    let command = adw::EntryRow::builder()
        .title("Command or script path")
        .build();
    let automatic = adw::SwitchRow::builder()
        .title("Run automatically")
        .subtitle("Run once for each newly generated transcript. Manual runs remain available.")
        .build();
    if let Some(action) = &previous {
        name.set_text(&action.name);
        command.set_text(&action.command);
        automatic.set_active(action.auto);
    }
    group.add(&name);
    group.add(&command);
    group.add(&automatic);
    let message = gtk::Label::builder()
        .wrap(true)
        .xalign(0.0)
        .margin_top(12)
        .build();
    group.add(&message);
    let buttons = gtk::Box::builder().spacing(12).margin_top(12).build();
    let save = gtk::Button::builder()
        .label("Save action")
        .css_classes(["suggested-action"])
        .build();
    buttons.append(&save);
    let done: Rc<dyn Fn()> = {
        let parent = parent.downgrade();
        let list = list.downgrade();
        let editor = dialog.downgrade();
        Rc::new(move || {
            if let (Some(parent), Some(list)) = (parent.upgrade(), list.upgrade()) {
                render(&parent, &list, hub.clone());
            }
            if let Some(hub) = hub.upgrade() {
                for recorder in hub.recorders() {
                    recorder.refresh_actions();
                }
            }
            if let Some(editor) = editor.upgrade() {
                editor.close();
            }
        })
    };
    let old = previous.clone();
    let saved = done.clone();
    let error_label = message.clone();
    save.connect_clicked(move |_| {
        let action = Action {
            name: name.text().to_string(),
            command: command.text().to_string(),
            auto: automatic.is_active(),
        };
        match actions::save(old.as_ref(), &action) {
            Ok(()) => saved(),
            Err(error) => error_label.set_label(&error),
        }
    });
    if let Some(previous) = previous {
        let remove = gtk::Button::builder()
            .label("Delete action")
            .css_classes(["destructive-action"])
            .build();
        let editor = dialog.downgrade();
        remove.connect_clicked(move |_| {
            let Some(editor) = editor.upgrade() else {
                return;
            };
            let confirm = adw::AlertDialog::new(
                Some("Delete this action?"),
                Some(
                    "It will be removed from every meeting. A command already running will finish.",
                ),
            );
            confirm.add_response("cancel", "Cancel");
            confirm.add_response("delete", "Delete action");
            confirm.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
            confirm.set_close_response("cancel");
            let previous = previous.clone();
            let done = done.clone();
            let message = message.clone();
            confirm.connect_response(Some("delete"), move |_, _| {
                match actions::remove(&previous) {
                    Ok(()) => done(),
                    Err(error) => message.set_label(&error),
                }
            });
            confirm.present(Some(&editor));
        });
        buttons.append(&remove);
    }
    group.add(&buttons);
    page.add(&group);
    dialog.add(&page);
    dialog.present(Some(parent));
}
