//! Settings (Cmd+,): the accounts (a table; add one of three kinds in a sheet; remove after a question, like the
//! command line, no editing) and the check and fix commands. Every change goes through the core into
//! config.json, validated there; the poll picks it up. One window, opened once.

use std::cell::RefCell;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::{MainThreadMarker, MainThreadOnly, sel};
use objc2_app_kit::{
    NSAccessibility, NSBackingStoreType, NSButton, NSColor, NSFont, NSGridView, NSModalResponse,
    NSModalResponseOK, NSOpenPanel, NSPanel, NSPopUpButton, NSScrollView, NSSecureTextField,
    NSTableColumn, NSTableView, NSTextField, NSUserInterfaceItemIdentification, NSView, NSWindow,
    NSWindowStyleMask,
};
use objc2_foundation::{NSArray, NSPoint, NSRect, NSSize};

use draftdesk_core::accounts::{self, AccountSecret, CommandSlot};
use draftdesk_core::config::{
    AccountCfg, CommandCfg, Config, GmailCfg, ImapCfg, Kind, Security, SenderCfg, SmtpCfg,
};
use draftdesk_core::gmail::{http::UreqClient, oauth};

use crate::app::{App, Delegate, with_app};
use crate::i18n::tr;
use crate::log::log;
use crate::ui::{insets, label, ns, stack};
use crate::{on_main, spawn};

/// The accounts table's identifier: the delegate serves the sidebar too.
pub const TABLE_ID: &str = "accounts";
const COLUMNS: [&str; 3] = ["name", "kind", "senders"];
/// The time limits offered for the check and fix commands and for command accounts, in seconds.
const TIMEOUTS: [u64; 7] = [30, 60, 120, 180, 300, 600, 900];

thread_local! {
    /// The account rows: their own slot, AppKit asks during a reload inside `with_app`.
    static ROWS: RefCell<Vec<[String; 3]>> = const { RefCell::new(Vec::new()) };
}

pub fn row_count() -> usize {
    ROWS.with(|r| r.borrow().len())
}

/// The table cell of `row` in the column with this identifier.
pub fn cell(mtm: MainThreadMarker, row: usize, column: &str) -> Option<Retained<NSView>> {
    let i = COLUMNS.iter().position(|c| *c == column)?;
    let text = ROWS.with(|r| r.borrow().get(row).map(|cols| cols[i].clone()))?;
    Some(Retained::into_super(Retained::into_super(label(
        mtm, &text,
    ))))
}

pub fn is_accounts(table: &NSTableView) -> bool {
    table
        .identifier()
        .is_some_and(|i| i.to_string() == TABLE_ID)
}

/// A regular file with an execute bit.
pub fn executable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

fn not_executable(path: &str) -> String {
    format!(
        "{path}: {}",
        tr(
            "nicht ausführbar (ein Programm mit Ausführrecht wählen)",
            "not executable (choose a program with the execute permission)"
        )
    )
}

/// The account a sheet describes; `get` reads a field by key. Ports must be numbers, programs runnable; the rest
/// the core's validation checks.
pub fn build(
    kind: Kind,
    get: impl Fn(&str) -> String,
    tls: bool,
    sent_copy: bool,
) -> Result<AccountCfg, String> {
    let text = |k: &str| get(k).trim().to_string();
    let port = |k: &str| {
        text(k).parse::<u16>().map_err(|_| {
            format!(
                "{} {}",
                text(k),
                tr("ist keine Portnummer", "is not a port number")
            )
        })
    };
    let display = Some(text("display")).filter(|s| !s.is_empty());
    let mut acc = AccountCfg {
        name: text("name"),
        kind,
        senders: vec![SenderCfg {
            address: text("address"),
            name: display,
            fixed_name: false,
        }],
        smtp: None,
        imap: None,
        gmail: None,
        send_command: None,
        discard_command: None,
        timeout_s: None,
        save_sent_copy: false,
    };
    match kind {
        Kind::Smtp => {
            acc.smtp = Some(SmtpCfg {
                host: text("host"),
                port: port("port")?,
                security: if tls {
                    Security::Tls
                } else {
                    Security::Starttls
                },
                username: text("user"),
            });
            if !text("imap_host").is_empty() {
                acc.imap = Some(ImapCfg {
                    host: text("imap_host"),
                    port: port("imap_port")?,
                    username: text("user"),
                });
            }
            acc.save_sent_copy = sent_copy;
        }
        Kind::Gmail => {
            acc.gmail = Some(GmailCfg {
                client_id: text("client_id"),
            })
        }
        Kind::Command => {
            for (key, required) in [("send", true), ("discard", false)] {
                let p = text(key);
                if p.is_empty() && !required {
                    continue;
                }
                if !executable(Path::new(&p)) {
                    return Err(not_executable(&p));
                }
                let argv = Some(vec![p]);
                if key == "send" {
                    acc.send_command = argv;
                } else {
                    acc.discard_command = argv;
                }
            }
            // The popup's value; the core checks the range.
            let t = text("timeout");
            if !t.is_empty() {
                acc.timeout_s = Some(t.parse().map_err(|_| format!("timeout_s: {t}"))?);
            }
        }
    }
    accounts::validate(&acc).map_err(|e| e.0)?;
    Ok(acc)
}

