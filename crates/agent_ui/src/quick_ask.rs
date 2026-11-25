use std::{path::PathBuf, rc::Rc, time::Duration};

use activity_indicator::{BackgroundActivities, BackgroundActivity, BackgroundActivityId};
use agent_client_protocol::v2::schema as acp_v2;
use db::kvp::KeyValueStore;
use editor::Editor;
use futures::{FutureExt as _, future::Shared};
use gpui::{
    Action, App, AsyncWindowContext, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    PromptButton, PromptLevel, Render, Task, WeakEntity, actions,
};
use project::AgentId;
use schemars::JsonSchema;
use serde::Deserialize;
use text::Point;
use ui::prelude::*;
use util::ResultExt as _;
use workspace::{DismissDecision, ModalView, Toast, Workspace, notifications::NotificationId};

use crate::{
    Agent, AgentInitialContent, AgentPanel, AgentThreadSource, agent_panel::CreateThreadOptions,
    conversation_view::ThreadView, thread_metadata_store::ThreadId,
};
/// Opens a one-line prompt and runs the request as a background agent thread,
/// with the cursor position as context.
#[derive(PartialEq, Clone, Default, Debug, Deserialize, JsonSchema, Action)]
#[action(namespace = agent)]
#[serde(deny_unknown_fields)]
pub struct QuickAsk {
    /// Agent ID for this request. Omit it to use the remembered Quick Ask agent.
    #[serde(default)]
    pub agent: Option<AgentId>,
    /// Model that takes the request first, matched against the agent's model ids and
    /// names. It hands the request to the agent's default model when the work turns
    /// out to be involved. Without it, every request goes to the default model.
    #[serde(default)]
    pub fast_model: Option<String>,
    /// Starts the agent reading the code around the cursor as soon as the prompt
    /// opens, so it already has context when the request arrives. This runs a turn
    /// even when the prompt is dismissed without a request.
    #[serde(default)]
    pub prewarm: bool,
}

actions!(
    agent,
    [
        /// Changes the default agent for Quick Ask.
        SelectQuickAskAgent,
    ]
);

pub(crate) const QUICK_ASK_AGENT_KEY: &str = "quick_ask_agent";
const SNIPPET_RADIUS: u32 = 20;
const MAX_SELECTION_BYTES: usize = 8 * 1024;
const MAX_TITLE_CHARS: usize = 60;
const MAX_OPEN_FILES: usize = 20;
const POLL_INTERVAL: Duration = Duration::from_millis(500);
const CONNECT_POLL_INTERVAL: Duration = Duration::from_millis(25);
const START_TIMEOUT: Duration = Duration::from_secs(180);
const ESCALATION_MARKER: &str = "QUICK_ASK_ESCALATE";
const ESCALATION_INSTRUCTIONS: &str = "\n\nYou are the fast first pass on this request. Do it \
     yourself when it is a contained change you are confident about. If, once you have looked \
     at the relevant code, it needs real design judgement or changes across many files, or you \
     are not confident you will get it right, make no edits and reply with only this line so a \
     stronger model takes over:\nQUICK_ASK_ESCALATE: <one-line reason>";

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(open_quick_ask);
        workspace.register_action(|workspace, _: &SelectQuickAskAgent, window, cx| {
            choose_quick_ask_agent(workspace, None, window, cx);
        });
    })
    .detach();
}

fn choose_quick_ask_agent(
    workspace: &Workspace,
    pending_ask: Option<QuickAsk>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let project = workspace.project().read(cx);
    let store = project.agent_server_store().read(cx);
    let mut choices = vec![(Agent::NativeAgent.label(), Agent::NativeAgent.id())];
    if !project.is_via_collab() {
        choices.extend(store.external_agents().map(|id| {
            (
                store.agent_display_name(id).unwrap_or_else(|| id.0.clone()),
                id.clone(),
            )
        }));
    }
    let db = KeyValueStore::global(cx);
    let current = db.read_kvp(QUICK_ASK_AGENT_KEY).log_err().flatten();
    choices.sort_by_key(|(label, id)| {
        (
            current.as_deref() != Some(id.as_ref()),
            label.clone(),
            id.clone(),
        )
    });
    let mut buttons = choices
        .iter()
        .map(|(label, id)| {
            if current.as_deref() == Some(id.as_ref()) {
                PromptButton::new(format!("{label} (current)"))
            } else {
                PromptButton::new(label.clone())
            }
        })
        .collect::<Vec<_>>();
    buttons.push(PromptButton::cancel("Cancel"));
    let response = window.prompt(
        PromptLevel::Info,
        "Choose your Quick Ask agent",
        None,
        &buttons,
        cx,
    );
    cx.spawn_in(window, async move |workspace, cx| {
        let Ok(index) = response.await else {
            return anyhow::Ok(());
        };
        let Some((_, id)) = choices.get(index) else {
            return Ok(());
        };
        if let Err(error) = db
            .write_kvp(QUICK_ASK_AGENT_KEY.into(), id.to_string())
            .await
        {
            workspace.update(cx, |workspace, cx| workspace.show_error(error, cx))?;
        }
        if let Some(mut action) = pending_ask {
            action.agent = Some(id.clone());
            workspace.update_in(cx, |workspace, window, cx| {
                open_quick_ask(workspace, &action, window, cx)
            })?;
        }
        Ok(())
    })
    .detach_and_log_err(cx);
}

