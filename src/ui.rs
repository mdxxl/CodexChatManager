use crate::catalog::{CatalogRow, SidebarCatalog};
use crate::discovery;
use crate::drawing::{self, Align, Canvas, Glyph, Rect, FG, MUTED, SECTION, SUBTITLE, TINY};
use crate::language::{delete_word, demo_title, tr, trf, Language};
use crate::service::{make_plan, Chat, DeletePlan, DeleteResult, Service};
use crate::time::{from_unix_local, now_local};
use crate::{err, wide, Result};
use chrono::{Datelike, Duration as Days};
use std::cell::RefCell;
use std::collections::HashSet;
use std::mem::{size_of, zeroed};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::result::Result::Ok;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use unicode_casefold::UnicodeCaseFold;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE, DWMWA_USE_IMMERSIVE_DARK_MODE,
    DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::Graphics::GdiPlus::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::Dialogs::*;
use windows_sys::Win32::UI::Controls::{
    InitCommonControlsEx, EM_LINESCROLL, EM_SETCUEBANNER, EM_SETMARGINS, EM_SETSEL,
    ICC_STANDARD_CLASSES, INITCOMMONCONTROLSEX, WM_MOUSELEAVE,
};
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const BG: u32 = 0x171717;
const SIDEBAR: u32 = 0x101010;
const PANEL: u32 = 0x1c1c1c;
const EDIT: u32 = 0x202020;
const FADE_IN: Duration = Duration::from_millis(230);
const FADE_OUT: Duration = Duration::from_millis(190);
const DIALOG_FADE_IN: Duration = Duration::from_millis(160);
const DIALOG_FADE_OUT: Duration = Duration::from_millis(120);
const APP_ICON_RESOURCE_ID: usize = 1; // app.rc

fn cleanup_dialog(status: &str) -> Dialog {
    let explanation = tr(
        concat!(
            "Старые ссылки — это записи в боковой панели Codex, которые ведут к уже удалённым чатам. ",
            "При открытии такой записи может появиться ошибка «no rollout found». ",
            "Существующие переписки сохранятся."
        ),
        concat!(
            "Stale links are sidebar entries in Codex that point to deleted chats. ",
            "Opening one can show a “no rollout found” error. Existing chats are kept."
        ),
    );
    Dialog::new(
        tr("Очистить старые ссылки?", "Remove stale links?"),
        format!("{explanation}\n\n{status}"),
    )
}

// UxTheme can request its own frame painting when a shell menu steals focus.
const WM_NCUAHDRAWCAPTION: u32 = 0x00ae;
const WM_NCUAHDRAWFRAME: u32 = 0x00af;

enum Work {
    Load(Option<Option<PathBuf>>),
    Prepare(HashSet<String>, bool),
    Delete(DeletePlan),
    Scan,
    Clean(HashSet<String>),
}

enum Outcome {
    Loaded(Vec<Chat>),
    Prepared(DeletePlan, bool),
    Deleted(DeleteResult),
    Scanned(Result<Vec<CatalogRow>>),
    Cleaned(usize),
}

enum Event {
    Progress(usize, usize),
    Done(Result<Outcome>, Option<PathBuf>),
}

enum Status {
    Loading,
    Updated(chrono::NaiveTime),
    Failed,
    RefreshNeeded,
    Deleting,
    Progress(usize, usize),
}

impl Status {
    fn text(&self) -> String {
        match self {
            Self::Loading => tr("Обновление списка…", "Refreshing chats…").to_owned(),
            Self::Updated(time) => trf!("Обновлено в {}", "Updated at {}", time.format("%H:%M")),
            Self::Failed => tr("Операция не завершена", "Operation failed").to_owned(),
            Self::RefreshNeeded => tr("Нужно обновить список", "Refresh required").to_owned(),
            Self::Deleting => tr("Удаление…", "Deleting…").to_owned(),
            Self::Progress(i, n) => trf!("Удаление {i} из {n}…", "Deleting {i} of {n}…"),
        }
    }
}