/// What a sheet row holds.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Text,
    Secret,
    /// A program path, chosen with a file dialog (index 0 send, 1 discard).
    Program(isize),
}

fn fields(kind: Kind) -> Vec<(&'static str, &'static str, Field)> {
    let mut f = vec![
        (
            "name",
            tr("Name (a-z, 0-9)", "Name (a-z, 0-9)"),
            Field::Text,
        ),
        ("address", tr("Adresse", "Address"), Field::Text),
        ("display", tr("Anzeigename", "Display name"), Field::Text),
    ];
    match kind {
        Kind::Smtp => f.extend([
            ("host", tr("SMTP-Server", "SMTP server"), Field::Text),
            ("port", tr("Port", "Port"), Field::Text),
            ("user", tr("Benutzer", "User name"), Field::Text),
            ("password", tr("Passwort", "Password"), Field::Secret),
            (
                "imap_host",
                tr("IMAP-Server (optional)", "IMAP server (optional)"),
                Field::Text,
            ),
            ("imap_port", tr("IMAP-Port", "IMAP port"), Field::Text),
        ]),
        Kind::Gmail => f.extend([
            (
                "client_id",
                tr("OAuth-Client-ID", "OAuth client ID"),
                Field::Text,
            ),
            (
                "client_secret",
                tr("OAuth-Client-Secret", "OAuth client secret"),
                Field::Secret,
            ),
        ]),
        Kind::Command => f.extend([
            (
                "send",
                tr("Sendeprogramm", "Send program"),
                Field::Program(0),
            ),
            (
                "discard",
                tr(
                    "Verwerfen-Programm (optional)",
                    "Discard program (optional)",
                ),
                Field::Program(1),
            ),
        ]),
    }
    f
}

fn button(
    mtm: MainThreadMarker,
    target: &AnyObject,
    title: &str,
    action: Sel,
) -> Retained<NSButton> {
    // SAFETY: the target is the app delegate, which lives as long as the app.
    unsafe { NSButton::buttonWithTitle_target_action(&ns(title), Some(target), Some(action), mtm) }
}