fn open_quick_ask(
    workspace: &mut Workspace,
    action: &QuickAsk,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if let Some(modal) = workspace.active_modal::<QuickAskModal>(cx) {
        modal.update(cx, |_, cx| cx.emit(DismissEvent));
        return;
    }
    if workspace.panel::<AgentPanel>(cx).is_none() {
        workspace.show_toast(
            Toast::new(
                NotificationId::unique::<QuickAskToast>(),
                "Quick Ask needs the agent panel, which is unavailable in this window.",
            )
            .autohide(),
            cx,
        );
        return;
    }
    let project = workspace.project().read(cx);
    let store = project.agent_server_store().read(cx);
    let id = action.agent.clone().or_else(|| {
        KeyValueStore::global(cx)
            .read_kvp(QUICK_ASK_AGENT_KEY)
            .log_err()
            .flatten()
            .map(AgentId::new)
    });
    let Some(agent) = id.map(Agent::from) else {
        choose_quick_ask_agent(workspace, Some(action.clone()), window, cx);
        return;
    };
    if let Agent::Custom { id } = &agent
        && (project.is_via_collab() || store.agent_source(id).is_none())
    {
        if action.agent.is_none() {
            choose_quick_ask_agent(workspace, Some(action.clone()), window, cx);
            return;
        }
        let toast_id = NotificationId::unique::<QuickAskToast>();
        let toast = if project.is_via_collab() {
            Toast::new(
                toast_id,
                "External agents are unavailable in a shared project.",
            )
            .autohide()
        } else {
            let message = format!(
                "Agent \"{id}\" is unavailable. Install it or configure it in agent_servers."
            );
            Toast::new(toast_id, message).on_click("Open Registry", |window, cx| {
                window.dispatch_action(Box::new(zed_actions::AcpRegistry), cx)
            })
        };
        workspace.show_toast(toast, cx);
        return;
    }
    let label = store
        .agent_display_name(&agent.id())
        .unwrap_or_else(|| agent.label());
    let context = workspace
        .active_item_as::<Editor>(cx)
        .and_then(|editor| EditorContext::capture(&editor, cx));
    let open_files = open_file_paths(workspace, cx);
    let fast_model = action.fast_model.clone();
    let prewarm = context
        .as_ref()
        .filter(|_| action.prewarm)
        .and_then(|context| {
            start_prewarm(
                workspace,
                agent.clone(),
                context,
                &open_files,
                fast_model.clone(),
                window,
                cx,
            )
        });
    let handle = cx.entity().downgrade();
    workspace.toggle_modal(window, cx, move |window, cx| {
        QuickAskModal::new(
            handle, agent, context, open_files, fast_model, prewarm, label, window, cx,
        )
    });
}

fn open_file_paths(workspace: &Workspace, cx: &App) -> Vec<PathBuf> {
    let project = workspace.project().read(cx);
    let mut paths = Vec::new();
    for item in workspace.items(cx) {
        if paths.len() == MAX_OPEN_FILES {
            break;
        }
        if let Some(project_path) = item.project_path(cx)
            && let Some(path) = project.absolute_path(&project_path, cx)
            && !paths.contains(&path)
        {
            paths.push(path);
        }
    }
    paths
}

struct EditorContext {
    path: Option<PathBuf>,
    file_name: Option<String>,
    row: u32,
    symbol: Option<String>,
    selection: Option<String>,
    snippet: String,
    snippet_rows: (u32, u32),
}

