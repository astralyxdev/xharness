//! The Git window's Remotes panel: the repository's remotes (add, edit,
//! remove, test), making a repository on GitHub to publish to, and signing
//! git in to GitHub. It covers the Changes and History panes while open.
//!
//! Everything runs on the repository's server (see services/github.rs for
//! where the sign-in is kept); pages open in the browser on this machine.

use std::{path::Path, rc::Rc, time::Duration};

use gpui::{
    AnyElement, App, ClickEvent, ClipboardItem, Context, Div, Focusable, FontWeight, Task, Window, div, prelude::*,
    rems,
};

use super::{GitEvent, GitView, checkbox, field};
use crate::services::git::{self, Remote};
use crate::i18n::{t, tf, tn};
use crate::services::github::{self, Accounts, DeviceCode, SignIn, SignedIn};
use crate::ui::assets::IconName;
use crate::ui::context_menu::MenuEntry;
use crate::ui::theme::{FONT_MONO, Theme};
use crate::ui;

/// What the panel's form is for.
#[derive(Clone, PartialEq, Eq)]
pub(super) enum RemoteForm {
    None,
    Add,
    /// Editing this remote.
    Edit(String),
    /// Making a repository on GitHub to publish to.
    Create,
}

enum SignInStep {
    Idle,
    Starting,
    /// Waiting for the code to be entered on GitHub.
    Code(DeviceCode),
    /// Pasting a token.
    Token,
}

pub(super) struct RemotesPanel {
    remotes: Option<Result<Vec<Remote>, String>>,
    github: Option<Result<Accounts, String>>,
    form: RemoteForm,
    sign_in: SignInStep,
    /// A URL test's answer (branches found), and the URL it was for.
    test: Option<(String, Result<usize, String>)>,
    /// The new repository's owner (`None`: you), and whether it's private.
    owner: Option<String>,
    private: bool,
    busy: Option<&'static str>,
    error: Option<String>,
    remotes_task: Option<Task<()>>,
    github_task: Option<Task<()>>,
    action_task: Option<Task<()>>,
    /// Asking for a code, then waiting for it to be entered.
    start_task: Option<Task<()>>,
    sign_in_task: Option<Task<()>>,
}

impl RemotesPanel {
    fn new() -> Self {
        Self {
            remotes: None,
            github: None,
            form: RemoteForm::None,
            sign_in: SignInStep::Idle,
            test: None,
            owner: None,
            private: true,
            busy: None,
            error: None,
            remotes_task: None,
            github_task: None,
            action_task: None,
            start_task: None,
            sign_in_task: None,
        }
    }

    fn account(&self) -> Option<&github::Account> {
        match &self.github {
            Some(Ok(Accounts { signed_in: SignedIn::Yes(account), .. })) => Some(account),
            _ => None,
        }
    }
}

/// A repository name GitHub takes, from a folder's.
fn repository_name(folder: &Path) -> String {
    let name: String = folder
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '-' })
        .collect();
    name.trim_matches('-').to_string()
}

/// Where git keeps the GitHub token (as [`Accounts::stored_in`] says it), in
/// the app's language.
fn stored_in_label(stored_in: &str) -> String {
    match stored_in {
        "the macOS keychain" => t("the macOS keychain").to_string(),
        "the GitHub CLI" => t("the GitHub CLI").to_string(),
        "the system keyring" => t("the system keyring").to_string(),
        "git's memory cache (for a while)" => t("git's memory cache (for a while)").to_string(),
        other => match other.strip_prefix("git's “").and_then(|rest| rest.strip_suffix("” helper")) {
            Some(helper) => tf("git's “{}” helper", &[&helper]),
            None => other.to_string(),
        },
    }
}

impl GitView {
    pub(super) fn remotes_open(&self) -> bool {
        self.remotes_panel.is_some()
    }

    /// Opens the panel (reading the remotes and GitHub again) on `form`.
    pub(super) fn open_remotes(&mut self, form: RemoteForm, window: &mut Window, cx: &mut Context<Self>) {
        if self.remotes_panel.is_none() {
            self.remotes_panel = Some(RemotesPanel::new());
            self.load_remotes(cx);
            self.load_github(cx);
        }
        self.set_form(form, window, cx);
    }