/// The sheet that adds one account.
pub struct Sheet {
    pub panel: Retained<NSPanel>,
    pub kind: Kind,
    fields: Vec<(&'static str, Retained<NSTextField>)>,
    tls: Option<Retained<NSPopUpButton>>,
    sent_copy: Option<Retained<NSButton>>,
    /// Command accounts: the time limit of the send and discard programs (key "timeout").
    timeout: Option<Retained<NSPopUpButton>>,
    pub error: Retained<NSTextField>,
    pub confirm: Retained<NSButton>,
}

/// A popup offering [`TIMEOUTS`] (plus `current` if it is not one of them), each item tagged with its seconds.
fn timeout_popup(popup: &NSPopUpButton, current: u64) {
    popup.removeAllItems();
    let mut values: Vec<u64> = TIMEOUTS.to_vec();
    if !values.contains(&current) {
        values.push(current);
        values.sort_unstable();
    }
    for v in &values {
        popup.addItemWithTitle(&ns(&format!("{v} s")));
        if let Some(item) = popup.lastItem() {
            item.setTag(*v as isize);
        }
    }
    popup.selectItemWithTag(current as isize);
}

impl Sheet {
    fn new(mtm: MainThreadMarker, delegate: &Delegate, kind: Kind) -> Sheet {
        let target: &AnyObject = delegate.as_ref();
        let mut list = Vec::new();
        let mut rows: Vec<Retained<NSArray<NSView>>> = Vec::new();
        for (key, title, field) in fields(kind) {
            let f: Retained<NSTextField> = match field {
                Field::Secret => Retained::into_super(NSSecureTextField::new(mtm)),
                _ => NSTextField::new(mtm),
            };
            f.setAccessibilityLabel(Some(&ns(title)));
            let l = label(mtm, title);
            let view: Retained<NSView> = match field {
                Field::Program(tag) => {
                    f.setEditable(false);
                    let choose = button(mtm, target, tr("Wählen …", "Choose…"), sel!(sheetChoose:));
                    choose.setTag(tag);
                    Retained::into_super(stack(mtm, &[&f, &choose], false, 6.0))
                }
                _ => Retained::into_super(Retained::into_super(f.clone())),
            };
            rows.push(NSArray::from_slice(&[&*l as &NSView, &view]));
            list.push((key, f));
        }
        let (mut tls, mut sent_copy) = (None, None);
        if kind == Kind::Smtp {
            let popup = NSPopUpButton::new(mtm);
            popup.addItemWithTitle(&ns("STARTTLS (587)"));
            popup.addItemWithTitle(&ns("TLS (465)"));
            popup.setAccessibilityLabel(Some(&ns(tr("Sicherheit", "Security"))));
            let l = label(mtm, tr("Sicherheit", "Security"));
            rows.push(NSArray::from_slice(&[&*l as &NSView, &popup]));
            // SAFETY: as above.
            let check = unsafe {
                NSButton::checkboxWithTitle_target_action(
                    &ns(tr(
                        "Gesendete Kopie im Postfach ablegen (IMAP)",
                        "Keep a sent copy in the mailbox (IMAP)",
                    )),
                    None,
                    None,
                    mtm,
                )
            };
            let empty = label(mtm, "");
            rows.push(NSArray::from_slice(&[&*empty as &NSView, &check]));
            tls = Some(popup);
            sent_copy = Some(check);
        }
        let mut timeout = None;
        if kind == Kind::Command {
            let popup = NSPopUpButton::new(mtm);
            timeout_popup(&popup, draftdesk_core::config::SEND_TIMEOUT_S);
            let title = tr("Zeitlimit", "Time limit");
            popup.setAccessibilityLabel(Some(&ns(title)));
            let l = label(mtm, title);
            rows.push(NSArray::from_slice(&[&*l as &NSView, &popup]));
            timeout = Some(popup);
        }
        let grid = NSGridView::gridViewWithViews(&NSArray::from_retained_slice(&rows), mtm);
        grid.setRowSpacing(6.0);
        grid.setColumnSpacing(8.0);
        let error = NSTextField::wrappingLabelWithString(&ns(""), mtm);
        error.setTextColor(Some(&NSColor::systemRedColor()));
        error.setHidden(true);
        let title = match kind {
            Kind::Smtp => tr("SMTP-Konto hinzufügen", "Add SMTP Account"),
            Kind::Gmail => tr("Google-Konto hinzufügen", "Add Google Account"),
            Kind::Command => tr("Befehls-Konto hinzufügen", "Add Command Account"),
        };
        let heading = label(mtm, title);
        heading.setFont(Some(&NSFont::boldSystemFontOfSize(13.0)));
        let cancel = button(mtm, target, tr("Abbrechen", "Cancel"), sel!(sheetCancel:));
        cancel.setKeyEquivalent(&ns("\u{1b}"));
        let go = match kind {
            Kind::Gmail => tr("Mit Google anmelden", "Sign In with Google"),
            _ => tr("Hinzufügen", "Add"),
        };
        let confirm = button(mtm, target, go, sel!(sheetConfirm:));
        confirm.setKeyEquivalent(&ns("\r"));
        let buttons = stack(mtm, &[&cancel, &confirm], false, 8.0);
        let root = stack(mtm, &[&heading, &grid, &error, &buttons], true, 12.0);
        root.setEdgeInsets(insets(20.0, 20.0, 20.0, 20.0));
        let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(480.0, 200.0));
        let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(mtm),
            rect,
            NSWindowStyleMask::Titled,
            NSBackingStoreType::Buffered,
            false,
        );
        // SAFETY: the `Retained` in `Sheet` owns the panel.
        unsafe { panel.setReleasedWhenClosed(false) };
        panel.setContentView(Some(&root));
        Sheet {
            panel,
            kind,
            fields: list,
            tls,
            sent_copy,
            timeout,
            error,
            confirm,
        }
    }

    fn field(&self, key: &str) -> Option<&NSTextField> {
        self.fields
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, f)| &**f)
    }

    /// A field's text; "timeout" is the popup's seconds (empty without the popup).
    pub fn get(&self, key: &str) -> String {
        if key == "timeout" {
            return self
                .timeout
                .as_ref()
                .map(|p| p.selectedTag().to_string())
                .unwrap_or_default();
        }
        self.field(key)
            .map(|f| f.stringValue().to_string())
            .unwrap_or_default()
    }

    pub fn set(&self, key: &str, value: &str) {
        if key == "timeout" {
            if let (Some(p), Ok(t)) = (&self.timeout, value.parse::<isize>()) {
                p.selectItemWithTag(t);
            }
            return;
        }
        if let Some(f) = self.field(key) {
            f.setStringValue(&ns(value));
        }
    }

    pub fn build(&self) -> Result<AccountCfg, String> {
        let tls = self
            .tls
            .as_ref()
            .is_some_and(|p| p.indexOfSelectedItem() == 1);
        let sent = self.sent_copy.as_ref().is_some_and(|c| c.state() != 0);
        build(self.kind, |k| self.get(k), tls, sent)
    }

    pub fn show_error(&self, text: Option<&str>) {
        self.error.setStringValue(&ns(text.unwrap_or("")));
        self.error.setHidden(text.is_none());
    }

    /// Locked while the Google sign-in runs: the account is saved as the sign-in started with it.
    fn set_locked(&self, locked: bool) {
        for (_, f) in &self.fields {
            f.setEnabled(!locked);
        }
    }
}