impl EditorContext {
    fn capture(editor: &Entity<Editor>, cx: &mut App) -> Option<Self> {
        // Multibuffers have no single file or cursor row to describe.
        let buffer = editor.read(cx).buffer().read(cx).as_singleton()?;
        let selection = editor.update(cx, |editor, cx| {
            editor
                .selections
                .newest::<Point>(&editor.display_snapshot(cx))
        });

        let buffer = buffer.read(cx);
        let path = buffer
            .file()
            .and_then(|file| file.as_local())
            .map(|file| file.abs_path(cx));
        let file_name = buffer.file().map(|file| file.file_name(cx).to_string());
        let snapshot = buffer.snapshot();

        let cursor = selection.head();
        let symbol = snapshot
            .symbols_containing(cursor, None)
            .last()
            .map(|item| item.text.to_string());

        let selected_text = (selection.start != selection.end).then(|| {
            let mut text: String = snapshot
                .text_for_range(selection.start..selection.end)
                .collect();
            if text.len() > MAX_SELECTION_BYTES {
                let mut end = MAX_SELECTION_BYTES;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                text.truncate(end);
            }
            text
        });

        let max_row = snapshot.max_point().row;
        let first_row = cursor.row.saturating_sub(SNIPPET_RADIUS);
        let last_row = (cursor.row + SNIPPET_RADIUS).min(max_row);
        let snippet: String = snapshot
            .text_for_range(
                Point::new(first_row, 0)..Point::new(last_row, snapshot.line_len(last_row)),
            )
            .collect();

        Some(Self {
            path,
            file_name,
            row: cursor.row,
            symbol,
            selection: selected_text,
            snippet,
            snippet_rows: (first_row, last_row),
        })
    }

    fn caption(&self) -> String {
        let mut caption = match &self.file_name {
            Some(name) => format!("{}:{}", name, self.row + 1),
            None => format!("Untitled:{}", self.row + 1),
        };
        if let Some(symbol) = &self.symbol {
            caption.push_str(" · ");
            caption.push_str(symbol);
        }
        caption
    }

    fn describe(&self, out: &mut String) {
        out.push_str("\n\n<editor_context>\n");
        match &self.path {
            Some(path) => out.push_str(&format!("File: {}\n", path.display())),
            None => out.push_str("File: an unsaved buffer\n"),
        }
        out.push_str(&format!("Cursor: line {}", self.row + 1));
        if let Some(symbol) = &self.symbol {
            out.push_str(&format!(", inside `{symbol}`"));
        }
        out.push('\n');
        if let Some(selection) = &self.selection {
            out.push_str(&format!("Selected text:\n```\n{selection}\n```\n"));
        }
        out.push_str(&format!(
            "Buffer lines {}-{}, which may include edits not yet saved to disk:\n```\n{}\n```\n",
            self.snippet_rows.0 + 1,
            self.snippet_rows.1 + 1,
            self.snippet
        ));
        out.push_str("</editor_context>");
    }
}

const EDITOR_REQUEST_INSTRUCTIONS: &str = "I typed this request from my editor without leaving \
     the file. \"This\", \"what I'm writing\" and similar refer to the code at my cursor. Treat it \
     as a one-shot background request: do not ask questions, make reasonable choices, follow \
     this project's conventions, and create or edit whatever files it needs. Keep exploration \
     short and start from the files named above. I am still typing near the cursor, so leave \
     that region alone unless the request requires changing it.";

fn describe_surroundings(
    context: Option<&EditorContext>,
    open_files: &[PathBuf],
    out: &mut String,
) {
    if let Some(context) = context {
        context.describe(out);
    }
    let current_path = context.and_then(|context| context.path.as_ref());
    let mut other_files = open_files
        .iter()
        .filter(|path| Some(*path) != current_path)
        .peekable();
    if other_files.peek().is_some() {
        out.push_str("\n\nOther files open in my editor, likely the most relevant ones:\n");
        for path in other_files {
            out.push_str(&format!("- {}\n", path.display()));
        }
    }
}

fn build_prompt(ask: &str, context: Option<&EditorContext>, open_files: &[PathBuf]) -> String {
    let mut prompt = ask.to_string();
    describe_surroundings(context, open_files, &mut prompt);
    if context.is_some() {
        prompt.push_str("\n\n");
        prompt.push_str(EDITOR_REQUEST_INSTRUCTIONS);
    } else {
        prompt.push_str(
            "\n\nTreat this as a one-shot background request: do not ask questions, make \
             reasonable choices, and follow this project's conventions.",
        );
    }
    prompt
}

fn build_priming_prompt(context: &EditorContext, open_files: &[PathBuf]) -> String {
    let mut prompt = "I am about to ask for a change at my cursor. Get oriented now so you can \
         act quickly when the request arrives: read the file at the cursor and the files a \
         change there would most likely touch. Do not edit anything. When you have the \
         picture, reply with only: ready"
        .to_string();
    describe_surroundings(Some(context), open_files, &mut prompt);
    prompt
}

fn build_follow_up(ask: &str) -> String {
    format!("{ask}\n\n{EDITOR_REQUEST_INSTRUCTIONS}")
}

fn title_for(ask: &str) -> SharedString {
    let line = ask.lines().next().unwrap_or(ask).trim();
    if line.chars().count() <= MAX_TITLE_CHARS {
        line.to_string().into()
    } else {
        let mut title: String = line.chars().take(MAX_TITLE_CHARS - 1).collect();
        title.push('…');
        title.into()
    }
}