    pub(super) fn close_remotes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(panel) = self.remotes_panel.take()
            && let SignInStep::Code(code) = panel.sign_in
        {
            self.cancel_on_server(code.id, cx);
        }
        window.focus(&self.focus_handle);
        cx.notify();
    }

    /// Escape: closes the form, then the sign-in, then the panel.
    pub(super) fn remotes_cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = self.remotes_panel.as_ref() else {
            return;
        };
        if panel.form != RemoteForm::None {
            self.set_form(RemoteForm::None, window, cx);
        } else if !matches!(panel.sign_in, SignInStep::Idle) {
            self.cancel_sign_in(window, cx);
        } else {
            self.close_remotes(window, cx);
        }
    }

    /// Enter: submits what's being typed.
    pub(super) fn remotes_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = self.remotes_panel.as_ref() else {
            return;
        };
        match (&panel.form, &panel.sign_in) {
            (RemoteForm::Add | RemoteForm::Edit(_), _) => self.save_remote(cx),
            (RemoteForm::Create, _) => self.create_on_github(cx),
            (RemoteForm::None, SignInStep::Token) => self.sign_in_with_token(window, cx),
            _ => {}
        }
    }

    fn set_form(&mut self, form: RemoteForm, window: &mut Window, cx: &mut Context<Self>) {
        let folder_name = repository_name(self.root().as_deref().unwrap_or(&self.folder));
        let Some(panel) = self.remotes_panel.as_mut() else {
            return;
        };
        panel.test = None;
        panel.error = None;
        match &form {
            RemoteForm::Add => {
                let origin_taken = self.remotes.iter().any(|name| name == "origin");
                let name = if origin_taken { "" } else { "origin" };
                self.remote_name.update(cx, |input, cx| input.set_text(name, cx));
                self.remote_url.update(cx, |input, cx| input.set_text("", cx));
                let focus = if origin_taken { &self.remote_name } else { &self.remote_url };
                window.focus(&focus.focus_handle(cx));
            }
            RemoteForm::Edit(name) => {
                let url = match &panel.remotes {
                    Some(Ok(remotes)) => remotes.iter().find(|r| &r.name == name).map(|r| r.url.clone()).unwrap_or_default(),
                    _ => String::new(),
                };
                self.remote_name.update(cx, |input, cx| input.set_text(name.clone(), cx));
                self.remote_url.update(cx, |input, cx| input.set_text(url, cx));
                window.focus(&self.remote_url.focus_handle(cx));
            }
            RemoteForm::Create => {
                panel.owner = None;
                panel.private = true;
                self.repo_name.update(cx, |input, cx| input.set_text(folder_name, cx));
                self.repo_description.update(cx, |input, cx| input.set_text("", cx));
                window.focus(&self.repo_name.focus_handle(cx));
            }
            RemoteForm::None => window.focus(&self.focus_handle),
        }
        panel.form = form;
        cx.notify();
    }

    /// Runs `work` on the repository's server, then `done` with what it
    /// returned (if the panel is still open).
    fn on_server<R: Send + 'static>(
        &self,
        work: impl FnOnce() -> R + Send + 'static,
        cx: &mut Context<Self>,
        done: impl FnOnce(&mut Self, R, &mut Context<Self>) + 'static,
    ) -> Task<()> {
        let server = self.server.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { crate::services::rpc::on(server.as_ref(), work) })
                .await;
            this.update(cx, |this, cx| {
                if this.remotes_panel.is_some() {
                    done(this, result, cx);
                }
                cx.notify();
            })
            .ok();
        })
    }

    fn panel(&mut self) -> Option<&mut RemotesPanel> {
        self.remotes_panel.as_mut()
    }

    fn load_remotes(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.root() else {
            return;
        };
        let task = self.on_server(move || git::remote_list(&root), cx, |this, remotes, _| {
            if let Some(panel) = this.panel() {
                panel.remotes = Some(remotes);
            }
        });
        if let Some(panel) = self.remotes_panel.as_mut() {
            panel.remotes_task = Some(task);
        }
    }

    fn load_github(&mut self, cx: &mut Context<Self>) {
        let task = self.on_server(github::accounts, cx, |this, accounts, _| {
            if let Some(panel) = this.panel() {
                panel.github = Some(accounts);
            }
        });
        if let Some(panel) = self.remotes_panel.as_mut() {
            panel.github_task = Some(task);
        }
    }

    /// Changes the remotes in the background; the form closes when it works.
    fn remote_act(
        &mut self,
        busy: &'static str,
        work: impl FnOnce(&Path) -> Result<(), String> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.root() else {
            return;
        };
        if self.remotes_panel.as_ref().is_none_or(|panel| panel.busy.is_some()) {
            return;
        }
        let task = self.on_server(move || work(&root), cx, |this, result, cx| {
            if let Some(panel) = this.panel() {
                panel.busy = None;
                match result {
                    Ok(()) => {
                        panel.form = RemoteForm::None;
                        panel.test = None;
                    }
                    Err(error) => panel.error = Some(error),
                }
            }
            this.load_remotes(cx);
            // The toolbar's remote and sync button follow.
            this.refresh(cx);
        });
        let panel = self.remotes_panel.as_mut().unwrap();
        panel.busy = Some(busy);
        panel.error = None;
        panel.action_task = Some(task);
        cx.notify();
    }

    fn save_remote(&mut self, cx: &mut Context<Self>) {
        let name = self.remote_name.read(cx).text().trim().to_string();
        let url = self.remote_url.read(cx).text().trim().to_string();
        let Some(form) = self.remotes_panel.as_ref().map(|panel| panel.form.clone()) else {
            return;
        };
        match form {
            RemoteForm::Add => self.remote_act(t("Adding…"), move |root| git::add_remote(root, &name, &url), cx),
            RemoteForm::Edit(old) => self.remote_act(t("Saving…"), move |root| git::edit_remote(root, &old, &name, &url), cx),
            _ => {}
        }
    }

    /// Asks the remote for its branches, with the credentials git has.
    fn test_remote(&mut self, cx: &mut Context<Self>) {
        let url = self.remote_url.read(cx).text().trim().to_string();
        let Some(root) = self.root() else {
            return;
        };
        if url.is_empty() || self.remotes_panel.as_ref().is_none_or(|panel| panel.busy.is_some()) {
            return;
        }
        let tested = url.clone();
        let task = self.on_server(move || git::check_remote(&root, &tested), cx, move |this, result, _| {
            if let Some(panel) = this.panel() {
                panel.busy = None;
                panel.test = Some((url, result));
            }
        });
        let panel = self.remotes_panel.as_mut().unwrap();
        panel.busy = Some(t("Testing…"));
        panel.test = None;
        panel.action_task = Some(task);
        cx.notify();
    }

    fn remove_remote(&mut self, name: String, cx: &mut Context<Self>) {
        let weak = cx.entity().downgrade();
        let target = name.clone();
        let action: Rc<dyn Fn(&mut App)> = Rc::new(move |cx: &mut App| {
            let target = target.clone();
            weak.update(cx, |this, cx| this.remote_act(t("Removing…"), move |root| git::remove_remote(root, &target), cx))
                .ok();
        });
        cx.emit(GitEvent::Confirm {
            title: tf("Remove the remote “{}”?", &[&name]),
            description: t(
                "Only this repository's link to it goes; nothing on the server is deleted. Branches that tracked it stop tracking.",
            )
            .to_string(),
            confirm: t("Remove remote").to_string(),
            action,
        });
    }

    /// Makes the repository on GitHub, adds it as a remote, and publishes
    /// the branch to it.
    fn create_on_github(&mut self, cx: &mut Context<Self>) {
        let Some(panel) = self.remotes_panel.as_ref() else {
            return;
        };
        let name = self.repo_name.read(cx).text().trim().to_string();
        if name.is_empty() {
            return;
        }
        let description = self.repo_description.read(cx).text().trim().to_string();
        let (owner, private) = (panel.owner.clone(), panel.private);
        let remote = if self.remotes.iter().any(|r| r == "origin") { "github" } else { "origin" };
        let publish = self.status.head.is_some();
        self.remote_act(
            t("Creating…"),
            move |root| {
                let repository = github::create_repository(owner.as_deref(), &name, &description, private)?;
                git::add_remote(root, remote, &repository.clone_url)?;
                if publish {
                    git::publish_to(root, remote)
                        .map_err(|error| tf("The repository was made, but publishing to it failed: {}", &[&error]))?;
                }
                Ok(())
            },
            cx,
        );
    }

    fn owner_menu(&self, position: crate::ui::context_menu::MenuAt, cx: &mut Context<Self>) {
        let Some(panel) = self.remotes_panel.as_ref() else {
            return;
        };
        let Some(account) = panel.account() else {
            return;
        };
        let this = cx.entity().downgrade();
        let owners = std::iter::once(None).chain(account.orgs.iter().cloned().map(Some));
        let entries = owners
            .map(|owner| {
                let label = owner.clone().unwrap_or_else(|| account.login.clone());
                let checked = owner == panel.owner;
                let this = this.clone();
                MenuEntry::check(label, checked, move |_, cx| {
                    let owner = owner.clone();
                    this.update(cx, |this, cx| {
                        if let Some(panel) = this.remotes_panel.as_mut() {
                            panel.owner = owner;
                            cx.notify();
                        }
                    })
                    .ok();
                })
            })
            .collect();
        cx.emit(GitEvent::Menu { entries, position });
    }

    // GitHub sign-in --------------------------------------------------------------------

    /// Signs in in the browser: copies the code, opens GitHub's page, and
    /// waits for the code to be entered there.
    fn begin_sign_in(&mut self, cx: &mut Context<Self>) {
        let Some(panel) = self.remotes_panel.as_mut() else {
            return;
        };
        panel.sign_in = SignInStep::Starting;
        panel.error = None;
        let task = self.on_server(github::begin_sign_in, cx, |this, result, cx| {
            let still_wanted = this.panel().is_some_and(|panel| matches!(panel.sign_in, SignInStep::Starting));
            match result {
                // Cancelled while GitHub was asked.
                Ok(code) if !still_wanted => this.cancel_on_server(code.id, cx),
                Ok(code) => {
                    cx.write_to_clipboard(ClipboardItem::new_string(code.code.clone()));
                    cx.open_url(&code.url);
                    let poll = this.poll_sign_in(code.id, cx);
                    if let Some(panel) = this.panel() {
                        panel.sign_in = SignInStep::Code(code);
                        panel.sign_in_task = Some(poll);
                    }
                }
                Err(_) if !still_wanted => {}
                Err(error) => {
                    if let Some(panel) = this.panel() {
                        panel.sign_in = SignInStep::Idle;
                        panel.error = Some(error);
                    }
                }
            }
        });
        if let Some(panel) = self.remotes_panel.as_mut() {
            panel.start_task = Some(task);
        }
        cx.notify();
    }

    fn poll_sign_in(&self, id: u64, cx: &mut Context<Self>) -> Task<()> {
        let server = self.server.clone();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let server = server.clone();
                let status = cx
                    .background_executor()
                    .spawn(async move { crate::services::rpc::on(server.as_ref(), || github::sign_in_status(id)) })
                    .await;
                let finished = this
                    .update(cx, |this, cx| {
                        let Some(panel) = this.remotes_panel.as_mut() else {
                            return true;
                        };
                        if !matches!(&panel.sign_in, SignInStep::Code(code) if code.id == id) {
                            return true;
                        }
                        let finished = match status {
                            Ok(SignIn::Waiting) => false,
                            Ok(SignIn::Done(_)) => {
                                panel.sign_in = SignInStep::Idle;
                                true
                            }
                            Ok(SignIn::Failed(error)) | Err(error) => {
                                panel.sign_in = SignInStep::Idle;
                                panel.error = Some(error);
                                true
                            }
                        };
                        if finished {
                            this.load_github(cx);
                            cx.notify();
                        }
                        finished
                    })
                    .unwrap_or(true);
                if finished {
                    break;
                }
            }
        })
    }

    fn cancel_on_server(&self, id: u64, cx: &mut Context<Self>) {
        let server = self.server.clone();
        cx.background_executor()
            .spawn(async move { crate::services::rpc::on(server.as_ref(), || github::cancel_sign_in(id)).ok() })
            .detach();
    }

    fn cancel_sign_in(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = self.remotes_panel.as_mut() else {
            return;
        };
        let step = std::mem::replace(&mut panel.sign_in, SignInStep::Idle);
        panel.sign_in_task = None;
        if let SignInStep::Code(code) = step {
            self.cancel_on_server(code.id, cx);
        }
        self.token_input.update(cx, |input, cx| input.set_text("", cx));
        window.focus(&self.focus_handle);
        cx.notify();
    }

    /// Signs in with `work` (a token, or a GitHub CLI account).
    fn github_act(
        &mut self,
        busy: &'static str,
        work: impl FnOnce() -> Result<github::Account, String> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        if self.remotes_panel.as_ref().is_none_or(|panel| panel.busy.is_some()) {
            return;
        }
        let task = self.on_server(work, cx, |this, result, cx| {
            let worked = result.is_ok();
            if let Some(panel) = this.panel() {
                panel.busy = None;
                match result {
                    Ok(_) => panel.sign_in = SignInStep::Idle,
                    Err(error) => panel.error = Some(error),
                }
            }
            if worked {
                this.token_input.update(cx, |input, cx| input.set_text("", cx));
            }
            this.load_github(cx);
        });
        let panel = self.remotes_panel.as_mut().unwrap();
        panel.busy = Some(busy);
        panel.error = None;
        panel.action_task = Some(task);
        cx.notify();
    }

    fn sign_in_with_token(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let token = self.token_input.read(cx).text().trim().to_string();
        if token.is_empty() {
            return;
        }
        window.focus(&self.focus_handle);
        self.github_act(t("Signing in…"), move || github::sign_in_with_token(&token), cx);
    }

    fn sign_in_with_cli(&mut self, login: String, cx: &mut Context<Self>) {
        self.github_act(t("Signing in…"), move || github::sign_in_with_cli(&login), cx);
    }

    fn sign_out(&mut self, login: String, stored_in: String, cx: &mut Context<Self>) {
        let weak = cx.entity().downgrade();
        let action: Rc<dyn Fn(&mut App)> = Rc::new(move |cx: &mut App| {
            weak.update(cx, |this, cx| {
                let Some(panel) = this.remotes_panel.as_ref() else {
                    return;
                };
                if panel.busy.is_some() {
                    return;
                }
                let task = this.on_server(github::sign_out, cx, |this, result, cx| {
                    if let Some(panel) = this.panel() {
                        panel.busy = None;
                        if let Err(error) = result {
                            panel.error = Some(error);
                        }
                    }
                    this.load_github(cx);
                });
                let panel = this.remotes_panel.as_mut().unwrap();
                panel.busy = Some(t("Signing out…"));
                panel.action_task = Some(task);
                cx.notify();
            })
            .ok();
        });
        cx.emit(GitEvent::Confirm {
            title: tf("Sign out of GitHub as {}?", &[&login]),
            description: tf(
                "The token is removed from {}, so git can't push or pull private repositories on github.com over HTTPS until you sign in again.",
                &[&stored_in_label(&stored_in)],
            ),
            confirm: t("Sign out").to_string(),
            action,
        });
    }

    // Rendering ------------------------------------------------------------------------

    pub(super) fn render_remotes(&self, theme: &Theme, cx: &Context<Self>) -> AnyElement {
        let Some(panel) = self.remotes_panel.as_ref() else {
            return div().into_any_element();
        };
        let header = div()
            .flex()
            .items_center()
            .gap_2()
            .child(div().flex_1().text_size(rems(0.9375)).font_weight(FontWeight::SEMIBOLD).child(t("Remotes & GitHub")))
            .children(panel.busy.map(|busy| div().text_size(rems(0.75)).text_color(theme.muted_foreground).child(busy)))
            .child(ui::small_icon_button("git-remotes-close", IconName::X, theme).on_click(cx.listener(
                |this, _, window, cx| this.close_remotes(window, cx),
            )));
        div()
            .id("git-remotes")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_5()
                    .w_full()
                    .max_w(rems(38.))
                    .mx_auto()
                    .px_4()
                    .py_4()
                    .child(header)
                    .children(panel.error.clone().map(|error| {
                        div()
                            .px_3()
                            .py_2()
                            .rounded(rems(0.5))
                            .bg(theme.tints.destructive.base.opacity(0.08))
                            .text_size(rems(0.75))
                            .text_color(theme.tints.destructive.soft_foreground)
                            .child(error)
                    }))
                    .child(self.render_remote_list(panel, theme, cx))
                    .child(self.render_github(panel, theme, cx)),
            )
            .into_any_element()
    }

    fn render_remote_list(&self, panel: &RemotesPanel, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let mut card = card(theme);
        match &panel.remotes {
            None => card = card.child(note(t("Reading the remotes…"), theme).px_3().py_3()),
            Some(Err(error)) => card = card.child(note(error, theme).px_3().py_3()),
            Some(Ok(remotes)) if remotes.is_empty() && panel.form == RemoteForm::None => {
                card = card.child(
                    note(
                        t("No remote yet. Add one to push your commits somewhere and pull others', or make a repository on GitHub."),
                        theme,
                    )
                    .px_3()
                    .py_3(),
                )
            }
            Some(Ok(remotes)) => {
                for (ix, remote) in remotes.iter().enumerate() {
                    if panel.form == RemoteForm::Edit(remote.name.clone()) {
                        card = card.child(self.render_remote_form(panel, false, theme, cx));
                    } else {
                        card = card.child(self.render_remote(ix, remote, theme, cx));
                    }
                }
            }
        }
        if panel.form == RemoteForm::Add {
            card = card.child(self.render_remote_form(panel, true, theme, cx));
        }
        if panel.form == RemoteForm::Create {
            card = card.child(self.render_create_form(panel, theme, cx));
        }
        let signed_in = panel.account().is_some();
        let idle = panel.form == RemoteForm::None;
        section(t("Remotes"), theme).child(card).when(idle, |section| {
            section.child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(ui::compact_button("git-remote-add", Some(IconName::Plus), t("Add remote"), false, theme).on_click(
                        cx.listener(|this, _, window, cx| this.set_form(RemoteForm::Add, window, cx)),
                    ))
                    .child(
                        ui::compact_button("git-remote-create", Some(IconName::Github), t("New repository on GitHub"), false, theme)
                            .when(!signed_in, |b| b.opacity(0.5))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                if signed_in {
                                    this.set_form(RemoteForm::Create, window, cx);
                                }
                            })),
                    )
                    .when(!signed_in, |row| {
                        row.child(div().text_size(rems(0.6875)).text_color(theme.muted_foreground).child(
                            t("Sign in to GitHub below to make one."),
                        ))
                    }),
            )
        })
    }

    fn render_remote(&self, ix: usize, remote: &Remote, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let web = github::web_url(&remote.url);
        let ssh_note = (web.is_some() && github::is_ssh(&remote.url)).then(|| t("Over SSH: git uses your SSH key, not the GitHub sign-in."));
        let (edit, remove) = (remote.name.clone(), remote.name.clone());
        div()
            .flex()
            .items_center()
            .gap_3()
            .px_3()
            .py_2()
            .when(ix > 0, |row| row.border_t_1().border_color(theme.border))
            .child(
                ui::icon(if web.is_some() { IconName::Github } else { IconName::Cloud })
                    .size_4()
                    .flex_shrink_0()
                    .text_color(theme.muted_foreground),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(div().text_size(rems(0.8125)).font_weight(FontWeight::SEMIBOLD).child(remote.name.clone()))
                    .child(mono(&remote.url, theme))
                    .children(remote.push_url.as_ref().map(|push| mono(&tf("pushes to {}", &[push]), theme)))
                    .children(ssh_note.map(|text| note(text, theme))),
            )
            .children(web.map(|url| {
                ui::small_icon_button(("git-remote-web", ix), IconName::ExternalLink, theme)
                    .on_click(cx.listener(move |_, _, _, cx| cx.open_url(&url)))
            }))
            .child(ui::small_icon_button(("git-remote-edit", ix), IconName::Pencil, theme).on_click(cx.listener(
                move |this, _, window, cx| this.set_form(RemoteForm::Edit(edit.clone()), window, cx),
            )))
            .child(
                ui::small_icon_button(("git-remote-remove", ix), IconName::Trash, theme)
                    .on_click(cx.listener(move |this, _, _, cx| this.remove_remote(remove.clone(), cx))),
            )
    }

    fn render_remote_form(&self, panel: &RemotesPanel, adding: bool, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let url = self.remote_url.read(cx).text().trim().to_string();
        let ready = !url.is_empty() && !self.remote_name.read(cx).text().trim().is_empty() && panel.busy.is_none();
        let test = panel.test.as_ref().filter(|(tested, _)| *tested == url).map(|(_, result)| match result {
            Ok(0) => (t("Reachable, and empty: publish a branch to it.").to_string(), true),
            Ok(n) => (tn(*n, "Reachable: {} branch.", "Reachable: {} branches."), true),
            Err(error) => (error.clone(), false),
        });
        let hint = if url.starts_with("https://github.com/") {
            t("HTTPS: git uses the GitHub sign-in below.")
        } else if github::is_ssh(&url) {
            t("SSH: git uses your SSH key.")
        } else {
            t("An HTTPS or SSH URL (git@host:owner/repo.git), or a path on the server.")
        };
        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.secondary.opacity(0.4))
            .child(labeled(t("Name"), field(theme).child(div().flex_1().min_w_0().child(self.remote_name.clone())), theme))
            .child(labeled("URL", field(theme).child(div().flex_1().min_w_0().child(self.remote_url.clone())), theme))
            .child(match test {
                Some((text, ok)) => div()
                    .text_size(rems(0.6875))
                    .text_color(if ok { theme.tints.green.soft_foreground } else { theme.tints.destructive.soft_foreground })
                    .child(text),
                None => note(hint, theme),
            })
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        ui::compact_button("git-remote-save", None, if adding { t("Add remote") } else { t("Save") }, true, theme)
                            .when(!ready, |b| b.opacity(0.5))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if ready {
                                    this.save_remote(cx);
                                }
                            })),
                    )
                    .child(
                        ui::compact_button("git-remote-test", None, t("Test connection"), false, theme)
                            .when(url.is_empty(), |b| b.opacity(0.5))
                            .on_click(cx.listener(|this, _, _, cx| this.test_remote(cx))),
                    )
                    .child(ui::compact_button("git-remote-cancel", None, t("Cancel"), false, theme).on_click(cx.listener(
                        |this, _, window, cx| this.set_form(RemoteForm::None, window, cx),
                    ))),
            )
    }

    fn render_create_form(&self, panel: &RemotesPanel, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let login = panel.account().map(|a| a.login.clone()).unwrap_or_default();
        let owner = panel.owner.clone().unwrap_or(login);
        let name = self.repo_name.read(cx).text().trim().to_string();
        let ready = !name.is_empty() && panel.busy.is_none();
        let remote = if self.remotes.iter().any(|r| r == "origin") { "github" } else { "origin" };
        let branch = self.status.branch.clone().unwrap_or_else(|| t("this branch").to_string());
        let what = if self.status.head.is_some() {
            tf("Makes github.com/{}/{}, adds it as “{}”, and publishes {} to it.", &[&owner, &name, &remote, &branch])
        } else {
            tf("Makes github.com/{}/{} and adds it as “{}”. Commit something, then publish.", &[&owner, &name, &remote])
        };
        let private = panel.private;
        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.secondary.opacity(0.4))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(labeled(
                        t("Owner"),
                        field(theme)
                            .id("git-repo-owner")
                            .cursor_pointer()
                            .child(ui::icon(IconName::Github).size_3p5().text_color(theme.muted_foreground))
                            .child(div().flex_1().min_w_0().truncate().child(owner))
                            .child(ui::chevron(false, theme).size_3())
                            .child(self.anchors.track("owner"))
                            .on_click(cx.listener(|this, event: &ClickEvent, _, cx| {
                                this.owner_menu(this.anchors.below("owner", ui::Align::Start, event.position()), cx)
                            })),
                        theme,
                    )
                    .w(rems(11.)))
                    .child(labeled(t("Name"), field(theme).child(div().flex_1().min_w_0().child(self.repo_name.clone())), theme).flex_1()),
            )
            .child(labeled(
                t("Description"),
                field(theme).child(div().flex_1().min_w_0().child(self.repo_description.clone())),
                theme,
            ))
            .child(
                div()
                    .id("git-repo-private")
                    .flex()
                    .items_center()
                    .gap_2()
                    .cursor_pointer()
                    .text_size(rems(0.75))
                    .child(checkbox(private, false, theme))
                    .child(t("Private: only you and people you add can see it"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(panel) = this.remotes_panel.as_mut() {
                            panel.private = !panel.private;
                            cx.notify();
                        }
                    })),
            )
            .child(note(&what, theme))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        ui::compact_button("git-repo-create", Some(IconName::Github), t("Create repository"), true, theme)
                            .when(!ready, |b| b.opacity(0.5))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if ready {
                                    this.create_on_github(cx);
                                }
                            })),
                    )
                    .child(ui::compact_button("git-repo-cancel", None, t("Cancel"), false, theme).on_click(cx.listener(
                        |this, _, window, cx| this.set_form(RemoteForm::None, window, cx),
                    ))),
            )
    }

    fn render_github(&self, panel: &RemotesPanel, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let body = card(theme).p_3().gap_3();
        let body = match &panel.github {
            None => body.child(note(t("Checking GitHub…"), theme)),
            Some(Err(error)) => body.child(note(error, theme)),
            Some(Ok(accounts)) => {
                let account_row = |icon_text: String, detail: String, login: String| {
                    let stored_in = accounts.stored_in.clone();
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .child(ui::icon(IconName::Github).size_5().flex_shrink_0())
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .flex_1()
                                .min_w_0()
                                .gap_0p5()
                                .child(div().text_size(rems(0.8125)).font_weight(FontWeight::SEMIBOLD).child(icon_text))
                                .child(note(&detail, theme)),
                        )
                        .child(ui::compact_button("git-github-sign-out", Some(IconName::LogOut), t("Sign out"), false, theme).on_click(
                            cx.listener(move |this, _, _, cx| this.sign_out(login.clone(), stored_in.clone(), cx)),
                        ))
                };
                match &accounts.signed_in {
                    SignedIn::Yes(account) => {
                        let title = match &account.name {
                            Some(name) => format!("{name} · @{}", account.login),
                            None => format!("@{}", account.login),
                        };
                        let detail = tf(
                            "Git pushes and pulls github.com over HTTPS as @{}, here and in your terminals. The token is in {}.",
                            &[&account.login, &stored_in_label(&accounts.stored_in)],
                        );
                        let others: Vec<String> = accounts.cli.iter().filter(|login| **login != account.login).cloned().collect();
                        body.child(account_row(title, detail, account.login.clone())).when(!others.is_empty(), |body| {
                            body.child(self.render_cli_accounts(t("Switch to an account the GitHub CLI has:"), &others, theme, cx))
                        })
                    }
                    SignedIn::Unchecked { login, error } => body.child(account_row(
                        format!("@{login}"),
                        tf("Git has a token for @{}, but GitHub couldn't be asked about it: {}", &[login, error]),
                        login.clone(),
                    )),
                    SignedIn::Rejected(login) => body
                        .child(
                            div()
                                .text_size(rems(0.75))
                                .text_color(theme.tints.amber.soft_foreground)
                                .child(tf("GitHub turns down the token git has for @{} (revoked or expired). Sign in again.", &[login])),
                        )
                        .child(self.render_sign_in(panel, accounts, theme, cx)),
                    SignedIn::No => body
                        .child(note(
                            t("Sign in so git can push and pull github.com over HTTPS: private repositories, and pushing to your own."),
                            theme,
                        ))
                        .child(self.render_sign_in(panel, accounts, theme, cx)),
                }
            }
        };
        section(t("GitHub"), theme).child(body)
    }

    fn render_cli_accounts(&self, label: &str, logins: &[String], theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let busy = self.remotes_panel.as_ref().is_some_and(|panel| panel.busy.is_some());
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .child(note(label, theme))
            .children(logins.iter().enumerate().map(|(ix, login)| {
                let target = login.clone();
                ui::compact_button(("git-github-cli", ix), Some(IconName::Github), &format!("@{login}"), false, theme)
                    .when(busy, |b| b.opacity(0.5))
                    .on_click(cx.listener(move |this, _, _, cx| this.sign_in_with_cli(target.clone(), cx)))
            }))
    }

    fn render_sign_in(&self, panel: &RemotesPanel, accounts: &Accounts, theme: &Theme, cx: &Context<Self>) -> AnyElement {
        match &panel.sign_in {
            SignInStep::Starting => note(t("Asking GitHub for a code…"), theme).into_any_element(),
            SignInStep::Code(code) => {
                let (copy, open) = (code.code.clone(), code.url.clone());
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(note(
                        &tf("Enter this code at {} (it's open in your browser, and copied):", &[&code.url]),
                        theme,
                    ))
                    .child(
                        div()
                            .font_family(FONT_MONO)
                            .text_size(rems(1.5))
                            .font_weight(FontWeight::BOLD)
                            .child(code.code.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(ui::compact_button("git-github-copy", Some(IconName::Copy), t("Copy code"), false, theme).on_click(
                                cx.listener(move |_, _, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()))),
                            ))
                            .child(
                                ui::compact_button("git-github-open", Some(IconName::ExternalLink), t("Open GitHub"), false, theme)
                                    .on_click(cx.listener(move |_, _, _, cx| cx.open_url(&open))),
                            )
                            .child(ui::compact_button("git-github-cancel", None, t("Cancel"), false, theme).on_click(cx.listener(
                                |this, _, window, cx| this.cancel_sign_in(window, cx),
                            ))),
                    )
                    .child(note(t("Waiting for GitHub…"), theme))
                    .into_any_element()
            }
            SignInStep::Token => {
                let ready = !self.token_input.read(cx).text().trim().is_empty() && panel.busy.is_none();
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(labeled(
                        t("Personal access token"),
                        field(theme)
                            .child(ui::icon(IconName::KeyRound).size_3p5().text_color(theme.muted_foreground))
                            .child(div().flex_1().min_w_0().child(self.token_input.clone())),
                        theme,
                    ))
                    .child(note(t("A classic token with the repo scope (and workflow, to push workflow files) works for every repository you can reach."), theme))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(
                                ui::compact_button("git-github-token-sign-in", None, t("Sign in"), true, theme)
                                    .when(!ready, |b| b.opacity(0.5))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        if ready {
                                            this.sign_in_with_token(window, cx);
                                        }
                                    })),
                            )
                            .child(
                                ui::compact_button("git-github-token-new", Some(IconName::ExternalLink), t("Make a token on GitHub"), false, theme)
                                    .on_click(cx.listener(|_, _, _, cx| cx.open_url(github::NEW_TOKEN_URL))),
                            )
                            .child(ui::compact_button("git-github-token-cancel", None, t("Cancel"), false, theme).on_click(cx.listener(
                                |this, _, window, cx| this.cancel_sign_in(window, cx),
                            ))),
                    )
                    .into_any_element()
            }
            SignInStep::Idle => {
                let busy = panel.busy.is_some();
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_2()
                            .when(accounts.browser, |row| {
                                row.child(
                                    ui::compact_button("git-github-sign-in", Some(IconName::Github), t("Sign in with GitHub"), true, theme)
                                        .when(busy, |b| b.opacity(0.5))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            if !busy {
                                                this.begin_sign_in(cx);
                                            }
                                        })),
                                )
                            })
                            .child(
                                ui::compact_button("git-github-use-token", Some(IconName::KeyRound), t("Use a token"), !accounts.browser, theme)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        if let Some(panel) = this.remotes_panel.as_mut() {
                                            panel.sign_in = SignInStep::Token;
                                            panel.error = None;
                                        }
                                        window.focus(&this.token_input.focus_handle(cx));
                                        cx.notify();
                                    })),
                            ),
                    )
                    .when(!accounts.cli.is_empty(), |el| {
                        el.child(self.render_cli_accounts(t("Or use an account the GitHub CLI has:"), &accounts.cli, theme, cx))
                    })
                    .into_any_element()
            }
        }
    }
}

// Pieces ---------------------------------------------------------------------------

fn section(title: &str, theme: &Theme) -> Div {
    div().flex().flex_col().gap_2().child(
        div()
            .text_size(rems(0.6875))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(theme.muted_foreground)
            .child(title.to_uppercase()),
    )
}

fn card(theme: &Theme) -> Div {
    div().flex().flex_col().rounded(rems(0.625)).border_1().border_color(theme.border).overflow_hidden()
}

fn note(text: &str, theme: &Theme) -> Div {
    div().text_size(rems(0.6875)).line_height(rems(1.)).text_color(theme.muted_foreground).child(text.to_string())
}

fn mono(text: &str, theme: &Theme) -> Div {
    div()
        .truncate()
        .font_family(FONT_MONO)
        .text_size(rems(0.6875))
        .text_color(theme.muted_foreground)
        .child(text.to_string())
}

fn labeled(label: &str, field: impl IntoElement, theme: &Theme) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(div().text_size(rems(0.6875)).font_weight(FontWeight::MEDIUM).text_color(theme.muted_foreground).child(label.to_string()))
        .child(field)
}