/// One of the two commands: program, the arguments config.json adds, choose, remove, time limit.
struct CommandRow {
    path: Retained<NSTextField>,
    clear: Retained<NSButton>,
    timeout: Retained<NSPopUpButton>,
}

/// The settings window.
pub struct Settings {
    pub window: Retained<NSWindow>,
    pub table: Retained<NSTableView>,
    pub remove: Retained<NSButton>,
    commands: [CommandRow; 2],
    /// A red line under the commands: why a program was refused.
    pub status: Retained<NSTextField>,
    pub sheet: Option<Sheet>,
    /// Counts the Google sign-ins: the answer for a sheet already closed is dropped.
    pub signin: u64,
}

fn slot_index(slot: CommandSlot) -> usize {
    match slot {
        CommandSlot::Check => 0,
        CommandSlot::Fix => 1,
    }
}

fn slot_of(tag: isize) -> CommandSlot {
    if tag == 1 {
        CommandSlot::Fix
    } else {
        CommandSlot::Check
    }
}

impl Settings {
    pub fn new(mtm: MainThreadMarker, delegate: &Delegate) -> Settings {
        let target: &AnyObject = delegate.as_ref();
        let table = NSTableView::new(mtm);
        table.setIdentifier(Some(&ns(TABLE_ID)));
        for (id, title, width) in [
            (COLUMNS[0], tr("Name", "Name"), 110.0),
            (COLUMNS[1], tr("Art", "Kind"), 80.0),
            (COLUMNS[2], tr("Absender", "Senders"), 260.0),
        ] {
            let c = NSTableColumn::initWithIdentifier(NSTableColumn::alloc(mtm), &ns(id));
            c.setTitle(&ns(title));
            c.setWidth(width);
            table.addTableColumn(&c);
        }
        // SAFETY: the delegate lives as long as the app.
        unsafe {
            table.setDataSource(Some(ProtocolObject::from_ref(delegate)));
            table.setDelegate(Some(ProtocolObject::from_ref(delegate)));
        }
        table.setAccessibilityLabel(Some(&ns(tr("Konten", "Accounts"))));
        let scroll = NSScrollView::new(mtm);
        scroll.setDocumentView(Some(&table));
        scroll.setHasVerticalScroller(true);
        scroll.setTranslatesAutoresizingMaskIntoConstraints(false);
        scroll
            .heightAnchor()
            .constraintEqualToConstant(140.0)
            .setActive(true);
        let mut add_buttons: Vec<Retained<NSButton>> = Vec::new();
        for (title, tag) in [
            ("SMTP …", 0),
            ("Google …", 1),
            (tr("Befehl …", "Command…"), 2),
        ] {
            let b = button(mtm, target, title, sel!(settingsAdd:));
            b.setTag(tag);
            add_buttons.push(b);
        }
        let remove = button(
            mtm,
            target,
            tr("Entfernen …", "Remove…"),
            sel!(settingsRemove:),
        );
        remove.setEnabled(false);
        let mut row: Vec<&NSView> = add_buttons.iter().map(|b| &**b as &NSView).collect();
        row.push(&remove);
        let account_buttons = stack(mtm, &row, false, 8.0);

        let mut commands = Vec::new();
        let mut command_rows: Vec<Retained<NSArray<NSView>>> = Vec::new();
        for (tag, title) in [
            (0, tr("Prüfbefehl", "Check command")),
            (1, tr("Korrekturbefehl", "Fix command")),
        ] {
            let path = label(mtm, "—");
            path.setAccessibilityLabel(Some(&ns(title)));
            let choose = button(
                mtm,
                target,
                tr("Wählen …", "Choose…"),
                sel!(settingsChooseCommand:),
            );
            choose.setTag(tag);
            let clear = button(
                mtm,
                target,
                tr("Entfernen", "Remove"),
                sel!(settingsClearCommand:),
            );
            clear.setTag(tag);
            let timeout = NSPopUpButton::new(mtm);
            // SAFETY: as above.
            unsafe {
                timeout.setTarget(Some(target));
                timeout.setAction(Some(sel!(settingsTimeout:)));
            }
            timeout.setTag(tag);
            timeout.setAccessibilityLabel(Some(&ns(&format!(
                "{title}: {}",
                tr("Zeitlimit", "Time limit")
            ))));
            let controls = stack(mtm, &[&path, &choose, &clear, &timeout], false, 8.0);
            let l = label(mtm, &format!("{title}:"));
            command_rows.push(NSArray::from_slice(&[&*l as &NSView, &controls]));
            commands.push(CommandRow {
                path,
                clear,
                timeout,
            });
        }
        let grid = NSGridView::gridViewWithViews(&NSArray::from_retained_slice(&command_rows), mtm);
        grid.setRowSpacing(8.0);
        let status = NSTextField::wrappingLabelWithString(&ns(""), mtm);
        status.setTextColor(Some(&NSColor::systemRedColor()));
        status.setHidden(true);
        let accounts_title = label(mtm, tr("Konten", "Accounts"));
        accounts_title.setFont(Some(&NSFont::boldSystemFontOfSize(13.0)));
        let check_title = label(mtm, tr("Prüfen und Korrigieren", "Check and Fix"));
        check_title.setFont(Some(&NSFont::boldSystemFontOfSize(13.0)));
        let root = stack(
            mtm,
            &[
                &accounts_title,
                &scroll,
                &account_buttons,
                &check_title,
                &grid,
                &status,
            ],
            true,
            10.0,
        );
        root.setEdgeInsets(insets(20.0, 20.0, 20.0, 20.0));
        scroll
            .widthAnchor()
            .constraintEqualToAnchor_constant(&root.widthAnchor(), -40.0)
            .setActive(true);
        let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(600.0, 420.0));
        // SAFETY: designated initializer on a fresh allocation.
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                rect,
                NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        // SAFETY: the `Retained` in `Settings` owns the window; closing only hides it.
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&ns(tr("Einstellungen", "Settings")));
        window.setContentView(Some(&root));
        window.setRestorable(false);
        window.center();
        let commands: [CommandRow; 2] = commands
            .try_into()
            .unwrap_or_else(|_| unreachable!("two command rows"));
        Settings {
            window,
            table,
            remove,
            commands,
            status,
            sheet: None,
            signin: 0,
        }
    }

    /// Fills the window from the configuration.
    fn reload(&self, cfg: &Config) {
        let rows: Vec<[String; 3]> = cfg
            .accounts
            .iter()
            .map(|a| {
                let kind = match a.kind {
                    Kind::Smtp => "SMTP",
                    Kind::Gmail => "Google",
                    Kind::Command => tr("Befehl", "Command"),
                };
                let senders: Vec<&str> = a.senders.iter().map(|s| s.address.as_str()).collect();
                [a.name.clone(), kind.to_string(), senders.join(", ")]
            })
            .collect();
        ROWS.with(|r| *r.borrow_mut() = rows);
        self.table.reloadData();
        self.selection_changed();
        for (i, cmd) in [&cfg.check_command, &cfg.fix_command]
            .into_iter()
            .enumerate()
        {
            let row = &self.commands[i];
            let text = match cmd {
                None => "—".to_string(),
                Some(c) if c.argv.len() > 1 => format!(
                    "{} (+ {} {})",
                    c.argv[0],
                    c.argv.len() - 1,
                    tr("Argumente aus config.json", "arguments from config.json")
                ),
                Some(c) => c.argv.first().cloned().unwrap_or_default(),
            };
            row.path.setStringValue(&ns(&text));
            row.clear.setEnabled(cmd.is_some());
            timeout_popup(&row.timeout, cmd.as_ref().map_or(300, |c| c.timeout_s));
            row.timeout.setEnabled(cmd.is_some());
        }
    }

    /// "Remove" needs a selected account.
    pub fn selection_changed(&self) {
        self.remove.setEnabled(self.table.selectedRow() >= 0);
    }

    fn selected_account(&self) -> Option<String> {
        let row = usize::try_from(self.table.selectedRow()).ok()?;
        ROWS.with(|r| r.borrow().get(row).map(|cols| cols[0].clone()))
    }

    fn show_status(&self, text: Option<&str>) {
        self.status.setStringValue(&ns(text.unwrap_or("")));
        self.status.setHidden(text.is_none());
    }
}