struct Worker {
    tx: Option<Sender<(Language, Work)>>,
    rx: Receiver<Event>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    fn new(demo: bool) -> Self {
        let (tx, incoming) = mpsc::channel();
        let (events, rx) = mpsc::channel();
        let thread = thread::spawn(move || {
            let mut service = Service::new(demo);
            while let Ok((language, work)) = incoming.recv() {
                Language::set_current(language);
                let result = (|| {
                    Ok(match work {
                        Work::Load(reconnect) => {
                            if let Some(path) = reconnect {
                                service.reconnect(path);
                            }
                            Outcome::Loaded(service.list_all()?)
                        }
                        Work::Prepare(ids, all) => {
                            Outcome::Prepared(make_plan(&service.list_all()?, &ids)?, all)
                        }
                        Work::Delete(plan) => Outcome::Deleted(service.delete(plan, |i, n| {
                            let _ = events.send(Event::Progress(i, n));
                        })?),
                        Work::Scan => Outcome::Scanned(SidebarCatalog::default().stale()),
                        Work::Clean(ids) => {
                            discovery::require_closed_codex_for_cleanup()?;
                            Outcome::Cleaned(SidebarCatalog::default().clean(ids)?.len())
                        }
                    })
                })();
                if events
                    .send(Event::Done(result, service.executable()))
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            tx: Some(tx),
            rx,
            thread: Some(thread),
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.tx.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Scope {
    All,
    Recent,
    Archive,
}

impl Scope {
    fn title(self) -> &'static str {
        match self {
            Self::All => tr("Все чаты", "All chats"),
            Self::Recent => tr("Недавние", "Recent"),
            Self::Archive => tr("Архив", "Archive"),
        }
    }
    fn filter(self, row: &Chat) -> bool {
        match self {
            Self::All => true,
            Self::Recent => !row.archived,
            Self::Archive => row.archived,
        }
    }
    fn count_label(self) -> &'static str {
        if self == Self::Recent {
            tr("Обычные", "Recent")
        } else {
            self.title()
        }
    }
}

#[derive(Clone, Copy)]
enum ListItem {
    Group(&'static str),
    Chat(usize),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Action {
    Scope(Scope),
    Repair,
    Settings,
    Language(Language),
    Minimize,
    Maximize,
    Close,
    Refresh,
    Search,
    ClearSearch,
    Auxiliary,
    Row(usize),
    Select,
    Clear,
    Delete,
    DeleteAll,
    CancelDialog,
    Confirm,
    Find,
    Choose,
    Phrase,
    Details,
    ScrollBar,
}

#[derive(Clone, Copy)]
enum Style {
    Ghost,
    Nav,
    Window,
    Primary,
    Danger,
}

struct Button<'a> {
    action: Action,
    rect: Rect,
    text: &'a str,
    icon: Option<Glyph>,
    style: Style,
    selected: bool,
    enabled: bool,
}

impl<'a> Button<'a> {
    fn new(action: Action, rect: Rect, text: &'a str) -> Self {
        Self {
            action,
            rect,
            text,
            icon: None,
            style: Style::Ghost,
            selected: false,
            enabled: true,
        }
    }
}

struct Dialog {
    title: String,
    body: String,
    details: String,
    prompt: Option<Prompt>,
}

enum Prompt {
    Delete(DeletePlan),
    Clean(HashSet<String>),
    Settings,
}

#[derive(Clone, Copy)]
enum DismissDialog {
    Cancel,
    Confirm,
    Find,
    Choose,
}

struct DialogTransition {
    fade: Animation,
    dismiss: Option<DismissDialog>,
}

impl DialogTransition {
    fn frame(&self, now: Instant) -> (u8, bool) {
        let duration = if self.dismiss.is_some() {
            DIALOG_FADE_OUT
        } else {
            DIALOG_FADE_IN
        };
        self.fade.frame(now, duration)
    }
}

fn deletion_details(rows: &[Chat], grouped: bool) -> String {
    if !grouped {
        return rows
            .iter()
            .map(|row| format!("• {}", row.title()))
            .collect::<Vec<_>>()
            .join("\n");
    }

    let mut sections = [
        (tr("Чаты", "Chats"), Vec::new()),
        (tr("Архив", "Archive"), Vec::new()),
        (tr("Служебные чаты", "Auxiliary chats"), Vec::new()),
    ];
    for row in rows {
        let section = if row.auxiliary() {
            2
        } else {
            usize::from(row.archived)
        };
        sections[section]
            .1
            .push(format!("\u{2003}• {}", row.title()));
    }
    sections
        .into_iter()
        .filter(|(_, titles)| !titles.is_empty())
        .map(|(heading, titles)| format!("{heading}\n{}", titles.join("\n")))
        .collect::<Vec<_>>()
        .join("\n\n")
}

impl Dialog {
    fn new(title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            body: body.into(),
            details: String::new(),
            prompt: None,
        }
    }
    fn with_details(mut self, details: String) -> Self {
        self.details = details;
        self
    }
    fn with_prompt(mut self, prompt: Prompt) -> Self {
        self.prompt = Some(prompt);
        self
    }
    fn phrase(&self) -> bool {
        matches!(self.prompt, Some(Prompt::Delete(_)))
    }

    fn can_confirm(&self, phrase: &str) -> bool {
        match &self.prompt {
            Some(Prompt::Delete(_)) => phrase.trim() == delete_word(),
            Some(Prompt::Clean(ids)) => !ids.is_empty(),
            _ => true,
        }
    }
}

#[derive(Clone, Copy)]
struct DialogLayout {
    shell: Rect,
    body: Rect,
    details: Option<Rect>,
    phrase: Option<Rect>,
    entry: Option<Rect>,
    connection: Option<(Rect, Rect)>,
    close: Rect,
    cancel: Option<Rect>,
    confirm: Rect,
}

#[derive(Clone, Copy)]
struct Layout {
    w: i32,
    h: i32,
    search: Rect,
    list: Rect,
    actions_y: i32,
    edit: Rect,
}

impl Layout {
    fn new(w: i32, h: i32, query: bool) -> Self {
        let search = Rect(254, 162, (w - 290).max(1), 46);
        Self {
            w,
            h,
            search,
            list: Rect(254, 277, (w - 290).max(1), (h - 410).max(1)),
            actions_y: h - 115,
            edit: Rect(301, 176, (w - if query { 377 } else { 352 }).max(1), 19),
        }
    }
}

struct Animation {
    start: Instant,
    from: u8,
    closing: bool,
}

impl Animation {
    fn frame(&self, now: Instant, duration: Duration) -> (u8, bool) {
        let t = (now.duration_since(self.start).as_secs_f64() / duration.as_secs_f64()).min(1.0);
        let alpha = if self.closing {
            f64::from(self.from) * (1.0 - t * t)
        } else {
            255.0 * (1.0 - (1.0 - t).powi(3))
        };
        (alpha.round() as u8, t >= 1.0)
    }
}

struct WindowState {
    app: RefCell<App>,
    edit_colors: EditColors,
}

struct EditColors {
    edit: HBRUSH,
    details: HBRUSH,
}

impl EditColors {
    fn new() -> Result<Self> {
        let colors = unsafe {
            Self {
                edit: CreateSolidBrush(drawing::colorref(EDIT)),
                details: CreateSolidBrush(drawing::colorref(PANEL)),
            }
        };
        if colors.edit.is_null() || colors.details.is_null() {
            return Err(err(tr(
                "Не удалось создать фон полей ввода.",
                "Could not create the text field background.",
            )));
        }
        Ok(colors)
    }

    fn apply(&self, control: HWND, dc: HDC) -> LRESULT {
        unsafe {
            let read_only = GetWindowLongPtrW(control, GWL_STYLE) & ES_READONLY as isize != 0;
            let (background, text, brush) = if read_only {
                (PANEL, MUTED, self.details)
            } else {
                (EDIT, FG, self.edit)
            };
            SetBkColor(dc, drawing::colorref(background));
            SetTextColor(dc, drawing::colorref(text));
            brush as LRESULT
        }
    }
}

impl Drop for EditColors {
    fn drop(&mut self) {
        unsafe {
            DeleteObject(self.edit);
            DeleteObject(self.details);
        }
    }
}

struct App {
    hwnd: HWND,
    search: HWND,
    entry: HWND,
    details: HWND,
    edit_font: HFONT,
    details_font: HFONT,
    dpi: u32,
    width: i32,
    height: i32,
    worker: Worker,
    demo: bool,
    smoke: bool,
    smoked: bool,
    smoke_error: Option<String>,
    busy: bool,
    loaded: bool,
    rows: Vec<Chat>,
    checked: HashSet<String>,
    scope: Scope,
    executable: Option<PathBuf>,
    query: String,
    phrase: String,
    auxiliary: bool,
    status: Status,
    list: Vec<ListItem>,
    visible: Vec<usize>,
    scroll: i32,
    drag: Option<(i32, i32)>,
    dialog: Option<Dialog>,
    dialog_layout: Option<DialogLayout>,
    dialog_transition: Option<DialogTransition>,
    cursor: Option<(i32, i32)>,
    hover: Option<Action>,
    hover_at: Instant,
    pressed: Option<Action>,
    focus: Option<Action>,
    keyboard: bool,
    alpha: u8,
    animation: Option<Animation>,
}

impl App {
    fn new(demo: bool, smoke: bool) -> Self {
        Self {
            hwnd: null_mut(),
            search: null_mut(),
            entry: null_mut(),
            details: null_mut(),
            edit_font: null_mut(),
            details_font: null_mut(),
            dpi: 96,
            width: 1180,
            height: 800,
            worker: Worker::new(demo),
            demo,
            smoke,
            smoked: false,
            smoke_error: None,
            busy: false,
            loaded: false,
            rows: Vec::new(),
            checked: HashSet::new(),
            scope: Scope::All,
            executable: None,
            query: String::new(),
            phrase: String::new(),
            auxiliary: false,
            status: Status::Loading,
            list: Vec::new(),
            visible: Vec::new(),
            scroll: 0,
            drag: None,
            dialog: None,
            dialog_layout: None,
            dialog_transition: None,
            cursor: None,
            hover: None,
            hover_at: Instant::now(),
            pressed: None,
            focus: None,
            keyboard: false,
            alpha: 0,
            animation: None,
        }
    }

    fn layout(&self) -> Layout {
        Layout::new(self.width, self.height, !self.query.is_empty())
    }
    fn px(&self, v: i32) -> i32 {
        (v * self.dpi as i32 + 48) / 96
    }
    fn logical(&self, v: i32) -> i32 {
        (v * 96 + if v < 0 { -1 } else { 1 } * self.dpi as i32 / 2) / self.dpi as i32
    }

    fn redraw(&self) {
        unsafe {
            InvalidateRect(self.hwnd, null(), 0);
        }
    }

    fn rebuild(&mut self) {
        let query: String = self.query.trim().case_fold().collect();
        let today = now_local().date();
        let yesterday = today - Days::days(1);
        let mut groups: Vec<(&str, Vec<usize>)> = Vec::new();
        for (index, row) in self.rows.iter().enumerate() {
            if !self.scope.filter(row) || !self.auxiliary && row.auxiliary() {
                continue;
            }
            if !query.is_empty() {
                let text: String = format!(
                    "{} {} {}",
                    row.title(),
                    row.id,
                    row.cwd.as_deref().unwrap_or_default()
                )
                .as_str()
                .case_fold()
                .collect();
                if !text.contains(&query) {
                    continue;
                }
            }
            let date = from_unix_local(row.updated_at.unwrap_or(0)).map(|v| v.date());
            let group = if date == Some(today) {
                tr("Сегодня", "Today")
            } else if date == Some(yesterday) {
                tr("Вчера", "Yesterday")
            } else {
                tr("Ранее", "Earlier")
            };
            if let Some((_, rows)) = groups.iter_mut().find(|(name, _)| *name == group) {
                rows.push(index);
            } else {
                groups.push((group, vec![index]));
            }
        }
        self.list.clear();
        self.visible.clear();
        for (name, rows) in groups {
            self.list.push(ListItem::Group(name));
            self.visible.extend(&rows);
            self.list.extend(rows.into_iter().map(ListItem::Chat));
        }
        self.scroll = self.scroll.min(self.max_scroll());
        if let Some(Action::Row(index)) = self.focus {
            if !self.visible.contains(&index) {
                self.focus = None;
            }
        }
        self.redraw();
    }

    fn accept_rows(&mut self, rows: Vec<Chat>) {
        let ids: HashSet<_> = rows.iter().map(|row| row.id.as_str()).collect();
        self.checked.retain(|id| ids.contains(id.as_str()));
        self.rows = rows;
        self.loaded = true;
        self.status = Status::Updated(now_local().time());
        self.rebuild();
    }

    fn start(&mut self, work: Work) {
        if self.busy {
            return;
        }
        self.busy = self
            .worker
            .tx
            .as_ref()
            .is_some_and(|tx| tx.send((Language::current(), work)).is_ok());
        self.redraw();
    }

    fn refresh(&mut self) {
        self.status = Status::Loading;
        self.start(Work::Load(None));
    }

    fn prepare(&mut self, all: bool) {
        let ids = if all {
            self.rows.iter().map(|row| row.id.clone()).collect()
        } else {
            self.checked.clone()
        };
        if !ids.is_empty() {
            self.start(Work::Prepare(ids, all));
        }
    }

    fn event(&mut self, event: Event) {
        let result = match event {
            Event::Progress(i, n) => {
                self.status = Status::Progress(i, n);
                self.redraw();
                return;
            }
            Event::Done(result, executable) => {
                self.busy = false;
                self.executable = executable;
                result
            }
        };
        match result {
            Err(error) => {
                self.status = Status::Failed;
                self.open_dialog(Dialog::new(
                    tr("Не удалось выполнить", "Operation failed"),
                    error.to_string(),
                ));
            }
            Ok(Outcome::Loaded(rows)) => self.accept_rows(rows),
            Ok(Outcome::Prepared(plan, all)) => {
                let details = deletion_details(&plan.affected, all);
                let n = plan.affected.len();
                let title = match Language::current() {
                    Language::English if n == 1 => "Delete 1 chat?".to_owned(),
                    Language::English => format!("Delete {n} chats?"),
                    Language::Russian => {
                        let noun = match (n % 10, n % 100) {
                            (1, n) if n != 11 => "чат",
                            (2..=4, n) if !(12..=14).contains(&n) => "чата",
                            _ => "чатов",
                        };
                        format!("Удалить {n} {noun}?")
                    }
                };
                let body = if self.demo {
                    tr(
                        "Это демо. Будут убраны только вымышленные чаты.",
                        "This is a demo. Only sample chats will be removed.",
                    )
                    .to_owned()
                } else {
                    let mut body = tr(
                        "Переписки и связанные подзадачи будут удалены без возможности восстановления.",
                        "Chats and their related subtasks will be permanently deleted.",
                    ).to_owned();
                    if all {
                        body.push_str(tr(
                            " Включены архив и чаты, скрытые поиском.",
                            " This includes archived chats and chats hidden by search.",
                        ));
                    }
                    body
                };
                self.open_dialog(
                    Dialog::new(title, body)
                        .with_details(details)
                        .with_prompt(Prompt::Delete(plan)),
                );
            }
            Ok(Outcome::Deleted(result)) => {
                if let Some(rows) = result.rows {
                    self.accept_rows(rows);
                } else {
                    self.status = Status::RefreshNeeded;
                }
                let mut text = trf!(
                    "Проверено удаление основных чатов: {}.",
                    "Confirmed deleted chats: {}.",
                    result.deleted.len()
                );
                if !self.demo {
                    text.push_str(tr(
                        " Теперь можно открыть Codex.",
                        " You can reopen Codex now.",
                    ));
                }
                let details = result
                    .failed
                    .into_iter()
                    .map(|(id, error)| format!("{id}: {error}"))
                    .chain(result.refresh_error)
                    .chain(result.sidebar_error)
                    .collect::<Vec<_>>()
                    .join("\n");
                let title = if details.is_empty() {
                    tr("Удаление завершено", "Deletion complete")
                } else {
                    tr("Проверьте результат", "Review the result")
                };
                self.open_dialog(Dialog::new(title, text).with_details(details));
            }
            Ok(Outcome::Scanned(result)) => self.show_cleanup_preview(result),
            Ok(Outcome::Cleaned(n)) => self.open_dialog(Dialog::new(
                tr("Старые ссылки убраны", "Stale links removed"),
                trf!(
                    "Убрано ссылок — {n}. Теперь можно открыть Codex.",
                    "Removed links: {n}. You can reopen Codex now."
                ),
            )),
        }
        self.redraw();
    }

    fn show_cleanup_preview(&mut self, result: Result<Vec<CatalogRow>>) {
        if !matches!(
            self.dialog
                .as_ref()
                .and_then(|dialog| dialog.prompt.as_ref()),
            Some(Prompt::Clean(_))
        ) {
            return;
        }
        let (status, rows) = match result {
            Ok(rows) if rows.is_empty() => (
                tr("Старых ссылок нет.", "No stale links found.").to_owned(),
                rows,
            ),
            Ok(rows) => (
                trf!(
                    "Найдено ссылок — {}. Перед очисткой полностью закройте Codex.",
                    "Links found: {}. Close Codex completely before removing them.",
                    rows.len()
                ),
                rows,
            ),
            Err(error) => (
                trf!(
                    "Проверить список не удалось.\n{error}",
                    "Could not check for stale links.\n{error}"
                ),
                Vec::new(),
            ),
        };
        let details = rows
            .iter()
            .map(|row| {
                if row.title.is_empty() {
                    &row.id
                } else {
                    &row.title
                }
            })
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n");
        self.update_dialog(
            cleanup_dialog(&status)
                .with_details(details)
                .with_prompt(Prompt::Clean(rows.into_iter().map(|row| row.id).collect())),
        );
    }
}

impl App {
    fn buttons(&self, layout: Layout) -> Vec<Button<'static>> {
        let Layout {
            w, h, actions_y: y, ..
        } = layout;
        let mut result = Vec::new();
        for (i, (scope, icon)) in [
            (Scope::All, Glyph::Chat),
            (Scope::Recent, Glyph::History),
            (Scope::Archive, Glyph::Archive),
        ]
        .into_iter()
        .enumerate()
        {
            let mut button = Button::new(
                Action::Scope(scope),
                Rect(15, 148 + i as i32 * 48, 192, 42),
                scope.title(),
            );
            button.icon = Some(icon);
            button.style = Style::Nav;
            button.selected = scope == self.scope;
            result.push(button);
        }
        for (i, (action, label, icon)) in [
            (
                Action::Repair,
                tr("Убрать старые ссылки", "Remove stale links"),
                Glyph::Repair,
            ),
            (
                Action::Settings,
                tr("Подключение", "Connection"),
                Glyph::Settings,
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let mut button = Button::new(action, Rect(15, h - 155 + i as i32 * 48, 192, 42), label);
            button.icon = Some(icon);
            button.style = Style::Nav;
            button.enabled = !self.busy;
            result.push(button);
        }
        for (i, (language, label)) in [
            (Language::English, "English"),
            (Language::Russian, "Russian"),
        ]
        .into_iter()
        .enumerate()
        {
            let rect = Rect(15 + i as i32 * 98, h - 59, 94, 34);
            let mut button = Button::new(Action::Language(language), rect, label);
            button.selected = language == Language::current();
            button.enabled = !self.busy;
            result.push(button);
        }
        for (i, (action, icon)) in [
            (Action::Minimize, Glyph::Minimize),
            (Action::Maximize, Glyph::Maximize),
            (Action::Close, Glyph::Close),
        ]
        .into_iter()
        .enumerate()
        {
            let mut button = Button::new(action, Rect(w - 128 + i as i32 * 40, 12, 34, 28), "");
            button.icon = Some(icon);
            button.style = Style::Window;
            result.push(button);
        }
        let mut refresh = Button::new(Action::Refresh, Rect(w - 213, 70, 36, 36), "");
        refresh.icon = Some(Glyph::Refresh);
        refresh.enabled = !self.busy;
        result.push(refresh);
        let mut delete_all = Button::new(
            Action::DeleteAll,
            Rect(w - 171, 70, 135, 36),
            tr("Полная очистка", "Delete all chats"),
        );
        delete_all.style = Style::Danger;
        delete_all.enabled = !self.busy && !self.rows.is_empty();
        result.push(delete_all);
        let mut auxiliary = Button::new(
            Action::Auxiliary,
            Rect(w - 141, 228, 105, 39),
            tr("Служебные", "Auxiliary"),
        );
        auxiliary.selected = self.auxiliary;
        result.push(auxiliary);
        let selection = !self.checked.is_empty();
        let mut select = Button::new(
            Action::Select,
            Rect(w - if selection { 422 } else { 315 }, y, 109, 39),
            tr("Выбрать все", "Select all"),
        );
        select.enabled = !self.busy;
        result.push(select);
        if selection {
            let mut clear = Button::new(
                Action::Clear,
                Rect(w - 310, y, 110, 39),
                tr("Снять выбор", "Clear selection"),
            );
            clear.enabled = !self.busy;
            result.push(clear);
        }
        let mut delete = Button::new(
            Action::Delete,
            Rect(w - 198, y, 162, 39),
            tr("Удалить выбранные", "Delete selected"),
        );
        delete.style = Style::Primary;
        delete.enabled = selection && !self.busy;
        result.push(delete);
        result
    }

    fn dialog_buttons(&self) -> Vec<Button<'static>> {
        let (Some(dialog), Some(layout)) = (&self.dialog, self.dialog_layout) else {
            return Vec::new();
        };
        let mut close = Button::new(Action::CancelDialog, layout.close, "");
        close.icon = Some(Glyph::Close);
        let mut result = vec![close];
        if let Some((find, choose)) = layout.connection {
            result.push(Button::new(
                Action::Find,
                find,
                tr("Найти Codex", "Find Codex"),
            ));
            result.push(Button::new(
                Action::Choose,
                choose,
                tr("Выбрать файл…", "Choose file…"),
            ));
        }
        if let Some(cancel) = layout.cancel {
            result.push(Button::new(
                Action::CancelDialog,
                cancel,
                tr("Отмена", "Cancel"),
            ));
        }
        let (text, style) = match &dialog.prompt {
            Some(Prompt::Delete(_)) => {
                (tr("Удалить навсегда", "Delete permanently"), Style::Danger)
            }
            Some(Prompt::Clean(_)) => (tr("Очистить", "Remove"), Style::Primary),
            _ => (tr("Понятно", "OK"), Style::Primary),
        };
        let mut confirm = Button::new(Action::Confirm, layout.confirm, text);
        confirm.style = style;
        confirm.enabled = dialog.can_confirm(&self.phrase);
        result.push(confirm);
        result
    }

    fn viewport(&self) -> Rect {
        self.layout().list
    }

    fn list_height(&self) -> i32 {
        self.list
            .iter()
            .map(|item| match item {
                ListItem::Group(_) => 39,
                ListItem::Chat(_) => 72,
            })
            .sum::<i32>()
            + 12
    }

    fn max_scroll(&self) -> i32 {
        (self.list_height() - self.viewport().3).max(0)
    }

    fn scrollbar(&self) -> Option<Rect> {
        let Rect(x, y, w, h) = self.viewport();
        let max_scroll = self.max_scroll();
        if max_scroll == 0 {
            return None;
        }
        let track = (h - 8).max(1);
        let thumb = (track * h / self.list_height()).max(32).min(track);
        let top = (track - thumb) * self.scroll / max_scroll;
        Some(Rect(x + w - 5, y + 4 + top, 5, thumb))
    }

    fn list_geometry(&self) -> impl Iterator<Item = (ListItem, Rect)> + '_ {
        let Rect(x, y, w, _) = self.viewport();
        let mut top = y - self.scroll;
        self.list.iter().copied().map(move |item| {
            let (height, space) = match item {
                ListItem::Group(_) => (39, 0),
                ListItem::Chat(_) => (69, 3),
            };
            let rect = Rect(x, top, w - 13, height);
            top += height + space;
            (item, rect)
        })
    }

    fn action_at(&self, x: i32, y: i32) -> Option<Action> {
        if self.dialog_transition.is_some() {
            return None;
        }
        let find = |buttons: Vec<Button<'_>>| {
            buttons
                .into_iter()
                .find(|button| button.enabled && button.rect.contains(x, y))
                .map(|button| button.action)
        };
        if let Some(layout) = self.dialog_layout {
            return find(self.dialog_buttons()).or_else(|| {
                if layout.entry.is_some_and(|r| r.contains(x, y)) {
                    Some(Action::Phrase)
                } else if layout.details.is_some_and(|r| r.contains(x, y)) {
                    Some(Action::Details)
                } else {
                    None
                }
            });
        }
        let layout = self.layout();
        if layout.search.contains(x, y) {
            return Some(if !self.query.is_empty() && x >= self.width - 69 {
                Action::ClearSearch
            } else {
                Action::Search
            });
        }
        if self.scrollbar().is_some()
            && x >= layout.list.0 + layout.list.2 - 8
            && layout.list.contains(x, y)
        {
            return Some(Action::ScrollBar);
        }
        if !self.busy && layout.list.contains(x, y) {
            if let Some(index) = self.list_geometry().find_map(|(item, rect)| match item {
                ListItem::Chat(index) if rect.contains(x, y) => Some(index),
                _ => None,
            }) {
                return Some(Action::Row(index));
            }
        }
        find(self.buttons(layout))
    }

