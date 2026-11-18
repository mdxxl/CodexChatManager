use crate::catalog::SidebarCatalog;
use crate::discovery;
use crate::language::{tr, trf, Language};
use crate::rpc::{Rpc, RpcClient};
use crate::{err, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const SOURCES: &[&str] = &[
    "cli",
    "vscode",
    "appServer",
    "exec",
    "unknown",
    "subAgent",
    "subAgentReview",
    "subAgentCompact",
    "subAgentThreadSpawn",
    "subAgentOther",
];

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Chat {
    pub id: String,
    name: Option<String>,
    preview: Option<String>,
    pub cwd: Option<String>,
    pub updated_at: Option<i64>,
    parent_thread_id: Option<String>,
    #[serde(default)]
    source: Value,
    #[serde(default)]
    status: Value,
    ephemeral: Option<bool>,
    #[serde(skip)]
    pub archived: bool,
    #[serde(skip)]
    demo_english_title: Option<&'static str>,
}

impl Chat {
    pub fn title(&self) -> String {
        if let (Language::English, Some(title)) = (Language::current(), self.demo_english_title) {
            return title.to_owned();
        }
        self.name
            .as_deref()
            .filter(|s| !s.is_empty())
            .or_else(|| self.preview.as_deref().filter(|s| !s.is_empty()))
            .unwrap_or_else(|| tr("Без названия", "Untitled"))
            .replace('\n', " ")
            .trim()
            .to_owned()
    }

    pub fn auxiliary(&self) -> bool {
        self.parent().is_some() || self.source.get("subAgent").is_some()
    }

    fn parent(&self) -> Option<&str> {
        self.parent_thread_id.as_deref().filter(|id| !id.is_empty())
    }

    fn active(&self) -> bool {
        self.status["type"] == "active"
    }
}

#[derive(Debug)]
pub struct DeletePlan {
    pub roots: Vec<String>,
    pub affected: Vec<Chat>,
}

impl DeletePlan {
    fn fingerprint(&self) -> BTreeMap<&str, Option<i64>> {
        self.affected
            .iter()
            .map(|row| (row.id.as_str(), row.updated_at))
            .collect()
    }
}

pub fn make_plan(rows: &[Chat], selected: &HashSet<String>) -> Result<DeletePlan> {
    let by_id: BTreeMap<_, _> = rows.iter().map(|row| (row.id.as_str(), row)).collect();
    if selected.is_empty() || selected.iter().any(|id| !by_id.contains_key(id.as_str())) {
        return Err(err(tr(
            "Некоторые выбранные чаты уже недоступны. Обновите список.",
            "Some selected chats are no longer available. Refresh the list.",
        )));
    }
    let mut children: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for row in rows {
        if let Some(parent) = row.parent() {
            children.entry(parent).or_default().push(&row.id);
        }
    }
    let mut pending: Vec<_> = selected.iter().map(String::as_str).collect();
    let mut affected = HashSet::new();
    while let Some(id) = pending.pop() {
        if affected.insert(id) {
            pending.extend(children.get(id).into_iter().flatten().copied());
        }
    }
    let mut roots = Vec::new();
    for id in selected {
        let mut seen = HashSet::from([id.as_str()]);
        let mut parent = by_id[id.as_str()].parent();
        let mut selected_ancestor = false;
        while let Some(ancestor) = parent.and_then(|ancestor| by_id.get(ancestor)) {
            if !seen.insert(&ancestor.id) {
                return Err(err(tr(
                    "Некорректные связи между чатами. Удаление остановлено.",
                    "The chats have invalid relationships. Deletion stopped.",
                )));
            }
            selected_ancestor |= selected.contains(&ancestor.id);
            parent = ancestor.parent();
        }
        if !selected_ancestor {
            roots.push(id.clone());
        }
    }
    roots.sort();
    let affected: Vec<_> = rows
        .iter()
        .filter(|row| affected.contains(row.id.as_str()))
        .cloned()
        .collect();
    if affected.iter().any(|row| row.ephemeral == Some(true)) {
        return Err(err(tr(
            "Временные чаты нельзя удалить этим способом.",
            "Temporary chats cannot be deleted this way.",
        )));
    }
    Ok(DeletePlan { roots, affected })
}

#[derive(Default)]
pub struct DeleteResult {
    pub deleted: Vec<String>,
    pub failed: Vec<(String, String)>,
    pub rows: Option<Vec<Chat>>,
    pub refresh_error: Option<String>,
    pub sidebar_error: Option<String>,
}

pub struct ChatService<R> {
    rpc: R,
    deletion_guard: fn() -> Result<()>,
    catalog: Option<SidebarCatalog>,
}

impl<R: Rpc> ChatService<R> {
    pub fn new(
        rpc: R,
        deletion_guard: fn() -> Result<()>,
        catalog: Option<SidebarCatalog>,
    ) -> Self {
        Self {
            rpc,
            deletion_guard,
            catalog,
        }
    }

    pub fn list_all(&mut self) -> Result<Vec<Chat>> {
        self.rpc.start()?;
        let mut rows = serde_json::Map::new();
        for archived in [false, true] {
            let mut cursor = None;
            let mut seen = HashSet::new();
            loop {
                let mut params = json!({"limit": 100, "archived": archived, "sortKey": "updated_at",
                    "modelProviders": [], "sourceKinds": SOURCES, "useStateDbOnly": true});
                if let Some(cursor) = cursor.take() {
                    params["cursor"] = cursor;
                }
                let page = self.rpc.call("thread/list", params, 30)?;
                let data = page["data"].as_array().ok_or_else(|| {
                    err(tr(
                        "Codex вернул неполный список чатов.",
                        "Codex returned an incomplete chat list.",
                    ))
                })?;
                for row in data {
                    let id = row["id"]
                        .as_str()
                        .filter(|id| !id.is_empty())
                        .ok_or_else(|| {
                            err(tr(
                                "Codex вернул чат без идентификатора.",
                                "Codex returned a chat without an ID.",
                            ))
                        })?;
                    let mut row = row.clone();
                    row["_archived"] = json!(archived);
                    rows.insert(id.to_owned(), row);
                }
                let Some(next) = page["nextCursor"]
                    .as_str()
                    .filter(|cursor| !cursor.is_empty())
                else {
                    break;
                };
                if !seen.insert(next.to_owned()) {
                    return Err(err(tr(
                        "Codex повторил курсор страницы; список неполон. Удаление остановлено.",
                        "Codex repeated a page cursor. The list is incomplete and deletion stopped.",
                    )));
                }
                cursor = Some(json!(next));
            }
        }
        let mut rows: Vec<_> = rows
            .into_iter()
            .map(|(_, row)| {
                let archived = row["_archived"] == true;
                serde_json::from_value::<Chat>(row).map(|mut row| {
                    row.archived = archived;
                    row
                })
            })
            .collect::<serde_json::Result<_>>()?;
        rows.sort_by_key(|row| std::cmp::Reverse(row.updated_at.unwrap_or(0)));
        Ok(rows)
    }

    pub fn delete(
        &mut self,
        plan: DeletePlan,
        progress: impl Fn(usize, usize),
    ) -> Result<DeleteResult> {
        (self.deletion_guard)()?;
        let current = make_plan(&self.list_all()?, &plan.roots.iter().cloned().collect())?;
        let fingerprint = current.fingerprint();
        if fingerprint != plan.fingerprint() {
            return Err(err(tr(
                "Список или содержимое чатов изменились после подтверждения. Обновите список и выберите заново.",
                "The chats changed after confirmation. Refresh the list and select them again.",
            )));
        }
        if current.affected.iter().any(Chat::active) {
            return Err(err(tr(
                "В одном из чатов идёт работа. Завершите её перед удалением.",
                "Work is in progress in one of the chats. Finish it before deleting.",
            )));
        }
        let mut result = DeleteResult::default();
        for (index, id) in plan.roots.iter().enumerate() {
            progress(index + 1, plan.roots.len());
            let remove = (|| {
                let response = self.rpc.call("thread/read", json!({"threadId":id}), 30)?;
                let row: Chat = serde_json::from_value(response["thread"].clone())?;
                if row.active() {
                    return Err(err(tr(
                        "В чате идёт работа; удаление пропущено.",
                        "Skipped deletion because work is in progress in this chat.",
                    )));
                }
                if row.id != *id || row.updated_at != fingerprint[id.as_str()] {
                    return Err(err(tr(
                        "Чат изменился после подтверждения; удаление пропущено.",
                        "Skipped deletion because the chat changed after confirmation.",
                    )));
                }
                self.rpc.call("thread/delete", json!({"threadId":id}), 60)?;
                Ok(())
            })();
            match remove {
                Ok(()) => result.deleted.push(id.clone()),
                Err(error) => {
                    result.failed.push((id.clone(), error.to_string()));
                    if !self.rpc.connected() {
                        result
                            .failed
                            .extend(plan.roots[index + 1..].iter().map(|id| {
                                (
                                    id.clone(),
                                    tr(
                                        "Не выполнено: соединение закрыто",
                                        "Skipped because the connection closed",
                                    )
                                    .to_owned(),
                                )
                            }));
                        break;
                    }
                }
            }
        }
        let mut sidebar_ids = HashSet::new();
        match self.list_all() {
            Ok(rows) => {
                let remaining: HashSet<_> = rows.iter().map(|row| row.id.as_str()).collect();
                let mut verified = Vec::new();
                for id in result.deleted.drain(..) {
                    let subtree = make_plan(&current.affected, &HashSet::from([id.clone()]))?;
                    let mut stale: Vec<_> = subtree
                        .affected
                        .iter()
                        .filter_map(|row| remaining.get(row.id.as_str()).copied())
                        .collect();
                    stale.sort_unstable();
                    if stale.is_empty() {
                        sidebar_ids.extend(subtree.affected.into_iter().map(|row| row.id));
                        verified.push(id);
                    } else {
                        result.failed.push((
                            id,
                            trf!(
                                "Codex подтвердил команду, но записи остались в списке: {}",
                                "Codex accepted the command, but these entries are still listed: {}",
                                stale.join(", ")
                            ),
                        ));
                    }
                }
                result.deleted = verified;
                result.rows = Some(rows);
            }
            Err(error) => {
                let error = error.to_string();
                result.failed.extend(result.deleted.drain(..).map(|id| {
                    (
                        id,
                        trf!("Команда принята, но отсутствие чата не проверено: {error}", "Codex accepted the command, but deletion could not be verified: {error}"),
                    )
                }));
                result.refresh_error = Some(error);
            }
        }
        if let Some(catalog) = &self.catalog {
            if let Err(error) = catalog.clean(sidebar_ids) {
                result.sidebar_error = Some(error.to_string());
            }
        }
        Ok(result)
    }
}

pub enum Service {
    Demo(Vec<Chat>),
    Real {
        service: Option<Box<ChatService<RpcClient>>>,
        executable: Option<PathBuf>,
        override_path: Option<PathBuf>,
    },
}

impl Service {
    pub fn new(demo: bool) -> Self {
        if !demo {
            return Self::Real {
                service: None,
                executable: None,
                override_path: None,
            };
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let titles = [
            ("Обновить главную страницу", "Update the home page"),
            ("Исправить поиск", "Fix search"),
            ("Подготовить отчёт", "Prepare a report"),
            ("Проверить тесты", "Review the tests"),
            ("Настроить приложение", "Configure the app"),
            ("Идеи интерфейса", "Interface ideas"),
            ("Старая задача", "Old task"),
            ("Архив проекта", "Project archive"),
        ];
        Self::Demo(
            titles
                .into_iter()
                .enumerate()
                .map(|(index, (russian, english))| {
                    let index = index as i64 + 1;
                    Chat {
                        id: format!("demo-{index}"),
                        name: Some(russian.to_owned()),
                        preview: None,
                        cwd: Some("C:/Projects/example".to_owned()),
                        updated_at: Some(now - index * 5400),
                        status: json!({"type":"notLoaded"}),
                        parent_thread_id: None,
                        source: Value::Null,
                        ephemeral: None,
                        archived: index > 5,
                        demo_english_title: Some(english),
                    }
                })
                .collect(),
        )
    }

    pub fn executable(&self) -> Option<PathBuf> {
        match self {
            Self::Demo(_) => None,
            Self::Real { executable, .. } => executable.clone(),
        }
    }

    pub fn reconnect(&mut self, path: Option<PathBuf>) {
        if let Self::Real {
            service,
            executable,
            override_path,
        } = self
        {
            *service = None;
            *executable = None;
            if path.is_some() {
                *override_path = path;
            }
        }
    }

    fn real(&mut self) -> Result<&mut ChatService<RpcClient>> {
        let Self::Real {
            service,
            executable,
            override_path,
        } = self
        else {
            unreachable!()
        };
        if service.is_none() {
            let path = discovery::find_codex(override_path.clone())?;
            *executable = Some(path.clone());
            *service = Some(Box::new(ChatService::new(
                RpcClient::new(path),
                discovery::require_closed_codex,
                Some(SidebarCatalog::default()),
            )));
        }
        Ok(service.as_deref_mut().expect("initialized service"))
    }

    pub fn list_all(&mut self) -> Result<Vec<Chat>> {
        match self {
            Self::Demo(rows) => Ok(rows.clone()),
            Self::Real { .. } => self.real()?.list_all(),
        }
    }

    pub fn delete(
        &mut self,
        plan: DeletePlan,
        progress: impl Fn(usize, usize),
    ) -> Result<DeleteResult> {
        if let Self::Demo(rows) = self {
            let ids = plan.fingerprint();
            rows.retain(|row| !ids.contains_key(row.id.as_str()));
            return Ok(DeleteResult {
                deleted: plan.roots,
                rows: Some(rows.clone()),
                ..DeleteResult::default()
            });
        }
        self.real()?.delete(plan, progress)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, BufRead, Write};

    fn row(id: &str, parent: Option<&str>) -> Value {
        json!({"id":id, "name":id, "updatedAt":10, "parentThreadId":parent,
            "status":{"type":"notLoaded"}})
    }

    fn chats(rows: Vec<Value>) -> Vec<Chat> {
        rows.into_iter()
            .map(|row| serde_json::from_value(row).unwrap())
            .collect()
    }

    fn ids(values: &[&str]) -> HashSet<String> {
        values.iter().map(|id| (*id).to_owned()).collect()
    }

    #[test]
    fn plan_includes_recursive_children_but_preserves_independent_forks() {
        let mut fork = row("fork", None);
        fork["forkedFromId"] = json!("a");
        let rows = chats(vec![
            row("a", None),
            row("b", Some("a")),
            row("c", Some("b")),
            fork,
        ]);
        let plan = make_plan(&rows, &ids(&["a", "b"])).unwrap();
        assert_eq!(plan.roots, ["a"]);
        assert_eq!(
            plan.fingerprint().keys().copied().collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
        assert_eq!(make_plan(&rows, &ids(&["b"])).unwrap().roots, ["b"]);
    }

    #[test]
    fn plan_rejects_empty_missing_ephemeral_and_cyclic_selections() {
        let mut temporary = row("temp", None);
        temporary["ephemeral"] = json!(true);
        let rows = chats(vec![row("a", Some("b")), row("b", Some("a")), temporary]);
        for selection in [ids(&[]), ids(&["absent"]), ids(&["a", "b"]), ids(&["temp"])] {
            assert!(make_plan(&rows, &selection).is_err());
        }
    }

    #[test]
    fn guardian_review_and_child_are_auxiliary() {
        let mut guardian = row("guardian", None);
        guardian["source"] = json!({"subAgent":{"other":"guardian"}});
        let rows = chats(vec![row("a", None), row("b", Some("a")), guardian]);
        assert_eq!(
            rows.iter().map(Chat::auxiliary).collect::<Vec<_>>(),
            [false, true, true]
        );
    }

    #[derive(Default)]
    struct Fake {
        rows: Vec<Value>,
        deleted: Vec<String>,
        fail_delete: Option<String>,
        stop_verification: bool,
        keep_child: bool,
        keep_all: bool,
    }

    impl Rpc for Fake {
        fn start(&mut self) -> Result<()> {
            Ok(())
        }
        fn connected(&self) -> bool {
            true
        }
        fn call(&mut self, method: &str, params: Value, _: u64) -> Result<Value> {
            match method {
                "thread/list" => {
                    if self.stop_verification && !self.deleted.is_empty() {
                        return Err(err("verification failure"));
                    }
                    let rows = if params["archived"] == true {
                        &[][..]
                    } else {
                        &self.rows
                    };
                    Ok(json!({"data": rows, "nextCursor":null}))
                }
                "thread/read" => Ok(
                    json!({"thread": self.rows.iter().find(|row| row["id"] == params["threadId"])}),
                ),
                "thread/delete" => {
                    let id = params["threadId"].as_str().unwrap();
                    if self.fail_delete.as_deref() == Some(id) {
                        return Err(err("test failure"));
                    }
                    self.deleted.push(id.to_owned());
                    let mut remove = ids(&[id]);
                    if !self.keep_child {
                        remove = make_plan(&chats(self.rows.clone()), &remove)?
                            .affected
                            .into_iter()
                            .map(|row| row.id)
                            .collect();
                    }
                    if !self.keep_all {
                        self.rows
                            .retain(|row| !remove.contains(row["id"].as_str().unwrap()));
                    }
                    Ok(json!({}))
                }
                _ => panic!("unsupported fixture method {method}"),
            }
        }
    }

    fn allow() -> Result<()> {
        Ok(())
    }
    fn deny() -> Result<()> {
        Err(err("закройте Codex"))
    }

    fn service() -> ChatService<Fake> {
        ChatService::new(
            Fake {
                rows: vec![row("a", None), row("b", Some("a")), row("c", None)],
                ..Fake::default()
            },
            allow,
            None,
        )
    }

    fn prepare(service: &mut ChatService<Fake>, selection: &[&str]) -> DeletePlan {
        make_plan(&service.list_all().unwrap(), &ids(selection)).unwrap()
    }

    #[test]
    fn guard_prevents_every_mutation() {
        let mut service = service();
        service.deletion_guard = deny;
        let plan = prepare(&mut service, &["a"]);
        assert!(service.delete(plan, |_, _| {}).is_err());
        assert!(service.rpc.deleted.is_empty());
    }

    #[test]
    fn changed_chat_new_child_and_active_chat_are_rejected() {
        for mutation in ["changed", "new", "active"] {
            let mut service = service();
            let plan = prepare(&mut service, &["a"]);
            match mutation {
                "changed" => service.rpc.rows[0]["updatedAt"] = json!(11),
                "new" => service.rpc.rows.push(row("new", Some("a"))),
                _ => service.rpc.rows[0]["status"] = json!({"type":"active"}),
            }
            assert!(service.delete(plan, |_, _| {}).is_err());
            assert!(service.rpc.deleted.is_empty());
        }
    }

    #[test]
    fn reports_partial_failure_without_repeating_mutation() {
        let mut service = service();
        service.rpc.fail_delete = Some("a".to_owned());
        let plan = prepare(&mut service, &["a", "c"]);
        let result = service.delete(plan, |_, _| {}).unwrap();
        assert_eq!(result.deleted, ["c"]);
        assert_eq!(result.failed[0].0, "a");
        assert_eq!(service.rpc.deleted, ["c"]);
    }

    #[test]
    fn success_without_removal_of_root_or_descendant_is_rejected() {
        for keep_all in [false, true] {
            let mut service = service();
            service.rpc.keep_child = true;
            service.rpc.keep_all = keep_all;
            let plan = prepare(&mut service, &["a"]);
            let result = service.delete(plan, |_, _| {}).unwrap();
            assert!(result.deleted.is_empty());
            assert!(result.failed[0].1.contains("остались в списке"));
            assert!(result.failed[0].1.ends_with('b'));
        }
    }

    #[test]
    fn transport_verification_error_does_not_retry_or_claim_success() {
        let mut service = service();
        service.rpc.stop_verification = true;
        let plan = prepare(&mut service, &["a"]);
        let result = service.delete(plan, |_, _| {}).unwrap();
        assert!(result.deleted.is_empty());
        assert_eq!(service.rpc.deleted, ["a"]);
        assert_eq!(
            result.refresh_error.as_deref(),
            Some("verification failure")
        );
    }

    #[test]
    #[ignore = "stdio protocol fixture, spawned by the transport test"]
    fn server_fixture() {
        let mut rows = BTreeMap::from([
            ("alpha".to_owned(), row("alpha", None)),
            ("child".to_owned(), row("child", Some("alpha"))),
            ("beta".to_owned(), row("beta", None)),
            ("archived".to_owned(), row("archived", None)),
        ]);
        let mut output = io::stdout().lock();
        writeln!(output).unwrap();
        for line in io::stdin().lock().lines() {
            let message: Value = serde_json::from_str(&line.unwrap()).unwrap();
            let Some(id) = message.get("id") else {
                continue;
            };
            let params = &message["params"];
            let result = match message["method"].as_str().unwrap() {
                "initialize" => json!({"userAgent":"fixture"}),
                "thread/list" => {
                    let archived = params["archived"].as_bool().unwrap();
                    let start = params["cursor"]
                        .as_str()
                        .unwrap_or("0")
                        .parse::<usize>()
                        .unwrap();
                    let candidates = rows
                        .iter()
                        .filter(|(id, _)| (**id == "archived") == archived)
                        .map(|(_, row)| row)
                        .collect::<Vec<_>>();
                    let cursor = (start + 1 < candidates.len()).then(|| (start + 1).to_string());
                    writeln!(
                        output,
                        "{}",
                        json!({"method":"fixture/notification","params":{}})
                    )
                    .unwrap();
                    json!({"data":candidates.get(start).into_iter().collect::<Vec<_>>(),"nextCursor":cursor})
                }
                "thread/read" => json!({"thread":rows[params["threadId"].as_str().unwrap()]}),
                "thread/delete" => {
                    let plan = make_plan(
                        &chats(rows.values().cloned().collect()),
                        &ids(&[params["threadId"].as_str().unwrap()]),
                    )
                    .unwrap();
                    for row in plan.affected {
                        rows.remove(&row.id);
                    }
                    json!({})
                }
                method => panic!("unexpected RPC {method}"),
            };
            writeln!(output, "{}", json!({"id":id,"result":result})).unwrap();
            output.flush().unwrap();
        }
    }

    #[test]
    fn stdio_pagination_notifications_selected_delete_and_delete_all() {
        let this = std::env::current_exe().unwrap();
        let mut rpc = RpcClient::new(this.clone());
        rpc.fixture = Some((
            this,
            [
                "--exact",
                "service::tests::server_fixture",
                "--ignored",
                "--nocapture",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        ));
        let mut service = ChatService::new(rpc, allow, None);
        let rows = service.list_all().unwrap();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows.iter().filter(|r| r.archived).count(), 1);
        let plan = make_plan(&rows, &ids(&["alpha"])).unwrap();
        let rest = service.delete(plan, |_, _| {}).unwrap().rows.unwrap();
        assert_eq!(
            rest.iter().map(|row| &row.id).collect::<Vec<_>>(),
            ["beta", "archived"]
        );
        let plan = make_plan(&rest, &ids(&["beta", "archived"])).unwrap();
        assert!(service
            .delete(plan, |_, _| {})
            .unwrap()
            .rows
            .unwrap()
            .is_empty());
    }
}