/// The settings' work on the app: everything goes through the core, then the window shows the file again.
impl App {
    pub fn open_settings(&mut self) {
        if self.settings.is_none() {
            self.settings = Some(Settings::new(self.mtm, &self.delegate));
        }
        self.reload_settings();
        if let Some(s) = &self.settings {
            s.window.makeKeyAndOrderFront(None);
        }
    }

    fn root(&self) -> Option<PathBuf> {
        self.desk.as_ref().map(|d| d.root().to_path_buf())
    }

    /// The window from config.json as it is on disk now (the desk takes changes with its next poll).
    fn reload_settings(&self) {
        let (Some(s), Some(root)) = (&self.settings, self.root()) else {
            return;
        };
        match Config::load(&root) {
            Ok(cfg) => s.reload(&cfg),
            Err(e) => s.show_status(Some(&e.0)),
        }
    }

    pub fn settings_add(&mut self, kind: Kind) {
        let Some(s) = &mut self.settings else {
            return;
        };
        if s.sheet.is_some() {
            return;
        }
        let sheet = Sheet::new(self.mtm, &self.delegate, kind);
        s.window.beginSheet_completionHandler(&sheet.panel, None);
        s.sheet = Some(sheet);
    }

    pub fn sheet_cancel(&mut self) {
        let Some(s) = &mut self.settings else {
            return;
        };
        s.signin += 1;
        if let Some(sheet) = s.sheet.take() {
            s.window.endSheet(&sheet.panel);
        }
    }