    fn paint_button(&self, canvas: &mut Canvas, button: &Button<'_>, text: &str) {
        let Button {
            action,
            rect: r,
            icon,
            style,
            selected,
            enabled,
            ..
        } = *button;
        let hover = enabled && self.hover == Some(action);
        let down = enabled && self.pressed == Some(action);
        let focus = enabled && self.keyboard && self.focus == Some(action);
        let (fill, ink, radius) = match style {
            Style::Ghost => (
                if selected || down {
                    Some(0x303030)
                } else if hover {
                    Some(0x242424)
                } else {
                    None
                },
                if !enabled {
                    TINY
                } else if hover || selected {
                    FG
                } else {
                    0xa4a4a4
                },
                10,
            ),
            Style::Nav => (
                if selected {
                    Some(0x252525)
                } else if hover {
                    Some(0x1d1d1d)
                } else {
                    None
                },
                if !enabled {
                    TINY
                } else if selected {
                    FG
                } else {
                    0xababab
                },
                9,
            ),
            Style::Window => (
                if hover {
                    Some(if action == Action::Close {
                        0x5b2929
                    } else {
                        0x2c2c2c
                    })
                } else {
                    None
                },
                MUTED,
                7,
            ),
            Style::Primary => (
                Some(if !enabled {
                    0x292929
                } else if hover {
                    0xffffff
                } else {
                    FG
                }),
                if enabled { BG } else { 0x666666 },
                10,
            ),
            Style::Danger => (
                Some(if !enabled {
                    0x262020
                } else if hover {
                    0x4b2b2b
                } else {
                    0x3b2424
                }),
                if enabled { 0xf1a0a0 } else { 0x765656 },
                10,
            ),
        };
        if fill.is_some() || focus {
            canvas.rounded(r, radius, fill, focus.then_some(0x666666));
        }
        if let Some(icon) = icon {
            let x = if text.is_empty() {
                r.0 + (r.2 - 18) / 2
            } else {
                r.0 + 13
            };
            canvas.icon(
                icon,
                Rect(x, r.1 + (r.3 - 18) / 2, 18, 18),
                if enabled { MUTED } else { TINY },
            );
        }
        if !text.is_empty() {
            let left_align = matches!(style, Style::Nav);
            let text_rect = if left_align {
                Rect(r.0 + 36, r.1, r.2 - 48, r.3)
            } else {
                r
            };
            canvas.text(
                text,
                text_rect,
                14,
                if matches!(style, Style::Primary) {
                    600
                } else {
                    400
                },
                ink,
                if left_align {
                    Align::Left
                } else {
                    Align::Center
                },
            );
        }
    }

