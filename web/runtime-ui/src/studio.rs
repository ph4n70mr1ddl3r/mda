//! Studio UI (Phase 8): the browser surface a business analyst uses to build
//! an app — the entity/field designer over the draft → validate → publish
//! lifecycle, page designers (forms / views / dashboards / navigation), a
//! report designer, automation (business rules + workflow state machines), the
//! security admin console, and metadata import/export/promote.
//!
//! Everything rides the admin-gated APIs (`/api/studio/*`, `/api/admin/*`,
//! `/api/{forms,views,dashboards,navigation}`, `/api/{reports,rules,workflows}`)
//! — the UI holds no security logic of its own; the server is authoritative.

use std::collections::HashSet;

use leptos::*;
use serde_json::{json, Value};

use crate::api;
use crate::AppState;

const FIELD_TYPES: &[&str] = &[
    "string",
    "text",
    "integer",
    "decimal",
    "money",
    "bool",
    "date",
    "datetime",
    "enum",
    "json",
    "auto_number",
    "attachment",
];
const WIDGETS: &[&str] = &[
    "auto", "text", "textarea", "number", "date", "datetime", "checkbox", "select",
];
const ON_DELETE: &[&str] = &["restrict", "set_null", "cascade"];
const STRENGTHS: &[&str] = &["lookup", "master_detail"];
const RULE_EVENTS: &[&str] = &[
    "before_create",
    "before_update",
    "after_create",
    "after_update",
];
const CMP_OPS: &[(&str, &str)] = &[
    ("eq", "="),
    ("ne", "≠"),
    ("lt", "<"),
    ("le", "≤"),
    ("gt", ">"),
    ("ge", "≥"),
];
const FILTER_OPS: &[&str] = &["eq", "ne", "gt", "gte", "lt", "lte", "like"];
const AGGREGATES: &[&str] = &["", "count", "sum", "avg", "min", "max"];
const OWD_LEVELS: &[&str] = &["private", "team", "public_read", "public_read_write"];
const VERBS: &[&str] = &["read", "create", "update", "delete"];

// ===== small building blocks =====

fn fmt_style(w: &str) -> String {
    format!("padding:3px 5px; border:1px solid #ccc; border-radius:3px; width:{w};")
}

fn btn(accent: bool) -> &'static str {
    if accent {
        "padding:4px 12px; cursor:pointer; background:#2563eb; color:#fff; border:none; border-radius:3px;"
    } else {
        "padding:4px 10px; cursor:pointer; border:1px solid #bbb; border-radius:3px; background:#fff;"
    }
}

fn del_btn() -> &'static str {
    "padding:2px 8px; cursor:pointer; border:1px solid #e11; color:#b00; border-radius:3px; background:#fff;"
}

fn row_style() -> &'static str {
    "display:flex; gap:6px; align-items:center; padding:5px 8px; border:1px solid #e2e6ea; border-radius:4px; margin:3px 0; flex-wrap:wrap;"
}

fn card_style() -> &'static str {
    "border:1px solid #e2e6ea; border-radius:6px; padding:10px 14px; margin:8px 0;"
}

fn opts(list: &[&str]) -> Vec<(String, String)> {
    list.iter()
        .map(|s| (s.to_string(), s.to_string()))
        .collect()
}

fn cmp_opts() -> Vec<(String, String)> {
    CMP_OPS
        .iter()
        .map(|(v, l)| (v.to_string(), l.to_string()))
        .collect()
}

/// Client-side UUID via WebCrypto (`crypto.randomUUID`), with a time+counter
/// fallback for exotic contexts. New draft artifacts need ids before save.
fn new_uuid() -> String {
    if let Some(c) = web_sys::window().and_then(|w| w.crypto().ok()) {
        let u = c.random_uuid();
        if !u.is_empty() {
            return u;
        }
    }
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(1);
    let n = N.fetch_add(1, Ordering::Relaxed);
    format!("00000000-0000-4000-8000-{n:012x}")
}

#[component]
fn Txt(sig: RwSignal<String>, ph: &'static str, w: &'static str) -> impl IntoView {
    view! {
        <input placeholder=ph prop:value=move || sig.get()
            on:input=move |ev| sig.set(event_target_value(&ev))
            style=fmt_style(w) />
    }
}

#[component]
fn Num(sig: RwSignal<String>, w: &'static str) -> impl IntoView {
    view! {
        <input type="number" prop:value=move || sig.get()
            on:input=move |ev| sig.set(event_target_value(&ev))
            style=fmt_style(w) />
    }
}

#[component]
fn Area(sig: RwSignal<String>, rows: usize, ph: &'static str) -> impl IntoView {
    view! {
        <textarea rows=rows placeholder=ph prop:value=move || sig.get()
            on:input=move |ev| sig.set(event_target_value(&ev))
            style="width:98%; padding:4px 6px; border:1px solid #ccc; border-radius:3px; font-family:ui-monospace,monospace; font-size:12px;"></textarea>
    }
}

/// Select over a static option list.
#[component]
fn Sel(sig: RwSignal<String>, options: Vec<(String, String)>) -> impl IntoView {
    let cur = sig.get_untracked();
    view! {
        <select on:input=move |ev| sig.set(event_target_value(&ev)) style=fmt_style("auto")>
            {options.into_iter().map(|(v, l)| {
                let selected = v == cur;
                view! { <option value=v.clone() selected=selected>{l}</option> }
            }).collect_view()}
        </select>
    }
}

#[component]
fn Chk(sig: RwSignal<bool>) -> impl IntoView {
    view! {
        <input type="checkbox" prop:checked=move || sig.get()
            on:input=move |ev| sig.set(event_target_checked(&ev)) />
    }
}

#[component]
fn MsgLine(sig: RwSignal<Option<(bool, String)>>) -> impl IntoView {
    view! {
        {move || match sig.get() {
            None => ().into_view(),
            Some((ok, ref m)) => view! {
                <div style=move || format!(
                    "padding:6px 10px; margin:6px 0; border-radius:4px; font-size:13px; {}",
                    if ok { "background:#e8f7ee; border:1px solid #3a9d5d;" }
                    else { "background:#fdecea; border:1px solid #c0392b;" })
                >{m.clone()}</div>
            }.into_view(),
        }}
    }
}

fn set_msg(sig: &RwSignal<Option<(bool, String)>>, ok: bool, m: impl Into<String>) {
    sig.set(Some((ok, m.into())));
}

/// Clickable tab bar (Studio + its sub-sections).
#[component]
fn Tabs(sig: RwSignal<usize>, labels: Vec<&'static str>) -> impl IntoView {
    view! {
        <div style="display:flex; gap:2px; border-bottom:2px solid #2563eb; margin-bottom:12px; flex-wrap:wrap;">
            {labels.into_iter().enumerate().map(move |(i, l)| {
                let sig2 = sig;
                view! {
                    <button
                        on:click=move |_: leptos::ev::MouseEvent| sig2.set(i)
                        style=move || {
                            let base = "padding:6px 14px; cursor:pointer; border:none; font-size:13px;";
                            if sig2.get() == i {
                                format!("{base} background:#2563eb; color:#fff;")
                            } else {
                                format!("{base} background:#eef1f5; color:#333;")
                            }
                        }>
                        {l}
                    </button>
                }
            }).collect_view()}
        </div>
    }
}

fn h3(t: &'static str) -> impl IntoView {
    view! { <h3 style="margin:16px 0 6px; font-size:15px;">{t}</h3> }
}

fn lbl(t: &str) -> impl IntoView {
    view! { <span style="font-size:11px; color:#667;">{t.to_string()}</span> }
}

/// Generic JSON result table (report runs).
fn value_table(cols: &[String], rows: &[Value]) -> impl IntoView {
    let cols2 = cols.to_vec();
    let rows2 = rows.to_vec();
    view! {
        <div style="overflow-x:auto;">
            <table style="border-collapse:collapse; font-size:12px; margin-top:6px;">
                <thead>
                    <tr>
                        {cols2.iter().map(|c| view! {
                            <th style="border:1px solid #ddd; background:#f4f4f4; padding:3px 8px; text-align:left;">{c.clone()}</th>
                        }).collect_view()}
                    </tr>
                </thead>
                <tbody>
                    {rows2.iter().map(|r| {
                        let cols3 = cols.to_vec();
                        view! {
                            <tr>
                                {cols3.iter().map(|c| {
                                    let text = match &r[c.as_str()] {
                                        Value::Null => String::new(),
                                        Value::String(s) => s.clone(),
                                        v => v.to_string(),
                                    };
                                    view! { <td style="border:1px solid #eee; padding:3px 8px;">{text}</td> }
                                }).collect_view()}
                            </tr>
                        }
                    }).collect_view()}
                </tbody>
            </table>
        </div>
    }
}