    /// "Add": the account is built and checked, then saved with its secret; Google first needs the sign-in in the
    /// browser (on its own thread, up to five minutes).
    pub fn sheet_confirm(&mut self) {
        let root = self.root();
        let (Some(s), Some(root)) = (&mut self.settings, root) else {
            return;
        };
        let Some(sheet) = &s.sheet else {
            return;
        };
        let acc = match sheet.build() {
            Ok(a) => a,
            Err(e) => return sheet.show_error(Some(&e)),
        };
        // A taken name is told now, not after five minutes of Google sign-in.
        if let Err(e) = accounts::check_new(&root, &acc) {
            return sheet.show_error(Some(&e.0));
        }
        let secrets = draftdesk_core::secrets::system();
        let saved = match sheet.kind {
            Kind::Smtp => accounts::add(
                &root,
                secrets.as_ref(),
                acc,
                AccountSecret::Smtp {
                    password: &sheet.get("password"),
                },
            ),
            Kind::Command => accounts::add(&root, secrets.as_ref(), acc, AccountSecret::None),
            Kind::Gmail => {
                // Pasted from Google's JSON or a mail, a secret may bring a line break or spaces along; it has none.
                let (id, secret) = (
                    sheet.get("client_id").trim().to_string(),
                    sheet.get("client_secret").trim().to_string(),
                );
                if secret.is_empty() {
                    return sheet.show_error(Some(tr(
                        "Das Client-Secret fehlt.",
                        "The client secret is missing.",
                    )));
                }
                s.signin += 1;
                let ticket = s.signin;
                sheet.confirm.setEnabled(false);
                sheet.set_locked(true);
                sheet.show_error(None);
                sheet
                    .error
                    .setTextColor(Some(&NSColor::secondaryLabelColor()));
                sheet.error.setStringValue(&ns(tr(
                    "Bitte im Browser bei Google anmelden (bis zu fünf Minuten) …",
                    "Please sign in with Google in your browser (up to five minutes)…",
                )));
                sheet.error.setHidden(false);
                spawn("draftdesk-signin", move || {
                    let open = |url: &str| {
                        let _ = std::process::Command::new("/usr/bin/open")
                            .arg(url)
                            .status();
                    };
                    let r = oauth::authorize(&UreqClient::new(), id.trim(), &secret, &open)
                        .map(|t| t.refresh_token);
                    on_main(move || with_app(move |a| a.signed_in(ticket, acc, secret, r)));
                });
                return;
            }
        };
        self.after_save(saved);
    }