    fn paint(&self, canvas: &mut Canvas, snapshot: bool) {
        use Align::*;
        let layout = self.layout();
        let Layout {
            w,
            h,
            search,
            list,
            actions_y,
            ..
        } = layout;
        let shell = Rect(0, 0, w, h);
        canvas.fill(shell, BG);
        canvas.fill(Rect(0, 1, 220, h - 2), SIDEBAR);
        if unsafe { IsZoomed(self.hwnd) } == 0 {
            canvas.rounded(shell, 8, None, Some(0x333333));
        } else {
            canvas.fill(Rect(0, 0, w, 1), 0x333333);
        }
        canvas.icon(Glyph::Code, Rect(25, 32, 26, 26), FG);
        canvas.text("Codex", Rect(56, 29, 145, 32), 19, 600, FG, Left);
        canvas.text("Chat Manager", Rect(27, 63, 155, 23), 11, 400, TINY, Left);
        canvas.text(
            tr("История", "History"),
            Rect(27, 115, 170, 23),
            12,
            400,
            SECTION,
            Left,
        );
        canvas.text(
            tr("История  /  Чаты", "History  /  Chats"),
            Rect(240, 13, 300, 26),
            12,
            400,
            MUTED,
            Left,
        );
        canvas.text(
            tr("История чатов", "Chat history"),
            Rect(254, 68, w - 479, 42),
            29,
            600,
            FG,
            Left,
        );
        canvas.text(
            tr(
                "Всё, над чем вы работали. Оставьте только нужное.",
                "Browse and manage your local Codex chats.",
            ),
            Rect(254, 113, w - 290, 28),
            14,
            400,
            SUBTITLE,
            Left,
        );
        canvas.rounded(search, 12, Some(EDIT), Some(0x303030));
        canvas.icon(Glyph::Search, Rect(270, 176, 19, 19), MUTED);
        if snapshot || !self.search_edit_visible() || self.query.is_empty() {
            let text = if self.query.is_empty() {
                tr("Поиск по чатам", "Search chats")
            } else {
                &self.query
            };
            canvas.text(
                text,
                Rect(301, 174, w - 370, 22),
                14,
                400,
                if self.query.is_empty() { MUTED } else { FG },
                Left,
            );
        }
        if !self.query.is_empty() {
            canvas.icon(
                Glyph::Close,
                Rect(w - 64, 177, 16, 16),
                if self.hover == Some(Action::ClearSearch) {
                    FG
                } else {
                    MUTED
                },
            );
        }
        let count = if self.loaded {
            format!("{}  ·  {}", self.scope.count_label(), self.visible.len())
        } else {
            tr("Загрузка…", "Loading…").to_owned()
        };
        canvas.text(&count, Rect(254, 233, 330, 28), 12, 400, MUTED, Left);
        let clip = canvas.physical(list);
        unsafe {
            SaveDC(canvas.dc);
            IntersectClipRect(canvas.dc, clip.left, clip.top, clip.right, clip.bottom);
        }
        if self.loaded && self.visible.is_empty() {
            canvas.text(
                if self.query.trim().is_empty() {
                    tr("Здесь пока нет чатов", "No chats yet")
                } else {
                    tr("Ничего не найдено", "No matching chats")
                },
                Rect(list.0, list.1 + list.3 / 2 - 10, list.2, 20),
                14,
                400,
                SUBTITLE,
                Center,
            );
        }
        let today = now_local().date();
        for (item, r) in self.list_geometry().filter(|(_, r)| r.visible_in(list)) {
            match item {
                ListItem::Group(name) => canvas.text(
                    name,
                    Rect(r.0 + 13, r.1 + 11, r.2 - 13, 21),
                    12,
                    400,
                    SECTION,
                    Left,
                ),
                ListItem::Chat(index) => {
                    let row = &self.rows[index];
                    let selected = self.checked.contains(&row.id);
                    if selected || self.hover == Some(Action::Row(index)) {
                        canvas.rounded(
                            r,
                            12,
                            Some(if selected { 0x242424 } else { 0x202020 }),
                            None,
                        );
                    }
                    let check = Rect(r.0 + 16, r.1 + 25, 18, 18);
                    canvas.rounded(
                        check,
                        5,
                        selected.then_some(0xe7e7e7),
                        Some(if selected { 0xd9d9d9 } else { 0x535353 }),
                    );
                    if selected {
                        canvas.check_mark(check.0, check.1);
                    }
                    if self.focus == Some(Action::Row(index)) && self.keyboard {
                        canvas.rounded(
                            Rect(check.0 - 2, check.1 - 2, 22, 22),
                            7,
                            None,
                            Some(0xa0a0a0),
                        );
                    }
                    canvas.text(
                        &row.title(),
                        Rect(r.0 + 51, r.1 + 8, r.2 - 149, 26),
                        15,
                        400,
                        FG,
                        Left,
                    );
                    let folder = row
                        .cwd
                        .as_deref()
                        .and_then(|cwd| Path::new(cwd).file_name())
                        .map(|s| s.to_string_lossy())
                        .unwrap_or_default();
                    let mut caption = if row.archived {
                        tr("Архив", "Archived").to_owned()
                    } else {
                        tr("Чат", "Chat").to_owned()
                    };
                    if !folder.is_empty() {
                        caption.push_str(&format!("  ·  {folder}"));
                    }
                    canvas.text(
                        &caption,
                        Rect(r.0 + 51, r.1 + 37, r.2 - 149, 22),
                        12,
                        400,
                        MUTED,
                        Left,
                    );
                    let date = from_unix_local(row.updated_at.unwrap_or(0))
                        .map(|timestamp| {
                            if timestamp.date() == today {
                                timestamp.format("%H:%M").to_string()
                            } else if Language::current() == Language::Russian {
                                let months = [
                                    "янв", "фев", "мар", "апр", "май", "июн", "июл", "авг", "сен",
                                    "окт", "ноя", "дек",
                                ];
                                format!(
                                    "{:02} {}",
                                    timestamp.day(),
                                    months[timestamp.month0() as usize]
                                )
                            } else {
                                timestamp.format("%d %b").to_string()
                            }
                        })
                        .unwrap_or_else(|| "—".to_owned());
                    canvas.text(
                        &date,
                        Rect(r.0 + r.2 - 91, r.1, 75, r.3),
                        12,
                        400,
                        MUTED,
                        Right,
                    );
                }
            }
        }
        if let Some(thumb) = self.scrollbar() {
            canvas.rounded(
                thumb,
                2,
                Some(if self.hover == Some(Action::ScrollBar) {
                    0x555555
                } else {
                    0x373737
                }),
                None,
            );
        }
        unsafe {
            RestoreDC(canvas.dc, -1);
        }
        canvas.fill(Rect(254, h - 133, w - 290, 1), 0x2a2a2a);
        let selection = if self.checked.is_empty() {
            tr("Выберите чаты", "Select chats").to_owned()
        } else {
            trf!("Выбрано: {}", "Selected: {}", self.checked.len())
        };
        canvas.text(
            &selection,
            Rect(254, actions_y, w - 697, 39),
            12,
            400,
            MUTED,
            Left,
        );
        canvas.text(
            &self.status.text(),
            Rect(254, h - 65, w - 290, 21),
            11,
            400,
            TINY,
            Left,
        );
        canvas.text(
            if self.demo {
                tr("Демо · вымышленные чаты", "Demo · sample chats")
            } else {
                tr(
                    "Перед удалением закройте Codex. Удаление необратимо.",
                    "Close Codex before deleting chats. Deletion is permanent.",
                )
            },
            Rect(254, h - 38, w - 290, 22),
            11,
            400,
            TINY,
            Left,
        );
        for button in self.buttons(layout) {
            if let Action::Scope(scope) = button.action {
                if self.loaded {
                    let n = self
                        .rows
                        .iter()
                        .filter(|row| !row.auxiliary() && scope.filter(row))
                        .count();
                    self.paint_button(canvas, &button, &format!("{}    {n}", button.text));
                    continue;
                }
            }
            self.paint_button(canvas, &button, button.text);
        }
        if let (Some(dialog), Some(dl)) = (&self.dialog, self.dialog_layout) {
            if let Some(transition) = self.dialog_transition.as_ref().filter(|_| !snapshot) {
                let (opacity, _) = transition.frame(Instant::now());
                canvas.faded(dl.shell, opacity, |canvas| {
                    self.paint_dialog(canvas, dialog, dl, true);
                });
            } else {
                self.paint_dialog(canvas, dialog, dl, snapshot);
            }
        } else {
            self.paint_tooltip(canvas);
        }
    }

    fn paint_dialog(&self, canvas: &mut Canvas, dialog: &Dialog, dl: DialogLayout, snapshot: bool) {
        let Rect(x, y, w, _) = dl.shell;
        canvas.rounded(dl.shell, 18, Some(PANEL), Some(0x3a3a3a));
        canvas.text(
            &dialog.title,
            Rect(x + 29, y + 25, w - 101, 36),
            23,
            600,
            FG,
            Align::Left,
        );
        canvas.wrapped(&dialog.body, dl.body, 14, SUBTITLE, false);
        if snapshot {
            if let Some(r) = dl.details {
                canvas.paint_control(self.details, r);
            }
        }
        if let (Some(label), Some(entry)) = (dl.phrase, dl.entry) {
            canvas.text(
                tr(
                    "Для подтверждения введите УДАЛИТЬ",
                    "Type DELETE to confirm",
                ),
                label,
                12,
                400,
                MUTED,
                Align::Left,
            );
            canvas.rounded(
                entry,
                10,
                Some(EDIT),
                Some(if self.focus == Some(Action::Phrase) {
                    0x737373
                } else {
                    0x383838
                }),
            );
            if snapshot || self.phrase.is_empty() {
                canvas.text(
                    if self.phrase.is_empty() {
                        delete_word()
                    } else {
                        &self.phrase
                    },
                    Rect(entry.0 + 16, entry.1, entry.2 - 32, entry.3),
                    14,
                    400,
                    if self.phrase.is_empty() { MUTED } else { FG },
                    Align::Left,
                );
            }
        }
        for button in self.dialog_buttons() {
            self.paint_button(canvas, &button, button.text);
        }
    }