/// The active model's entity names + fields (drives every pick list).
fn model_entities(state: &AppState) -> Vec<(String, Vec<(String, String)>)> {
    state
        .model
        .get()
        .map(|m| {
            m.entities
                .iter()
                .map(|e| {
                    (
                        e.name.clone(),
                        e.fields
                            .iter()
                            .map(|f| {
                                (
                                    f.name.clone(),
                                    f.label.clone().unwrap_or_else(|| f.name.clone()),
                                )
                            })
                            .collect(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn entity_field_list(state: &AppState, entity: &str) -> Vec<(String, String)> {
    model_entities(state)
        .into_iter()
        .find(|(n, _)| n == entity)
        .map(|(_, f)| f)
        .unwrap_or_default()
}

fn entity_options(state: &AppState) -> Vec<(String, String)> {
    model_entities(state)
        .into_iter()
        .map(|(n, _)| (n.clone(), n))
        .collect()
}

/// Entity pick list for permission grants ("*" = every entity).
fn role_ent_opts(state: &AppState) -> Vec<(String, String)> {
    let mut v = vec![("*".to_string(), "* (every entity)".to_string())];
    v.extend(entity_options(state));
    v
}

/// field/op/value → bounded-DSL `Cmp` expression.
fn cmp_expr(field: &str, op: &str, value: &str) -> Value {
    json!({"op":"Cmp","kind":op,
           "lhs":{"op":"Field","name":field},
           "rhs":{"op":"Lit","value":value}})
}

fn lit_true() -> Value {
    json!({"op":"Lit","value":true})
}

fn default_cardinality() -> String {
    "many_to_one".to_string()
}

/// Parse a literal action/guard value by kind (rules + workflow actions).
fn lit_value(kind: &str, raw: &str) -> Value {
    match kind {
        "number" => raw
            .parse::<f64>()
            .map(|n| json!(n))
            .unwrap_or_else(|_| json!(raw)),
        "bool" => json!(raw == "true"),
        "empty" => json!(null),
        "now" => json!({"op":"Call","name":"now","args":[]}),
        _ => json!(raw),
    }
}

// ===== the Studio shell =====

#[component]
pub fn Studio() -> impl IntoView {
    let tab = create_rw_signal(0usize);
    view! {
        <div>
            <h2 style="margin-top:0;">"Studio"</h2>
            <Tabs sig=tab labels=vec!["Model", "Pages", "Reports", "Automation", "Security", "Data"]/>
            {move || match tab.get() {
                0 => view! { <StudioModel/> }.into_view(),
                1 => view! { <StudioPages/> }.into_view(),
                2 => view! { <StudioReports/> }.into_view(),
                3 => view! { <StudioAutomation/> }.into_view(),
                4 => view! { <StudioSecurity/> }.into_view(),
                _ => view! { <StudioData/> }.into_view(),
            }}
        </div>
    }
}

// ===== Model designer: draft lifecycle + entity/field editor =====

#[derive(Clone, Default, serde::Deserialize, serde::Serialize)]
struct SModel {
    #[serde(default)]
    entities: Vec<SEntity>,
}

#[derive(Clone, serde::Deserialize, serde::Serialize)]
struct SEntity {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    table_name: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    fields: Vec<SField>,
    #[serde(default)]
    relationships: Vec<SRel>,
}

#[derive(Clone, serde::Deserialize, serde::Serialize)]
struct SField {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    field_type: String,
    #[serde(default)]
    required: bool,
    #[serde(default)]
    is_unique: bool,
    #[serde(default)]
    is_indexed: bool,
    #[serde(default)]
    config: Value,
}

#[derive(Clone, serde::Deserialize, serde::Serialize)]
struct SRel {
    id: String,
    #[serde(default)]
    source_field_name: String,
    #[serde(default)]
    target_entity_id: String,
    /// The server requires this field (no serde default); the Studio authors
    /// the only cardinality the engine supports — many-to-one over the FK.
    #[serde(default = "default_cardinality")]
    cardinality: String,
    #[serde(default)]
    strength: String,
    #[serde(default)]
    on_delete: Option<String>,
    #[serde(default)]
    required: bool,
}

/// Artifacts already in the ACTIVE model: locked (an edit is a Phase-2
/// transform; a removal is an allowed two-phase retire).
#[derive(Default, Clone)]
struct ActiveIds {
    entities: HashSet<String>,
    fields: HashSet<String>,
    rels: HashSet<String>,
}

#[component]
fn StudioModel() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let token = state.token;
    let drafts = create_rw_signal(Vec::<Value>::new());
    let open = create_rw_signal(None::<String>);
    let msg = create_rw_signal(None::<(bool, String)>);
    let new_name = create_rw_signal(String::new());

    let reload = {
        move || {
            let t = token.get().unwrap_or_default();
            let drafts = drafts;
            let msg = msg;
            spawn_local(async move {
                match api::sget(&t, "/api/studio/drafts").await {
                    Ok(list) => drafts.set(list.as_array().cloned().unwrap_or_default()),
                    Err(e) => set_msg(&msg, false, e),
                }
            });
        }
    };
    reload();

    view! {
        {move || {
            let open = open;
            let reload = reload;
            if let Some(id) = open.get() {
                view! {
                    <div>
                        <button on:click=move |_: leptos::ev::MouseEvent| {
                            open.set(None);
                            reload();
                        } style=btn(false)>"← All drafts"</button>
                        <DraftEditor id=id.clone()/>
                    </div>
                }.into_view()
            } else {
                view! {
                    <div>
                        <MsgLine sig=msg/>
                        <div style="display:flex; gap:6px; align-items:center; margin-bottom:10px;">
                            <Txt sig=new_name ph="draft name" w="200px"/>
                            <button style=btn(true)
                                on:click=move |_: leptos::ev::MouseEvent| {
                                    let name = new_name.get().trim().to_string();
                                    if name.is_empty() { return; }
                                    let t = token.get().unwrap_or_default();
                                    let msg = msg;
                                    let open = open;
                                    spawn_local(async move {
                                        match api::spost(&t, "/api/studio/drafts", json!({"name": name})).await {
                                            Ok(d) => {
                                                if let Some(id) = d["id"].as_str() {
                                                    open.set(Some(id.to_string()));
                                                }
                                            }
                                            Err(e) => set_msg(&msg, false, e),
                                        }
                                    });
                                }>"New draft"</button>
                            <span style="font-size:12px; color:#667;">"branches the current active model"</span>
                        </div>
                        <For each=move || drafts.get()
                             key=|d| d["id"].as_str().unwrap_or_default().to_string()
                             children=move |d: Value| {
                                 let token = token;
                                 let msg = msg;
                                 let drafts = drafts;
                                 let open = open;
                                 let id_open = d["id"].as_str().unwrap_or_default().to_string();
                                 let id_del = d["id"].as_str().unwrap_or_default().to_string();
                                 let can_discard = d["status"].as_str() == Some("draft");
                                 view! {
                                     <div style=row_style()>
                                         <strong style="min-width:140px;">{d["name"].as_str().unwrap_or_default().to_string()}</strong>
                                         <span style="font-size:12px; color:#667;">
                                             {format!("{} · updated {}",
                                                 d["status"].as_str().unwrap_or("?"),
                                                 d["updated_at"].as_str().unwrap_or("?").chars().take(19).collect::<String>())}
                                         </span>
                                         <span style="flex:1;"></span>
                                         <button style=btn(false)
                                             on:click=move |_: leptos::ev::MouseEvent| open.set(Some(id_open.clone()))>"Open"</button>
                                         <button style=del_btn() disabled=move || !can_discard
                                             on:click=move |_: leptos::ev::MouseEvent| {
                                                 let t = token.get().unwrap_or_default();
                                                 let id = id_del.clone();
                                                 spawn_local(async move {
                                                     match api::sdelete(&t, &format!("/api/studio/drafts/{id}")).await {
                                                         Ok(_) => {
                                                             set_msg(&msg, true, "draft discarded");
                                                             if let Ok(l) = api::sget(&t, "/api/studio/drafts").await {
                                                                 drafts.set(l.as_array().cloned().unwrap_or_default());
                                                             }
                                                         }
                                                         Err(e) => set_msg(&msg, false, e),
                                                     }
                                                 });
                                             }>"Discard"</button>
                                     </div>
                                 }
                             }/>
                    </div>
                }.into_view()
            }
        }}
    }
}

#[component]
fn DraftEditor(id: String) -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let token = state.token;
    let model = create_rw_signal(SModel::default());
    let etag = create_rw_signal(String::new());
    let status = create_rw_signal("…".to_string());
    let name = create_rw_signal(String::new());
    let sel = create_rw_signal(None::<String>);
    let active_ids = create_rw_signal(ActiveIds::default());
    let report = create_rw_signal(None::<Value>);
    let msg = create_rw_signal(None::<(bool, String)>);
    let loaded = create_rw_signal(false);

    // Load the draft + the active model (to know which rows are locked).
    {
        let t = token.get().unwrap_or_default();
        let id = id.clone();

        spawn_local(async move {
            let draft = match api::sget(&t, &format!("/api/studio/drafts/{id}")).await {
                Ok(d) => d,
                Err(e) => return set_msg(&msg, false, e),
            };
            etag.set(
                draft["version_etag"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            );
            status.set(draft["status"].as_str().unwrap_or("draft").to_string());
            name.set(draft["name"].as_str().unwrap_or_default().to_string());
            if let Ok(m) = serde_json::from_value::<SModel>(draft["model"].clone()) {
                model.set(m);
            }
            if let Ok(active) = api::sget(&t, "/api/studio/model").await {
                let mut ids = ActiveIds::default();
                let empty = vec![];
                for e in active["entities"].as_array().unwrap_or(&empty) {
                    ids.entities
                        .insert(e["id"].as_str().unwrap_or_default().to_string());
                    for f in e["fields"].as_array().unwrap_or(&empty) {
                        ids.fields
                            .insert(f["id"].as_str().unwrap_or_default().to_string());
                    }
                    for r in e["relationships"].as_array().unwrap_or(&empty) {
                        ids.rels
                            .insert(r["id"].as_str().unwrap_or_default().to_string());
                    }
                }
                active_ids.set(ids);
            }
            loaded.set(true);
        });
    }

    // Save (PUT, OCC etag) → optionally publish; reflects the fresh etag +
    // server diff report, and refreshes the shell's model/nav on publish.
    fn launch_save(
        state: AppState,
        token: RwSignal<Option<String>>,
        model: RwSignal<SModel>,
        etag: RwSignal<String>,
        report: RwSignal<Option<Value>>,
        msg: RwSignal<Option<(bool, String)>>,
        status: RwSignal<String>,
        id: String,
        publish: bool,
    ) {
        spawn_local(async move {
            let t = token.get().unwrap_or_default();
            let m = serde_json::to_value(model.get()).unwrap_or_default();
            let e = etag.get();
            match api::save_draft_model(&t, &id, &e, m).await {
                Ok(resp) => {
                    etag.set(
                        resp["version_etag"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                    );
                    report.set(Some(resp["validation"].clone()));
                    if !publish {
                        set_msg(&msg, true, "Saved.");
                        return;
                    }
                    match api::spost(&t, &format!("/api/studio/drafts/{id}/publish"), json!({}))
                        .await
                    {
                        Ok(res) => {
                            set_msg(&msg, true, format!(
                                "Published — model v{} · +{} entities, +{} fields, +{} relationships · {} entities / {} fields retired",
                                res["version"],
                                res["additions"]["entities"], res["additions"]["fields"], res["additions"]["relationships"],
                                res["retirements"]["entities"], res["retirements"]["fields"]));
                            status.set("published".to_string());
                            if let Ok(m) = api::get_model(&t).await {
                                state.model.set(Some(m));
                            }
                            if let Ok(n) = api::get_navigation(&t).await {
                                state.nav.set(n);
                            }
                        }
                        Err(e) => set_msg(&msg, false, format!("publish rejected: {e}")),
                    }
                }
                Err(e) => set_msg(&msg, false, e),
            }
        });
    }
    let id_save = id.clone();
    let save_only = move |_: leptos::ev::MouseEvent| {
        launch_save(
            state,
            token,
            model,
            etag,
            report,
            msg,
            status,
            id_save.clone(),
            false,
        )
    };
    let id_pub = id.clone();
    let save_pub = move |_: leptos::ev::MouseEvent| {
        launch_save(
            state,
            token,
            model,
            etag,
            report,
            msg,
            status,
            id_pub.clone(),
            true,
        )
    };

    let validate = {
        let id = id.clone();
        move |_: leptos::ev::MouseEvent| {
            let t = token.get().unwrap_or_default();
            let id = id.clone();
            spawn_local(async move {
                match api::spost(&t, &format!("/api/studio/drafts/{id}/validate"), json!({})).await
                {
                    Ok(r) => {
                        report.set(Some(r));
                        set_msg(&msg, true, "Validated.");
                    }
                    Err(e) => set_msg(&msg, false, e),
                }
            });
        }
    };

    view! {
        <div style="margin-top:10px;">
            <div style="display:flex; align-items:center; gap:10px; margin:8px 0; flex-wrap:wrap;">
                <h3 style="margin:0;">
                    {move || format!("Draft “{}” ({})", name.get(), status.get())}
                </h3>
                <button style=btn(false) disabled=move || !(loaded.get() && status.get() == "draft")
                    on:click=save_only>"Save"</button>
                <button style=btn(true) disabled=move || !(loaded.get() && status.get() == "draft")
                    on:click=save_pub>"Save + Publish"</button>
                <button style=btn(false) on:click=validate>"Validate"</button>
            </div>
            <MsgLine sig=msg/>
            <DiffReportView report=report/>
            <Show when=move || loaded.get() && status.get() == "draft">
                <EntityDesigner model_sig=model sel=sel active=active_ids/>
            </Show>
        </div>
    }
}

#[component]
fn DiffReportView(report: RwSignal<Option<Value>>) -> impl IntoView {
    view! {
        {move || match report.get() {
            None => ().into_view(),
            Some(r) => {
                view! {
                    <div style=card_style()>
                        <strong>{if r["valid"].as_bool().unwrap_or(false) { "✔ publishable" } else { "✘ not publishable" }.to_string()}</strong>
                        <span style="margin-left:12px; color:#667; font-size:12px;">{format!(
                            "+{} entities · +{} fields · +{} relationships · retires {} entities / {} fields",
                            r["additions"]["entities"], r["additions"]["fields"], r["additions"]["relationships"],
                            r["retirements"]["entities"], r["retirements"]["fields"])}</span>
                        {move || {
                            let empty = vec![];
                            let lists = [
                                ("violations (transform — Phase 2)", r["violations"].as_array().unwrap_or(&empty)),
                                ("errors", r["errors"].as_array().unwrap_or(&empty)),
                                ("warnings", r["warnings"].as_array().unwrap_or(&empty)),
                            ];
                            lists.iter()
                                .filter(|(_, v)| !v.is_empty())
                                .map(|(t, v)| view! {
                                    <div style="margin-top:6px;">
                                        <div style="font-weight:600; font-size:13px;">{t.to_string()}</div>
                                        {v.iter().map(|e| view! {
                                            <div style="color:#b00; font-size:12px;">{format!("• {e}")}</div>
                                        }).collect_view()}
                                    </div>
                                }).collect_view()
                        }}
                    </div>
                }.into_view()
            }
        }}
    }
}

#[component]
fn EntityDesigner(
    model_sig: RwSignal<SModel>,
    sel: RwSignal<Option<String>>,
    active: RwSignal<ActiveIds>,
) -> impl IntoView {
    let new_name = create_rw_signal(String::new());
    let new_label = create_rw_signal(String::new());

    view! {
        <div style="display:flex; gap:24px; align-items:flex-start; margin-top:10px;">
            <div style="min-width:270px;">
                {h3("Entities")}
                <div style="display:flex; gap:6px; margin-bottom:8px;">
                    <Txt sig=new_name ph="name" w="110px"/>
                    <Txt sig=new_label ph="label" w="110px"/>
                    <button style=btn(true)
                        on:click=move |_: leptos::ev::MouseEvent| {
                            let name = new_name.get().trim().to_string();
                            if name.is_empty() { return; }
                            let label = new_label.get().trim().to_string();
                            model_sig.update(|m| m.entities.push(SEntity {
                                id: new_uuid(),
                                name: name.clone(),
                                table_name: format!("{}_{}", name.to_lowercase(), &new_uuid()[..8]),
                                label: if label.is_empty() { None } else { Some(label) },
                                fields: Vec::new(),
                                relationships: Vec::new(),
                            }));
                            new_name.set(String::new());
                            new_label.set(String::new());
                        }>"Add"</button>
                </div>
                <For each=move || {
                    let list: Vec<(String, String)> = model_sig.get().entities.iter()
                        .map(|e| (e.id.clone(), e.name.clone())).collect();
                    list
                }
                     key=|(eid, _)| eid.clone()
                     children=move |pair| {
                         let (eid, ename) = pair;
                         let eid = create_rw_signal(eid);
                         let model2 = model_sig;
                         let sel2 = sel;
                         let active2 = active;
                         view! {
                             <div style="display:flex; gap:6px; align-items:center; padding:4px 8px; margin:2px 0; border:1px solid #e2e6ea; border-radius:4px; cursor:pointer;"
                                 on:click=move |_: leptos::ev::MouseEvent| sel2.set(Some(eid.get_untracked()))>
                                 <span style="flex:1;">{ename}</span>
                                 {move || if active2.get().entities.contains(&eid.get()) {
                                     view! { <span style="font-size:10px; color:#2563eb; border:1px solid #2563eb; border-radius:3px; padding:0 4px;">"active"</span> }.into_view()
                                 } else {
                                     view! { <span style="font-size:10px; color:#3a9d5d; border:1px solid #3a9d5d; border-radius:3px; padding:0 4px;">"new"</span> }.into_view()
                                 }}
                                 <button style=del_btn()
                                     on:click=move |ev: leptos::ev::MouseEvent| {
                                         ev.stop_propagation();
                                         let eid = eid.get_untracked();
                                         model2.update(|m| m.entities.retain(|e| e.id != eid));
                                         if sel2.get_untracked().as_deref() == Some(eid.as_str()) {
                                             sel2.set(None);
                                         }
                                     }>"×"</button>
                             </div>
                         }
                     }/>
                <p style="font-size:11px; color:#667; margin-top:8px; max-width:270px;">
                    "Removing an “active” artifact retires it (two-phase: live data is kept for the grace period). Editing an active artifact is a Phase-2 transform — the validator will reject it."
                </p>
            </div>
            <div style="flex:1; min-width:0;">
                {move || match sel.get() {
                    None => view! { <p style="color:#889;">"Select an entity."</p> }.into_view(),
                    Some(eid) => view! { <EntityEditor model_sig=model_sig eid=eid active=active/> }.into_view(),
                }}
            </div>
        </div>
    }
}

/// Read one entity out of the draft model (O(n) — studio authoring scale).
fn with_entity<T>(model: &SModel, eid: &str, f: impl FnOnce(&SEntity) -> T) -> Option<T> {
    model.entities.iter().find(|e| e.id == eid).map(f)
}

fn field_of(model: &SModel, eid: &str, fid: &str) -> Option<SField> {
    with_entity(model, eid, |e| {
        e.fields.iter().find(|f| f.id == fid).cloned()
    })
    .flatten()
}

fn rel_of(model: &SModel, eid: &str, rid: &str) -> Option<SRel> {
    with_entity(model, eid, |e| {
        e.relationships.iter().find(|r| r.id == rid).cloned()
    })
    .flatten()
}

#[component]
fn EntityEditor(
    model_sig: RwSignal<SModel>,
    eid: String,
    active: RwSignal<ActiveIds>,
) -> impl IntoView {
    let eid = create_rw_signal(eid);
    let new_f_name = create_rw_signal(String::new());
    let new_f_type = create_rw_signal("string".to_string());
    let new_f_label = create_rw_signal(String::new());
    let new_r_field = create_rw_signal(String::new());
    let new_r_target = create_rw_signal(String::new());
    let new_r_strength = create_rw_signal("lookup".to_string());
    let new_r_ondelete = create_rw_signal("restrict".to_string());

    let title =
        move || with_entity(&model_sig.get(), &eid.get(), |e| e.name.clone()).unwrap_or_default();
    let is_active = move || active.get().entities.contains(&eid.get());

    view! {
        <div>
            <h3 style="margin-top:0;">
                {move || format!("{} {}", title(), if is_active() { "(active — additive edits only)" } else { "" })}
            </h3>

            <h3 style="font-size:14px;">"Fields"</h3>
            <For each=move || {
                    let list: Vec<String> = with_entity(&model_sig.get(), &eid.get(),
                        |e| e.fields.iter().map(|f| f.id.clone()).collect()).unwrap_or_default();
                    list
                }
                 key=|fid| fid.clone()
                 children=move |fid| {
                     view! { <FieldRow model_sig=model_sig eid=eid.get_untracked() fid=fid active=active/> }
                 }/>

            <div style="display:flex; gap:6px; align-items:center; margin:8px 0 16px; flex-wrap:wrap;">
                <Txt sig=new_f_name ph="field name" w="130px"/>
                <Sel sig=new_f_type options=opts(FIELD_TYPES)/>
                <Txt sig=new_f_label ph="label (optional)" w="150px"/>
                <button style=btn(true)
                    on:click=move |_: leptos::ev::MouseEvent| {
                        let name = new_f_name.get().trim().to_string();
                        if name.is_empty() { return; }
                        let ft = new_f_type.get();
                        let label = new_f_label.get().trim().to_string();
                        let eid = eid.get_untracked();
                        model_sig.update(|m| {
                            if let Some(e) = m.entities.iter_mut().find(|e| e.id == eid) {
                                e.fields.push(SField {
                                    id: new_uuid(),
                                    name: name.clone(),
                                    label: if label.is_empty() { None } else { Some(label) },
                                    field_type: ft.clone(),
                                    required: false,
                                    is_unique: false,
                                    is_indexed: false,
                                    config: if ft == "enum" { json!({"options": []}) } else { json!({}) },
                                });
                            }
                        });
                        new_f_name.set(String::new());
                        new_f_label.set(String::new());
                    }>"Add field"</button>
            </div>

            <h3 style="font-size:14px;">"References (relationships)"</h3>
            <For each=move || {
                    let list: Vec<String> = with_entity(&model_sig.get(), &eid.get(),
                        |e| e.relationships.iter().map(|r| r.id.clone()).collect()).unwrap_or_default();
                    list
                }
                 key=|rid| rid.clone()
                 children=move |rid| {
                     view! { <RelRow model_sig=model_sig eid=eid.get_untracked() rid=rid active=active/> }
                 }/>
            <div style="display:flex; gap:6px; align-items:center; margin-top:8px; flex-wrap:wrap;">
                <Txt sig=new_r_field ph="field name (ref_…)" w="140px"/>
                <select prop:value=move || new_r_target.get()
                    on:input=move |ev| new_r_target.set(event_target_value(&ev))
                    style=fmt_style("auto")>
                    <option value="">"— target entity —"</option>
                    {move || {
                        model_sig.get().entities.iter()
                            .filter(|e| e.id != eid.get())
                            .map(|e| view! { <option value=e.id.clone()>{e.name.clone()}</option> })
                            .collect_view()
                    }}
                </select>
                <Sel sig=new_r_strength options=opts(STRENGTHS)/>
                <Sel sig=new_r_ondelete options=opts(ON_DELETE)/>
                <button style=btn(true)
                    on:click=move |_: leptos::ev::MouseEvent| {
                        let field = new_r_field.get().trim().to_string();
                        let target = new_r_target.get();
                        if field.is_empty() || target.is_empty() { return; }
                        let strength = new_r_strength.get();
                        let on_delete = new_r_ondelete.get();
                        let eid = eid.get_untracked();
                        model_sig.update(|m| {
                            if let Some(e) = m.entities.iter_mut().find(|e| e.id == eid) {
                                e.relationships.push(SRel {
                                    id: new_uuid(),
                                    source_field_name: field.clone(),
                                    target_entity_id: target.clone(),
                                    cardinality: default_cardinality(),
                                    strength: strength.clone(),
                                    on_delete: Some(on_delete.clone()),
                                    required: false,
                                });
                            }
                        });
                        new_r_field.set(String::new());
                    }>"Add reference"</button>
            </div>
            <p style="font-size:11px; color:#667; margin-top:6px;">
                "A reference becomes a real typed FK column (§5.7) — pick the target entity and the on-delete behavior."
            </p>
        </div>
    }
}

/// One field row. New fields are fully editable; active fields render
/// read-only (a transform would be rejected at publish) with a retire button.
#[component]
fn FieldRow(
    model_sig: RwSignal<SModel>,
    eid: String,
    fid: String,
    active: RwSignal<ActiveIds>,
) -> impl IntoView {
    let eid = create_rw_signal(eid);
    let fid = create_rw_signal(fid);
    let is_active = move || active.get().fields.contains(&fid.get());
    let initial = field_of(
        &model_sig.get_untracked(),
        &eid.get_untracked(),
        &fid.get_untracked(),
    )
    .unwrap_or(SField {
        id: fid.get_untracked(),
        name: String::new(),
        label: None,
        field_type: "string".into(),
        required: false,
        is_unique: false,
        is_indexed: false,
        config: json!({}),
    });

    let name_sig = create_rw_signal(initial.name.clone());
    let label_sig = create_rw_signal(initial.label.clone().unwrap_or_default());
    let type_sig = create_rw_signal(initial.field_type.clone());
    let req_sig = create_rw_signal(initial.required);
    let uniq_sig = create_rw_signal(initial.is_unique);
    let idx_sig = create_rw_signal(initial.is_indexed);
    let enum_sig = create_rw_signal(
        initial.config["options"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default(),
    );

    // Mirror the bound widgets into the draft model on every change.
    {
        let ms = model_sig;
        let (eid2, fid2) = (eid, fid);
        let (n, l, t, r, u, i, e) = (
            name_sig, label_sig, type_sig, req_sig, uniq_sig, idx_sig, enum_sig,
        );
        create_effect(move |_| {
            let (n, l, t, r, u, i, e) = (
                n.get(),
                l.get(),
                t.get(),
                r.get(),
                u.get(),
                i.get(),
                e.get(),
            );
            ms.update(|m| {
                if let Some(ent) = m.entities.iter_mut().find(|x| x.id == eid2.get_untracked()) {
                    if let Some(f) = ent.fields.iter_mut().find(|x| x.id == fid2.get_untracked()) {
                        f.name = n.clone();
                        f.label = if l.is_empty() { None } else { Some(l.clone()) };
                        f.field_type = t.clone();
                        f.required = r;
                        f.is_unique = u;
                        f.is_indexed = i;
                        if t == "enum" {
                            f.config["options"] = json!(e
                                .split(',')
                                .map(|s| s.trim().to_string())
                                .filter(|s| !s.is_empty())
                                .collect::<Vec<_>>());
                        }
                    }
                }
            });
        });
    }

    let read_name = {
        let ms = model_sig;
        move || {
            field_of(&ms.get(), &eid.get(), &fid.get())
                .map(|f| f.name)
                .unwrap_or_default()
        }
    };
    let read_label = {
        let ms = model_sig;
        move || {
            field_of(&ms.get(), &eid.get(), &fid.get())
                .map(|f| f.label.unwrap_or_default())
                .unwrap_or_default()
        }
    };
    let read_type = {
        let ms = model_sig;
        move || {
            field_of(&ms.get(), &eid.get(), &fid.get())
                .map(|f| f.field_type)
                .unwrap_or_default()
        }
    };

    view! {
        <div style=row_style()>
            <Show
                when=move || !is_active()
                fallback=move || view! {
                    <span style="min-width:130px;">{move || read_name()}</span>
                    <span style="min-width:130px; color:#667;">{move || read_label()}</span>
                    <span style="min-width:90px; color:#2563eb; font-size:12px;">{move || read_type()}</span>
                }.into_view()>
                <span style="display:flex; gap:6px; align-items:center; flex-wrap:wrap;">
                    <Txt sig=name_sig ph="name" w="130px"/>
                    <Txt sig=label_sig ph="label" w="130px"/>
                    <Sel sig=type_sig options=opts(FIELD_TYPES)/>
                    {lbl("required")} <Chk sig=req_sig/>
                    {lbl("unique")} <Chk sig=uniq_sig/>
                    {lbl("indexed")} <Chk sig=idx_sig/>
                    {move || if type_sig.get() == "enum" {
                        view! {
                            <span style="display:flex; gap:4px; align-items:center;">
                                {lbl("options")} <Txt sig=enum_sig ph="A, B, C" w="140px"/>
                            </span>
                        }.into_view()
                    } else { ().into_view() }}
                </span>
            </Show>
            <span style="flex:1;"></span>
            <button style=del_btn()
                on:click=move |_: leptos::ev::MouseEvent| {
                    let (eid, fid) = (eid.get_untracked(), fid.get_untracked());
                    model_sig.update(|m| {
                        if let Some(e) = m.entities.iter_mut().find(|e| e.id == eid) {
                            e.fields.retain(|f| f.id != fid);
                        }
                    });
                }>{move || if is_active() { "retire" } else { "×" }.to_string()}</button>
        </div>
    }
}

#[component]
fn RelRow(
    model_sig: RwSignal<SModel>,
    eid: String,
    rid: String,
    active: RwSignal<ActiveIds>,
) -> impl IntoView {
    let eid = create_rw_signal(eid);
    let rid = create_rw_signal(rid);
    let is_active = move || active.get().rels.contains(&rid.get());
    let read_field = {
        let ms = model_sig;
        move || rel_of(&ms.get(), &eid.get(), &rid.get())
    };
    let read_field2 = {
        let ms = model_sig;
        move || rel_of(&ms.get(), &eid.get(), &rid.get())
    };
    let target_name = {
        let ms = model_sig;
        move || {
            let target = read_field2().map(|r| r.target_entity_id.clone());
            ms.get()
                .entities
                .iter()
                .find(|e| Some(e.id.clone()) == target)
                .map(|e| e.name.clone())
                .unwrap_or_else(|| "?".to_string())
        }
    };

    view! {
        <div style=row_style()>
            <span style="min-width:140px;">{move || read_field().map(|r| r.source_field_name).unwrap_or_default()}</span>
            <span>{"→ "}{move || target_name()}</span>
            <span style="color:#667; font-size:12px;">{move || read_field().map(|r| r.strength).unwrap_or_default()}</span>
            <span style="color:#667; font-size:12px;">
                {"on delete: "}{move || read_field().and_then(|r| r.on_delete).unwrap_or_else(|| "restrict".to_string())}
            </span>
            <span style="flex:1;"></span>
            <button style=del_btn()
                on:click=move |_: leptos::ev::MouseEvent| {
                    let (eid, rid) = (eid.get_untracked(), rid.get_untracked());
                    model_sig.update(|m| {
                        if let Some(e) = m.entities.iter_mut().find(|e| e.id == eid) {
                            e.relationships.retain(|r| r.id != rid);
                        }
                    });
                }>{move || if is_active() { "retire" } else { "×" }.to_string()}</button>
        </div>
    }
}

// ===== Pages designer: forms / views / dashboards / navigation =====

#[component]
fn StudioPages() -> impl IntoView {
    let tab = create_rw_signal(0usize);
    view! {
        <div>
            <Tabs sig=tab labels=vec!["Forms", "Views", "Dashboards", "Navigation"]/>
            {move || match tab.get() {
                0 => view! { <FormsTab/> }.into_view(),
                1 => view! { <ViewsTab/> }.into_view(),
                2 => view! { <DashboardsTab/> }.into_view(),
                _ => view! { <NavigationTab/> }.into_view(),
            }}
        </div>
    }
}

/// One editable pick-list row shared by the form/view designers: a model
/// field with an include toggle, a label override, and (forms) a widget.
/// Bound to the field *name* (stable identity), never a positional index —
/// `For` reuses same-key children across list edits, so an index captured at
/// creation goes stale as soon as a row above is removed.
#[derive(Clone)]
struct FieldPick {
    name: String,
    included: bool,
    label: String,
    widget: String,
}

/// Load the stored form/view definition for an entity into editor rows
/// (default: every model field included, widget auto, model labels).
fn seed_picks(fields: &[(String, String)], stored: Option<&Value>) -> Vec<FieldPick> {
    let mut used: Vec<String> = Vec::new();
    let mut rows: Vec<FieldPick> = Vec::new();
    let empty = vec![];
    let stored_fields = stored
        .and_then(|s| {
            s["sections"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(|sec| sec["fields"].as_array().cloned())
                .or_else(|| s["columns"].as_array().cloned())
        })
        .unwrap_or(empty);
    for f in stored_fields.iter() {
        let name = f["name"].as_str().unwrap_or_default().to_string();
        if let Some((_, model_label)) = fields.iter().find(|(n, _)| *n == name) {
            used.push(name.clone());
            rows.push(FieldPick {
                label: f["label"].as_str().unwrap_or(model_label).to_string(),
                widget: f["widget"].as_str().unwrap_or("auto").to_string(),
                name,
                included: true,
            });
        }
    }
    for (n, model_label) in fields {
        if !used.contains(n) {
            rows.push(FieldPick {
                name: n.clone(),
                label: model_label.clone(),
                widget: "auto".to_string(),
                included: stored.is_none(),
            });
        }
    }
    rows
}

#[component]
fn PickRow(rows: RwSignal<Vec<FieldPick>>, name: String, widgets: bool) -> impl IntoView {
    let read = {
        let name = name.clone();
        move || rows.get().iter().find(|r| r.name == name).cloned()
    };
    let set = {
        let name = name.clone();
        move |f: &dyn Fn(&mut FieldPick)| {
            rows.update(|rs| {
                if let Some(r) = rs.iter_mut().find(|r| r.name == name) {
                    f(r);
                }
            })
        }
    };
    let inc = create_rw_signal(read().map(|r| r.included).unwrap_or(false));
    let lab = create_rw_signal(read().map(|r| r.label).unwrap_or_default());
    let wid = create_rw_signal(read().map(|r| r.widget).unwrap_or_default());
    {
        let (inc, lab, wid) = (inc, lab, wid);
        create_effect(move |_| {
            let (i, l, w) = (inc.get(), lab.get(), wid.get());
            set(&move |r| {
                r.included = i;
                r.label = l.clone();
                r.widget = w.clone();
            });
        });
    }
    let name = read().map(|r| r.name).unwrap_or_default();
    view! {
        <div style=row_style()>
            <Chk sig=inc/>
            <span style="min-width:150px;">{name.clone()}</span>
            <Txt sig=lab ph="label" w="150px"/>
            {move || if widgets {
                view! { <Sel sig=wid options=opts(WIDGETS)/> }.into_view()
            } else { ().into_view() }}
        </div>
    }
}

#[component]
fn FormsTab() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let token = state.token;
    let entity = create_rw_signal(String::new());
    let rows = create_rw_signal(Vec::<FieldPick>::new());
    let section_title = create_rw_signal(String::new());
    let msg = create_rw_signal(None::<(bool, String)>);

    let load = move |ent: String| {
        let t = token.get().unwrap_or_default();
        spawn_local(async move {
            let fields = entity_field_list(&state, &ent);
            match api::sget(&t, &format!("/api/forms/{ent}")).await {
                Ok(stored) => {
                    section_title.set(
                        stored["sections"][0]["title"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                    );
                    rows.set(seed_picks(&fields, Some(&stored)));
                }
                Err(_) => {
                    section_title.set(String::new());
                    rows.set(seed_picks(&fields, None));
                }
            }
        });
    };

    view! {
        <div>
            <MsgLine sig=msg/>
            <div style="display:flex; gap:8px; align-items:center; margin-bottom:10px;">
                {lbl("entity")}
                {move || {
                    let e = entity;
                    let load = load;
                    view! {
                        <select prop:value=move || e.get()
                            on:input=move |ev| {
                                let v = event_target_value(&ev);
                                e.set(v.clone());
                                load(v);
                            }
                            style=fmt_style("200px")>
                            <option value="">"— entity —"</option>
                            {entity_options(&state).into_iter()
                                .map(|(v, l)| view! { <option value=v.clone()>{l}</option> })
                                .collect_view()}
                        </select>
                    }
                }}
            </div>
            {move || {
                let ent = entity.get();
                if ent.is_empty() {
                    return view! { <p style="color:#889;">"Pick an entity to design its form."</p> }.into_view();
                }
                // Track ONLY `entity` in this outer closure (see the NB below
                // inside the For). Reading `rows` here would re-run it on every
                // PickRow edit-effect write, tearing down + recreating the
                // keyed rows in a loop until the runtime panics (OwnerDisposed)
                // — found by browser test.
                let rows_c = rows;
                let entity_c = entity;
                view! {
                    <div>
                        <div style="display:flex; gap:8px; align-items:center; margin:6px 0;">
                            {lbl("section title")} <Txt sig=section_title ph="e.g. Details" w="180px"/>
                        </div>
                        <For each=move || {
                            let rows_c = rows_c;
                            let ent = entity_c.get();
                            (0..rows_c.get().len())
                                .map(|i| (ent.clone(), rows_c.get()[i].name.clone()))
                                .collect::<Vec<(String, String)>>()
                        }
                             // The entity is part of the key: switching entity
                             // remounts the rows with fresh values instead of
                             // reusing same-name children whose input signals
                             // still hold the previous entity's data.
                             key=|(ent, name)| format!("{ent}::{name}")
                             children=move |(_, name)| {
                                 view! { <PickRow rows=rows name=name widgets=true/> }
                             }/>
                        <div style="margin-top:10px;">
                            <button style=btn(true)
                                on:click=move |_: leptos::ev::MouseEvent| {
                                    let t = token.get().unwrap_or_default();
                                    let ent = entity.get();
                                    let title = section_title.get();
                                    let picks = rows.get().iter()
                                        .filter(|r| r.included)
                                        .map(|r| {
                                            let mut f = json!({"name": r.name});
                                            if r.label != r.name && !r.label.is_empty() {
                                                f["label"] = json!(r.label.clone());
                                            }
                                            if r.widget != "auto" {
                                                f["widget"] = json!(r.widget.clone());
                                            }
                                            f
                                        })
                                        .collect::<Vec<_>>();
                                    let body = json!({
                                        "name": "default",
                                        "layout": {"sections": [{"title": if title.is_empty() { None } else { Some(title) }, "fields": picks}]}
                                    });
                                    let msg = msg;
                                    spawn_local(async move {
                                        match api::spost(&t, &format!("/api/forms/{ent}"), body).await {
                                            Ok(_) => set_msg(&msg, true, "form saved"),
                                            Err(e) => set_msg(&msg, false, e),
                                        }
                                    });
                                }>"Save form"</button>
                            <button style=btn(false)
                                on:click=move |_: leptos::ev::MouseEvent| {
                                    let t = token.get().unwrap_or_default();
                                    let ent = entity.get();
                                    let msg = msg;
                                    spawn_local(async move {
                                        match api::sdelete(&t, &format!("/api/forms/{ent}/default")).await {
                                            Ok(_) => set_msg(&msg, true, "form definition removed (runtime falls back to the model)"),
                                            Err(e) => set_msg(&msg, false, e),
                                        }
                                    });
                                }>"Delete form"</button>
                            <span style="font-size:12px; color:#667; margin-left:8px;">
                                {move || format!("{} fields included", rows.get().iter().filter(|r| r.included).count())}
                            </span>
                        </div>
                    </div>
                }.into_view()
            }}
        </div>
    }
}

#[component]
fn ViewsTab() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let token = state.token;
    let entity = create_rw_signal(String::new());
    let rows = create_rw_signal(Vec::<FieldPick>::new());
    let page_size = create_rw_signal(String::new());
    let msg = create_rw_signal(None::<(bool, String)>);

    let load = move |ent: String| {
        let t = token.get().unwrap_or_default();
        let (rows, page_size) = (rows, page_size);
        spawn_local(async move {
            let fields = entity_field_list(&state, &ent);
            match api::sget(&t, &format!("/api/views/{ent}")).await {
                Ok(stored) => {
                    page_size.set(
                        stored["page_size"]
                            .as_i64()
                            .map(|v| v.to_string())
                            .unwrap_or_default(),
                    );
                    rows.set(seed_picks(&fields, Some(&stored)));
                }
                Err(_) => {
                    page_size.set(String::new());
                    rows.set(seed_picks(&fields, None));
                }
            }
        });
    };

    view! {
        <div>
            <MsgLine sig=msg/>
            <div style="display:flex; gap:8px; align-items:center; margin-bottom:10px;">
                {lbl("entity")}
                {move || {
                    let e = entity;
                    let load = load;
                    view! {
                        <select prop:value=move || e.get()
                            on:input=move |ev| {
                                let v = event_target_value(&ev);
                                e.set(v.clone());
                                load(v);
                            }
                            style=fmt_style("200px")>
                            <option value="">"— entity —"</option>
                            {entity_options(&state).into_iter()
                                .map(|(v, l)| view! { <option value=v.clone()>{l}</option> })
                                .collect_view()}
                        </select>
                    }
                }}
            </div>
            {move || {
                let ent = entity.get();
                if ent.is_empty() {
                    return view! { <p style="color:#889;">"Pick an entity to design its list view."</p> }.into_view();
                }
                // Track ONLY `entity` here (see the FormsTab note above).
                let rows_c = rows;
                let entity_c = entity;
                view! {
                    <div>
                        <For each=move || {
                            let rows_c = rows_c;
                            let ent = entity_c.get();
                            (0..rows_c.get().len())
                                .map(|i| (ent.clone(), rows_c.get()[i].name.clone()))
                                .collect::<Vec<(String, String)>>()
                        }
                             // Entity-scoped key: rows remount (with fresh
                             // values) when the designed entity changes.
                             key=|(ent, name)| format!("{ent}::{name}")
                             children=move |(_, name)| {
                                 view! { <PickRow rows=rows name=name widgets=false/> }
                             }/>
                        <div style="display:flex; gap:8px; align-items:center; margin-top:10px;">
                            {lbl("page size")} <Num sig=page_size w="80px"/>
                            <button style=btn(true)
                                on:click=move |_: leptos::ev::MouseEvent| {
                                    let t = token.get().unwrap_or_default();
                                    let ent = entity.get();
                                    let picks = rows.get().iter()
                                        .filter(|r| r.included)
                                        .map(|r| {
                                            let mut c = json!({"field": r.name});
                                            if r.label != r.name && !r.label.is_empty() {
                                                c["label"] = json!(r.label.clone());
                                            }
                                            c
                                        })
                                        .collect::<Vec<_>>();
                                    let mut body = json!({"name": "default", "columns": picks});
                                    if let Ok(n) = page_size.get().parse::<i64>() {
                                        body["page_size"] = json!(n);
                                    }
                                    let msg = msg;
                                    spawn_local(async move {
                                        match api::spost(&t, &format!("/api/views/{ent}"), body).await {
                                            Ok(_) => set_msg(&msg, true, "view saved"),
                                            Err(e) => set_msg(&msg, false, e),
                                        }
                                    });
                                }>"Save view"</button>
                            <button style=btn(false)
                                on:click=move |_: leptos::ev::MouseEvent| {
                                    let t = token.get().unwrap_or_default();
                                    let ent = entity.get();
                                    let msg = msg;
                                    spawn_local(async move {
                                        match api::sdelete(&t, &format!("/api/views/{ent}/default")).await {
                                            Ok(_) => set_msg(&msg, true, "view definition removed"),
                                            Err(e) => set_msg(&msg, false, e),
                                        }
                                    });
                                }>"Delete view"</button>
                        </div>
                    </div>
                }.into_view()
            }}
        </div>
    }
}

/// One dashboard tile row (report + title).
#[derive(Clone)]
struct TileRow {
    id: String,
    report_id: String,
    title: String,
}

#[component]
fn DashboardsTab() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let token = state.token;
    let reports = create_rw_signal(Vec::<(String, String)>::new()); // (id, name)
    let tiles = create_rw_signal(Vec::<TileRow>::new());
    let name = create_rw_signal(String::new());
    let label = create_rw_signal(String::new());
    let existing = create_rw_signal(Vec::<Value>::new());
    let msg = create_rw_signal(None::<(bool, String)>);

    let reload = move || {
        let t = token.get().unwrap_or_default();
        let (reports, existing) = (reports, existing);
        spawn_local(async move {
            if let Ok(list) = api::sget(&t, "/api/reports").await {
                reports.set(
                    list.as_array()
                        .map(|a| {
                            a.iter()
                                .map(|r| {
                                    (
                                        r["id"].as_str().unwrap_or_default().to_string(),
                                        r["name"].as_str().unwrap_or_default().to_string(),
                                    )
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                );
            }
            if let Ok(list) = api::sget(&t, "/api/dashboards").await {
                existing.set(list.as_array().cloned().unwrap_or_default());
            }
        });
    };
    reload();

    view! {
        <div>
            <MsgLine sig=msg/>
            {h3("Existing dashboards")}
            <For each=move || existing.get()
                 key=|d| d["id"].as_str().unwrap_or_default().to_string()
                 children=move |d: Value| {
                     let token = token;
                     let msg = msg;
                     let existing = existing;
                     let id = d["id"].as_str().unwrap_or_default().to_string();
                     view! {
                         <div style=row_style()>
                             <strong>{d["label"].as_str().unwrap_or_default().to_string()}</strong>
                             <span style="color:#667; font-size:12px;">
                                 {format!("{} tile(s)", d["items"].as_array().map(|a| a.len()).unwrap_or(0))}
                             </span>
                             <span style="flex:1;"></span>
                             <button style=del_btn()
                                 on:click=move |_: leptos::ev::MouseEvent| {
                                     let t = token.get().unwrap_or_default();
                                     let msg = msg;
                                     let existing = existing;
                                     let id = id.clone();
                                     spawn_local(async move {
                                         match api::sdelete(&t, &format!("/api/dashboards/{id}")).await {
                                             Ok(_) => {
                                                 set_msg(&msg, true, "dashboard deleted");
                                                 if let Ok(l) = api::sget(&t, "/api/dashboards").await {
                                                     existing.set(l.as_array().cloned().unwrap_or_default());
                                                 }
                                             }
                                             Err(e) => set_msg(&msg, false, e),
                                         }
                                     });
                                 }>"delete"</button>
                         </div>
                     }
                 }/>

            {h3("New / replace dashboard")}
            <div style="display:flex; gap:8px; align-items:center; margin-bottom:8px;">
                {lbl("name (unique)")} <Txt sig=name ph="sales" w="140px"/>
                {lbl("label")} <Txt sig=label ph="Sales overview" w="180px"/>
            </div>
            <For each=move || {
                let list: Vec<String> = tiles.get().iter().map(|t| t.id.clone()).collect();
                list
            }
                 key=|rid| rid.clone()
                 children=move |rid| {
                     let tiles2 = tiles;
                     let reports2 = reports;
                     // Children are keyed by id and reused across edits, so all
                     // reads/writes resolve the row by id — never by position.
                     let title_sig = create_rw_signal(
                         tiles2.get_untracked().iter().find(|t| t.id == rid)
                             .map(|t| t.title.clone()).unwrap_or_default(),
                     );
                     let title_sig2 = title_sig;
                     let rid_eff = rid.clone();
                     create_effect(move |_| {
                         let v = title_sig2.get();
                         tiles2.update(|ts| { if let Some(t) = ts.iter_mut().find(|t| t.id == rid_eff) { t.title = v.clone(); } });
                     });
                     let rid_sel = rid.clone();
                     let rid_inp = rid.clone();
                     view! {
                         <div style=row_style()>
                             {lbl("report")}
                             <select prop:value=move || tiles2.get().iter().find(|t| t.id == rid_sel).map(|t| t.report_id.clone()).unwrap_or_default()
                                 on:input=move |ev| {
                                     let v = event_target_value(&ev);
                                     tiles2.update(|ts| { if let Some(t) = ts.iter_mut().find(|t| t.id == rid_inp) { t.report_id = v; } });
                                 }
                                 style=fmt_style("auto")>
                                 <option value="">"— report —"</option>
                                 {move || reports2.get().into_iter()
                                     .map(|(id, n)| view! { <option value=id.clone()>{n.clone()}</option> })
                                     .collect_view()}
                             </select>
                             {lbl("tile title")} <Txt sig=title_sig ph="optional" w="160px"/>
                             <button style=del_btn()
                                 on:click=move |_: leptos::ev::MouseEvent| {
                                     let rid = rid.clone();
                                     tiles2.update(|ts| { ts.retain(|t| t.id != rid); });
                                 }>"×"</button>
                         </div>
                     }
                 }/>
            <div style="margin-top:8px;">
                <button style=btn(false)
                    on:click=move |_: leptos::ev::MouseEvent| {
                        tiles.update(|ts| ts.push(TileRow { id: new_uuid(), report_id: String::new(), title: String::new() }));
                    }>"+ tile"</button>
                <button style=btn(true)
                    on:click=move |_: leptos::ev::MouseEvent| {
                        let t = token.get().unwrap_or_default();
                        let name = name.get();
                        let label = label.get();
                        let items = tiles.get().iter()
                            .map(|tile| json!({"report_id": tile.report_id, "title": if tile.title.is_empty() { tile.report_id.clone() } else { tile.title.clone() }}))
                            .collect::<Vec<_>>();
                        let msg = msg;
                        let existing = existing;
                        spawn_local(async move {
                            match api::spost(&t, "/api/dashboards", json!({"name": name, "label": label, "items": items})).await {
                                Ok(_) => {
                                    set_msg(&msg, true, "dashboard saved");
                                    if let Ok(l) = api::sget(&t, "/api/dashboards").await {
                                        existing.set(l.as_array().cloned().unwrap_or_default());
                                    }
                                }
                                Err(e) => set_msg(&msg, false, e),
                            }
                        });
                    }>"Save dashboard"</button>
            </div>
            <p style="font-size:11px; color:#667; margin-top:6px;">
                "Every tile's report runs under the viewing user's security — a dashboard is a saved lens, not a stored result set."
            </p>
        </div>
    }
}

/// One navigation item row.
#[derive(Clone)]
struct NavRow {
    id: String,
    kind: String, // entity | link
    entity: String,
    url: String,
    label: String,
}

#[component]
fn NavigationTab() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let token = state.token;
    let rows = create_rw_signal(Vec::<NavRow>::new());
    let msg = create_rw_signal(None::<(bool, String)>);

    // prefill from the current (permission-filtered) navigation
    {
        let t = token.get().unwrap_or_default();
        spawn_local(async move {
            if let Ok(nav) = api::get_navigation(&t).await {
                rows.set(
                    nav.into_iter()
                        .map(|i| NavRow {
                            id: new_uuid(),
                            // Preserve the fetched kind: a stored link item
                            // must prefill as a link, not silently become an
                            // entity row with an empty target.
                            kind: if i.kind == "entity" {
                                "entity".into()
                            } else {
                                "link".into()
                            },
                            entity: i.entity.clone().unwrap_or_default(),
                            url: i.url.clone().unwrap_or_default(),
                            label: i.label.clone(),
                        })
                        .collect(),
                );
            }
        });
    }

    view! {
        <div>
            <MsgLine sig=msg/>
            <For each=move || {
                let list: Vec<String> = rows.get().iter().map(|r| r.id.clone()).collect();
                list
            }
                 key=|rid| rid.clone()
                 children=move |rid| {
                     let rows2 = rows;
                     let state2 = state;
                     // Keyed children are reused across edits/removals, so the
                     // row is always resolved by id — never by position.
                     let row_by_id = {
                         let rid = rid.clone();
                         move || rows2.get().iter().find(|r| r.id == rid).cloned()
                     };
                     let kind_sig = create_rw_signal(row_by_id().map(|r| r.kind.clone()).unwrap_or_default());
                     let label_sig = create_rw_signal(row_by_id().map(|r| r.label.clone()).unwrap_or_default());
                     let label_sig2 = label_sig;
                     let rid_lab = rid.clone();
                     create_effect(move |_| {
                         let v = label_sig2.get();
                         rows2.update(|rs| { if let Some(r) = rs.iter_mut().find(|r| r.id == rid_lab) { r.label = v.clone(); } });
                     });
                     let rid_kind = rid.clone();
                     let rid_url0 = rid.clone();
                     let rid_url = rid.clone();
                     let rid_ent = rid.clone();
                     let rid_ent_sel = rid.clone();
                     view! {
                         <div style=row_style()>
                             <select prop:value=move || kind_sig.get()
                                 on:input=move |ev| {
                                     let v = event_target_value(&ev);
                                     kind_sig.set(v.clone());
                                     rows2.update(|rs| { if let Some(r) = rs.iter_mut().find(|r| r.id == rid_kind) { r.kind = v; } });
                                 }
                                 style=fmt_style("auto")>
                                 <option value="entity">"entity"</option>
                                 <option value="link">"link"</option>
                             </select>
                             {move || if kind_sig.get() == "link" {
                                 // Per-run clone: this reactive closure re-runs on
                                 // every kind toggle and must stay `Fn`.
                                 let rid_url = rid_url.clone();
                                 let url_sig = create_rw_signal(
                                     rows2.get_untracked().iter().find(|r| r.id == rid_url0)
                                         .map(|r| r.url.clone()).unwrap_or_default(),
                                 );
                                 let url_sig2 = url_sig;
                                 create_effect(move |_| {
                                     let v = url_sig2.get();
                                     rows2.update(|rs| { if let Some(r) = rs.iter_mut().find(|r| r.id == rid_url) { r.url = v.clone(); } });
                                 });
                                 view! { <Txt sig=url_sig ph="https://…" w="260px"/> }.into_view()
                             } else {
                                 let rid_ent = rid_ent.clone();
                                 let rid_ent_sel = rid_ent_sel.clone();
                                 view! {
                                     <select prop:value=move || rows2.get().iter().find(|r| r.id == rid_ent_sel).map(|r| r.entity.clone()).unwrap_or_default()
                                         on:input=move |ev| {
                                             let v = event_target_value(&ev);
                                             rows2.update(|rs| { if let Some(r) = rs.iter_mut().find(|r| r.id == rid_ent) { r.entity = v; } });
                                         }
                                         style=fmt_style("auto")>
                                         <option value="">"— entity —"</option>
                                         {entity_options(&state2).into_iter()
                                             .map(|(v, l)| view! { <option value=v.clone()>{l}</option> })
                                             .collect_view()}
                                     </select>
                                 }.into_view()
                             }}
                             {lbl("label")} <Txt sig=label_sig ph="menu label" w="160px"/>
                             <button style=del_btn()
                                 on:click=move |_: leptos::ev::MouseEvent| {
                                     let rid = rid.clone();
                                     rows2.update(|rs| { rs.retain(|r| r.id != rid); });
                                 }>"×"</button>
                         </div>
                     }
                 }/>
            <div style="margin-top:8px; display:flex; gap:8px;">
                <button style=btn(false)
                    on:click=move |_: leptos::ev::MouseEvent| {
                        rows.update(|rs| rs.push(NavRow {
                            id: new_uuid(),
                            kind: "entity".into(), entity: String::new(), url: String::new(), label: String::new(),
                        }));
                    }>"+ item"</button>
                <button style=btn(true)
                    on:click=move |_: leptos::ev::MouseEvent| {
                        let t = token.get().unwrap_or_default();
                        let items = rows.get().iter().map(|r| {
                            if r.kind == "link" {
                                json!({"type": "link", "url": r.url, "label": r.label})
                            } else {
                                json!({"type": "entity", "entity": r.entity, "label": r.label})
                            }
                        }).collect::<Vec<_>>();
                        let msg = msg;
                        spawn_local(async move {
                            match api::spost(&t, "/api/navigation", json!({"items": items})).await {
                                Ok(_) => set_msg(&msg, true, "navigation saved (re-login or refresh to see it)"),
                                Err(e) => set_msg(&msg, false, e),
                            }
                        });
                    }>"Save navigation"</button>
                <button style=btn(false)
                    on:click=move |_: leptos::ev::MouseEvent| {
                        let t = token.get().unwrap_or_default();
                        let msg = msg;
                        spawn_local(async move {
                            match api::sdelete(&t, "/api/navigation/default").await {
                                Ok(_) => set_msg(&msg, true, "navigation reset to the default menu"),
                                Err(e) => set_msg(&msg, false, e),
                            }
                        });
                    }>"Reset to default"</button>
            </div>
            <p style="font-size:11px; color:#667; margin-top:6px;">
                "Entity items are permission-filtered per viewer: an entity a user cannot read never appears in their menu."
            </p>
        </div>
    }
}

// ===== Report designer =====

#[derive(Clone)]
struct SelectRow {
    id: String,
    field: String,
    aggregate: String,
    alias: String,
}

#[derive(Clone)]
struct FilterRow {
    id: String,
    field: String,
    op: String,
    value: String,
}

#[component]
fn StudioReports() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let token = state.token;
    let reports = create_rw_signal(Vec::<Value>::new());
    let msg = create_rw_signal(None::<(bool, String)>);
    let result = create_rw_signal(None::<Value>);

    // editor state
    let name = create_rw_signal(String::new());
    let entity = create_rw_signal(String::new());
    let selects = create_rw_signal(Vec::<SelectRow>::new());
    let filters = create_rw_signal(Vec::<FilterRow>::new());
    let group_by = create_rw_signal(String::new());
    let limit = create_rw_signal(String::new());

    let reload = move || {
        let t = token.get().unwrap_or_default();
        let reports = reports;
        spawn_local(async move {
            match api::sget(&t, "/api/reports").await {
                Ok(list) => reports.set(list.as_array().cloned().unwrap_or_default()),
                Err(e) => set_msg(&msg, false, e),
            }
        });
    };
    reload();

    view! {
        <div>
            <MsgLine sig=msg/>
            {h3("Saved reports")}
            <For each=move || reports.get()
                 key=|r| r["id"].as_str().unwrap_or_default().to_string()
                 children=move |r: Value| {
                     let token = token;
                     let msg = msg;
                     let result = result;
                     let reports = reports;
                     let id_run = r["id"].as_str().unwrap_or_default().to_string();
                     let id_del = r["id"].as_str().unwrap_or_default().to_string();
                     let rname = r["name"].as_str().unwrap_or_default().to_string();
                     let base = r["dataset"]["base_entity"].as_str().unwrap_or_default().to_string();
                     view! {
                         <div style=row_style()>
                             <strong style="min-width:160px;">{rname.clone()}</strong>
                             <span style="color:#667; font-size:12px;">{format!("on {base}")}</span>
                             <span style="flex:1;"></span>
                             <button style=btn(false)
                                 on:click=move |_: leptos::ev::MouseEvent| {
                                     let t = token.get().unwrap_or_default();
                                     let result = result;
                                     let msg = msg;
                                     let id = id_run.clone();
                                     spawn_local(async move {
                                         match api::sget(&t, &format!("/api/reports/{id}/run")).await {
                                             Ok(res) => result.set(Some(res)),
                                             Err(e) => set_msg(&msg, false, e),
                                         }
                                     });
                                 }>"Run"</button>
                             <button style=del_btn()
                                 on:click=move |_: leptos::ev::MouseEvent| {
                                     let t = token.get().unwrap_or_default();
                                     let msg = msg;
                                     let reports = reports;
                                     let id = id_del.clone();
                                     spawn_local(async move {
                                         match api::sdelete(&t, &format!("/api/reports/{id}")).await {
                                             Ok(_) => {
                                                 set_msg(&msg, true, "report deleted");
                                                 if let Ok(l) = api::sget(&t, "/api/reports").await {
                                                     reports.set(l.as_array().cloned().unwrap_or_default());
                                                 }
                                             }
                                             Err(e) => set_msg(&msg, false, e),
                                         }
                                     });
                                 }>"delete"</button>
                         </div>
                     }
                 }/>
            {move || match result.get() {
                None => ().into_view(),
                Some(res) => view! {
                    <div style=card_style()>
                        <strong style="font-size:13px;">"Result"</strong>
                        {value_table(
                            &res["columns"].as_array().map(|a| a.iter().map(|c| c.as_str().unwrap_or_default().to_string()).collect::<Vec<_>>()).unwrap_or_default(),
                            res["rows"].as_array().cloned().unwrap_or_default().as_slice(),
                        )}
                    </div>
                }.into_view(),
            }}

            {h3("New / replace report")}
            <div style="display:flex; gap:8px; align-items:center; margin-bottom:8px; flex-wrap:wrap;">
                {lbl("name")} <Txt sig=name ph="sales-by-month" w="160px"/>
                {lbl("base entity")}
                <select prop:value=move || entity.get()
                    on:input=move |ev| {
                        entity.set(event_target_value(&ev));
                        selects.set(Vec::new());
                        filters.set(Vec::new());
                    }
                    style=fmt_style("160px")>
                    <option value="">"— entity —"</option>
                    {entity_options(&state).into_iter()
                        .map(|(v, l)| view! { <option value=v.clone()>{l}</option> })
                        .collect_view()}
                </select>
                {lbl("group by (comma-separated)")} <Txt sig=group_by ph="tier, status" w="160px"/>
                {lbl("limit")} <Num sig=limit w="70px"/>
            </div>

            {move || {
                let ent = entity.get();
                if ent.is_empty() {
                    return ().into_view();
                }
                let selects2 = selects;
                let filters2 = filters;
                view! {
                    <div>
                        <For each=move || {
                            let list: Vec<String> = selects2.get().iter().map(|r| r.id.clone()).collect();
                            list
                        }
                             key=|rid| rid.clone()
                             children=move |rid| {
                                 let sel3 = selects;
                                 let fields3 = entity_field_list(&state, &entity.get());
                                 // Keyed children are reused across edits, so
                                 // all reads/writes resolve the row by id — a
                                 // captured index would go stale after any
                                 // removal above.
                                 let rid_field = rid.clone();
                                 let rid_inp = rid.clone();
                                 let rid_del = rid.clone();
                                 view! {
                                     <div style=row_style()>
                                         {lbl("select")}
                                         <select prop:value=move || sel3.get().iter().find(|r| r.id == rid_field).map(|r| r.field.clone()).unwrap_or_default()
                                             on:input=move |ev| {
                                                 let v = event_target_value(&ev);
                                                 sel3.update(|rs| { if let Some(r) = rs.iter_mut().find(|r| r.id == rid_inp) { r.field = v; } });
                                             }
                                             style=fmt_style("auto")>
                                             <option value="">"— field —"</option>
                                             {fields3.clone().into_iter()
                                                 .map(|(v, l)| view! { <option value=v.clone()>{l}</option> })
                                                 .collect_view()}
                                             <option value="*">"* (count only)"</option>
                                         </select>
                                         <AggSel rows=selects rid=rid.clone()/>
                                         <AliasInput rows=selects rid=rid/>
                                         <button style=del_btn()
                                             on:click=move |_: leptos::ev::MouseEvent| {
                                                 sel3.update(|rs| { rs.retain(|r| r.id != rid_del); });
                                             }>"×"</button>
                                     </div>
                                 }
                             }/>
                        <div style="margin:6px 0 12px;">
                            <button style=btn(false)
                                on:click=move |_: leptos::ev::MouseEvent| {
                                    selects.update(|rs| rs.push(SelectRow {
                                        id: new_uuid(), field: String::new(), aggregate: String::new(), alias: String::new(),
                                    }));
                                }>"+ column"</button>
                        </div>

                        <For each=move || {
                            let list: Vec<String> = filters2.get().iter().map(|r| r.id.clone()).collect();
                            list
                        }
                             key=|rid| rid.clone()
                             children=move |rid| {
                                 let fil3 = filters;
                                 let fields3 = entity_field_list(&state, &entity.get());
                                 let rid_field = rid.clone();
                                 let rid_field_inp = rid.clone();
                                 let rid_op_sel = rid.clone();
                                 let rid_op_inp = rid.clone();
                                 let rid_val = rid.clone();
                                 let rid_val_inp = rid.clone();
                                 let rid_del = rid.clone();
                                 view! {
                                     <div style=row_style()>
                                         {lbl("filter")}
                                         <select prop:value=move || fil3.get().iter().find(|r| r.id == rid_field).map(|r| r.field.clone()).unwrap_or_default()
                                             on:input=move |ev| {
                                                 let v = event_target_value(&ev);
                                                 fil3.update(|rs| { if let Some(r) = rs.iter_mut().find(|r| r.id == rid_field_inp) { r.field = v; } });
                                             }
                                             style=fmt_style("auto")>
                                             <option value="">"— field —"</option>
                                             {fields3.clone().into_iter()
                                                 .map(|(v, l)| view! { <option value=v.clone()>{l}</option> })
                                                 .collect_view()}
                                         </select>
                                         <select prop:value=move || fil3.get().iter().find(|r| r.id == rid_op_sel).map(|r| r.op.clone()).unwrap_or_default()
                                             on:input=move |ev| {
                                                 let v = event_target_value(&ev);
                                                 fil3.update(|rs| { if let Some(r) = rs.iter_mut().find(|r| r.id == rid_op_inp) { r.op = v; } });
                                             }
                                             style=fmt_style("auto")>
                                             {opts(FILTER_OPS).into_iter()
                                                 .map(|(v, l)| view! { <option value=v.clone()>{l}</option> })
                                                 .collect_view()}
                                         </select>
                                         <input placeholder="value" prop:value=move || fil3.get().iter().find(|r| r.id == rid_val).map(|r| r.value.clone()).unwrap_or_default()
                                             on:input=move |ev| {
                                                 let v = event_target_value(&ev);
                                                 fil3.update(|rs| { if let Some(r) = rs.iter_mut().find(|r| r.id == rid_val_inp) { r.value = v; } });
                                             }
                                             style=fmt_style("140px") />
                                         <button style=del_btn()
                                             on:click=move |_: leptos::ev::MouseEvent| {
                                                 fil3.update(|rs| { rs.retain(|r| r.id != rid_del); });
                                             }>"×"</button>
                                     </div>
                                 }
                             }/>
                        <div style="margin-top:6px;">
                            <button style=btn(false)
                                on:click=move |_: leptos::ev::MouseEvent| {
                                    filters.update(|rs| rs.push(FilterRow {
                                        id: new_uuid(), field: String::new(), op: "eq".into(), value: String::new(),
                                    }));
                                }>"+ filter"</button>
                            <button style=btn(true)
                                on:click=move |_: leptos::ev::MouseEvent| {
                                    let t = token.get().unwrap_or_default();
                                    let name = name.get();
                                    let fields_sel = selects.get().iter().map(|r| {
                                        let mut f = json!({"field": r.field});
                                        if !r.aggregate.is_empty() { f["aggregate"] = json!(r.aggregate.clone()); }
                                        if !r.alias.is_empty() { f["alias"] = json!(r.alias.clone()); }
                                        f
                                    }).collect::<Vec<_>>();
                                    let fil = filters.get().iter().map(|r| json!({
                                        "field": r.field, "op": r.op, "value": r.value
                                    })).collect::<Vec<_>>();
                                    let gb = group_by.get().split(',')
                                        .map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
                                        .collect::<Vec<_>>();
                                    let mut ds = json!({
                                        "base_entity": entity.get(),
                                        "fields": fields_sel,
                                        "filters": fil,
                                        "group_by": gb,
                                    });
                                    if let Ok(n) = limit.get().parse::<u64>() {
                                        ds["limit"] = json!(n);
                                    }
                                    let msg = msg;
                                    let reports = reports;
                                    spawn_local(async move {
                                        match api::spost(&t, "/api/reports", json!({"name": name, "dataset": ds})).await {
                                            Ok(_) => {
                                                set_msg(&msg, true, "report saved");
                                                if let Ok(l) = api::sget(&t, "/api/reports").await {
                                                    reports.set(l.as_array().cloned().unwrap_or_default());
                                                }
                                            }
                                            Err(e) => set_msg(&msg, false, e),
                                        }
                                    });
                                }>"Save report"</button>
                        </div>
                    </div>
                }.into_view()
            }}
            <p style="font-size:11px; color:#667; margin-top:8px;">
                "Reports run under the requesting user's security (object + field + record scope, per join hop). Fields may traverse references: “customer.name”."
            </p>
        </div>
    }
}

/// Aggregate select for a report column row (bound to the row's stable id).
#[component]
fn AggSel(rows: RwSignal<Vec<SelectRow>>, rid: String) -> impl IntoView {
    let rid_sel = rid.clone();
    let rid_inp = rid;
    view! {
        <select prop:value=move || rows.get().iter().find(|r| r.id == rid_sel).map(|r| r.aggregate.clone()).unwrap_or_default()
            on:input=move |ev| {
                let v = event_target_value(&ev);
                rows.update(|rs| { if let Some(r) = rs.iter_mut().find(|r| r.id == rid_inp) { r.aggregate = v; } });
            }
            style=fmt_style("auto")>
            {opts(AGGREGATES).into_iter()
                .map(|(v, l)| view! { <option value=v.clone()>{l}</option> })
                .collect_view()}
        </select>
    }
}

#[component]
fn AliasInput(rows: RwSignal<Vec<SelectRow>>, rid: String) -> impl IntoView {
    let rid_sel = rid.clone();
    let rid_inp = rid;
    view! {
        <input placeholder="alias" prop:value=move || rows.get().iter().find(|r| r.id == rid_sel).map(|r| r.alias.clone()).unwrap_or_default()
            on:input=move |ev| {
                let v = event_target_value(&ev);
                rows.update(|rs| { if let Some(r) = rs.iter_mut().find(|r| r.id == rid_inp) { r.alias = v; } });
            }
            style=fmt_style("120px") />
    }
}

// ===== Automation: rule editor + workflow designer =====

#[component]
fn StudioAutomation() -> impl IntoView {
    let tab = create_rw_signal(0usize);
    view! {
        <div>
            <Tabs sig=tab labels=vec!["Rules", "Workflows"]/>
            {move || match tab.get() {
                0 => view! { <RulesTab/> }.into_view(),
                _ => view! { <WorkflowsTab/> }.into_view(),
            }}
        </div>
    }
}

/// A condition/guard builder: "always" or `<field> <op> <literal>` → DSL JSON.
#[component]
fn CondBuilder(
    mode: RwSignal<String>,
    field: RwSignal<String>,
    op: RwSignal<String>,
    value: RwSignal<String>,
    fields: Vec<(String, String)>,
    prefix: &'static str,
) -> impl IntoView {
    let fields2 = fields.clone();
    view! {
        <span style="display:flex; gap:4px; align-items:center;">
            <Sel sig=mode options=vec![
                ("always".into(), if prefix.is_empty() { "always".to_string() } else { format!("{prefix}: always") }),
                ("when".into(), if prefix.is_empty() { "when…".to_string() } else { format!("{prefix}: when") }),
            ]/>
            {move || if mode.get() == "when" {
                view! {
                    <select prop:value=move || field.get()
                        on:input=move |ev| field.set(event_target_value(&ev))
                        style=fmt_style("auto")>
                        <option value="">"— field —"</option>
                        {fields2.clone().into_iter()
                            .map(|(v, l)| view! { <option value=v.clone()>{l}</option> })
                            .collect_view()}
                    </select>
                    <Sel sig=op options=cmp_opts()/>
                    <Txt sig=value ph="value" w="110px"/>
                }.into_view()
            } else { ().into_view() }}
        </span>
    }
}

#[component]
fn RulesTab() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let token = state.token;
    let rules = create_rw_signal(Vec::<Value>::new());
    let msg = create_rw_signal(None::<(bool, String)>);

    let ent = create_rw_signal(String::new());
    let event = create_rw_signal("after_update".to_string());
    let c_mode = create_rw_signal("always".to_string());
    let c_field = create_rw_signal(String::new());
    let c_op = create_rw_signal("eq".to_string());
    let c_val = create_rw_signal(String::new());
    let a_field = create_rw_signal(String::new());
    let a_kind = create_rw_signal("text".to_string());
    let a_val = create_rw_signal(String::new());

    let reload = move || {
        let t = token.get().unwrap_or_default();
        let rules = rules;
        spawn_local(async move {
            match api::sget(&t, "/api/rules").await {
                Ok(list) => rules.set(list.as_array().cloned().unwrap_or_default()),
                Err(e) => set_msg(&msg, false, e),
            }
        });
    };
    reload();

    view! {
        <div>
            <MsgLine sig=msg/>
            {h3("Business rules")}
            <For each=move || rules.get()
                 key=|r| r["id"].as_str().unwrap_or_default().to_string()
                 children=move |r: Value| {
                     let token = token;
                     let msg = msg;
                     let rules = rules;
                     let id_act = r["id"].as_str().unwrap_or_default().to_string();
                     let id_del = r["id"].as_str().unwrap_or_default().to_string();
                     let active = r["active"].as_bool().unwrap_or(true);
                     let label = format!(
                         "on {} of {} — if {} → set {} = {}",
                         r["event"].as_str().unwrap_or("?"),
                         r["entity"].as_str().unwrap_or("?"),
                         serde_json::to_string(&r["condition"]).unwrap_or_default(),
                         r["action_field"].as_str().unwrap_or("?"),
                         serde_json::to_string(&r["action_value"]).unwrap_or_default(),
                     );
                     view! {
                         <div style=row_style()>
                             <span style="font-size:13px; flex:1;">{label}</span>
                             <button style=btn(false)
                                 on:click=move |_: leptos::ev::MouseEvent| {
                                     let t = token.get().unwrap_or_default();
                                     let msg = msg;
                                     let rules = rules;
                                     let id = id_act.clone();
                                     let to = !active;
                                     spawn_local(async move {
                                         match api::spatch(&t, &format!("/api/rules/{id}"), json!({"active": to})).await {
                                             Ok(_) => {
                                                 set_msg(&msg, true, if to { "rule activated" } else { "rule deactivated" });
                                                 if let Ok(l) = api::sget(&t, "/api/rules").await {
                                                     rules.set(l.as_array().cloned().unwrap_or_default());
                                                 }
                                             }
                                             Err(e) => set_msg(&msg, false, e),
                                         }
                                     });
                                 }>{if active { "deactivate" } else { "activate" }}</button>
                             <button style=del_btn()
                                 on:click=move |_: leptos::ev::MouseEvent| {
                                     let t = token.get().unwrap_or_default();
                                     let msg = msg;
                                     let rules = rules;
                                     let id = id_del.clone();
                                     spawn_local(async move {
                                         match api::sdelete(&t, &format!("/api/rules/{id}")).await {
                                             Ok(_) => {
                                                 set_msg(&msg, true, "rule deleted");
                                                 if let Ok(l) = api::sget(&t, "/api/rules").await {
                                                     rules.set(l.as_array().cloned().unwrap_or_default());
                                                 }
                                             }
                                             Err(e) => set_msg(&msg, false, e),
                                         }
                                     });
                                 }>"delete"</button>
                         </div>
                     }
                 }/>

            {h3("New rule")}
            <div style=card_style()>
                <div style="display:flex; gap:8px; align-items:center; margin-bottom:8px; flex-wrap:wrap;">
                    {lbl("on")}
                    <select prop:value=move || ent.get()
                        on:input=move |ev| ent.set(event_target_value(&ev))
                        style=fmt_style("160px")>
                        <option value="">"— entity —"</option>
                        {entity_options(&state).into_iter()
                            .map(|(v, l)| view! { <option value=v.clone()>{l}</option> })
                            .collect_view()}
                    </select>
                    <Sel sig=event options=opts(RULE_EVENTS)/>
                    <ConditionEditor state=state ent=ent c_mode=c_mode c_field=c_field c_op=c_op c_val=c_val/>
                </div>
                <div style="display:flex; gap:8px; align-items:center; flex-wrap:wrap;">
                    {lbl("→ set")}
                    {move || {
                        let e = ent.get();
                        let a_field = a_field;
                        let fields = entity_field_list(&state, &e);
                        view! {
                            <select prop:value=move || a_field.get()
                                on:input=move |ev| a_field.set(event_target_value(&ev))
                                style=fmt_style("auto")>
                                <option value="">"— field —"</option>
                                {fields.into_iter()
                                    .map(|(v, l)| view! { <option value=v.clone()>{l}</option> })
                                    .collect_view()}
                            </select>
                        }
                    }}
                    {lbl("=")}
                    <Sel sig=a_kind options=vec![
                        ("text".into(), "text".into()),
                        ("number".into(), "number".into()),
                        ("bool".into(), "true/false".into()),
                        ("now".into(), "now()".into()),
                        ("empty".into(), "empty".into()),
                    ]/>
                    {move || if a_kind.get() != "now" && a_kind.get() != "empty" {
                        view! { <Txt sig=a_val ph="value" w="130px"/> }.into_view()
                    } else { ().into_view() }}
                    <button style=btn(true)
                        on:click=move |_: leptos::ev::MouseEvent| {
                            let t = token.get().unwrap_or_default();
                            let body = json!({
                                "entity": ent.get(),
                                "event": event.get(),
                                "condition": if c_mode.get() == "when" {
                                    cmp_expr(&c_field.get(), &c_op.get(), &c_val.get())
                                } else { lit_true() },
                                "action_field": a_field.get(),
                                "action_value": lit_value(&a_kind.get(), &a_val.get()),
                            });
                            let msg = msg;
                            let rules = rules;
                            spawn_local(async move {
                                match api::spost(&t, "/api/rules", body).await {
                                    Ok(_) => {
                                        set_msg(&msg, true, "rule created");
                                        if let Ok(l) = api::sget(&t, "/api/rules").await {
                                            rules.set(l.as_array().cloned().unwrap_or_default());
                                        }
                                    }
                                    Err(e) => set_msg(&msg, false, e),
                                }
                            });
                        }>"Create rule"</button>
                </div>
                <p style="font-size:11px; color:#667; margin-top:6px;">
                    "Rules fire synchronously in the write transaction; the engine caps cascades to prevent cycles."
                </p>
            </div>
        </div>
    }
}

/// Condition builder wrapped as a component (used by the rule editor).
#[component]
fn ConditionEditor(
    state: AppState,
    ent: RwSignal<String>,
    c_mode: RwSignal<String>,
    c_field: RwSignal<String>,
    c_op: RwSignal<String>,
    c_val: RwSignal<String>,
) -> impl IntoView {
    view! {
        {move || {
            let e = ent.get();
            let fields = entity_field_list(&state, &e);
            let (m, f, o, v) = (c_mode, c_field, c_op, c_val);
            view! {
                <CondBuilder mode=m field=f op=o value=v fields=fields prefix="if"/>
            }.into_view()
        }}
    }
}

#[derive(Clone)]
struct TransRow {
    id: String,
    name: String,
    from_state: String,
    to_state: String,
    g_mode: String,
    g_field: String,
    g_op: String,
    g_val: String,
    action_field: String,
    a_kind: String,
    a_val: String,
    creates_task: bool,
}

#[component]
fn WorkflowsTab() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let token = state.token;
    let wfs = create_rw_signal(Vec::<Value>::new());
    let msg = create_rw_signal(None::<(bool, String)>);

    let ent = create_rw_signal(String::new());
    let name = create_rw_signal(String::new());
    let states_txt = create_rw_signal(String::new());
    let trans = create_rw_signal(Vec::<TransRow>::new());

    let reload = move || {
        let t = token.get().unwrap_or_default();
        let wfs = wfs;
        spawn_local(async move {
            match api::sget(&t, "/api/workflows").await {
                Ok(list) => wfs.set(list.as_array().cloned().unwrap_or_default()),
                Err(e) => set_msg(&msg, false, e),
            }
        });
    };
    reload();

    let state_list = move || {
        states_txt
            .get()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
    };

    view! {
        <div>
            <MsgLine sig=msg/>
            {h3("Workflows (state machines)")}
            <For each=move || wfs.get()
                 key=|w| w["id"].as_str().unwrap_or_default().to_string()
                 children=move |w: Value| {
                     let token = token;
                     let msg = msg;
                     let wfs = wfs;
                     let id_act = w["id"].as_str().unwrap_or_default().to_string();
                     let id_del = w["id"].as_str().unwrap_or_default().to_string();
                     let active = w["active"].as_bool().unwrap_or(true);
                     let label = format!(
                         "“{}” on {} — {} states, {} transitions",
                         w["name"].as_str().unwrap_or("?"),
                         w["entity"].as_str().unwrap_or("?"),
                         w["states"].as_array().map(|a| a.len()).unwrap_or(0),
                         w["transitions"].as_array().map(|a| a.len()).unwrap_or(0),
                     );
                     view! {
                         <div style=row_style()>
                             <span style="font-size:13px; flex:1;">{label}</span>
                             <button style=btn(false)
                                 on:click=move |_: leptos::ev::MouseEvent| {
                                     let t = token.get().unwrap_or_default();
                                     let msg = msg;
                                     let wfs = wfs;
                                     let id = id_act.clone();
                                     let to = !active;
                                     spawn_local(async move {
                                         match api::spatch(&t, &format!("/api/workflows/{id}"), json!({"active": to})).await {
                                             Ok(_) => {
                                                 set_msg(&msg, true, if to { "workflow activated" } else { "workflow deactivated" });
                                                 if let Ok(l) = api::sget(&t, "/api/workflows").await {
                                                     wfs.set(l.as_array().cloned().unwrap_or_default());
                                                 }
                                             }
                                             Err(e) => set_msg(&msg, false, e),
                                         }
                                     });
                                 }>{if active { "deactivate" } else { "activate" }}</button>
                             <button style=del_btn()
                                 on:click=move |_: leptos::ev::MouseEvent| {
                                     let t = token.get().unwrap_or_default();
                                     let msg = msg;
                                     let wfs = wfs;
                                     let id = id_del.clone();
                                     spawn_local(async move {
                                         match api::sdelete(&t, &format!("/api/workflows/{id}")).await {
                                             Ok(_) => {
                                                 set_msg(&msg, true, "workflow deleted");
                                                 if let Ok(l) = api::sget(&t, "/api/workflows").await {
                                                     wfs.set(l.as_array().cloned().unwrap_or_default());
                                                 }
                                             }
                                             Err(e) => set_msg(&msg, false, e),
                                         }
                                     });
                                 }>"delete"</button>
                         </div>
                     }
                 }/>

            {h3("New workflow")}
            <div style=card_style()>
                <div style="display:flex; gap:8px; align-items:center; margin-bottom:8px; flex-wrap:wrap;">
                    {lbl("entity")}
                    <select prop:value=move || ent.get()
                        on:input=move |ev| ent.set(event_target_value(&ev))
                        style=fmt_style("160px")>
                        <option value="">"— entity —"</option>
                        {entity_options(&state).into_iter()
                            .map(|(v, l)| view! { <option value=v.clone()>{l}</option> })
                            .collect_view()}
                    </select>
                    {lbl("name")} <Txt sig=name ph="approval" w="120px"/>
                    {lbl("states")} <Txt sig=states_txt ph="new, active, closed" w="200px"/>
                    <span style="font-size:11px; color:#667;">"(new records start in “active”)"</span>
                </div>

                <For each=move || {
                    let list: Vec<String> = trans.get().iter().map(|r| r.id.clone()).collect();
                    list
                }
                     key=|rid| rid.clone()
                     children=move |rid| {
                         let t2 = trans;
                         let ent2 = ent;
                         // Keyed children are reused across edits/removals, so
                         // the row is always resolved by id — never by position.
                         let init = t2.get_untracked().iter().find(|r| r.id == rid).cloned();
                         let name_sig = create_rw_signal(init.as_ref().map(|r| r.name.clone()).unwrap_or_default());
                         let (g_m, g_f, g_o, g_v, af, ak, av, ct) = (
                             create_rw_signal(init.as_ref().map(|r| r.g_mode.clone()).unwrap_or_default()),
                             create_rw_signal(init.as_ref().map(|r| r.g_field.clone()).unwrap_or_default()),
                             create_rw_signal(init.as_ref().map(|r| r.g_op.clone()).unwrap_or_default()),
                             create_rw_signal(init.as_ref().map(|r| r.g_val.clone()).unwrap_or_default()),
                             create_rw_signal(init.as_ref().map(|r| r.action_field.clone()).unwrap_or_default()),
                             create_rw_signal(init.as_ref().map(|r| r.a_kind.clone()).unwrap_or_default()),
                             create_rw_signal(init.as_ref().map(|r| r.a_val.clone()).unwrap_or_default()),
                             create_rw_signal(init.as_ref().map(|r| r.creates_task).unwrap_or(false)),
                         );
                         let name_sig2 = name_sig;
                         let rid_name = rid.clone();
                         create_effect(move |_| {
                             let v = name_sig2.get();
                             t2.update(|ts| { if let Some(r) = ts.iter_mut().find(|r| r.id == rid_name) { r.name = v.clone(); } });
                         });
                         let rid_from_sel = rid.clone();
                         let rid_from_inp = rid.clone();
                         let rid_to_sel = rid.clone();
                         let rid_to_inp = rid.clone();
                         let rid_del = rid;
                         view! {
                             <div style="border:1px solid #e8ebef; border-radius:4px; padding:8px; margin:6px 0;">
                                 <div style="display:flex; gap:6px; align-items:center; flex-wrap:wrap;">
                                     {lbl("transition")} <Txt sig=name_sig ph="approve" w="110px"/>
                                     <select prop:value=move || t2.get().iter().find(|r| r.id == rid_from_sel).map(|r| r.from_state.clone()).unwrap_or_default()
                                         on:input=move |ev| {
                                             let v = event_target_value(&ev);
                                             t2.update(|ts| { if let Some(r) = ts.iter_mut().find(|r| r.id == rid_from_inp) { r.from_state = v; } });
                                         }
                                         style=fmt_style("auto")>
                                         <option value="">"— from —"</option>
                                         {move || state_list().into_iter()
                                             .map(|s| view! { <option value=s.clone()>{s.clone()}</option> })
                                             .collect_view()}
                                     </select>
                                     <span>{"→"}</span>
                                     <select prop:value=move || t2.get().iter().find(|r| r.id == rid_to_sel).map(|r| r.to_state.clone()).unwrap_or_default()
                                         on:input=move |ev| {
                                             let v = event_target_value(&ev);
                                             t2.update(|ts| { if let Some(r) = ts.iter_mut().find(|r| r.id == rid_to_inp) { r.to_state = v; } });
                                         }
                                         style=fmt_style("auto")>
                                         <option value="">"— to —"</option>
                                         {move || state_list().into_iter()
                                             .map(|s| view! { <option value=s.clone()>{s.clone()}</option> })
                                             .collect_view()}
                                     </select>
                                     {lbl("creates task")} <Chk sig=ct/>
                                     <button style=del_btn()
                                         on:click=move |_: leptos::ev::MouseEvent| {
                                             t2.update(|ts| { ts.retain(|r| r.id != rid_del); });
                                         }>"×"</button>
                                 </div>
                                 <div style="display:flex; gap:6px; align-items:center; margin-top:6px; flex-wrap:wrap;">
                                     <GuardEditor ent=ent2 g_m=g_m g_f=g_f g_o=g_o g_v=g_v/>
                                 </div>
                                 <div style="display:flex; gap:6px; align-items:center; margin-top:6px; flex-wrap:wrap;">
                                     {lbl("on run: set")}
                                     {move || {
                                         let e = ent2.get();
                                         let af = af;
                                         let fields = entity_field_list(&state, &e);
                                         view! {
                                             <select prop:value=move || af.get()
                                                 on:input=move |ev| af.set(event_target_value(&ev))
                                                 style=fmt_style("auto")>
                                                 <option value="">"— none —"</option>
                                                 {fields.into_iter()
                                                     .map(|(v, l)| view! { <option value=v.clone()>{l}</option> })
                                                     .collect_view()}
                                             </select>
                                         }
                                     }}
                                     <Sel sig=ak options=vec![
                                         ("text".into(), "text".into()),
                                         ("number".into(), "number".into()),
                                         ("bool".into(), "true/false".into()),
                                         ("now".into(), "now()".into()),
                                         ("empty".into(), "empty".into()),
                                     ]/>
                                     {move || if ak.get() != "now" && ak.get() != "empty" {
                                         view! { <Txt sig=av ph="value" w="120px"/> }.into_view()
                                     } else { ().into_view() }}
                                 </div>
                             </div>
                         }
                     }/>
                <div style="margin-top:8px; display:flex; gap:8px;">
                    <button style=btn(false)
                        on:click=move |_: leptos::ev::MouseEvent| {
                            trans.update(|ts| ts.push(TransRow {
                                id: new_uuid(),
                                name: String::new(),
                                from_state: String::new(),
                                to_state: String::new(),
                                g_mode: "always".into(),
                                g_field: String::new(),
                                g_op: "eq".into(),
                                g_val: String::new(),
                                action_field: String::new(),
                                a_kind: "text".into(),
                                a_val: String::new(),
                                creates_task: false,
                            }));
                        }>"+ transition"</button>
                    <button style=btn(true)
                        on:click=move |_: leptos::ev::MouseEvent| {
                            let t = token.get().unwrap_or_default();
                            let states = state_list();
                            let trs = trans.get().iter().map(|r| {
                                let mut tr = json!({
                                    "name": r.name,
                                    "from_state": r.from_state,
                                    "to_state": r.to_state,
                                    "guard": if r.g_mode == "when" {
                                        cmp_expr(&r.g_field, &r.g_op, &r.g_val)
                                    } else { lit_true() },
                                    "creates_task": r.creates_task,
                                });
                                if !r.action_field.is_empty() {
                                    tr["actions"] = json!([{"field": r.action_field, "value": lit_value(&r.a_kind, &r.a_val)}]);
                                }
                                tr
                            }).collect::<Vec<_>>();
                            let body = json!({
                                "entity": ent.get(),
                                "name": name.get(),
                                "states": states,
                                "transitions": trs,
                            });
                            let msg = msg;
                            let wfs = wfs;
                            spawn_local(async move {
                                match api::spost(&t, "/api/workflows", body).await {
                                    Ok(_) => {
                                        set_msg(&msg, true, "workflow saved");
                                        if let Ok(l) = api::sget(&t, "/api/workflows").await {
                                            wfs.set(l.as_array().cloned().unwrap_or_default());
                                        }
                                    }
                                    Err(e) => set_msg(&msg, false, e),
                                }
                            });
                        }>"Save workflow"</button>
                </div>
                <p style="font-size:11px; color:#667; margin-top:6px;">
                    "Transitions run in the write transaction; guards are bounded-DSL expressions; “creates task” opens an approval task."
                </p>
            </div>
        </div>
    }
}

/// Guard builder for one transition row (field list from the entity).
#[component]
fn GuardEditor(
    ent: RwSignal<String>,
    g_m: RwSignal<String>,
    g_f: RwSignal<String>,
    g_o: RwSignal<String>,
    g_v: RwSignal<String>,
) -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    view! {
        {move || {
            let e = ent.get();
            let fields = entity_field_list(&state, &e);
            let (m, f, o, v) = (g_m, g_f, g_o, g_v);
            view! { <CondBuilder mode=m field=f op=o value=v fields=fields prefix="guard"/> }.into_view()
        }}
    }
}

// ===== Security admin: teams / roles / OWD / users / sharing =====

#[component]
fn StudioSecurity() -> impl IntoView {
    let tab = create_rw_signal(0usize);
    view! {
        <div>
            <Tabs sig=tab labels=vec!["Teams", "Roles", "Org-wide defaults", "Users", "Sharing rules"]/>
            {move || match tab.get() {
                0 => view! { <TeamsTab/> }.into_view(),
                1 => view! { <RolesTab/> }.into_view(),
                2 => view! { <OwdTab/> }.into_view(),
                3 => view! { <UsersTab/> }.into_view(),
                _ => view! { <ShareTab/> }.into_view(),
            }}
        </div>
    }
}

#[component]
fn TeamsTab() -> impl IntoView {
    let token = use_context::<AppState>().unwrap().token;
    let teams = create_rw_signal(Vec::<Value>::new());
    let msg = create_rw_signal(None::<(bool, String)>);
    let new_name = create_rw_signal(String::new());
    let new_parent = create_rw_signal(String::new());

    let reload = move || {
        let t = token.get().unwrap_or_default();
        let teams = teams;
        spawn_local(async move {
            match api::sget(&t, "/api/admin/teams").await {
                Ok(l) => teams.set(l.as_array().cloned().unwrap_or_default()),
                Err(e) => set_msg(&msg, false, e),
            }
        });
    };
    reload();

    let team_opts = move || {
        let mut o = vec![(String::new(), "— none —".to_string())];
        o.extend(teams.get().iter().map(|t| {
            (
                t["id"].as_str().unwrap_or_default().to_string(),
                t["name"].as_str().unwrap_or_default().to_string(),
            )
        }));
        o
    };
    let name_of = move |id: &str| {
        teams
            .get()
            .iter()
            .find(|t| t["id"].as_str() == Some(id))
            .and_then(|t| t["name"].as_str())
            .unwrap_or("?")
            .to_string()
    };

    view! {
        <div>
            <MsgLine sig=msg/>
            <For each=move || teams.get()
                 key=|t| t["id"].as_str().unwrap_or_default().to_string()
                 children=move |t: Value| {
                     let token = token;
                     let msg = msg;
                     let teams = teams;
                     let id_set = t["id"].as_str().unwrap_or_default().to_string();
                     let id_del = t["id"].as_str().unwrap_or_default().to_string();
                     let parent_id = t["parent_id"].as_str().unwrap_or_default().to_string();
                     // Read the parent live from the teams signal: `For` reuses
                     // children by key, so values captured at creation go stale
                     // after the post-action reload.
                     let teams_disp = teams;
                     let name_of_disp = name_of;
                     let tid_disp = id_set.clone();
                     let parent_line = move || {
                         let pid = teams_disp
                             .get()
                             .iter()
                             .find(|x| x["id"].as_str() == Some(tid_disp.as_str()))
                             .and_then(|x| x["parent_id"].as_str())
                             .unwrap_or("")
                             .to_string();
                         if pid.is_empty() {
                             "root team".to_string()
                         } else {
                             format!("under {}", name_of_disp(&pid))
                         }
                     };
                     let row_parent = create_rw_signal(parent_id.clone());
                     view! {
                         <div style=row_style()>
                             <strong style="min-width:140px;">{t["name"].as_str().unwrap_or_default().to_string()}</strong>
                             <span style="color:#667; font-size:12px;">
                                 {parent_line}
                             </span>
                             <span style="flex:1;"></span>
                             {lbl("re-parent")}
                             <select prop:value=move || row_parent.get()
                                 on:input=move |ev| row_parent.set(event_target_value(&ev))
                                 style=fmt_style("auto")>
                                 {move || team_opts().into_iter()
                                     .map(|(v, l)| view! { <option value=v.clone()>{l}</option> })
                                     .collect_view()}
                             </select>
                             <button style=btn(false)
                                 on:click=move |_: leptos::ev::MouseEvent| {
                                     let t = token.get().unwrap_or_default();
                                     let msg = msg;
                                     let teams = teams;
                                     let id = id_set.clone();
                                     let pid = row_parent.get_untracked();
                                     let body = if pid.is_empty() { json!({"parent_id": null}) } else { json!({"parent_id": pid}) };
                                     spawn_local(async move {
                                         match api::spatch(&t, &format!("/api/admin/teams/{id}"), body).await {
                                             Ok(_) => {
                                                 set_msg(&msg, true, "team updated");
                                                 if let Ok(l) = api::sget(&t, "/api/admin/teams").await {
                                                     teams.set(l.as_array().cloned().unwrap_or_default());
                                                 }
                                             }
                                             Err(e) => set_msg(&msg, false, e),
                                         }
                                     });
                                 }>"set"</button>
                             <button style=del_btn()
                                 on:click=move |_: leptos::ev::MouseEvent| {
                                     let t = token.get().unwrap_or_default();
                                     let msg = msg;
                                     let teams = teams;
                                     let id = id_del.clone();
                                     spawn_local(async move {
                                         match api::sdelete(&t, &format!("/api/admin/teams/{id}")).await {
                                             Ok(_) => {
                                                 set_msg(&msg, true, "team deleted (children re-rooted)");
                                                 if let Ok(l) = api::sget(&t, "/api/admin/teams").await {
                                                     teams.set(l.as_array().cloned().unwrap_or_default());
                                                 }
                                             }
                                             Err(e) => set_msg(&msg, false, e),
                                         }
                                     });
                                 }>"delete"</button>
                         </div>
                     }
                 }/>
            <div style="display:flex; gap:6px; align-items:center; margin-top:10px;">
                <Txt sig=new_name ph="new team name" w="160px"/>
                <select prop:value=move || new_parent.get()
                    on:input=move |ev| new_parent.set(event_target_value(&ev))
                    style=fmt_style("auto")>
                    {/* Reactive: the teams list arrives async after mount, so
                        options computed once at render would stay empty. */}
                    {move || team_opts().into_iter()
                        .map(|(v, l)| view! { <option value=v.clone()>{l}</option> })
                        .collect_view()}
                </select>
                <button style=btn(true)
                    on:click=move |_: leptos::ev::MouseEvent| {
                        let t = token.get().unwrap_or_default();
                        let msg = msg;
                        let teams = teams;
                        let name = new_name.get();
                        let pid = new_parent.get();
                        let body = if pid.is_empty() { json!({"name": name}) } else { json!({"name": name, "parent_id": pid}) };
                        spawn_local(async move {
                            match api::spost(&t, "/api/admin/teams", body).await {
                                Ok(_) => {
                                    set_msg(&msg, true, "team created");
                                    new_name.set(String::new());
                                    new_parent.set(String::new());
                                    if let Ok(l) = api::sget(&t, "/api/admin/teams").await {
                                        teams.set(l.as_array().cloned().unwrap_or_default());
                                    }
                                }
                                Err(e) => set_msg(&msg, false, e),
                            }
                        });
                    }>"Create team"</button>
            </div>
            <p style="font-size:11px; color:#667; margin-top:6px;">
                "A team sees its own records and everything below it in the hierarchy (ADR-0025)."
            </p>
        </div>
    }
}

#[component]
fn RolesTab() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let token = state.token;
    let roles = create_rw_signal(Vec::<Value>::new());
    let sel_role = create_rw_signal(None::<String>);
    let msg = create_rw_signal(None::<(bool, String)>);

    let p_ent = create_rw_signal("*".to_string());
    let p_verb = create_rw_signal("read".to_string());
    let fp_ent = create_rw_signal(String::new());
    let fp_field = create_rw_signal(String::new());
    let fp_access = create_rw_signal("read".to_string());
    let parent_role = create_rw_signal(String::new());
    let new_role = create_rw_signal(String::new());

    let reload = move || {
        let t = token.get().unwrap_or_default();
        let roles = roles;
        spawn_local(async move {
            match api::sget(&t, "/api/admin/roles").await {
                Ok(l) => roles.set(l.as_array().cloned().unwrap_or_default()),
                Err(e) => set_msg(&msg, false, e),
            }
        });
    };
    reload();

    let verb_opts = {
        let mut v = vec![("*".to_string(), "* (any)".to_string())];
        v.extend(VERBS.iter().map(|x| (x.to_string(), x.to_string())));
        v
    };

    let selected = move || {
        let id = sel_role.get()?;
        roles
            .get()
            .into_iter()
            .find(|r| r["id"].as_str() == Some(id.as_str()))
    };

    view! {
        <div>
            <MsgLine sig=msg/>
            {h3("Roles")}
            <For each=move || roles.get()
                 key=|r| r["id"].as_str().unwrap_or_default().to_string()
                 children=move |r: Value| {
                     let token = token;
                     let msg = msg;
                     let roles = roles;
                     let sel_role = sel_role;
                     let id_sel = r["id"].as_str().unwrap_or_default().to_string();
                     let id_del = r["id"].as_str().unwrap_or_default().to_string();
                     view! {
                         <div style=row_style()>
                             <span style="cursor:pointer; font-weight:600; min-width:150px;"
                                 on:click=move |_: leptos::ev::MouseEvent| sel_role.set(Some(id_sel.clone()))>
                                 {r["name"].as_str().unwrap_or_default().to_string()}
                             </span>
                             <span style="color:#667; font-size:12px;">
                                 {format!("{} permissions · {} users", r["permissions"].as_array().map(|a| a.len()).unwrap_or(0), r["user_count"])}
                             </span>
                             <span style="flex:1;"></span>
                             <button style=del_btn()
                                 on:click=move |_: leptos::ev::MouseEvent| {
                                     let t = token.get().unwrap_or_default();
                                     let msg = msg;
                                     let roles = roles;
                                     let id = id_del.clone();
                                     spawn_local(async move {
                                         match api::sdelete(&t, &format!("/api/admin/roles/{id}")).await {
                                             Ok(_) => {
                                                 set_msg(&msg, true, "role deleted");
                                                 if let Ok(l) = api::sget(&t, "/api/admin/roles").await {
                                                     roles.set(l.as_array().cloned().unwrap_or_default());
                                                 }
                                             }
                                             Err(e) => set_msg(&msg, false, e),
                                         }
                                     });
                                 }>"delete"</button>
                         </div>
                     }
                 }/>
            <div style="display:flex; gap:6px; align-items:center; margin:8px 0 14px;">
                <Txt sig=new_role ph="new role name" w="160px"/>
                <button style=btn(true)
                    on:click=move |_: leptos::ev::MouseEvent| {
                        let t = token.get().unwrap_or_default();
                        let msg = msg;
                        let roles = roles;
                        let name = new_role.get();
                        spawn_local(async move {
                            match api::spost(&t, "/api/admin/roles", json!({"name": name})).await {
                                Ok(_) => {
                                    set_msg(&msg, true, "role created (now grant permissions)");
                                    new_role.set(String::new());
                                    if let Ok(l) = api::sget(&t, "/api/admin/roles").await {
                                        roles.set(l.as_array().cloned().unwrap_or_default());
                                    }
                                }
                                Err(e) => set_msg(&msg, false, e),
                            }
                        });
                    }>"Create role"</button>
            </div>

            {move || match selected() {
                None => view! { <p style="color:#889;">"Select a role to edit its permissions."</p> }.into_view(),
                Some(role) => {
                    let rid = create_rw_signal(role["id"].as_str().unwrap_or_default().to_string());
                    let role_name = role["name"].as_str().unwrap_or_default().to_string();
                    let perms = role["permissions"].as_array().cloned().unwrap_or_default();
                    let fperms = role["field_permissions"].as_array().cloned().unwrap_or_default();
                    view! {
                        <div style=card_style()>
                            <strong>{format!("Role “{role_name}”")}</strong>

                            <h4 style="margin:10px 0 4px;">"Object permissions"</h4>
                            <For each=move || perms.clone()
                                 key=|p| format!("{}:{}", p["entity"].as_str().unwrap_or_default(), p["verb"].as_str().unwrap_or_default())
                                 children=move |p: Value| {
                                     let token = token;
                                     let msg = msg;
                                     let roles = roles;
                                     let rid = rid.get_untracked();
                                     let (e, v) = (p["entity"].as_str().unwrap_or_default().to_string(), p["verb"].as_str().unwrap_or_default().to_string());
                                     view! {
                                         <div style="font-size:12px; display:flex; gap:6px; align-items:center; margin:2px 0;">
                                             <code>{format!("{e} : {v}")}</code>
                                             <button style=del_btn()
                                                 on:click=move |_: leptos::ev::MouseEvent| {
                                                     let t = token.get().unwrap_or_default();
                                                     let msg = msg;
                                                     let roles = roles;
                                                     let (rid, e, v) = (rid.clone(), e.clone(), v.clone());
                                                     spawn_local(async move {
                                                         match api::sdelete(&t, &format!("/api/admin/roles/{rid}/permissions/{e}/{v}")).await {
                                                             Ok(_) => {
                                                                 set_msg(&msg, true, "permission revoked");
                                                                 if let Ok(l) = api::sget(&t, "/api/admin/roles").await {
                                                                     roles.set(l.as_array().cloned().unwrap_or_default());
                                                                 }
                                                             }
                                                             Err(e) => set_msg(&msg, false, e),
                                                         }
                                                     });
                                                 }>"revoke"</button>
                                         </div>
                                     }
                                 }/>
                            <div style="display:flex; gap:6px; align-items:center; margin:6px 0;">
                                {/* Reactive: entity options come from the
                                    (async-loaded) model — a once-at-mount list
                                    stays empty if the model lands late. */}
                                {move || view! { <Sel sig=p_ent options=role_ent_opts(&state)/> }.into_view()}
                                <Sel sig=p_verb options=verb_opts.clone()/>
                                <button style=btn(false)
                                    on:click=move |_: leptos::ev::MouseEvent| {
                                        let t = token.get().unwrap_or_default();
                                        let msg = msg;
                                        let rid = rid.get_untracked();
                                        let body = json!({"entity": p_ent.get(), "verb": p_verb.get()});
                                        let roles = roles;
                                        spawn_local(async move {
                                            match api::spost(&t, &format!("/api/admin/roles/{rid}/permissions"), body).await {
                                                Ok(_) => {
                                                    set_msg(&msg, true, "permission granted");
                                                    if let Ok(l) = api::sget(&t, "/api/admin/roles").await {
                                                        roles.set(l.as_array().cloned().unwrap_or_default());
                                                    }
                                                }
                                                Err(e) => set_msg(&msg, false, e),
                                            }
                                        });
                                    }>"Grant"</button>
                            </div>

                            <h4 style="margin:10px 0 4px;">"Field permissions"</h4>
                            <For each=move || fperms.clone()
                                 key=|p| format!("{}:{}", p["entity"].as_str().unwrap_or_default(), p["field"].as_str().unwrap_or_default())
                                 children=move |p: Value| {
                                     let token = token;
                                     let msg = msg;
                                     let roles = roles;
                                     let rid = rid.get_untracked();
                                     let (e, f) = (p["entity"].as_str().unwrap_or_default().to_string(), p["field"].as_str().unwrap_or_default().to_string());
                                     let a = p["access"].as_str().unwrap_or_default().to_string();
                                     view! {
                                         <div style="font-size:12px; display:flex; gap:6px; align-items:center; margin:2px 0;">
                                             <code>{format!("{e}.{f} : {a}")}</code>
                                             <button style=del_btn()
                                                 on:click=move |_: leptos::ev::MouseEvent| {
                                                     let t = token.get().unwrap_or_default();
                                                     let msg = msg;
                                                     let roles = roles;
                                                     let (rid, e, f) = (rid.clone(), e.clone(), f.clone());
                                                     spawn_local(async move {
                                                         match api::sdelete(&t, &format!("/api/admin/roles/{rid}/field-permissions/{e}/{f}")).await {
                                                             Ok(_) => {
                                                                 set_msg(&msg, true, "field permission removed");
                                                                 if let Ok(l) = api::sget(&t, "/api/admin/roles").await {
                                                                     roles.set(l.as_array().cloned().unwrap_or_default());
                                                                 }
                                                             }
                                                             Err(e) => set_msg(&msg, false, e),
                                                         }
                                                     });
                                                 }>"remove"</button>
                                         </div>
                                     }
                                 }/>
                            <div style="display:flex; gap:6px; align-items:center; margin:6px 0; flex-wrap:wrap;">
                                {move || view! { <Sel sig=fp_ent options=role_ent_opts(&state)/> }.into_view()}
                                {move || {
                                    let e = fp_ent.get();
                                    let fields = entity_field_list(&state, &e);
                                    view! {
                                        <select prop:value=move || fp_field.get()
                                            on:input=move |ev| fp_field.set(event_target_value(&ev))
                                            style=fmt_style("auto")>
                                            <option value="">"— field —"</option>
                                            {fields.into_iter()
                                                .map(|(v, l)| view! { <option value=v.clone()>{l}</option> })
                                                .collect_view()}
                                        </select>
                                    }
                                }}
                                <Sel sig=fp_access options=vec![
                                    ("none".into(), "no access".into()),
                                    ("read".into(), "read".into()),
                                    ("write".into(), "write".into()),
                                ]/>
                                <button style=btn(false)
                                    on:click=move |_: leptos::ev::MouseEvent| {
                                        let t = token.get().unwrap_or_default();
                                        let msg = msg;
                                        let rid = rid.get_untracked();
                                        let body = json!({"entity": fp_ent.get(), "field": fp_field.get(), "access": fp_access.get()});
                                        let roles = roles;
                                        spawn_local(async move {
                                            match api::spost(&t, &format!("/api/admin/roles/{rid}/field-permissions"), body).await {
                                                Ok(_) => {
                                                    set_msg(&msg, true, "field permission set");
                                                    if let Ok(l) = api::sget(&t, "/api/admin/roles").await {
                                                        roles.set(l.as_array().cloned().unwrap_or_default());
                                                    }
                                                }
                                                Err(e) => set_msg(&msg, false, e),
                                            }
                                        });
                                    }>"Set"</button>
                            </div>

                            <h4 style="margin:10px 0 4px;">"Role hierarchy"</h4>
                            <p style="font-size:11px; color:#667; margin:4px 0;">
                                "A user holding a parent role READS records owned by users in descendant roles (“see records below me”)."
                            </p>
                            <RoleParents rid=rid.get_untracked() roles=roles parent_role=parent_role msg=msg token=token/>
                        </div>
                    }.into_view()
                }
            }}
        </div>
    }
}

/// (Re)fetch a role's parent roles into `parents` (used at mount and after
/// add/detach, so the displayed list updates in place).
fn fetch_role_parents(
    token: RwSignal<Option<String>>,
    rid: &str,
    parents: RwSignal<Vec<(String, String)>>,
) {
    let t = token.get().unwrap_or_default();
    let id = rid.to_string();
    spawn_local(async move {
        if let Ok(v) = api::sget(&t, &format!("/api/admin/roles/{id}/parents")).await {
            parents.set(
                v["parents"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .map(|p| {
                                (
                                    p["id"].as_str().unwrap_or_default().to_string(),
                                    p["name"].as_str().unwrap_or_default().to_string(),
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            );
        }
    });
}

/// Parent-role list + add/remove for the selected role (fetched live).
#[component]
fn RoleParents(
    rid: String,
    roles: RwSignal<Vec<Value>>,
    parent_role: RwSignal<String>,
    msg: RwSignal<Option<(bool, String)>>,
    token: RwSignal<Option<String>>,
) -> impl IntoView {
    let rid = create_rw_signal(rid);
    let parents = create_rw_signal(Vec::<(String, String)>::new());
    fetch_role_parents(token, &rid.get_untracked(), parents);
    let role_opts = move || {
        roles
            .get()
            .iter()
            .filter(|r| r["id"].as_str() != Some(rid.get().as_str()))
            .map(|r| {
                (
                    r["id"].as_str().unwrap_or_default().to_string(),
                    r["name"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect::<Vec<_>>()
    };
    let rid2 = rid;
    view! {
        <div>
            <For each=move || parents.get()
                 key=|(id, _)| id.clone()
                 children=move |pair| {
                     let (pid, pname): (String, String) = pair;
                     let token = token;
                     let msg = msg;
                     let parents = parents;
                     let rid = rid2;
                     view! {
                         <div style="font-size:12px; display:flex; gap:6px; align-items:center; margin:2px 0;">
                             <code>{"parent: "}{pname.clone()}</code>
                             <button style=del_btn()
                                 on:click=move |_: leptos::ev::MouseEvent| {
                                     let t = token.get().unwrap_or_default();
                                     let msg = msg;
                                     let (rid, pid) = (rid.get_untracked(), pid.clone());
                                     let parents = parents;
                                     spawn_local(async move {
                                         match api::sdelete(&t, &format!("/api/admin/roles/{rid}/parents/{pid}")).await {
                                             Ok(_) => {
                                                 set_msg(&msg, true, "parent detached — effective on next query");
                                                 fetch_role_parents(token, &rid, parents);
                                             }
                                             Err(e) => set_msg(&msg, false, e),
                                         }
                                     });
                                 }>"detach"</button>
                         </div>
                     }
                 }/>
            <div style="display:flex; gap:6px; align-items:center; margin-top:6px;">
                <select prop:value=move || parent_role.get()
                    on:input=move |ev| parent_role.set(event_target_value(&ev))
                    style=fmt_style("auto")>
                    <option value="">"— parent role —"</option>
                    {role_opts().into_iter()
                        .map(|(v, l)| view! { <option value=v.clone()>{l}</option> })
                        .collect_view()}
                </select>
                <button style=btn(false)
                    on:click=move |_: leptos::ev::MouseEvent| {
                        let t = token.get().unwrap_or_default();
                        let msg = msg;
                        let (rid, pid) = (rid.get_untracked(), parent_role.get_untracked());
                        let parents = parents;
                        spawn_local(async move {
                            match api::spost(&t, &format!("/api/admin/roles/{rid}/parents/{pid}"), json!({})).await {
                                Ok(_) => {
                                    set_msg(&msg, true, "parent added — effective on next query");
                                    fetch_role_parents(token, &rid, parents);
                                }
                                Err(e) => set_msg(&msg, false, e),
                            }
                        });
                    }>"Add parent"</button>
            </div>
        </div>
    }
}

#[component]
fn OwdTab() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let token = state.token;
    let owd = create_rw_signal(Vec::<Value>::new());
    let msg = create_rw_signal(None::<(bool, String)>);

    let reload = move || {
        let t = token.get().unwrap_or_default();
        let owd = owd;
        spawn_local(async move {
            match api::sget(&t, "/api/admin/owd").await {
                Ok(l) => owd.set(l.as_array().cloned().unwrap_or_default()),
                Err(e) => set_msg(&msg, false, e),
            }
        });
    };
    reload();

    view! {
        <div>
            <MsgLine sig=msg/>
            <p style="font-size:12px; color:#667; margin-top:0;">
                "The org-wide default is the baseline visibility for every record; sharing rules and manual shares only widen it."
            </p>
            {/* The `each` closure reads the OWD list *and* the model (both
                reactive — rows appear when the async-loaded model lands and
                follow publishes without a tab remount), and rows are keyed by
                entity + current level: a reload after "Set" remounts the row
                with the fresh value — `For` children captured at creation
                would keep showing the pre-fetch default. */}
            <For each=move || {
                    let list = owd.get();
                    entity_options(&state)
                        .iter()
                        .map(|(name, label)| {
                            let current = list
                                .iter()
                                .find(|o| o["entity"].as_str() == Some(name.as_str()))
                                .and_then(|o| o["default_access"].as_str())
                                .unwrap_or("team")
                                .to_string();
                            (name.clone(), label.clone(), current)
                        })
                        .collect::<Vec<_>>()
                }
                 key=|(name, _, current)| format!("{name}::{current}")
                 children=move |(name, label, current): (String, String, String)| {
                     let token = token;
                     let msg = msg;
                     let owd = owd;
                     let sig = create_rw_signal(current);
                     view! {
                         <div style=row_style()>
                             <strong style="min-width:170px;">{label.clone()}</strong>
                             <Sel sig=sig options=opts(OWD_LEVELS)/>
                             <button style=btn(false)
                                 on:click=move |_: leptos::ev::MouseEvent| {
                                     let t = token.get().unwrap_or_default();
                                     let msg = msg;
                                     let (name, level) = (name.clone(), sig.get());
                                     spawn_local(async move {
                                         match api::sput(&t, &format!("/api/admin/owd/{name}"), json!({"default_access": level})).await {
                                             Ok(_) => {
                                                 set_msg(&msg, true, format!("OWD for {name} = {level}"));
                                                 if let Ok(l) = api::sget(&t, "/api/admin/owd").await {
                                                     owd.set(l.as_array().cloned().unwrap_or_default());
                                                 }
                                             }
                                             Err(e) => set_msg(&msg, false, e),
                                         }
                                     });
                                 }>"Set"</button>
                         </div>
                     }
                 }/>
        </div>
    }
}

#[component]
fn UsersTab() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let token = state.token;
    let users = create_rw_signal(Vec::<Value>::new());
    let teams = create_rw_signal(Vec::<Value>::new());
    let roles = create_rw_signal(Vec::<Value>::new());
    let msg = create_rw_signal(None::<(bool, String)>);

    let n_email = create_rw_signal(String::new());
    let n_name = create_rw_signal(String::new());
    let n_pw = create_rw_signal(String::new());
    let n_team = create_rw_signal(String::new());

    let reload = move || {
        let t = token.get().unwrap_or_default();
        let (users, teams, roles) = (users, teams, roles);
        spawn_local(async move {
            if let Ok(l) = api::sget(&t, "/api/admin/users").await {
                users.set(l.as_array().cloned().unwrap_or_default());
            }
            if let Ok(l) = api::sget(&t, "/api/admin/teams").await {
                teams.set(l.as_array().cloned().unwrap_or_default());
            }
            if let Ok(l) = api::sget(&t, "/api/admin/roles").await {
                roles.set(l.as_array().cloned().unwrap_or_default());
            }
        });
    };
    reload();

    let team_opts = move || {
        let mut o = vec![(String::new(), "— no team —".to_string())];
        o.extend(teams.get().iter().map(|t| {
            (
                t["id"].as_str().unwrap_or_default().to_string(),
                t["name"].as_str().unwrap_or_default().to_string(),
            )
        }));
        o
    };

    view! {
        <div>
            <MsgLine sig=msg/>
            <For each=move || users.get()
                 key=|u| u["id"].as_str().unwrap_or_default().to_string()
                 children=move |u: Value| {
                     view! { <UserRow user=u teams_sig=teams roles_sig=roles token=token msg=msg users=users/> }
                 }/>

            {h3("New user")}
            <div style="display:flex; gap:6px; align-items:center; flex-wrap:wrap;">
                <Txt sig=n_email ph="email" w="180px"/>
                <Txt sig=n_name ph="name" w="140px"/>
                <input type="password" placeholder="password" prop:value=move || n_pw.get()
                    on:input=move |ev| n_pw.set(event_target_value(&ev))
                    style=fmt_style("140px") />
                <select prop:value=move || n_team.get()
                    on:input=move |ev| n_team.set(event_target_value(&ev))
                    style=fmt_style("auto")>
                    {/* Reactive: the teams list arrives async after mount, so
                        options computed once at render would stay empty. */}
                    {move || team_opts().into_iter()
                        .map(|(v, l)| view! { <option value=v.clone()>{l}</option> })
                        .collect_view()}
                </select>
                <button style=btn(true)
                    on:click=move |_: leptos::ev::MouseEvent| {
                        let t = token.get().unwrap_or_default();
                        let msg = msg;
                        let users = users;
                        let mut body = json!({"email": n_email.get(), "name": n_name.get(), "password": n_pw.get()});
                        if !n_team.get().is_empty() {
                            body["team_id"] = json!(n_team.get());
                        }
                        spawn_local(async move {
                            match api::spost(&t, "/api/admin/users", body).await {
                                Ok(_) => {
                                    set_msg(&msg, true, "user created");
                                    if let Ok(l) = api::sget(&t, "/api/admin/users").await {
                                        users.set(l.as_array().cloned().unwrap_or_default());
                                    }
                                }
                                Err(e) => set_msg(&msg, false, e),
                            }
                        });
                    }>"Create user"</button>
            </div>
        </div>
    }
}

/// One user row: team, active toggle, role assignment, password reset.
/// Role lists are fetched lazily on expand. The user id rides a signal so
/// every button closure can capture it cheaply (RwSignal is Copy).
#[component]
fn UserRow(
    user: Value,
    teams_sig: RwSignal<Vec<Value>>,
    roles_sig: RwSignal<Vec<Value>>,
    token: RwSignal<Option<String>>,
    msg: RwSignal<Option<(bool, String)>>,
    users: RwSignal<Vec<Value>>,
) -> impl IntoView {
    let uid = create_rw_signal(user["id"].as_str().unwrap_or_default().to_string());
    let email = user["email"].as_str().unwrap_or_default().to_string();
    let name = user["name"].as_str().unwrap_or_default().to_string();
    let team_id = create_rw_signal(user["team_id"].as_str().unwrap_or_default().to_string());
    // Read live from the users list: `For` reuses children by key, so a value
    // captured at creation would go stale after the post-action reload (the
    // disable/activate button would show — and send — the wrong state).
    let users_live = users;
    let active = move || {
        users_live
            .get()
            .iter()
            .find(|u| u["id"].as_str() == Some(uid.get().as_str()))
            .and_then(|u| u["active"].as_bool())
            .unwrap_or(true)
    };
    let team_name = move || {
        teams_sig
            .get()
            .iter()
            .find(|t| t["id"].as_str() == Some(team_id.get().as_str()))
            .and_then(|t| t["name"].as_str())
            .unwrap_or("—")
            .to_string()
    };

    let expanded = create_rw_signal(false);
    let my_roles = create_rw_signal(Vec::<(String, String)>::new());
    let assign_role = create_rw_signal(String::new());
    let new_pw = create_rw_signal(String::new());

    // (Re)fetch this user's roles — at expand and after every assign/revoke,
    // so the chip list updates in place instead of going stale.
    let refresh_roles = move || {
        let t = token.get().unwrap_or_default();
        let id = uid.get_untracked();
        let my_roles = my_roles;
        spawn_local(async move {
            if let Ok(v) = api::sget(&t, &format!("/api/admin/users/{id}/roles")).await {
                my_roles.set(
                    v.as_array()
                        .map(|a| {
                            a.iter()
                                .map(|r| {
                                    (
                                        r["id"].as_str().unwrap_or_default().to_string(),
                                        r["name"].as_str().unwrap_or_default().to_string(),
                                    )
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                );
            }
        });
    };

    view! {
        <div style=row_style()>
            <strong style="min-width:190px;">{email.clone()}</strong>
            <span style="color:#667; font-size:12px; min-width:110px;">{name.clone()}</span>
            <span style="color:#667; font-size:12px;">{move || team_name()}</span>
            <span style=move || format!("font-size:11px; color:{};", if active() { "#3a9d5d" } else { "#b00" })>
                {move || if active() { "active" } else { "disabled" }}
            </span>
            <span style="flex:1;"></span>
            <button style=btn(false)
                on:click=move |_: leptos::ev::MouseEvent| {
                    expanded.update(|e| *e = !*e);
                    if expanded.get_untracked() {
                        refresh_roles();
                    }
                }>{move || if expanded.get() { "▾ less" } else { "▸ manage" }.to_string()}</button>
        </div>
        {move || if !expanded.get() {
            ().into_view()
        } else {
            view! {
                <div style="border:1px solid #e8ebef; border-radius:4px; padding:8px 10px; margin:2px 0 6px; display:flex; flex-direction:column; gap:8px;">
                    <div style="display:flex; gap:6px; align-items:center; flex-wrap:wrap;">
                        {lbl("team")}
                        {move || {
                            let t2 = teams_sig.get();
                            let sig = team_id;
                            view! {
                                <select prop:value=move || sig.get()
                                    on:input=move |ev| sig.set(event_target_value(&ev))
                                    style=fmt_style("auto")>
                                    {t2.iter().map(|t| {
                                        let tid = t["id"].as_str().unwrap_or_default().to_string();
                                        let selected = tid == team_id.get_untracked();
                                        view! { <option value=tid.clone() selected=selected>{t["name"].as_str().unwrap_or_default().to_string()}</option> }
                                    }).collect_view()}
                                </select>
                                <button style=btn(false)
                                    on:click=move |_: leptos::ev::MouseEvent| {
                                        let t = token.get().unwrap_or_default();
                                        let msg = msg;
                                        let id = uid.get_untracked();
                                        let v = sig.get_untracked();
                                        let body = if v.is_empty() { json!({"team_id": null}) } else { json!({"team_id": v}) };
                                        let users = users;
                                        spawn_local(async move {
                                            match api::spatch(&t, &format!("/api/admin/users/{id}"), body).await {
                                                Ok(_) => {
                                                    set_msg(&msg, true, "team updated");
                                                    if let Ok(l) = api::sget(&t, "/api/admin/users").await {
                                                        users.set(l.as_array().cloned().unwrap_or_default());
                                                    }
                                                }
                                                Err(e) => set_msg(&msg, false, e),
                                            }
                                        });
                                    }>"set"</button>
                            }.into_view()
                        }}
                        {lbl("account")}
                        <button style=btn(false)
                            on:click=move |_: leptos::ev::MouseEvent| {
                                let t = token.get().unwrap_or_default();
                                let msg = msg;
                                let id = uid.get_untracked();
                                let to = !active();
                                let users = users;
                                spawn_local(async move {
                                    match api::spatch(&t, &format!("/api/admin/users/{id}"), json!({"active": to})).await {
                                        Ok(_) => {
                                            set_msg(&msg, true, if to { "user activated" } else { "user disabled" });
                                            if let Ok(l) = api::sget(&t, "/api/admin/users").await {
                                                users.set(l.as_array().cloned().unwrap_or_default());
                                            }
                                        }
                                        Err(e) => set_msg(&msg, false, e),
                                    }
                                });
                            }>{move || if active() { "disable" } else { "activate" }}</button>
                    </div>

                    <div style="display:flex; gap:6px; align-items:center; flex-wrap:wrap;">
                        {lbl("roles")}
                        <For each=move || my_roles.get()
                             key=|(rid, _)| rid.clone()
                             children=move |pair| {
                                 let (rid, rname): (String, String) = pair;
                                 let token = token;
                                 let msg = msg;
                                 let uid = uid;
                                 view! {
                                     <span style="display:inline-flex; gap:4px; align-items:center; border:1px solid #ccc; border-radius:10px; padding:1px 8px; font-size:12px;">
                                         {rname.clone()}
                                         <a style="color:#b00; cursor:pointer;"
                                             on:click=move |_: leptos::ev::MouseEvent| {
                                                 let t = token.get().unwrap_or_default();
                                                 let msg = msg;
                                                 let (id, rid) = (uid.get_untracked(), rid.clone());
                                                 spawn_local(async move {
                                                     match api::sdelete(&t, &format!("/api/admin/users/{id}/roles/{rid}")).await {
                                                         Ok(_) => {
                                                             set_msg(&msg, true, "role revoked");
                                                             refresh_roles();
                                                         }
                                                         Err(e) => set_msg(&msg, false, e),
                                                     }
                                                 });
                                             }>"×"</a>
                                     </span>
                                 }
                             }/>
                        <select prop:value=move || assign_role.get()
                            on:input=move |ev| assign_role.set(event_target_value(&ev))
                            style=fmt_style("auto")>
                            <option value="">"— assign role —"</option>
                            {roles_sig.get().iter()
                                .map(|r| view! { <option value=r["id"].as_str().unwrap_or_default().to_string()>{r["name"].as_str().unwrap_or_default().to_string()}</option> })
                                .collect_view()}
                        </select>
                        <button style=btn(false)
                            on:click=move |_: leptos::ev::MouseEvent| {
                                let t = token.get().unwrap_or_default();
                                let msg = msg;
                                let id = uid.get_untracked();
                                let rid = assign_role.get();
                                spawn_local(async move {
                                    match api::spost(&t, &format!("/api/admin/users/{id}/roles"), json!({"role_id": rid})).await {
                                        Ok(_) => {
                                            set_msg(&msg, true, "role assigned");
                                            refresh_roles();
                                        }
                                        Err(e) => set_msg(&msg, false, e),
                                    }
                                });
                            }>"assign"</button>
                    </div>

                    <div style="display:flex; gap:6px; align-items:center; flex-wrap:wrap;">
                        {lbl("reset password")}
                        <input type="password" placeholder="new password" prop:value=move || new_pw.get()
                            on:input=move |ev| new_pw.set(event_target_value(&ev))
                            style=fmt_style("150px") />
                        <button style=btn(false)
                            on:click=move |_: leptos::ev::MouseEvent| {
                                let t = token.get().unwrap_or_default();
                                let msg = msg;
                                let id = uid.get_untracked();
                                let pw = new_pw.get();
                                spawn_local(async move {
                                    match api::spost(&t, &format!("/api/admin/users/{id}/password"), json!({"password": pw})).await {
                                        Ok(_) => set_msg(&msg, true, "password reset"),
                                        Err(e) => set_msg(&msg, false, e),
                                    }
                                });
                            }>"reset"</button>
                    </div>
                </div>
            }.into_view()
        }}
    }
}

#[component]
fn ShareTab() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let token = state.token;
    let rules = create_rw_signal(Vec::<Value>::new());
    let users = create_rw_signal(Vec::<Value>::new());
    let teams = create_rw_signal(Vec::<Value>::new());
    let msg = create_rw_signal(None::<(bool, String)>);

    let n_ent = create_rw_signal(String::new());
    let n_principal = create_rw_signal(String::new());
    let n_access = create_rw_signal("read".to_string());
    let c_mode = create_rw_signal("when".to_string());
    let c_field = create_rw_signal(String::new());
    let c_op = create_rw_signal("ge".to_string());
    let c_val = create_rw_signal(String::new());

    let reload = move || {
        let t = token.get().unwrap_or_default();
        let (rules, users, teams) = (rules, users, teams);
        spawn_local(async move {
            if let Ok(l) = api::sget(&t, "/api/admin/share-rules").await {
                rules.set(l.as_array().cloned().unwrap_or_default());
            }
            if let Ok(l) = api::sget(&t, "/api/admin/users").await {
                users.set(l.as_array().cloned().unwrap_or_default());
            }
            if let Ok(l) = api::sget(&t, "/api/admin/teams").await {
                teams.set(l.as_array().cloned().unwrap_or_default());
            }
        });
    };
    reload();

    let principal_opts = move || {
        let mut o = Vec::new();
        o.extend(users.get().iter().map(|u| {
            (
                u["id"].as_str().unwrap_or_default().to_string(),
                format!("user: {}", u["email"].as_str().unwrap_or_default()),
            )
        }));
        o.extend(teams.get().iter().map(|t| {
            (
                t["id"].as_str().unwrap_or_default().to_string(),
                format!("team: {}", t["name"].as_str().unwrap_or_default()),
            )
        }));
        o
    };

    view! {
        <div>
            <MsgLine sig=msg/>
            {h3("Criteria-based sharing rules")}
            <For each=move || rules.get()
                 key=|r| r["id"].as_str().unwrap_or_default().to_string()
                 children=move |r: Value| {
                     let token = token;
                     let msg = msg;
                     let rules = rules;
                     let id_act = r["id"].as_str().unwrap_or_default().to_string();
                     let id_del = r["id"].as_str().unwrap_or_default().to_string();
                     let active = r["active"].as_bool().unwrap_or(true);
                     view! {
                         <div style=row_style()>
                             <span style="font-size:12px; flex:1;">
                                 {format!(
                                     "{} → {} ({}) when {}",
                                     r["entity"].as_str().unwrap_or("?"),
                                     r["principal_id"].as_str().unwrap_or("?"),
                                     r["access"].as_str().unwrap_or("?"),
                                     serde_json::to_string(&r["condition"]).unwrap_or_default(),
                                 )}
                             </span>
                             <button style=btn(false)
                                 on:click=move |_: leptos::ev::MouseEvent| {
                                     let t = token.get().unwrap_or_default();
                                     let msg = msg;
                                     let rules = rules;
                                     let id = id_act.clone();
                                     let to = !active;
                                     spawn_local(async move {
                                         match api::spatch(&t, &format!("/api/admin/share-rules/{id}"), json!({"active": to})).await {
                                             Ok(_) => {
                                                 set_msg(&msg, true, if to { "rule activated (re-materialized)" } else { "rule deactivated (shares revoked instantly)" });
                                                 if let Ok(l) = api::sget(&t, "/api/admin/share-rules").await {
                                                     rules.set(l.as_array().cloned().unwrap_or_default());
                                                 }
                                             }
                                             Err(e) => set_msg(&msg, false, e),
                                         }
                                     });
                                 }>{if active { "deactivate" } else { "activate" }}</button>
                             <button style=del_btn()
                                 on:click=move |_: leptos::ev::MouseEvent| {
                                     let t = token.get().unwrap_or_default();
                                     let msg = msg;
                                     let rules = rules;
                                     let id = id_del.clone();
                                     spawn_local(async move {
                                         match api::sdelete(&t, &format!("/api/admin/share-rules/{id}")).await {
                                             Ok(_) => {
                                                 set_msg(&msg, true, "rule deleted (shares cascade away)");
                                                 if let Ok(l) = api::sget(&t, "/api/admin/share-rules").await {
                                                     rules.set(l.as_array().cloned().unwrap_or_default());
                                                 }
                                             }
                                             Err(e) => set_msg(&msg, false, e),
                                         }
                                     });
                                 }>"delete"</button>
                         </div>
                     }
                 }/>

            {h3("New sharing rule")}
            <div style=card_style()>
                <div style="display:flex; gap:8px; align-items:center; flex-wrap:wrap;">
                    {lbl("share")}
                    <select prop:value=move || n_ent.get()
                        on:input=move |ev| n_ent.set(event_target_value(&ev))
                        style=fmt_style("160px")>
                        <option value="">"— entity —"</option>
                        {entity_options(&state).into_iter()
                            .map(|(v, l)| view! { <option value=v.clone()>{l}</option> })
                            .collect_view()}
                    </select>
                    {lbl("records where")}
                    {move || {
                        let e = n_ent.get();
                        let fields = entity_field_list(&state, &e);
                        let (m, f, o, v) = (c_mode, c_field, c_op, c_val);
                        view! { <CondBuilder mode=m field=f op=o value=v fields=fields prefix=""/> }.into_view()
                    }}
                </div>
                <div style="display:flex; gap:8px; align-items:center; margin-top:8px; flex-wrap:wrap;">
                    {lbl("with")}
                    <select prop:value=move || n_principal.get()
                        on:input=move |ev| n_principal.set(event_target_value(&ev))
                        style=fmt_style("220px")>
                        <option value="">"— user or team —"</option>
                        {/* Reactive: users/teams arrive async after mount, so
                            options computed once at render would stay empty. */}
                        {move || principal_opts().into_iter()
                            .map(|(v, l)| view! { <option value=v.clone()>{l}</option> })
                            .collect_view()}
                    </select>
                    <Sel sig=n_access options=vec![("read".into(), "read".into()), ("write".into(), "write".into())]/>
                    <button style=btn(true)
                        on:click=move |_: leptos::ev::MouseEvent| {
                            let t = token.get().unwrap_or_default();
                            let msg = msg;
                            let rules = rules;
                            let body = json!({
                                "entity": n_ent.get(),
                                "principal_id": n_principal.get(),
                                "access": n_access.get(),
                                "condition": cmp_expr(&c_field.get(), &c_op.get(), &c_val.get()),
                            });
                            spawn_local(async move {
                                match api::spost(&t, "/api/admin/share-rules", body).await {
                                    Ok(_) => {
                                        set_msg(&msg, true, "sharing rule created + materialized");
                                        if let Ok(l) = api::sget(&t, "/api/admin/share-rules").await {
                                            rules.set(l.as_array().cloned().unwrap_or_default());
                                        }
                                    }
                                    Err(e) => set_msg(&msg, false, e),
                                }
                            });
                        }>"Create rule"</button>
                </div>
                <p style="font-size:11px; color:#667; margin-top:6px;">
                    "Matches materialize into record shares immediately (bounded); edits revoke instantly via epoch invalidation (ADR-0013)."
                </p>
            </div>
        </div>
    }
}

// ===== Data: export / import / snapshots =====

#[component]
fn StudioData() -> impl IntoView {
    let token = use_context::<AppState>().unwrap().token;
    let model_json = create_rw_signal(String::new());
    let import_json = create_rw_signal(String::new());
    let snapshots = create_rw_signal(Vec::<Value>::new());
    let msg = create_rw_signal(None::<(bool, String)>);

    let reload_snapshots = move || {
        let t = token.get().unwrap_or_default();
        let snapshots = snapshots;
        spawn_local(async move {
            if let Ok(l) = api::sget(&t, "/api/studio/snapshots").await {
                snapshots.set(l.as_array().cloned().unwrap_or_default());
            }
        });
    };
    reload_snapshots();

    view! {
        <div>
            <MsgLine sig=msg/>
            {h3("Export active model")}
            <button style=btn(false)
                on:click=move |_: leptos::ev::MouseEvent| {
                    let t = token.get().unwrap_or_default();
                    let model_json = model_json;
                    spawn_local(async move {
                        match api::sget(&t, "/api/studio/model").await {
                            Ok(m) => model_json.set(serde_json::to_string_pretty(&m).unwrap_or_default()),
                            Err(e) => set_msg(&msg, false, e),
                        }
                    });
                }>"Fetch model JSON"</button>
            <Show when=move || !model_json.get().is_empty()>
                <div style="margin-top:8px;">
                    <Area sig=model_json rows=14 ph=""/>
                </div>
            </Show>

            {h3("Import model → new draft")}
            <p style="font-size:12px; color:#667; margin:4px 0;">
                "Paste an exported model bundle. Importing stages a draft (never publishes) — open it in the Model tab to validate and publish."
            </p>
            <Area sig=import_json rows=10 ph="paste model JSON here"/>
            <button style=btn(true)
                on:click=move |_: leptos::ev::MouseEvent| {
                    let t = token.get().unwrap_or_default();
                    let raw = import_json.get();
                    let msg = msg;
                    spawn_local(async move {
                        match serde_json::from_str::<Value>(&raw) {
                            Err(e) => set_msg(&msg, false, format!("not valid JSON: {e}")),
                            Ok(model) => match api::spost(&t, "/api/studio/import", model).await {
                                Ok(d) => set_msg(&msg, true, format!(
                                    "draft “{}” staged ({}) — open it in the Model tab",
                                    d["name"].as_str().unwrap_or("?"),
                                    d["id"].as_str().unwrap_or("?"),
                                )),
                                Err(e) => set_msg(&msg, false, e),
                            },
                        }
                    });
                }>"Import as draft"</button>

            {h3("Publish snapshots (history)")}
            <For each=move || snapshots.get()
                 key=|s| s["id"].as_str().unwrap_or_default().to_string()
                 children=move |s: Value| {
                     view! {
                         <div style="font-size:12px; color:#667; padding:3px 0;">
                             {format!("v{} · {}", s["version"], s["created_at"].as_str().unwrap_or("?").chars().take(19).collect::<String>())}
                         </div>
                     }
                 }/>
        </div>
    }
}