    fn signed_in(
        &mut self,
        ticket: u64,
        acc: AccountCfg,
        secret: String,
        r: draftdesk_core::Result<String>,
    ) {
        let (Some(s), Some(root)) = (&self.settings, self.root()) else {
            return;
        };
        if s.signin != ticket || s.sheet.is_none() {
            log("google sign-in answered for a closed sheet; dropped");
            return;
        }
        let saved = r.and_then(|refresh| {
            accounts::add(
                &root,
                draftdesk_core::secrets::system().as_ref(),
                acc,
                AccountSecret::Gmail {
                    client_secret: &secret,
                    refresh_token: &refresh,
                },
            )
        });
        if let Some(sheet) = &s.sheet {
            sheet.confirm.setEnabled(true);
            sheet.set_locked(false);
            sheet.error.setTextColor(Some(&NSColor::systemRedColor()));
        }
        self.after_save(saved);
    }

    fn after_save(&mut self, saved: draftdesk_core::Result<()>) {
        let Some(s) = &mut self.settings else {
            return;
        };
        match saved {
            Ok(()) => {
                if let Some(sheet) = s.sheet.take() {
                    s.window.endSheet(&sheet.panel);
                }
                self.reload_settings();
            }
            Err(e) => {
                if let Some(sheet) = &s.sheet {
                    sheet.show_error(Some(&e.0));
                }
            }
        }
    }

    /// "Choose…" in the command sheet: a file dialog for the program (send or discard).
    pub fn sheet_choose(&mut self, which: isize) {
        let Some(sheet) = self.settings.as_ref().and_then(|s| s.sheet.as_ref()) else {
            return;
        };
        let key = if which == 1 { "discard" } else { "send" };
        self.choose_program(&sheet.panel.clone(), move |a, path| {
            if let Some(sheet) = a.settings.as_ref().and_then(|s| s.sheet.as_ref()) {
                sheet.set(key, &path.to_string_lossy());
            }
        });
    }

    /// A file dialog for one program, as a sheet on `on`; `then` gets the chosen path.
    fn choose_program(&self, on: &NSWindow, then: impl Fn(&mut App, PathBuf) + 'static) {
        let panel = NSOpenPanel::openPanel(self.mtm);
        panel.setAllowsMultipleSelection(false);
        panel.setCanChooseDirectories(false);
        panel.setCanChooseFiles(true);
        let p = panel.clone();
        let then = std::rc::Rc::new(then);
        let done = RcBlock::new(move |response: NSModalResponse| {
            if response != NSModalResponseOK {
                return;
            }
            let Some(path) = p.URLs().firstObject().and_then(|u| u.path()) else {
                return;
            };
            let (then, path) = (std::rc::Rc::clone(&then), PathBuf::from(path.to_string()));
            with_app(move |a| then(a, path));
        });
        panel.beginSheetModalForWindow_completionHandler(on, &done);
    }

    pub fn settings_choose_command(&mut self, tag: isize) {
        let Some(window) = self.settings.as_ref().map(|s| s.window.clone()) else {
            return;
        };
        self.choose_program(&window, move |a, path| {
            a.settings_set_command(slot_of(tag), &path)
        });
    }

    /// The check or fix command becomes this program alone (also replacing arguments from config.json).
    pub fn settings_set_command(&mut self, slot: CommandSlot, program: &Path) {
        let (Some(s), Some(root)) = (&self.settings, self.root()) else {
            return;
        };
        let shown = program.to_string_lossy().to_string();
        if !executable(program) {
            return s.show_status(Some(&not_executable(&shown)));
        }
        let timeout = u64::try_from(s.commands[slot_index(slot)].timeout.selectedTag())
            .ok()
            .filter(|t| *t > 0)
            .unwrap_or(300);
        let cmd = CommandCfg {
            argv: vec![shown],
            timeout_s: timeout,
        };
        self.save_command(&root, slot, Some(cmd));
    }

    pub fn settings_clear_command(&mut self, tag: isize) {
        let Some(root) = self.root() else {
            return;
        };
        self.save_command(&root, slot_of(tag), None);
    }

    /// A new time limit for a command that is set.
    pub fn settings_timeout(&mut self, tag: isize) {
        let (Some(s), Some(root)) = (&self.settings, self.root()) else {
            return;
        };
        let slot = slot_of(tag);
        let Ok(cfg) = Config::load(&root) else {
            return;
        };
        let current = match slot {
            CommandSlot::Check => cfg.check_command,
            CommandSlot::Fix => cfg.fix_command,
        };
        let Some(mut cmd) = current else {
            return;
        };
        let Ok(t) = u64::try_from(s.commands[slot_index(slot)].timeout.selectedTag()) else {
            return;
        };
        cmd.timeout_s = t;
        self.save_command(&root, slot, Some(cmd));
    }