    fn paint_tooltip(&self, canvas: &mut Canvas) {
        let (Some(action), Some((x, y))) = (self.hover, self.cursor) else {
            return;
        };
        if self.hover_at.elapsed() < Duration::from_millis(650)
            || self.hover_at.elapsed() > Duration::from_secs(8)
        {
            return;
        }
        let value = match action {
            Action::Refresh => tr("Обновить список", "Refresh chats").to_owned(),
            Action::Auxiliary => tr(
                "Показать подзадачи и служебные чаты",
                "Show subtasks and auxiliary chats",
            )
            .to_owned(),
            Action::Row(index) => {
                let Some(row) = self.rows.get(index) else {
                    return;
                };
                format!("{}\n{}", row.id, row.cwd.as_deref().unwrap_or_default())
            }
            _ => return,
        };
        let h = if value.contains('\n') { 44 } else { 28 };
        let width = (value
            .lines()
            .map(|line| line.chars().count())
            .max()
            .unwrap_or(0) as i32
            * 7
            + 16)
            .clamp(108, self.width - 16);
        let rect = Rect(
            (x + 8).min(self.width - width - 8),
            (y + 20).min(self.height - h - 8),
            width,
            h,
        );
        canvas.rounded(rect, 6, Some(0x303030), Some(0x444444));
        canvas.wrapped(
            &value,
            Rect(rect.0 + 7, rect.1 + 5, width - 14, h - 8),
            12,
            FG,
            false,
        );
    }
}

impl App {
    fn measure_dialog(&mut self) {
        let Some(dialog) = &self.dialog else {
            self.dialog_layout = None;
            return;
        };
        let dc = unsafe { GetDC(self.hwnd) };
        let mut canvas = Canvas::new(dc, 1, 1, self.dpi);
        let body_h = canvas.wrapped(&dialog.body, Rect(0, 0, 472, 0), 14, SUBTITLE, true);
        let show_details = !dialog.details.is_empty();
        let detail_h = if show_details {
            let lines = dialog.details.lines().count() as i32;
            let measured = canvas.wrapped(&dialog.details, Rect(0, 0, 460, 0), 12, MUTED, true);
            (32 + lines * 22).min(180).max(measured + 8).min(210)
        } else {
            0
        };
        drop(canvas);
        unsafe {
            ReleaseDC(self.hwnd, dc);
        }
        let connection = matches!(dialog.prompt, Some(Prompt::Settings));
        let cleanup = matches!(dialog.prompt, Some(Prompt::Clean(_)));
        let phrase = dialog.phrase();
        let body_bottom = 75 + body_h;
        let details_top = body_bottom + 22;
        let details_end = if show_details {
            details_top + detail_h
        } else {
            body_bottom
        };
        let phrase_y = details_end + 12;
        let entry_y = phrase_y + 32;
        let connect_y = details_end + 12;
        let buttons_y = if phrase {
            entry_y + 63
        } else if connection {
            connect_y + 52
        } else {
            (details_end + 17).max(135)
        };
        let w = 530;
        let h = (buttons_y + 65).max(200);
        let x = (self.width - w) / 2;
        let y = (self.height - h) / 2;
        let confirm_w = if phrase { 141 } else { 101 };
        self.dialog_layout = Some(DialogLayout {
            shell: Rect(x, y, w, h),
            body: Rect(x + 29, y + 75, 472, body_h + 2),
            details: show_details.then_some(Rect(x + 29, y + details_top, 472, detail_h)),
            phrase: phrase.then_some(Rect(x + 29, y + phrase_y, 472, 18)),
            entry: phrase.then_some(Rect(x + 29, y + entry_y, 472, 47)),
            connection: connection.then_some((
                Rect(x + 29, y + connect_y, 125, 39),
                Rect(x + 157, y + connect_y, 139, 39),
            )),
            close: Rect(x + w - 59, y + 27, 30, 30),
            cancel: cleanup.then_some(Rect(x + w - 29 - confirm_w - 104, y + buttons_y, 94, 39)),
            confirm: Rect(x + w - 29 - confirm_w, y + buttons_y, confirm_w, 39),
        });
    }

    fn open_dialog(&mut self, dialog: Dialog) {
        self.dialog_transition = dialog_animations_enabled().then(|| DialogTransition {
            fade: Animation {
                start: Instant::now(),
                from: 0,
                closing: false,
            },
            dismiss: None,
        });
        self.update_dialog(dialog);
    }

    fn update_dialog(&mut self, dialog: Dialog) {
        let has_phrase = dialog.phrase();
        self.pressed = None;
        self.hover = None;
        self.phrase.clear();
        unsafe {
            SetWindowTextW(self.entry, wide("").as_ptr());
            SetWindowTextW(
                self.details,
                wide(dialog.details.replace('\n', "\r\n")).as_ptr(),
            );
        }
        self.dialog = Some(dialog);
        self.measure_dialog();
        self.place_edits();
        self.set_focus(
            if has_phrase {
                Some(Action::Phrase)
            } else {
                None
            },
            false,
        );
        self.redraw();
    }

    fn dismiss_dialog(&mut self, action: DismissDialog) {
        if self.dialog.is_none()
            || self
                .dialog_transition
                .as_ref()
                .is_some_and(|transition| transition.dismiss.is_some())
        {
            return;
        }
        let now = Instant::now();
        let from = self
            .dialog_transition
            .as_ref()
            .map_or(255, |transition| transition.frame(now).0);
        if from == 0 || !dialog_animations_enabled() {
            self.finish_dialog(action);
            return;
        }
        self.dialog_transition = Some(DialogTransition {
            fade: Animation {
                start: now,
                from,
                closing: true,
            },
            dismiss: Some(action),
        });
        self.pressed = None;
        self.hover = None;
        self.place_edits();
        unsafe { SetFocus(self.hwnd) };
        self.redraw();
    }

    fn finish_dialog(&mut self, action: DismissDialog) {
        let prompt = self.dialog.take().and_then(|dialog| dialog.prompt);
        self.dialog_layout = None;
        self.dialog_transition = None;
        self.hover = None;
        self.pressed = None;
        self.place_edits();
        self.set_focus(None, false);
        match action {
            DismissDialog::Cancel => {}
            DismissDialog::Confirm => match prompt {
                Some(Prompt::Delete(plan)) => {
                    self.status = Status::Deleting;
                    self.start(Work::Delete(plan));
                }
                Some(Prompt::Clean(ids)) => self.start(Work::Clean(ids)),
                _ => {}
            },
            DismissDialog::Find => {
                if !self.demo {
                    self.start(Work::Load(Some(None)));
                }
            }
            DismissDialog::Choose => self.choose_cli(),
        }
    }

    fn search_edit_visible(&self) -> bool {
        self.dialog_layout.is_none()
    }

    fn position_edit(&self, hwnd: HWND, rect: Option<Rect>, visible: bool) {
        if let Some(Rect(x, y, w, h)) = rect {
            let (x, y) = (self.px(x), self.px(y));
            unsafe {
                SetWindowPos(
                    hwnd,
                    null_mut(),
                    x,
                    y,
                    self.px(w),
                    self.px(h),
                    SWP_NOACTIVATE
                        | SWP_NOZORDER
                        | if visible {
                            SWP_SHOWWINDOW
                        } else {
                            SWP_HIDEWINDOW
                        },
                );
            }
        } else {
            unsafe {
                ShowWindow(hwnd, SW_HIDE);
            }
        }
    }

    fn place_edits(&self) {
        if self.search.is_null() {
            return;
        }
        // The native edit is above the canvas, so hide it when the canvas
        // draws a dialog over the search field.
        let search = self.search_edit_visible().then(|| self.layout().edit);
        self.position_edit(self.search, search, true);
        let dl = self.dialog_layout;
        // The dialog's controls take part in the fade as part of the painted frame.
        let visible = self.dialog_transition.is_none();
        self.position_edit(
            self.entry,
            dl.and_then(|d| d.entry)
                .map(|r| Rect(r.0 + 16, r.1 + 15, r.2 - 32, 19)),
            visible,
        );
        self.position_edit(self.details, dl.and_then(|d| d.details), visible);
    }

    fn translate_control(&mut self, hwnd: HWND) {
        if hwnd == self.search {
            self.query = edit_text(hwnd);
            self.rebuild();
            self.place_edits();
        } else if hwnd == self.entry {
            self.phrase = edit_text(hwnd);
            self.redraw();
        }
    }

    fn set_focus(&mut self, focus: Option<Action>, keyboard: bool) {
        self.focus = focus;
        self.keyboard = keyboard;
        let target = match (focus, self.dialog_transition.is_none()) {
            (Some(Action::Search), true) => self.search,
            (Some(Action::Phrase), true) => self.entry,
            (Some(Action::Details), true) => self.details,
            _ => self.hwnd,
        };
        unsafe {
            SetFocus(target);
        }
        if let Some(Action::Row(index)) = focus {
            let view = self.viewport();
            let target = self.list_geometry().find_map(|(item, r)| {
                matches!(item, ListItem::Chat(row) if row == index).then_some(r)
            });
            if let Some(r) = target {
                if r.1 < view.1 {
                    self.scroll += r.1 - view.1;
                }
                if r.1 + r.3 > view.1 + view.3 {
                    self.scroll += r.1 + r.3 - view.1 - view.3;
                }
                self.scroll = self.scroll.clamp(0, self.max_scroll());
            }
        }
        self.redraw();
    }

    fn tab(&mut self, backward: bool) {
        let mut targets = Vec::new();
        if let Some(layout) = self.dialog_layout {
            if layout.entry.is_some() {
                targets.push(Action::Phrase);
            }
            if layout.details.is_some() {
                targets.push(Action::Details);
            }
            for button in self.dialog_buttons().into_iter().filter(|b| b.enabled) {
                if !targets.contains(&button.action) {
                    targets.push(button.action);
                }
            }
        } else {
            for button in self
                .buttons(self.layout())
                .into_iter()
                .filter(|b| b.enabled)
            {
                if button.action == Action::Auxiliary {
                    targets.push(Action::Search);
                }
                if button.action == Action::Select && !self.busy {
                    targets.extend(self.visible.iter().copied().map(Action::Row));
                }
                targets.push(button.action);
            }
        }
        if targets.is_empty() {
            return;
        }
        let index = self
            .focus
            .and_then(|focus| targets.iter().position(|action| *action == focus));
        let len = targets.len();
        let next = match (index, backward) {
            (None, false) => 0,
            (None, true) => len - 1,
            (Some(i), false) => (i + 1) % len,
            (Some(i), true) => (i + len - 1) % len,
        };
        self.set_focus(Some(targets[next]), true);
    }

