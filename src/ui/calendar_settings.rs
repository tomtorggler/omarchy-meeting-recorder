//! Configuration is explicit: saving a client ID alone never starts network access.
use super::calendar::{Calendar, Operation};
use super::*;

pub(super) struct Preferences {
    pub(super) dialog: adw::PreferencesDialog,
    email: adw::EntryRow,
    client: adw::EntryRow,
    connect: gtk::Button,
    disconnect: gtk::Button,
    cancel: gtk::Button,
    message: gtk::Label,
}

impl Preferences {
    pub(super) fn new(calendar: &Rc<Calendar>) -> Rc<Self> {
        let dialog = adw::PreferencesDialog::builder()
            .title("Microsoft Calendar")
            .build();
        let page = adw::PreferencesPage::new();
        let group = adw::PreferencesGroup::builder().title("Work or school account")
            .description("Optional. Read upcoming meetings from your primary Microsoft 365 calendar. The tenant is discovered from your email domain.").build();
        let email = adw::EntryRow::builder()
            .title("Email address")
            .input_purpose(gtk::InputPurpose::Email)
            .build();
        let client = adw::EntryRow::builder()
            .title("Application (client) ID")
            .build();
        group.add(&email);
        group.add(&client);
        let actions = gtk::Box::builder().spacing(12).margin_top(12).build();
        let connect = gtk::Button::builder()
            .label("Connect with Microsoft")
            .css_classes(["suggested-action"])
            .build();
        let disconnect = gtk::Button::with_label("Disconnect");
        let cancel = gtk::Button::with_label("Cancel");
        actions.append(&connect);
        actions.append(&disconnect);
        actions.append(&cancel);
        group.add(&actions);
        let message = gtk::Label::builder()
            .wrap(true)
            .xalign(0.0)
            .margin_top(12)
            .use_markup(false)
            .build();
        group.add(&message);
        page.add(&group);
        let setup = adw::PreferencesGroup::builder().title("Entra app setup")
            .description("Register a public Mobile and desktop application with redirect URI http://localhost and delegated Microsoft Graph Calendars.ReadBasic and User.Read permissions. No client secret is needed. Your organization may require administrator consent.").build();
        let help = gtk::LinkButton::with_label(
            "https://learn.microsoft.com/en-us/entra/identity-platform/quickstart-register-app",
            "App registration instructions",
        );
        help.set_halign(gtk::Align::Start);
        setup.add(&help);
        page.add(&setup);
        page.add(&adw::PreferencesGroup::builder().title("Privacy")
            .description("Sign-in credentials are stored in your desktop keyring. Calendar access is read-only; your basic profile confirms the signed-in email. Titles and invitees are saved with a recording only when you choose Record. Disconnect turns calendar access off and removes the saved sign-in; existing recordings keep their details.").build());
        dialog.add(&page);
        let this = Rc::new(Self {
            dialog,
            email,
            client,
            connect,
            disconnect,
            cancel,
            message,
        });
        this.load_config(&calendar.config.borrow());
        let weak = Rc::downgrade(calendar);
        let prefs = Rc::downgrade(&this);
        this.connect.connect_clicked(move |_| {
            if let (Some(calendar), Some(prefs)) = (weak.upgrade(), prefs.upgrade()) {
                match crate::calendar::Config::new(&prefs.email.text(), &prefs.client.text()) {
                    Ok(config) => calendar.run(Operation::Connect(config)),
                    Err(message) => prefs.message.set_label(&message),
                }
            }
        });
        let weak = Rc::downgrade(calendar);
        this.disconnect.connect_clicked(move |_| {
            if let Some(calendar) = weak.upgrade() {
                calendar.run(Operation::Disconnect);
            }
        });
        let weak = Rc::downgrade(calendar);
        this.cancel.connect_clicked(move |_| {
            if let Some(calendar) = weak.upgrade() {
                calendar.cancel();
            }
        });
        this
    }

    pub(super) fn load_config(&self, config: &crate::calendar::Config) {
        self.email.set_text(&config.email);
        self.client.set_text(&config.client_id);
    }

    pub(super) fn update(&self, calendar: &Calendar) {
        let busy = calendar.busy.get();
        let config = calendar.config.borrow();
        self.email.set_sensitive(!busy && !config.enabled);
        self.client.set_sensitive(!busy && !config.enabled);
        self.connect.set_sensitive(!busy);
        self.connect.set_label(if config.enabled {
            "Reconnect with Microsoft"
        } else {
            "Connect with Microsoft"
        });
        self.disconnect.set_sensitive(!busy);
        self.disconnect.set_visible(!config.tenant.is_empty());
        self.cancel.set_visible(busy && calendar.connecting.get());
        self.message.set_label(&calendar.status_text());
    }
}