    fn save_command(&mut self, root: &Path, slot: CommandSlot, cmd: Option<CommandCfg>) {
        let saved = accounts::set_command(root, slot, cmd);
        if let Some(s) = &self.settings {
            s.show_status(saved.as_ref().err().map(|e| e.0.as_str()));
        }
        self.reload_settings();
    }

    /// "Remove…": after a question the account leaves config.json and its secrets the keychain.
    pub fn settings_remove(&mut self) {
        let Some(s) = &self.settings else {
            return;
        };
        let Some(name) = s.selected_account() else {
            return;
        };
        let window = s.window.clone();
        let message = if crate::i18n::is_german() {
            format!("Konto „{name}“ entfernen?")
        } else {
            format!("Remove the account “{name}”?")
        };
        let info = tr(
            "Seine Zugangsdaten werden aus dem Schlüsselbund gelöscht. Entwürfe dieses Kontos können danach nicht mehr gesendet werden.",
            "Its credentials are deleted from the keychain. Drafts of this account cannot be sent afterwards.",
        );
        self.ask(
            &window,
            &message,
            info,
            tr("Entfernen", "Remove"),
            move |a| a.remove_account(&name),
        );
    }

    fn remove_account(&mut self, name: &str) {
        let Some(root) = self.root() else {
            return;
        };
        let removed = accounts::remove(&root, draftdesk_core::secrets::system().as_ref(), name);
        if let (Err(e), Some(s)) = (&removed, &self.settings) {
            s.show_status(Some(&e.0));
        }
        self.reload_settings();
    }

    pub fn settings_selection_changed(&mut self) {
        if let Some(s) = &self.settings {
            s.selection_changed();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> String + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| v.to_string())
                .unwrap_or_default()
        }
    }

    #[test]
    fn build_checks_ports_programs_and_kinds() {
        crate::i18n::set_german(true);
        let smtp = [
            ("name", "mail"),
            ("address", "me@example.org"),
            ("host", "smtp.example.org"),
            ("port", "five"),
            ("user", "me"),
        ];
        let e = build(Kind::Smtp, values(&smtp), false, false).unwrap_err();
        assert_eq!(e, "five ist keine Portnummer");
        let ok = [
            ("name", "mail"),
            ("address", "me@example.org"),
            ("host", "smtp.example.org"),
            ("port", "465"),
            ("user", "me"),
        ];
        let acc = build(Kind::Smtp, values(&ok), true, false).unwrap();
        let s = acc.smtp.unwrap();
        assert_eq!((s.port, s.security), (465, Security::Tls));
        assert!(acc.imap.is_none() && !acc.save_sent_copy);
        // A sent copy needs IMAP (the core's rule).
        assert!(
            build(Kind::Smtp, values(&ok), true, true)
                .unwrap_err()
                .contains("needs imap")
        );
        let google = [
            ("name", "g"),
            ("address", "me@example.org"),
            ("client_id", "abc.apps.googleusercontent.com"),
        ];
        let acc = build(Kind::Gmail, values(&google), false, false).unwrap();
        assert_eq!(
            acc.gmail.unwrap().client_id,
            "abc.apps.googleusercontent.com"
        );
        let cmd = [("name", "c"), ("address", "me@example.org"), ("send", "")];
        assert!(
            build(Kind::Command, values(&cmd), false, false)
                .unwrap_err()
                .contains("nicht ausführbar")
        );
        let cmd = [
            ("name", "c"),
            ("address", "me@example.org"),
            ("send", "/bin/cat"),
        ];
        let acc = build(Kind::Command, values(&cmd), false, false).unwrap();
        assert_eq!(acc.send_command.unwrap(), vec!["/bin/cat"]);
        assert!(acc.discard_command.is_none());
        // The core's validation still applies: a bad name.
        let bad = [
            ("name", "Bad Name"),
            ("address", "me@example.org"),
            ("send", "/bin/cat"),
        ];
        assert!(build(Kind::Command, values(&bad), false, false).is_err());
    }

    #[test]
    fn command_time_limit_from_the_sheet() {
        let base = [
            ("name", "c"),
            ("address", "me@example.org"),
            ("send", "/bin/cat"),
        ];
        let acc = build(Kind::Command, values(&base), false, false).unwrap();
        assert_eq!(acc.timeout_s, None, "no popup value: the core's default");
        let with = |t: &'static str| {
            let mut v = base.to_vec();
            v.push(("timeout", t));
            build(Kind::Command, values(&v), false, false)
        };
        assert_eq!(with("300").unwrap().timeout_s, Some(300));
        assert!(with("5").unwrap_err().contains("timeout_s"));
    }
}