    fn choose_cli(&mut self) {
        if self.demo || self.busy {
            return;
        }
        let mut name = vec![0u16; 32768];
        let title = wide(tr("Выберите Codex CLI", "Choose Codex CLI"));
        let filter = wide("Codex CLI (codex.exe)\0codex.exe\0");
        let mut dialog: OPENFILENAMEW = unsafe { zeroed() };
        dialog.lStructSize = size_of::<OPENFILENAMEW>() as u32;
        dialog.hwndOwner = self.hwnd;
        dialog.lpstrFilter = filter.as_ptr();
        dialog.lpstrTitle = title.as_ptr();
        dialog.lpstrFile = name.as_mut_ptr();
        dialog.nMaxFile = name.len() as u32;
        dialog.Flags = OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_EXPLORER | OFN_NOCHANGEDIR;
        if unsafe { GetOpenFileNameW(&mut dialog) } != 0 {
            use std::os::windows::ffi::OsStringExt;
            let end = name.iter().position(|c| *c == 0).unwrap_or(name.len());
            let path = PathBuf::from(std::ffi::OsString::from_wide(&name[..end]));
            self.status = Status::Loading;
            self.start(Work::Load(Some(Some(path))));
        }
    }

    fn activate(&mut self, action: Action) {
        match action {
            Action::Scope(scope) => {
                self.scope = scope;
                self.rebuild();
            }
            Action::Language(language) if !self.busy && language != Language::current() => {
                self.change_language(language);
            }
            Action::Settings => {
                let details = self
                    .executable
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|| {
                        tr(
                            "Codex находится автоматически при обновлении списка.",
                            "The app detects Codex when it refreshes the chat list.",
                        )
                        .to_owned()
                    });
                self.open_dialog(
                    Dialog::new(
                        tr("Подключение", "Connection"),
                        tr(
                            "Локальная история Codex на этом компьютере.",
                            "Local Codex history on this computer.",
                        ),
                    )
                    .with_details(details)
                    .with_prompt(Prompt::Settings),
                );
            }
            Action::Repair if !self.busy => {
                let status = if self.demo {
                    tr(
                        "В демо показаны вымышленные чаты.",
                        "Demo mode shows sample chats.",
                    )
                } else {
                    tr("Проверяем список…", "Checking for stale links…")
                };
                self.open_dialog(cleanup_dialog(status).with_prompt(Prompt::Clean(HashSet::new())));
                if !self.demo {
                    self.start(Work::Scan);
                }
            }
            Action::CancelDialog => self.dismiss_dialog(DismissDialog::Cancel),
            Action::Confirm => {
                if self
                    .dialog
                    .as_ref()
                    .is_some_and(|dialog| dialog.can_confirm(&self.phrase))
                {
                    self.dismiss_dialog(DismissDialog::Confirm);
                }
            }
            Action::Find => self.dismiss_dialog(DismissDialog::Find),
            Action::Choose => self.dismiss_dialog(DismissDialog::Choose),
            Action::Minimize => unsafe {
                PostMessageW(self.hwnd, WM_SYSCOMMAND, SC_MINIMIZE as usize, 0);
            },
            Action::Maximize => unsafe {
                PostMessageW(
                    self.hwnd,
                    WM_SYSCOMMAND,
                    if IsZoomed(self.hwnd) == 0 {
                        SC_MAXIMIZE
                    } else {
                        SC_RESTORE
                    } as usize,
                    0,
                );
            },
            Action::Close => self.request_close(),
            Action::Refresh => self.refresh(),
            Action::Search | Action::Phrase | Action::Details => {
                self.set_focus(Some(action), false)
            }
            Action::ClearSearch => {
                unsafe {
                    SetWindowTextW(self.search, wide("").as_ptr());
                }
                self.translate_control(self.search);
                self.set_focus(Some(Action::Search), false);
            }
            Action::Auxiliary => {
                self.auxiliary = !self.auxiliary;
                self.rebuild();
            }
            Action::Row(index) if !self.busy => {
                if let Some(row) = self.rows.get(index) {
                    if !self.checked.remove(&row.id) {
                        self.checked.insert(row.id.clone());
                    }
                }
            }
            Action::Select if !self.busy => self
                .checked
                .extend(self.visible.iter().map(|i| self.rows[*i].id.clone())),
            Action::Clear if !self.busy => self.checked.clear(),
            Action::Delete if !self.busy => self.prepare(false),
            Action::DeleteAll if !self.busy => self.prepare(true),
            _ => {}
        }
        self.redraw();
    }

    fn key(&mut self, hwnd: HWND, key: WPARAM) -> bool {
        let key = key as u16;
        if key == VK_ESCAPE && self.dialog.is_some() {
            self.dismiss_dialog(DismissDialog::Cancel);
            return true;
        }
        if self.dialog_transition.is_some() {
            return true;
        }
        let control = unsafe { GetKeyState(VK_CONTROL as i32) < 0 };
        if key == VK_TAB {
            self.tab(unsafe { GetKeyState(VK_SHIFT as i32) < 0 });
            return true;
        }
        if key == b'F' as u16 && control && self.dialog.is_none() {
            self.set_focus(Some(Action::Search), true);
            return true;
        }
        if key == b'A' as u16 && control && hwnd != self.hwnd {
            unsafe {
                SendMessageW(hwnd, EM_SETSEL, 0, -1);
            }
            return true;
        }
        if key == VK_RETURN && self.dialog.is_some() {
            match self.focus {
                Some(action @ (Action::Find | Action::Choose | Action::CancelDialog)) => {
                    self.activate(action)
                }
                _ => self.activate(Action::Confirm),
            }
            return true;
        }
        if hwnd == self.hwnd && matches!(key, VK_SPACE | VK_RETURN) {
            if let Some(action) = self.focus {
                self.activate(action);
            }
            return true;
        }
        if hwnd == self.hwnd
            && self.dialog.is_none()
            && matches!(key, VK_UP | VK_DOWN | VK_PRIOR | VK_NEXT | VK_HOME | VK_END)
        {
            let delta = match key {
                VK_UP => -72,
                VK_DOWN => 72,
                VK_PRIOR => -self.viewport().3,
                VK_NEXT => self.viewport().3,
                VK_HOME => -self.max_scroll(),
                _ => self.max_scroll(),
            };
            self.shift_scroll(delta);
            return true;
        }
        false
    }

    fn shift_scroll(&mut self, delta: i32) {
        self.scroll = (self.scroll + delta).clamp(0, self.max_scroll());
        self.hover = self.cursor.and_then(|(x, y)| self.action_at(x, y));
        self.hover_at = Instant::now();
        self.redraw();
    }

    fn change_language(&mut self, language: Language) {
        language.set_current();
        self.update_language_controls();
        self.rebuild();
        if let Err(error) = language.save() {
            self.open_dialog(Dialog::new(
                tr("Не удалось сохранить язык", "Could not save language"),
                error.to_string(),
            ));
        }
    }

    fn update_language_controls(&self) {
        unsafe {
            SendMessageW(
                self.search,
                EM_SETCUEBANNER,
                1,
                wide(tr("Поиск по чатам", "Search chats")).as_ptr() as LPARAM,
            );
            SendMessageW(
                self.entry,
                EM_SETCUEBANNER,
                1,
                wide(delete_word()).as_ptr() as LPARAM,
            );
            if self.demo {
                SetWindowTextW(self.hwnd, wide(demo_title()).as_ptr());
            }
        }
    }

    fn mouse_move(&mut self, x: i32, y: i32) {
        self.cursor = Some((x, y));
        let hover = self.action_at(x, y);
        if hover != self.hover {
            self.hover = hover;
            self.hover_at = Instant::now();
            self.redraw();
        }
        if let (Some((start, original)), Some(thumb)) = (self.drag, self.scrollbar()) {
            let travel = (self.viewport().3 - 8 - thumb.3).max(1);
            let scroll =
                (original + (y - start) * self.max_scroll() / travel).clamp(0, self.max_scroll());
            if scroll != self.scroll {
                self.scroll = scroll;
                self.redraw();
            }
        }
        unsafe {
            let mut track = TRACKMOUSEEVENT {
                cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: self.hwnd,
                dwHoverTime: 0,
            };
            TrackMouseEvent(&mut track);
        }
    }

    fn mouse_down(&mut self, x: i32, y: i32) {
        let action = self.action_at(x, y);
        self.pressed = action;
        if let Some(action) = action {
            match action {
                Action::ScrollBar => {
                    if let Some(thumb) = self.scrollbar() {
                        if !thumb.contains(x, y) {
                            self.shift_scroll(if y < thumb.1 {
                                -self.viewport().3
                            } else {
                                self.viewport().3
                            });
                        } else {
                            self.drag = Some((y, self.scroll));
                        }
                    }
                }
                _ => self.set_focus(Some(action), false),
            }
            unsafe {
                SetCapture(self.hwnd);
            }
        }
        self.redraw();
    }

    fn mouse_up(&mut self, x: i32, y: i32) {
        let action = self.action_at(x, y);
        let pressed = self.pressed.take();
        self.drag = None;
        if !unsafe { GetCapture() }.is_null() {
            unsafe {
                ReleaseCapture();
            }
        }
        if let Some(action) = action.filter(|a| Some(*a) == pressed) {
            self.activate(action);
        }
        self.redraw();
    }

    fn request_close(&mut self) {
        if self.animation.as_ref().is_some_and(|anim| anim.closing) {
            return;
        }
        if self.dialog.is_some() {
            self.dismiss_dialog(DismissDialog::Cancel);
            return;
        }
        if self.busy {
            self.open_dialog(Dialog::new(
                tr("Операция выполняется", "Operation in progress"),
                tr(
                    "Дождитесь завершения, чтобы увидеть результат.",
                    "Wait for the operation to finish to see the result.",
                ),
            ));
            return;
        }
        unsafe {
            let style = GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE);
            if style & WS_EX_LAYERED as isize == 0 {
                SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, style | WS_EX_LAYERED as isize);
                SetLayeredWindowAttributes(self.hwnd, 0, self.alpha, LWA_ALPHA);
            }
        }
        self.animation = Some(Animation {
            start: Instant::now(),
            from: self.alpha,
            closing: true,
        });
        unsafe {
            EnableWindow(self.hwnd, 0);
        }
    }

    fn finish_opening(&mut self) {
        if !self
            .animation
            .as_ref()
            .is_some_and(|animation| !animation.closing)
        {
            return;
        }
        self.animation = None;
        self.alpha = 255;
        unsafe {
            // Normal top-level windows get the shell's minimize and restore transitions.
            let style = GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE);
            SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, style & !(WS_EX_LAYERED as isize));
            RedrawWindow(
                self.hwnd,
                null(),
                null_mut(),
                RDW_INVALIDATE | RDW_FRAME | RDW_ALLCHILDREN,
            );
        }
    }

    fn tick(&mut self) -> bool {
        let now = Instant::now();
        if let Some(animation) = &self.animation {
            let duration = if animation.closing { FADE_OUT } else { FADE_IN };
            let (alpha, done) = animation.frame(now, duration);
            if self.alpha != alpha {
                self.alpha = alpha;
                unsafe {
                    SetLayeredWindowAttributes(self.hwnd, 0, alpha, LWA_ALPHA);
                }
            }
            if done {
                if animation.closing {
                    return true;
                }
                self.finish_opening();
            }
        }
        if let Some(transition) = &self.dialog_transition {
            let (_, done) = transition.frame(now);
            self.redraw();
            match (done, transition.dismiss) {
                (true, Some(action)) => self.finish_dialog(action),
                (true, None) => {
                    self.dialog_transition = None;
                    self.place_edits();
                    self.set_focus(self.focus, self.keyboard);
                }
                (false, Some(_)) => return false,
                (false, None) => {}
            }
        }
        while let Ok(event) = self.worker.rx.try_recv() {
            self.event(event);
        }
        if self.hover.is_some() {
            let age = self.hover_at.elapsed().as_millis();
            if (650..=693).contains(&age) || (8000..=8043).contains(&age) {
                self.redraw();
            }
        }
        if self.smoke && !self.smoked && self.loaded && !self.busy && self.animation.is_none() {
            self.smoked = true;
            if let Err(error) = self.verify_demo() {
                self.smoke_error = Some(error.to_string());
            }
            self.finish_dialog(DismissDialog::Cancel);
            self.request_close();
        }
        false
    }
}