pub struct QuickAskModal {
    agent: Agent,
    agent_label: SharedString,
    prompt_editor: Entity<Editor>,
    workspace: WeakEntity<Workspace>,
    context: Option<EditorContext>,
    open_files: Vec<PathBuf>,
    fast_model: Option<String>,
    prewarm: Option<Prewarm>,
}

impl ModalView for QuickAskModal {
    fn on_before_dismiss(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> DismissDecision {
        // Still set only when the prompt closes without a request, so the turn that was
        // started for it has nothing to wait for.
        if let Some(prewarm) = self.prewarm.take() {
            window.defer(cx, move |window, cx| prewarm.session.discard(window, cx));
        }
        DismissDecision::Dismiss(true)
    }
}

impl EventEmitter<DismissEvent> for QuickAskModal {}

impl Focusable for QuickAskModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.prompt_editor.focus_handle(cx)
    }
}

impl QuickAskModal {
    fn new(
        workspace: WeakEntity<Workspace>,
        agent: Agent,
        context: Option<EditorContext>,
        open_files: Vec<PathBuf>,
        fast_model: Option<String>,
        prewarm: Option<Prewarm>,
        agent_label: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let prompt_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Quick Ask", window, cx);
            editor
        });
        Self {
            prompt_editor,
            agent,
            agent_label,
            workspace,
            context,
            open_files,
            fast_model,
            prewarm,
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let ask = self.prompt_editor.read(cx).text(cx).trim().to_string();
        if ask.is_empty() {
            return;
        }
        let prompt = build_prompt(&ask, self.context.as_ref(), &self.open_files);
        let request = Request {
            agent: self.agent.clone(),
            prompt,
            title: title_for(&ask),
        };
        let fast_model = self.fast_model.clone();
        let prewarm = self.prewarm.take();
        let workspace = self.workspace.clone();
        cx.emit(DismissEvent);
        // Deferred so the modal is dismissed and focus is back in the editor before the
        // thread is created.
        window.defer(cx, move |window, cx| {
            workspace
                .update(cx, |workspace, cx| match prewarm {
                    Some(prewarm) => {
                        let follow_up = build_follow_up(&ask);
                        submit_to_prewarmed(prewarm, follow_up, request, fast_model, window, cx);
                    }
                    None => start_thread(workspace, request, fast_model, false, window, cx),
                })
                .log_err();
        });
    }
}

impl Render for QuickAskModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let caption = self
            .context
            .as_ref()
            .map(EditorContext::caption)
            .unwrap_or_else(|| "No file context".to_string());

        v_flex()
            .w(rems(34.))
            .elevation_2(cx)
            .key_context("QuickAsk")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .child(
                div()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .px_2()
                    .py_1p5()
                    .child(self.prompt_editor.clone()),
            )
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_2()
                    .justify_between()
                    .child(
                        Label::new(caption)
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate(),
                    )
                    .child(
                        Label::new(format!("{} · Runs in the background", self.agent_label))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
    }
}

struct QuickAskToast;
struct QuickAskModelToast;

#[derive(Clone)]
struct Request {
    agent: Agent,
    prompt: String,
    title: SharedString,
}

struct Session {
    panel: WeakEntity<AgentPanel>,
    thread_id: ThreadId,
    agent: Agent,
    agent_label: SharedString,
}