fn dialog_animations_enabled() -> bool {
    let mut enabled: BOOL = 1;
    unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            (&mut enabled as *mut BOOL).cast(),
            0,
        );
    }
    enabled != 0
}

fn edit_text(hwnd: HWND) -> String {
    let mut value = vec![0; unsafe { GetWindowTextLengthW(hwnd) }.max(0) as usize + 1];
    let count = unsafe { GetWindowTextW(hwnd, value.as_mut_ptr(), value.len() as i32) };
    String::from_utf16_lossy(&value[..count.max(0) as usize])
}

fn screen_rect(monitor: HMONITOR) -> MONITORINFO {
    let mut info: MONITORINFO = unsafe { zeroed() };
    info.cbSize = size_of::<MONITORINFO>() as u32;
    unsafe {
        GetMonitorInfoW(monitor, &mut info);
    }
    info
}

fn configure_frame(hwnd: HWND) {
    for (attribute, value) in [
        (DWMWA_USE_IMMERSIVE_DARK_MODE, 1),
        (DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND as u32),
        (DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE),
    ] {
        unsafe {
            DwmSetWindowAttribute(
                hwnd,
                attribute as u32,
                (&value as *const u32).cast(),
                size_of::<u32>() as u32,
            );
        }
    }
    unsafe {
        SetWindowPos(
            hwnd,
            null_mut(),
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

impl App {
    fn initialize(&mut self, hwnd: HWND) -> Result<()> {
        self.hwnd = hwnd;
        configure_frame(hwnd);
        self.dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
        let edit = wide("EDIT");
        let instance = unsafe { GetModuleHandleW(null()) };
        let create = |style| unsafe {
            CreateWindowExW(
                0,
                edit.as_ptr(),
                null(),
                WS_CHILD | style,
                0,
                0,
                1,
                1,
                hwnd,
                null_mut(),
                instance,
                null(),
            )
        };
        self.search = create(ES_AUTOHSCROLL as u32);
        self.entry = create(ES_AUTOHSCROLL as u32);
        self.details = create((ES_MULTILINE | ES_READONLY | ES_AUTOVSCROLL) as u32);
        if [self.search, self.entry, self.details]
            .into_iter()
            .any(|control| control.is_null())
        {
            return Err(std::io::Error::last_os_error().into());
        }
        self.update_language_controls();
        unsafe {
            for child in [self.search, self.entry, self.details] {
                SendMessageW(child, EM_SETMARGINS, 3, 0);
            }
        }
        self.update_fonts();
        self.resize();
        self.alpha = 0;
        self.animation = Some(Animation {
            start: Instant::now(),
            from: 0,
            closing: false,
        });
        if unsafe { SetLayeredWindowAttributes(hwnd, 0, 0, LWA_ALPHA) } == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        unsafe {
            SetTimer(hwnd, 1, 15, None);
        }
        self.refresh();
        Ok(())
    }

    fn update_fonts(&mut self) {
        let edit_font = drawing::font(14, 400, self.dpi);
        let details_font = drawing::font(12, 400, self.dpi);
        unsafe {
            SendMessageW(self.search, WM_SETFONT, edit_font as usize, 1);
            SendMessageW(self.entry, WM_SETFONT, edit_font as usize, 1);
            SendMessageW(self.details, WM_SETFONT, details_font as usize, 1);
            DeleteObject(self.edit_font);
            DeleteObject(self.details_font);
        }
        self.edit_font = edit_font;
        self.details_font = details_font;
    }

    fn resize(&mut self) {
        if self.hwnd.is_null() || unsafe { IsIconic(self.hwnd) } != 0 {
            return;
        }
        let mut size: RECT = unsafe { zeroed() };
        unsafe {
            GetClientRect(self.hwnd, &mut size);
        }
        self.width = self.logical(size.right);
        self.height = self.logical(size.bottom);
        self.measure_dialog();
        self.scroll = self.scroll.min(self.max_scroll());
        self.place_edits();
        self.redraw();
    }

    fn nc_hit_test(&self, x: i32, y: i32) -> LRESULT {
        if self.dialog.is_some() || self.animation.as_ref().is_some_and(|a| a.closing) {
            return HTCLIENT as LRESULT;
        }
        if unsafe { IsZoomed(self.hwnd) } == 0 {
            let left = x < 7;
            let right = x >= self.width - 7;
            let top = y < 7;
            let bottom = y >= self.height - 7;
            let hit = match (left, right, top, bottom) {
                (true, _, true, _) => HTTOPLEFT,
                (_, true, true, _) => HTTOPRIGHT,
                (true, _, _, true) => HTBOTTOMLEFT,
                (_, true, _, true) => HTBOTTOMRIGHT,
                (true, _, _, _) => HTLEFT,
                (_, true, _, _) => HTRIGHT,
                (_, _, true, _) => HTTOP,
                (_, _, _, true) => HTBOTTOM,
                _ => HTCLIENT,
            };
            if hit != HTCLIENT {
                return hit as LRESULT;
            }
        }
        if (220..self.width - 130).contains(&x) && y < 49 {
            HTCAPTION as LRESULT
        } else {
            HTCLIENT as LRESULT
        }
    }

    fn minmax(&self, hwnd: HWND, info: &mut MINMAXINFO) {
        let dpi = unsafe { GetDpiForWindow(hwnd) }.max(self.dpi);
        let monitor = screen_rect(unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) });
        let monitor_bounds = monitor.rcMonitor;
        let work = monitor.rcWork;
        info.ptMaxPosition = POINT {
            x: work.left - monitor_bounds.left,
            y: work.top - monitor_bounds.top,
        };
        info.ptMaxSize = POINT {
            x: work.right - work.left,
            y: work.bottom - work.top,
        };
        info.ptMinTrackSize = POINT {
            x: 960 * dpi as i32 / 96,
            y: 660 * dpi as i32 / 96,
        };
    }

    fn draw_window(&self) {
        unsafe {
            let mut paint: PAINTSTRUCT = zeroed();
            let target = BeginPaint(self.hwnd, &mut paint);
            self.draw_to(target, false);
            EndPaint(self.hwnd, &paint);
        }
    }

    fn draw_to(&self, target: HDC, snapshot: bool) {
        if target.is_null() || self.search.is_null() {
            return;
        }
        let (width, height) = if snapshot {
            (self.px(self.width), self.px(self.height))
        } else {
            let mut client: RECT = unsafe { zeroed() };
            unsafe {
                GetClientRect(self.hwnd, &mut client);
            }
            (client.right, client.bottom)
        };
        let mut canvas = Canvas::new(target, width, height, self.dpi);
        self.paint(&mut canvas, snapshot);
        canvas.present(target, width, height);
    }

    fn snapshot(&self, path: &Path, area: Rect) -> Result<()> {
        let dc = unsafe { GetDC(self.hwnd) };
        let mut canvas = Canvas::new(dc, self.px(self.width), self.px(self.height), self.dpi);
        self.paint(&mut canvas, true);
        let result = canvas.save_png(path, area);
        drop(canvas);
        unsafe {
            ReleaseDC(self.hwnd, dc);
        }
        result
    }

    fn verify_demo(&mut self) -> Result<()> {
        let verify = |ok, name| {
            if ok {
                Ok(())
            } else {
                Err(err(format!("Демо-проверка: {name}")))
            }
        };
        verify(
            self.rows.len() == 8 && self.visible.len() == 8,
            "список чатов",
        )?;
        verify(self.alpha == 255, "конечная непрозрачность при запуске")?;
        self.activate(Action::Row(0));
        self.activate(Action::Row(1));
        verify(self.checked.len() == 2, "выбор чатов")?;
        let snapshot = std::env::var_os("CODEX_MANAGER_PREVIEW_PATH").map(PathBuf::from);
        if let Some(path) = &snapshot {
            self.snapshot(path, Rect(0, 0, self.width, self.height))?;
        }
        self.activate(Action::Scope(Scope::Archive));
        verify(
            self.visible.len() == 3 && self.checked.len() == 2,
            "архив и сохранение выбора",
        )?;
        self.activate(Action::Scope(Scope::All));
        self.activate(Action::Select);
        unsafe {
            SetWindowTextW(self.search, wide(tr("поиск", "search")).as_ptr());
        }
        self.translate_control(self.search);
        verify(
            self.visible.len() == 1 && self.checked.len() == 8,
            "поиск и выбор видимых",
        )?;
        self.activate(Action::Clear);
        verify(self.checked.is_empty(), "снятие выбора")?;
        self.activate(Action::ClearSearch);
        let plan = make_plan(
            &self.rows,
            &HashSet::from(["demo-1".to_owned(), "demo-2".to_owned()]),
        )?;
        self.event(Event::Done(Ok(Outcome::Prepared(plan, false)), None));
        for (text, enabled) in [
            ("", false),
            (tr("удалить", "delete"), false),
            (delete_word(), true),
        ] {
            unsafe {
                SetWindowTextW(self.entry, wide(text).as_ptr());
            }
            self.translate_control(self.entry);
            let result = self
                .dialog_buttons()
                .iter()
                .find(|button| button.action == Action::Confirm)
                .is_some_and(|button| button.enabled);
            verify(result == enabled, "ввод фразы подтверждения")?;
        }
        if let (Some(path), Some(layout)) = (
            std::env::var_os("CODEX_MANAGER_MODAL_PREVIEW_PATH").map(PathBuf::from),
            self.dialog_layout,
        ) {
            self.snapshot(&path, layout.shell)?;
        }
        self.finish_dialog(DismissDialog::Cancel);
        verify(
            self.dialog.is_none() && self.rows.len() == 8,
            "отмена подтверждения",
        )?;
        crate::stdout(
            &serde_json::json!({"ok":true, "ui":"Win32", "demo_chats":8, "snapshot":snapshot})
                .to_string(),
        )
    }
}

impl Drop for App {
    fn drop(&mut self) {
        unsafe {
            DeleteObject(self.edit_font);
            DeleteObject(self.details_font);
        }
    }
}

fn coordinates(value: LPARAM) -> POINT {
    POINT {
        x: value as i16 as i32,
        y: (value >> 16) as i16 as i32,
    }
}

unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match message {
            WM_NCCREATE => {
                let data = (*(lp as *const CREATESTRUCTW)).lpCreateParams;
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, data as isize);
                return DefWindowProcW(hwnd, message, wp, lp);
            }
            WM_NCCALCSIZE | WM_NCPAINT | WM_NCUAHDRAWCAPTION | WM_NCUAHDRAWFRAME => return 0,
            WM_NCACTIVATE if IsIconic(hwnd) == 0 => return 1,
            WM_ERASEBKGND => return 1,
            WM_DESTROY => {
                KillTimer(hwnd, 1);
                PostQuitMessage(0);
                return 0;
            }
            WM_NCDESTROY => {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                return DefWindowProcW(hwnd, message, wp, lp);
            }
            _ => {}
        }
        let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WindowState;
        let Some(state) = state.as_ref() else {
            return DefWindowProcW(hwnd, message, wp, lp);
        };
        // Showing or focusing an edit can request its colors synchronously
        // while the app state is already borrowed by the outer message.
        if matches!(message, WM_CTLCOLOREDIT | WM_CTLCOLORSTATIC) {
            return state.edit_colors.apply(lp as HWND, wp as HDC);
        }
        let Ok(mut app) = state.app.try_borrow_mut() else {
            return DefWindowProcW(hwnd, message, wp, lp);
        };
        match message {
            WM_PAINT => {
                app.draw_window();
                return 0;
            }
            WM_PRINT | WM_PRINTCLIENT => {
                let flags = lp as i32;
                // The full frame is client-drawn, including in shell previews.
                if (message == WM_PRINTCLIENT || flags & PRF_CLIENT != 0)
                    && (flags & PRF_CHECKVISIBLE == 0 || IsWindowVisible(hwnd) != 0)
                {
                    app.draw_to(wp as HDC, true);
                }
                return 0;
            }
            WM_GETMINMAXINFO => {
                app.minmax(hwnd, &mut *(lp as *mut MINMAXINFO));
                return 0;
            }
            WM_NCHITTEST => {
                let mut point = coordinates(lp);
                ScreenToClient(hwnd, &mut point);
                return app.nc_hit_test(app.logical(point.x), app.logical(point.y));
            }
            WM_SIZE => {
                if wp == SIZE_MINIMIZED as usize {
                    app.finish_opening();
                }
                app.resize();
                return 0;
            }
            WM_SYSCOMMAND
                if matches!((wp & 0xfff0) as u32, SC_MINIMIZE | SC_MAXIMIZE | SC_RESTORE) =>
            {
                app.finish_opening();
            }
            WM_DPICHANGED => {
                app.dpi = (wp as u16 as u32).max(96);
                app.update_fonts();
                let rect = &*(lp as *const RECT);
                SetWindowPos(
                    hwnd,
                    null_mut(),
                    rect.left,
                    rect.top,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
                app.resize();
                return 0;
            }
            WM_MOUSEMOVE | WM_LBUTTONDOWN | WM_LBUTTONDBLCLK | WM_LBUTTONUP => {
                let point = coordinates(lp);
                let (x, y) = (app.logical(point.x), app.logical(point.y));
                match message {
                    WM_MOUSEMOVE => app.mouse_move(x, y),
                    WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => app.mouse_down(x, y),
                    _ => app.mouse_up(x, y),
                }
                return 0;
            }
            WM_MOUSELEAVE => {
                if app.drag.is_none() {
                    app.hover = None;
                    app.cursor = None;
                    app.redraw();
                }
                return 0;
            }
            WM_CAPTURECHANGED => {
                app.pressed = None;
                app.drag = None;
                app.redraw();
                return 0;
            }
            WM_MOUSEWHEEL => {
                if app.dialog_transition.is_some() {
                    return 0;
                }
                let delta = (wp >> 16) as i16 as i32;
                let mut point = coordinates(lp);
                ScreenToClient(hwnd, &mut point);
                if app.dialog.is_none()
                    && app
                        .viewport()
                        .contains(app.logical(point.x), app.logical(point.y))
                {
                    app.shift_scroll(-delta * 3 * 24 / WHEEL_DELTA as i32);
                } else if let Some(details) = app.dialog_layout.and_then(|dialog| dialog.details) {
                    if details.contains(app.logical(point.x), app.logical(point.y)) {
                        SendMessageW(
                            app.details,
                            EM_LINESCROLL,
                            0,
                            (-delta * 3 / WHEEL_DELTA as i32) as LPARAM,
                        );
                    }
                }
                return 0;
            }
            WM_SETCURSOR if lp as u16 as u32 == HTCLIENT => {
                let mut point: POINT = zeroed();
                GetCursorPos(&mut point);
                ScreenToClient(hwnd, &mut point);
                let action = app.action_at(app.logical(point.x), app.logical(point.y));
                let cursor = match action {
                    Some(Action::Search | Action::Phrase | Action::Details) => IDC_IBEAM,
                    Some(_) => IDC_HAND,
                    None => IDC_ARROW,
                };
                SetCursor(LoadCursorW(null_mut(), cursor));
                return 1;
            }
            WM_COMMAND => {
                let child = lp as HWND;
                let notice = (wp >> 16) as u32;
                if notice == EN_CHANGE {
                    app.translate_control(child);
                } else if notice == EN_SETFOCUS {
                    app.focus = if child == app.search {
                        Some(Action::Search)
                    } else if child == app.entry {
                        Some(Action::Phrase)
                    } else if child == app.details {
                        Some(Action::Details)
                    } else {
                        app.focus
                    };
                    app.redraw();
                }
                return 0;
            }
            WM_CLOSE => {
                app.request_close();
                return 0;
            }
            WM_TIMER if wp == 1 => {
                let close = app.tick();
                drop(app);
                if close {
                    DestroyWindow(hwnd);
                }
                return 0;
            }
            _ => {}
        }
    }
    unsafe { DefWindowProcW(hwnd, message, wp, lp) }
}

pub fn run(demo: bool, smoke: bool) -> Result<()> {
    unsafe {
        InitCommonControlsEx(&INITCOMMONCONTROLSEX {
            dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_STANDARD_CLASSES,
        });
    }
    let mut token = 0;
    let gdip = GdiplusStartupInput {
        GdiplusVersion: 1,
        ..unsafe { zeroed() }
    };
    if unsafe { GdiplusStartup(&mut token, &gdip, null_mut()) } != 0 {
        return Err(err(tr(
            "Не удалось запустить графический интерфейс Windows.",
            "Could not start the Windows interface.",
        )));
    }
    let result = run_window(demo, smoke);
    unsafe {
        GdiplusShutdown(token);
    }
    result
}

fn run_window(demo: bool, smoke: bool) -> Result<()> {
    let instance = unsafe { GetModuleHandleW(null()) };
    let class = wide("CodexChatManagerWindow");
    let state = WindowState {
        app: RefCell::new(App::new(demo, smoke)),
        edit_colors: EditColors::new()?,
    };
    let app = &state.app;
    let wc = WNDCLASSW {
        style: CS_DBLCLKS,
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        lpszClassName: class.as_ptr(),
        hCursor: unsafe { LoadCursorW(null_mut(), IDC_ARROW) },
        hIcon: unsafe { LoadIconW(instance, APP_ICON_RESOURCE_ID as *const u16) },
        ..unsafe { zeroed() }
    };
    if unsafe { RegisterClassW(&wc) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let monitor = unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) };
    let work = screen_rect(monitor).rcWork;
    let dpi = unsafe { GetDpiForSystem() }.max(96) as i32;
    let dims = if smoke {
        std::env::var("CODEX_MANAGER_PREVIEW_SIZE")
            .ok()
            .and_then(|value| {
                let (w, h) = value.split_once('x')?;
                Some((w.parse::<i32>().ok()?, h.parse::<i32>().ok()?))
            })
            .unwrap_or((1180, 800))
    } else {
        (
            (work.right - work.left) * 96 / dpi - 50,
            (work.bottom - work.top) * 96 / dpi - 60,
        )
    };
    let w = dims.0.clamp(960, if smoke { 3840 } else { 1180 }) * dpi / 96;
    let h = dims.1.clamp(660, if smoke { 2160 } else { 820 }) * dpi / 96;
    let title = if demo {
        demo_title()
    } else {
        "Codex Chat Manager"
    };
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_APPWINDOW | WS_EX_LAYERED,
            class.as_ptr(),
            wide(title).as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            (work.left + work.right - w) / 2,
            (work.top + work.bottom - h) / 2,
            w,
            h,
            null_mut(),
            null_mut(),
            instance,
            (&state as *const WindowState).cast(),
        )
    };
    if hwnd.is_null() {
        return Err(std::io::Error::last_os_error().into());
    }
    let initialized = app.borrow_mut().initialize(hwnd);
    if let Err(error) = initialized {
        unsafe {
            DestroyWindow(hwnd);
        }
        return Err(error);
    }
    unsafe {
        ShowWindow(hwnd, SW_SHOW);
        UpdateWindow(hwnd);
    }
    let mut msg: MSG = unsafe { zeroed() };
    loop {
        let status = unsafe { GetMessageW(&mut msg, null_mut(), 0, 0) };
        if status == 0 {
            break;
        }
        if status < 0 {
            unsafe {
                DestroyWindow(hwnd);
            }
            return Err(std::io::Error::last_os_error().into());
        }
        if msg.message == WM_SYSKEYDOWN
            && msg.wParam == VK_F4 as usize
            && msg.lParam & (1 << 29) != 0
        {
            unsafe {
                PostMessageW(hwnd, WM_CLOSE, 0, 0);
            }
            continue;
        }
        if msg.message == WM_KEYDOWN && app.borrow_mut().key(msg.hwnd, msg.wParam) {
            continue;
        }
        if msg.message == WM_MOUSEWHEEL && msg.hwnd == app.borrow().details {
            unsafe {
                SendMessageW(hwnd, msg.message, msg.wParam, msg.lParam);
            }
            continue;
        }
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    let app = state.app.into_inner();
    if let Some(error) = app.smoke_error.as_deref() {
        return Err(err(error));
    }
    Ok(())
}