impl Session {
    fn create(
        workspace: &mut Workspace,
        agent: Agent,
        title: SharedString,
        first_message: String,
        auto_submit: bool,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Option<Self> {
        let Some(panel) = workspace.panel::<AgentPanel>(cx) else {
            workspace.show_toast(
                Toast::new(
                    NotificationId::unique::<QuickAskToast>(),
                    "Quick Ask needs the agent panel, which is unavailable in this window.",
                )
                .autohide(),
                cx,
            );
            return None;
        };

        let agent_label = workspace
            .project()
            .read(cx)
            .agent_server_store()
            .read(cx)
            .agent_display_name(&agent.id())
            .unwrap_or_else(|| agent.label());
        let thread_id = panel.update(cx, |panel, cx| {
            panel.create_thread_with_options(
                CreateThreadOptions {
                    title: Some(title),
                    initial_content: Some(AgentInitialContent::ContentBlock {
                        blocks: vec![acp_v2::ContentBlock::Text(acp_v2::TextContent::new(
                            first_message,
                        ))],
                        auto_submit,
                    }),
                    agent: Some(agent.clone()),
                    ..Default::default()
                },
                AgentThreadSource::AgentPanel,
                window,
                cx,
            )
        });
        Some(Self {
            panel: panel.downgrade(),
            thread_id,
            agent,
            agent_label,
        })
    }

    fn discard(&self, window: &mut Window, cx: &mut App) {
        let thread_id = self.thread_id;
        self.panel
            .update(cx, |panel, cx| {
                panel.remove_thread_without_activating_draft(thread_id, window, cx);
            })
            .log_err();
    }
}

fn start_thread(
    workspace: &mut Workspace,
    request: Request,
    fast_model: Option<String>,
    escalated: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let prompt = match &fast_model {
        Some(_) => format!("{}{ESCALATION_INSTRUCTIONS}", request.prompt),
        None => request.prompt.clone(),
    };
    // A model choice has to reach the session before the first prompt, so that case
    // submits from `connect_and_send` instead.
    let auto_submit = fast_model.is_none();
    let Some(session) = Session::create(
        workspace,
        request.agent.clone(),
        request.title.clone(),
        prompt,
        auto_submit,
        window,
        cx,
    ) else {
        return;
    };

    let title = request.title.clone();
    let escalation = fast_model.is_some().then_some(request);
    if fast_model.is_some() {
        connect_and_send(
            session.panel.clone(),
            session.thread_id,
            fast_model,
            window,
            cx,
        )
        .detach();
    }
    begin_watch(
        workspace, session, title, escalation, escalated, 0, window, cx,
    );
}

fn begin_watch(
    workspace: &mut Workspace,
    session: Session,
    title: SharedString,
    escalation: Option<Request>,
    escalated: bool,
    baseline_user_messages: usize,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Session {
        panel,
        thread_id,
        agent,
        agent_label,
    } = session;
    let handle = cx.entity().downgrade();
    let activity_message: SharedString = if escalated {
        format!("{agent_label} (escalated): {title}").into()
    } else {
        format!("{agent_label}: {title}").into()
    };
    let activity = BackgroundActivities::global(cx).update(cx, |activities, cx| {
        activities.start(
            BackgroundActivity {
                project: workspace.project().entity_id(),
                message: activity_message.clone(),
                on_click: Some(Rc::new({
                    let handle = handle.clone();
                    let agent = agent.clone();
                    move |window, cx| open_thread(&handle, agent.clone(), thread_id, window, cx)
                })),
            },
            cx,
        )
    });
    watch_thread(
        ThreadWatch {
            workspace: handle,
            panel,
            thread_id,
            agent,
            title,
            escalation,
            activity,
            activity_message,
            baseline_user_messages,
        },
        window,
        cx,
    );
}

struct Prewarm {
    session: Session,
    sent: Shared<Task<Option<Entity<ThreadView>>>>,
}

fn start_prewarm(
    workspace: &mut Workspace,
    agent: Agent,
    context: &EditorContext,
    open_files: &[PathBuf],
    fast_model: Option<String>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Prewarm> {
    let session = Session::create(
        workspace,
        agent,
        "Quick Ask".into(),
        build_priming_prompt(context, open_files),
        false,
        window,
        cx,
    )?;
    let sent = connect_and_send(
        session.panel.clone(),
        session.thread_id,
        fast_model,
        window,
        cx,
    )
    .shared();
    Some(Prewarm { session, sent })
}

fn submit_to_prewarmed(
    prewarm: Prewarm,
    follow_up: String,
    request: Request,
    fast_model: Option<String>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    cx.spawn_in(window, async move |workspace, cx| {
        let Prewarm { session, sent } = prewarm;
        let Some(thread_view) = sent.await else {
            workspace.update_in(cx, |workspace, window, cx| {
                session.discard(window, cx);
                start_thread(workspace, request, fast_model, false, window, cx);
            })?;
            return anyhow::Ok(());
        };

        let title = request.title.clone();
        let escalation = fast_model.is_some().then_some(request);
        let message = match &escalation {
            Some(_) => format!("{follow_up}{ESCALATION_INSTRUCTIONS}"),
            None => follow_up,
        };
        let baseline_user_messages = thread_view.update_in(cx, |thread_view, window, cx| {
            let baseline_user_messages = user_message_count(thread_view.thread.read(cx));
            thread_view
                .thread
                .update(cx, |thread, cx| thread.set_title(title.clone(), cx))
                .detach_and_log_err(cx);
            thread_view.message_editor.update(cx, |message_editor, cx| {
                message_editor.set_message(
                    vec![acp_v2::ContentBlock::Text(acp_v2::TextContent::new(
                        message.clone(),
                    ))],
                    window,
                    cx,
                );
            });
            // The priming turn may still be reading; the request should not wait for it.
            thread_view.interrupt_and_send(window, cx);
            baseline_user_messages
        })?;
        workspace.update_in(cx, |workspace, window, cx| {
            begin_watch(
                workspace,
                session,
                title,
                escalation,
                false,
                baseline_user_messages,
                window,
                cx,
            );
        })?;
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

fn user_message_count(thread: &acp_thread::AcpThread) -> usize {
    thread
        .entries()
        .iter()
        .filter(|entry| matches!(entry, acp_thread::AgentThreadEntry::UserMessage(_)))
        .count()
}

/// Waits for the thread's agent session, selects `model` when one is given, and sends
/// the message waiting in the thread's editor.
fn connect_and_send(
    panel: WeakEntity<AgentPanel>,
    thread_id: ThreadId,
    model: Option<String>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<Option<Entity<ThreadView>>> {
    cx.spawn_in(window, async move |workspace, cx| {
        let sent = async {
            let mut waited = Duration::ZERO;
            let (thread_view, config_options) = loop {
                let connected = panel.read_with(cx, |panel, cx| {
                    let conversation_view = panel.conversation_view_for_id(&thread_id, cx)?;
                    let thread_view = conversation_view.read(cx).root_thread_view()?;
                    let thread = thread_view.read(cx).thread.read(cx);
                    let config_options = thread
                        .connection()
                        .session_config_options(thread.session_id(), cx);
                    Some((thread_view, config_options))
                })?;
                if let Some(connected) = connected {
                    break connected;
                }
                if waited >= START_TIMEOUT {
                    anyhow::bail!("the agent did not connect in time");
                }
                cx.background_executor().timer(CONNECT_POLL_INTERVAL).await;
                waited += CONNECT_POLL_INTERVAL;
            };

            if let Some(model) = model {
                let choice = match &config_options {
                    Some(config_options) => {
                        find_model_option(&config_options.config_options(), &model)
                    }
                    None => ModelChoice::NotOffered,
                };
                match (choice, config_options) {
                    (ModelChoice::Switch(config_id, value), Some(config_options)) => {
                        cx.update(|_, cx| config_options.set_config_option(config_id, value, cx))?
                            .await
                            .log_err();
                    }
                    (ModelChoice::NotOffered, _) => {
                        workspace.update(cx, |workspace, cx| {
                            workspace.show_toast(
                                Toast::new(
                                    NotificationId::unique::<QuickAskModelToast>(),
                                    format!(
                                        "No model matching \"{model}\". Using the agent's default."
                                    ),
                                )
                                .autohide(),
                                cx,
                            );
                        })?;
                    }
                    _ => {}
                }
            }

            thread_view.update_in(cx, |thread_view, window, cx| thread_view.send(window, cx))?;
            anyhow::Ok(thread_view)
        };
        sent.await.log_err()
    })
}

enum ModelChoice {
    Switch(acp_v2::SessionConfigId, acp_v2::SessionConfigOptionValue),
    AlreadySelected,
    NotOffered,
}

fn find_model_option(options: &[acp_v2::SessionConfigOption], wanted: &str) -> ModelChoice {
    let wanted = wanted.to_lowercase();
    let Some((option, select)) = options.iter().find_map(|option| {
        let is_model = matches!(
            option.category,
            Some(acp_v2::SessionConfigOptionCategory::Model)
        );
        match &option.kind {
            acp_v2::SessionConfigKind::Select(select) if is_model => Some((option, select)),
            _ => None,
        }
    }) else {
        return ModelChoice::NotOffered;
    };

    let candidates: Vec<&acp_v2::SessionConfigSelectOption> = match &select.options {
        acp_v2::SessionConfigSelectOptions::Ungrouped(options) => options.iter().collect(),
        acp_v2::SessionConfigSelectOptions::Grouped(groups) => groups
            .iter()
            .flat_map(|group| group.options.iter())
            .collect(),
        _ => Vec::new(),
    };
    let exact = candidates
        .iter()
        .find(|candidate| candidate.value.0.to_lowercase() == wanted);
    let fuzzy = || {
        candidates.iter().find(|candidate| {
            candidate.value.0.to_lowercase().contains(&wanted)
                || candidate.name.to_lowercase().contains(&wanted)
        })
    };
    match exact.or_else(fuzzy) {
        Some(candidate) if candidate.value == select.current_value => ModelChoice::AlreadySelected,
        Some(candidate) => ModelChoice::Switch(
            option.config_id.clone(),
            acp_v2::SessionConfigOptionValue::id(candidate.value.0.to_string()),
        ),
        None => ModelChoice::NotOffered,
    }
}

fn open_thread(
    workspace: &WeakEntity<Workspace>,
    agent: Agent,
    thread_id: ThreadId,
    window: &mut Window,
    cx: &mut App,
) {
    workspace
        .update(cx, |workspace, cx| {
            if let Some(panel) = workspace.focus_panel::<AgentPanel>(window, cx) {
                panel.update(cx, |panel, cx| {
                    panel.load_agent_thread(
                        agent,
                        thread_id,
                        None,
                        None,
                        true,
                        AgentThreadSource::AgentPanel,
                        window,
                        cx,
                    );
                });
            }
        })
        .log_err();
}

fn show_toast(
    workspace: &mut Workspace,
    handle: WeakEntity<Workspace>,
    message: String,
    thread_id: ThreadId,
    agent: Agent,
    cx: &mut Context<Workspace>,
) {
    let toast = Toast::new(NotificationId::unique::<QuickAskToast>(), message)
        .on_click("View", move |window, cx| {
            open_thread(&handle, agent.clone(), thread_id, window, cx)
        });
    workspace.show_toast(toast, cx);
}

enum ThreadState {
    Gone,
    Connecting,
    NotStarted,
    Running,
    NeedsApproval,
    Failed,
    Finished { wants_escalation: bool },
}

fn thread_state(
    panel: &AgentPanel,
    thread_id: &ThreadId,
    baseline_user_messages: usize,
    cx: &App,
) -> ThreadState {
    let Some(view) = panel.conversation_view_for_id(thread_id, cx) else {
        return ThreadState::Gone;
    };
    let Some(thread) = view.read(cx).root_thread(cx) else {
        return ThreadState::Connecting;
    };
    let thread = thread.read(cx);
    if thread.is_waiting_for_confirmation() {
        ThreadState::NeedsApproval
    } else if thread.status() != acp_thread::ThreadStatus::Idle {
        ThreadState::Running
    } else if thread.had_error() {
        ThreadState::Failed
    } else if user_message_count(thread) <= baseline_user_messages {
        ThreadState::NotStarted
    } else {
        let wants_escalation = thread
            .entries()
            .iter()
            .rev()
            .find_map(|entry| match entry {
                acp_thread::AgentThreadEntry::AssistantMessage(message) => {
                    Some(message.to_markdown(cx).contains(ESCALATION_MARKER))
                }
                _ => None,
            })
            .unwrap_or(false);
        ThreadState::Finished { wants_escalation }
    }
}

/// Files the agent wrote that had no previous content, as reported by its edit tool calls.
fn created_file_paths(panel: &AgentPanel, thread_id: &ThreadId, cx: &App) -> Vec<String> {
    let Some(thread) = panel
        .conversation_view_for_id(thread_id, cx)
        .and_then(|view| view.read(cx).root_thread(cx))
    else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    for entry in thread.read(cx).entries() {
        for diff in entry.diffs() {
            let diff = diff.read(cx);
            if diff.base_text().is_empty()
                && let Some(path) = diff.file_path(cx)
                && !paths.contains(&path)
            {
                paths.push(path);
            }
        }
    }
    paths
}

struct ThreadWatch {
    workspace: WeakEntity<Workspace>,
    panel: WeakEntity<AgentPanel>,
    thread_id: ThreadId,
    agent: Agent,
    title: SharedString,
    escalation: Option<Request>,
    activity: BackgroundActivityId,
    activity_message: SharedString,
    baseline_user_messages: usize,
}

enum Outcome {
    Done { created_files: Vec<String> },
    Report(String),
    Escalate(Request),
    Silent,
}

impl ThreadWatch {
    fn set_activity_message(&self, message: SharedString, cx: &mut AsyncWindowContext) {
        let activity = self.activity;
        cx.update(|_, cx| {
            BackgroundActivities::global(cx).update(cx, |activities, cx| {
                activities.set_message(activity, message, cx);
            });
        })
        .log_err();
    }

    async fn settle(&mut self, cx: &mut AsyncWindowContext) -> Outcome {
        let title = self.title.clone();
        let thread_id = self.thread_id;
        let baseline_user_messages = self.baseline_user_messages;
        let mut has_run = false;
        let mut asked_for_approval = false;
        let mut idle_for = Duration::ZERO;
        loop {
            cx.background_executor().timer(POLL_INTERVAL).await;
            let Ok(state) = self.panel.read_with(cx, |panel, cx| {
                thread_state(panel, &thread_id, baseline_user_messages, cx)
            }) else {
                return Outcome::Silent;
            };
            match state {
                ThreadState::Running => {
                    has_run = true;
                    if asked_for_approval {
                        asked_for_approval = false;
                        self.set_activity_message(self.activity_message.clone(), cx);
                    }
                }
                ThreadState::NeedsApproval => {
                    has_run = true;
                    if !asked_for_approval {
                        asked_for_approval = true;
                        let message = format!("Needs approval · {title}");
                        self.set_activity_message(format!("Needs approval: {title}").into(), cx);
                        let handle = self.workspace.clone();
                        let agent = self.agent.clone();
                        self.workspace
                            .update(cx, |workspace, cx| {
                                show_toast(workspace, handle, message, thread_id, agent, cx);
                            })
                            .log_err();
                    }
                }
                ThreadState::Failed => return Outcome::Report(format!("Failed · {title}")),
                ThreadState::Finished {
                    wants_escalation: true,
                } if self.escalation.is_some() => {
                    return match self.escalation.take() {
                        Some(request) => Outcome::Escalate(request),
                        None => Outcome::Silent,
                    };
                }
                ThreadState::Finished { .. } => {
                    let created_files = self
                        .panel
                        .read_with(cx, |panel, cx| created_file_paths(panel, &thread_id, cx))
                        .unwrap_or_default();
                    return Outcome::Done { created_files };
                }
                ThreadState::Gone if has_run => {
                    return Outcome::Done {
                        created_files: Vec::new(),
                    };
                }
                ThreadState::Gone => return Outcome::Silent,
                ThreadState::NotStarted | ThreadState::Connecting => {
                    idle_for += POLL_INTERVAL;
                    if idle_for >= START_TIMEOUT {
                        return Outcome::Report(format!("Did not start · {title}"));
                    }
                }
            }
        }
    }
}

fn watch_thread(mut watch: ThreadWatch, window: &mut Window, cx: &mut Context<Workspace>) {
    cx.spawn_in(window, async move |_, cx| {
        let outcome = watch.settle(cx).await;

        let activity = watch.activity;
        cx.update(|_, cx| {
            BackgroundActivities::global(cx).update(cx, |activities, cx| {
                activities.finish(activity, cx);
            });
        })
        .log_err();

        let ThreadWatch {
            workspace,
            panel,
            thread_id,
            agent,
            title,
            ..
        } = watch;
        match outcome {
            Outcome::Done { created_files } => {
                let handle = workspace.clone();
                workspace
                    .update_in(cx, |workspace, window, cx| {
                        let mut opened = 0;
                        for path in &created_files {
                            let project_path =
                                workspace.project().read(cx).find_project_path(path, cx);
                            if let Some(project_path) = project_path {
                                // Opened as background tabs so the buffer being typed in
                                // stays in front.
                                workspace
                                    .open_path_preview(
                                        project_path,
                                        None,
                                        false,
                                        false,
                                        false,
                                        window,
                                        cx,
                                    )
                                    .detach_and_log_err(cx);
                                opened += 1;
                            }
                        }
                        let message = match opened {
                            0 => format!("Done · {title}"),
                            1 => format!("Done · {title} · opened 1 new file"),
                            count => format!("Done · {title} · opened {count} new files"),
                        };
                        show_toast(workspace, handle, message, thread_id, agent, cx);
                    })
                    .log_err();
            }
            Outcome::Report(message) => {
                let handle = workspace.clone();
                workspace
                    .update(cx, |workspace, cx| {
                        show_toast(workspace, handle, message, thread_id, agent, cx);
                    })
                    .log_err();
            }
            Outcome::Escalate(request) => {
                workspace
                    .update_in(cx, |workspace, window, cx| {
                        panel
                            .update(cx, |panel, cx| {
                                panel.remove_thread_without_activating_draft(thread_id, window, cx);
                            })
                            .log_err();
                        start_thread(workspace, request, None, true, window, cx);
                    })
                    .log_err();
            }
            Outcome::Silent => {}
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_title_for_truncates_to_first_line() {
        assert_eq!(title_for("make an endpoint").as_ref(), "make an endpoint");
        assert_eq!(title_for("first line\nsecond line").as_ref(), "first line");

        let long = "x".repeat(MAX_TITLE_CHARS + 10);
        let title = title_for(&long);
        assert_eq!(title.chars().count(), MAX_TITLE_CHARS);
        assert!(title.ends_with('…'));
    }

    #[test]
    fn test_build_prompt_describes_editor_context() {
        let context = EditorContext {
            path: Some(PathBuf::from("/project/src/invoices.rs")),
            file_name: Some("invoices.rs".to_string()),
            row: 9,
            symbol: Some("fn mark_paid".to_string()),
            selection: Some("invoice.paid = true;".to_string()),
            snippet: "fn mark_paid() {\n    invoice.paid = true;\n}".to_string(),
            snippet_rows: (8, 10),
        };
        assert_eq!(context.caption(), "invoices.rs:10 · fn mark_paid");

        let open_files = [
            PathBuf::from("/project/src/invoices.rs"),
            PathBuf::from("/project/src/server.rs"),
        ];
        let prompt = build_prompt("make an endpoint for this", Some(&context), &open_files);
        assert!(prompt.starts_with("make an endpoint for this\n\n<editor_context>\n"));
        assert!(prompt.contains("File: /project/src/invoices.rs\n"));
        assert!(prompt.contains("Cursor: line 10, inside `fn mark_paid`\n"));
        assert!(prompt.contains("Selected text:\n```\ninvoice.paid = true;\n```\n"));
        assert!(prompt.contains("Buffer lines 9-11"));
        assert!(prompt.contains("Other files open in my editor"));
        assert!(prompt.contains("- /project/src/server.rs\n"));
        assert!(!prompt.contains("- /project/src/invoices.rs\n"));
    }

    #[test]
    fn test_build_prompt_without_editor_context() {
        let prompt = build_prompt("add a changelog entry", None, &[]);
        assert!(prompt.starts_with("add a changelog entry\n\n"));
        assert!(!prompt.contains("<editor_context>"));
    }
}
