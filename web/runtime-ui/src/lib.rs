//! MDA Runtime UI — metadata-driven Leptos CSR app (Phase 6).
//! Login → navigation shell → list (view definitions) → form (form
//! definitions) with a real-time conflict banner over the SSE event channel,
//! plus dashboards that run their reports under the logged-in user.
//!
//! Everything renders from server-resolved metadata (`/api/navigation`,
//! `/api/views/:entity`, `/api/forms/:entity`, `/api/dashboards`) — the server
//! applies the caller's object/field security when resolving, so the client
//! never has to (and never can) widen it.

mod api;
mod studio;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use leptos::*;
use wasm_bindgen::{closure::Closure, JsCast};

use api::{
    local_get, local_remove, local_set, DashSummary, EntityInfo, FormField, FormSection,
    ListResult, ModelInfo, NavItem, ReportResult, ViewColumn,
};

/// Owns an SSE `EventSource` and its `onmessage` closure for a record form, so
/// both can be dropped (→ connection closed) when the form unmounts.
type EsHandle = Rc<
    RefCell<
        Option<(
            web_sys::EventSource,
            Closure<dyn FnMut(web_sys::MessageEvent)>,
        )>,
    >,
>;

// ===== global app state =====

#[derive(Clone, Copy)]
pub struct AppState {
    pub token: RwSignal<Option<String>>,
    pub page: RwSignal<Page>,
    pub model: RwSignal<Option<ModelInfo>>,
    /// The permission-filtered navigation tree (`/api/navigation`).
    pub nav: RwSignal<Vec<NavItem>>,
    /// Available dashboards (`/api/dashboards`).
    pub dashboards: RwSignal<Vec<DashSummary>>,
    /// Bumped after a create/update/delete so list views refetch on return
    /// (doesn't rely on SPA component remount to pick up changes).
    pub refresh: RwSignal<u64>,
    /// Set from `/api/auth/me` — gates the Studio (admin-only surfaces).
    pub is_admin: RwSignal<bool>,
}

#[derive(Clone, PartialEq)]
pub enum Page {
    Home,
    Dashboard(String),
    List(String),
    /// Edit an existing record (Some id) or create one (None).
    Form {
        entity: String,
        id: Option<String>,
    },
    Studio,
}

// ===== root =====

#[component]
pub fn App() -> impl IntoView {
    let token = create_rw_signal(local_get("mda_token"));
    let page = create_rw_signal(Page::Home);
    let model = create_rw_signal(None);
    let nav = create_rw_signal(Vec::new());
    let dashboards = create_rw_signal(Vec::new());
    let refresh = create_rw_signal(0u64);
    let is_admin = create_rw_signal(false);
    let state = AppState {
        token,
        page,
        model,
        nav,
        dashboards,
        refresh,
        is_admin,
    };
    provide_context(state);

    let token_for_effect = token;
    let model_for_effect = model;
    let nav_for_effect = nav;
    let dash_for_effect = dashboards;
    let admin_for_effect = is_admin;
    create_effect(move |_| {
        if let Some(t) = token_for_effect.get() {
            spawn_local(async move {
                if let Ok(m) = api::get_model(&t).await {
                    model_for_effect.set(Some(m));
                }
                if let Ok(items) = api::get_navigation(&t).await {
                    nav_for_effect.set(items);
                }
                if let Ok(d) = api::list_dashboards(&t).await {
                    dash_for_effect.set(d);
                }
                if let Ok(admin) = api::is_admin(&t).await {
                    admin_for_effect.set(admin);
                }
            });
        }
    });

    let state = use_context::<AppState>().unwrap();
    view! {
        <div style="font-family: system-ui, sans-serif; max-width: 1100px; margin: 1rem auto;">
            <header style="display:flex; justify-content:space-between; align-items:center; padding:0.5rem 0; border-bottom:1px solid #ccc;">
                <strong
                    style="font-size:1.2rem; cursor:pointer;"
                    on:click=move |_: leptos::ev::MouseEvent| state.page.set(Page::Home)>
                    "MDA"
                </strong>
                {move || if state.is_admin.get() {
                    let s = state;
                    view! {
                        <button style="cursor:pointer; margin-left:14px; font-size:0.85rem;"
                            on:click=move |_: leptos::ev::MouseEvent| s.page.set(Page::Studio)>
                            "Studio"
                        </button>
                    }.into_view()
                } else { ().into_view() }}
                {move || if state.token.get().is_some() {
                    let s = state;
                    view! {
                        <button style="cursor:pointer;"
                            on:click=move |_: leptos::ev::MouseEvent| {
                                local_remove("mda_token");
                                s.token.set(None);
                                s.page.set(Page::Home);
                                s.nav.set(Vec::new());
                                s.dashboards.set(Vec::new());
                                s.model.set(None);
                                s.is_admin.set(false);
                            }>
                            "Logout"
                        </button>
                    }.into_view()
                } else { ().into_view() }}
            </header>
            <main>
                {move || {
                    let s = state;
                    if s.token.get().is_none() {
                        view! { <Login/> }.into_view()
                    } else {
                        match s.page.get() {
                            Page::Home => view! { <Home/> }.into_view(),
                            Page::Dashboard(id) => view! { <DashboardView id/> }.into_view(),
                            Page::List(name) => view! { <EntityList entity=name/> }.into_view(),
                            Page::Form { entity, id } => {
                                view! { <RecordForm entity id/> }.into_view()
                            }
                            Page::Studio => view! { <studio::Studio/> }.into_view(),
                        }
                    }
                }}
            </main>
        </div>
    }
}

// ===== login =====

#[component]
fn Login() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let (tenant, set_tenant) = create_signal("default".to_string());
    let (email, set_email) = create_signal("admin@mda.local".to_string());
    let (password, set_password) = create_signal("admin123".to_string());
    let (error, set_error) = create_signal(String::new());
    let (loading, set_loading) = create_signal(false);

    let s = state;
    view! {
        <div style="max-width: 300px; margin: 2rem auto;">
            <h2>"Login"</h2>
            <p><label>"Tenant"</label><br/>
                <input prop:value=tenant
                    on:input=move |ev| set_tenant.set(event_target_value(&ev))
                    style="width:100%; padding:4px;" /></p>
            <p><label>"Email"</label><br/>
                <input prop:value=email
                    on:input=move |ev| set_email.set(event_target_value(&ev))
                    style="width:100%; padding:4px;" /></p>
            <p><label>"Password"</label><br/>
                <input type="password" prop:value=password
                    on:input=move |ev| set_password.set(event_target_value(&ev))
                    style="width:100%; padding:4px;" /></p>
            <button
                disabled=move || loading.get()
                on:click=move |_: leptos::ev::MouseEvent| {
                    set_loading.set(true);
                    set_error.set(String::new());
                    let (t, e, p) = (tenant.get(), email.get(), password.get());
                    spawn_local(async move {
                        match api::login(&t, &e, &p).await {
                            Ok(token) => {
                                local_set("mda_token", &token);
                                s.token.set(Some(token.clone()));
                                if let Ok(m) = api::get_model(&token).await {
                                    s.model.set(Some(m));
                                }
                                if let Ok(items) = api::get_navigation(&token).await {
                                    s.nav.set(items);
                                }
                                if let Ok(d) = api::list_dashboards(&token).await {
                                    s.dashboards.set(d);
                                }
                                if let Ok(admin) = api::is_admin(&token).await {
                                    s.is_admin.set(admin);
                                }
                                s.page.set(Page::Home);
                            }
                            Err(err) => set_error.set(err),
                        }
                        set_loading.set(false);
                    });
                }
                style="padding:6px 16px; cursor:pointer;">
                "Login"
            </button>
            {move || {
                let e = error.get();
                if e.is_empty() { ().into_view() }
                else { view!{ <p style="color:red;">{e}</p> }.into_view() }
            }}
        </div>
    }
}

// ===== home: navigation shell + dashboards =====

#[component]
fn Home() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let s = state;
    view! {
        <div>
            <h2>"Home"</h2>

            <h3>"Navigation"</h3>
            <For each=move || s.nav.get()
                 key=|i| format!("{}-{}-{}", i.kind, i.label, i.url.clone().unwrap_or_default())
                 children=move |item: NavItem| {
                     let s2 = s;
                     match item.kind.as_str() {
                         "entity" => {
                             let entity = item.entity.clone().unwrap_or_default();
                             view! {
                                 <div style="padding:8px; margin:4px 0; border:1px solid #ddd; border-radius:4px; cursor:pointer;"
                                     on:click=move |_: leptos::ev::MouseEvent| {
                                         s2.page.set(Page::List(entity.clone()));
                                     }>
                                     <strong>{item.label.clone()}</strong>
                                 </div>
                             }.into_view()
                         }
                         _ => {
                             let url = item.url.clone().unwrap_or_default();
                             // Nav URLs come from tenant-authored UI definitions —
                             // only navigate to http(s) or same-app paths, never
                             // javascript: (stored XSS via a modeler-crafted link).
                             let safe = is_safe_nav_url(&url);
                             view! {
                                 <div style="padding:8px; margin:4px 0; border:1px solid #ddd; border-radius:4px;">
                                     <a href={if safe { url.clone() } else { "#".to_string() }}
                                        target="_blank" rel="noopener noreferrer">{item.label.clone()}</a>
                                 </div>
                             }.into_view()
                         }
                     }
                 }/>

            {move || {
                let d = s.dashboards.get();
                if d.is_empty() { ().into_view() }
                else {
                    view! {
                        <h3 style="margin-top:1.5rem;">"Dashboards"</h3>
                        <For each=move || s.dashboards.get()
                             key=|d| d.id.clone()
                             children=move |d: DashSummary| {
                                 let s2 = s;
                                 view! {
                                     <div style="padding:8px; margin:4px 0; border:1px solid #ddd; border-radius:4px; cursor:pointer;"
                                         on:click=move |_: leptos::ev::MouseEvent| {
                                             s2.page.set(Page::Dashboard(d.id.clone()));
                                         }>
                                         <strong>{d.label.clone()}</strong>
                                     </div>
                                 }
                             }/>
                    }.into_view()
                }
            }}
        </div>
    }
}

// ===== dashboard: reports run under the logged-in user =====

#[component]
fn DashboardView(id: String) -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let token = state.token;
    let id_fetch = id.clone();
    let resource = create_resource(
        || (),
        move |_| {
            let token = token.get().unwrap_or_default();
            let id = id_fetch.clone();
            async move { api::get_dashboard(&token, &id).await }
        },
    );
    let s = state;
    view! {
        <div>
            <div style="display:flex; align-items:center; gap:1rem; margin:1rem 0;">
                <button on:click=move |_: leptos::ev::MouseEvent| s.page.set(Page::Home)
                    style="cursor:pointer;">"← Back"</button>
            </div>
            <Suspense fallback=move || view!{ <p>"Loading…"</p> }>
                {move || match resource.get() {
                    Some(Ok(d)) => view! {
                        <div>
                            <h2 style="margin-top:0;">{d.label.clone()}</h2>
                            <For each=move || d.items.clone()
                                 key=|t| t.title.to_string()
                                 children=move |tile| view! { <DashTileView tile/> } />
                        </div>
                    }.into_view(),
                    Some(Err(e)) => view!{ <p style="color:red;">{format!("Error: {e}")}</p> }.into_view(),
                    None => ().into_view(),
                }}
            </Suspense>
        </div>
    }
}

#[component]
fn DashTileView(tile: api::DashTile) -> impl IntoView {
    let title = tile.title.as_str().unwrap_or("Report").to_string();
    match (tile.error, tile.result) {
        (Some(err), _) => view! {
            <div style="border:1px solid #ddd; border-radius:4px; padding:8px; margin:8px 0;">
                <strong>{title}</strong>
                <p style="color:#a00; margin:4px 0 0;">{err}</p>
            </div>
        }
        .into_view(),
        (None, Some(res)) => view! {
            <div style="border:1px solid #ddd; border-radius:4px; padding:8px; margin:8px 0;">
                <strong>{title}</strong>
                <ReportTable res/>
            </div>
        }
        .into_view(),
        (None, None) => view! {
            <div style="border:1px solid #ddd; border-radius:4px; padding:8px; margin:8px 0;">
                <strong>{title}</strong>
                <p style="color:#888; margin:4px 0 0;">"no result"</p>
            </div>
        }
        .into_view(),
    }
}

#[component]
fn ReportTable(res: ReportResult) -> impl IntoView {
    let cols = res.columns.clone();
    view! {
        <div style="overflow-x:auto;">
            <table border="0" cellpadding="4" cellspacing="0"
                   style="border-collapse:collapse; margin-top:6px; font-size:13px;">
                <thead>
                    <tr>
                        <For each=move || cols.clone()
                             key=|c| c.clone()
                             children=move |c| view! {
                                 <th style="border:1px solid #ddd; background:#f4f4f4; text-align:left;">{c}</th>
                             }/>
                    </tr>
                </thead>
                <tbody>
                    {/* Key rows by position, not content: projected report rows
                        carry no unique id, so identical rows would collide
                        under a content key and break the For diff. */}
                    <For each=move || {
                            res.rows.clone().into_iter().enumerate().collect::<Vec<_>>()
                        }
                         key=|(i, _)| *i
                         children=move |(_i, row)| {
                             let cols2 = res.columns.clone();
                             view! {
                                 <tr>
                                     <For each=move || cols2.clone()
                                          key=|c| c.clone()
                                          children=move |c| view! {
                                              <td style="border:1px solid #eee;">{cell_text(&row, &c)}</td>
                                          }/>
                                 </tr>
                             }
                         }/>
                </tbody>
            </table>
        </div>
    }
}

fn cell_text(row: &serde_json::Value, col: &str) -> String {
    match row.get(col) {
        None | Some(serde_json::Value::Null) => String::new(),
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

// ===== entity list (view-definition driven grid) =====

#[component]
fn EntityList(entity: String) -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let token = state.token;
    let refresh = state.refresh;
    let entity_fetch = entity.clone();
    let resource = create_resource(
        move || refresh.get(),
        move |_| {
            let token = token.get().unwrap_or_default();
            let entity = entity_fetch.clone();
            async move {
                let view = api::get_view(&token, &entity).await.ok().flatten();
                let data = api::list_records(&token, &entity).await;
                (view, data)
            }
        },
    );
    let s = state;
    let entity_new = entity.clone();
    let ent_sig = create_rw_signal(entity.clone());

    view! {
        <div>
            <div style="display:flex; align-items:center; gap:1rem; margin:1rem 0;">
                <button on:click=move |_: leptos::ev::MouseEvent| s.page.set(Page::Home)
                    style="cursor:pointer;">"← Back"</button>
                <h2 style="margin:0;">{move || ent_sig.get()}</h2>
                <button style="cursor:pointer; margin-left:auto;"
                    on:click=move |_: leptos::ev::MouseEvent| {
                        s.page.set(Page::Form { entity: entity_new.clone(), id: None });
                    }>
                    "+ New"
                </button>
            </div>
            <Suspense fallback=move || view!{ <p>"Loading…"</p> }>
                {move || match resource.get() {
                    Some((view, Ok(data))) => {
                        if data.items.is_empty() {
                            view! { <p style="color:#888;">"No records."</p> }.into_view()
                        } else {
                            let columns: Vec<ViewColumn> = view
                                .map(|v| v.columns)
                                .unwrap_or_else(|| fallback_columns(&data));
                            let columns3 = columns.clone();
                            view! {
                                <div style="overflow-x:auto;">
                                    <table border="0" cellpadding="4" cellspacing="0"
                                           style="border-collapse:collapse; width:100%; font-size:13px;">
                                        <thead>
                                            <tr>
                                                <For each=move || columns3.clone()
                                                     key=|c| c.field.clone()
                                                     children=move |c| view! {
                                                         <th style="border:1px solid #ddd; background:#f4f4f4; text-align:left;">
                                                             {c.label.clone()}
                                                         </th>
                                                     }/>
                                                <th></th>
                                            </tr>
                                        </thead>
                                        <tbody>
                                            {data.items.iter().map(|row| {
                                                let id = row["id"].as_str().unwrap_or("").to_string();
                                                let s_edit = s;
                                                let id_edit = id.clone();
                                                let id_edit2 = id.clone();
                                                let id_del = id.clone();
                                                let token_del = token;
                                                let ent_sig_edit = ent_sig;
                                                let ent_sig_del = ent_sig;
                                                let res_ref = resource;
                                                let cols = columns.clone();
                                                view! {
                                                    <tr>
                                                        {cols.clone().iter().map(|c| {
                                                            let txt = cell_text(row, &c.field);
                                                            view! {
                                                                <td style="border:1px solid #eee; cursor:pointer;"
                                                                    on:click={
                                                                        let id_edit2 = id_edit2.clone();
                                                                        move |_: leptos::ev::MouseEvent| {
                                                                            s_edit.page.set(Page::Form {
                                                                                entity: ent_sig_edit.get(),
                                                                                id: Some(id_edit2.clone()),
                                                                            });
                                                                        }
                                                                    }>
                                                                    {txt}
                                                                </td>
                                                            }
                                                        }).collect_view()}
                                                        <td style="border:1px solid #eee; white-space:nowrap;">
                                                            <button style="cursor:pointer; margin-left:4px;"
                                                                on:click=move |_: leptos::ev::MouseEvent| {
                                                                    s_edit.page.set(Page::Form {
                                                                        entity: ent_sig_edit.get(),
                                                                        id: Some(id_edit.clone()),
                                                                    });
                                                                }>"Edit"</button>
                                                            <button style="cursor:pointer; margin-left:4px; color:#a00;"
                                                                on:click=move |_: leptos::ev::MouseEvent| {
                                                                    // Destructive: confirm before the API call.
                                                                    let confirmed = web_sys::window()
                                                                        .and_then(|w| {
                                                                            w.confirm_with_message(&format!(
                                                                                "Delete this {} record?",
                                                                                ent_sig_del.get()))
                                                                                .ok()
                                                                        })
                                                                        .unwrap_or(true);
                                                                    if !confirmed {
                                                                        return;
                                                                    }
                                                                    let tok = token_del.get().unwrap_or_default();
                                                                    let ent = ent_sig_del.get();
                                                                    let idd = id_del.clone();
                                                                    let res = res_ref;
                                                                    spawn_local(async move {
                                                                        let _ = api::delete_record(&tok, &ent, &idd).await;
                                                                        res.refetch();
                                                                    });
                                                                }>"Delete"</button>
                                                        </td>
                                                    </tr>
                                                }
                                            }).collect_view()}
                                        </tbody>
                                    </table>
                                </div>
                            }.into_view()
                        }
                    }
                    Some((_, Err(e))) => view!{ <p style="color:red;">{format!("Error: {e}")}</p> }.into_view(),
                    None => ().into_view(),
                }}
            </Suspense>
        </div>
    }
}

/// Without a view definition, show the id + the first non-system column per row.
fn fallback_columns(data: &ListResult) -> Vec<ViewColumn> {
    let mut cols = vec![ViewColumn {
        field: "id".to_string(),
        label: "Id".to_string(),
        r#type: "string".to_string(),
    }];
    if let Some(first) = data.items.first() {
        if let Some(obj) = first.as_object() {
            for (k, _) in obj.iter() {
                if matches!(
                    k.as_str(),
                    "id" | "version" | "owner_id" | "state" | "created_at" | "updated_at"
                ) {
                    continue;
                }
                cols.push(ViewColumn {
                    field: k.clone(),
                    label: k.clone(),
                    r#type: "string".to_string(),
                });
                break;
            }
        }
    }
    cols
}

// ===== record form (form-definition driven) + real-time conflict banner =====

#[component]
fn RecordForm(entity: String, id: Option<String>) -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let token = state.token;
    let s = state;

    let values: RwSignal<HashMap<String, String>> = create_rw_signal(HashMap::new());
    let version = create_rw_signal::<Option<i64>>(None);
    let loaded = create_rw_signal(false);
    let error = create_rw_signal(String::new());
    // remote new version from the SSE channel (None = no concurrent change).
    let conflict = create_rw_signal::<Option<i64>>(None);
    // the resolved form definition (None until fetched; None+loaded => fall back to the model)
    let form = create_rw_signal::<Option<Vec<FormSection>>>(None);
    // reference-field option lists (entity -> (label list, id list))
    let ref_options: RwSignal<HashMap<String, Vec<(String, String)>>> =
        create_rw_signal(HashMap::new());

    // load the form definition + reference options + the record (on edit)
    let entity_load = entity.clone();
    let id_load = id.clone();
    create_effect(move |_| {
        if loaded.get() {
            return;
        }
        let tok = token.get().unwrap_or_default();
        let ent = entity_load.clone();
        let rid = id_load.clone();
        spawn_local(async move {
            // form definition (falls back to the model when absent)
            let f = api::get_form(&tok, &ent).await.ok().flatten();
            let sections = f.map(|f| f.sections);
            // gather reference targets and fetch their option lists
            if let Some(sections) = &sections {
                let mut targets: Vec<String> = Vec::new();
                for sec in sections {
                    for fld in &sec.fields {
                        if fld.widget == "reference" {
                            if let Some(t) = &fld.target_entity {
                                if !targets.contains(t) {
                                    targets.push(t.clone());
                                }
                            }
                        }
                    }
                }
                let mut opts: HashMap<String, Vec<(String, String)>> = HashMap::new();
                for t in targets {
                    if let Ok(list) = api::list_records(&tok, &t).await {
                        opts.insert(
                            t,
                            list.items
                                .iter()
                                .map(|r| {
                                    let label = r
                                        .as_object()
                                        .and_then(|o| {
                                            o.iter().find(|(k, _)| {
                                                !matches!(
                                                    k.as_str(),
                                                    "id" | "version"
                                                        | "owner_id"
                                                        | "state"
                                                        | "created_at"
                                                        | "updated_at"
                                                )
                                            })
                                        })
                                        .map(|(_, v)| match v {
                                            serde_json::Value::String(x) => x.clone(),
                                            o => o.to_string(),
                                        })
                                        .unwrap_or_else(|| {
                                            r["id"].as_str().unwrap_or("?").to_string()
                                        });
                                    let id = r["id"].as_str().unwrap_or("").to_string();
                                    (label, id)
                                })
                                .collect(),
                        );
                    }
                }
                ref_options.set(opts);
            }
            form.set(sections);

            // load the record itself on edit
            if let Some(rid) = rid {
                if let Ok(rec) = api::get_record(&tok, &ent, &rid).await {
                    if let Some(obj) = rec.as_object() {
                        let mut m = HashMap::new();
                        for (k, v) in obj {
                            let txt = match v {
                                serde_json::Value::Null => String::new(),
                                serde_json::Value::String(x) => x.clone(),
                                other => other.to_string(),
                            };
                            m.insert(k.clone(), txt);
                        }
                        values.set(m);
                    }
                    version.set(rec["version"].as_i64());
                }
            }
            loaded.set(true);
        });
    });

    // SSE: watch this record for remote changes (edit only) → conflict banner.
    // The EventSource + its onmessage closure are owned by `es_holder` and
    // closed/dropped on unmount via `on_cleanup`, so navigating the SPA back to
    // the list releases the connection instead of leaking one per form visit.
    let es_holder: EsHandle = Rc::new(RefCell::new(None));
    {
        if let Some(rid) = id.clone() {
            if let Some(tok) = token.get() {
                let channel = format!("record:{}:{rid}", entity);
                let my_id = rid.clone();
                let set_conflict = conflict;
                let es_holder2 = es_holder.clone();
                // EventSource can't set headers, so first fetch a short-lived,
                // one-shot ticket — never putting the access JWT in the URL.
                spawn_local(async move {
                    let Ok(ticket) = api::event_ticket(&tok).await else {
                        return;
                    };
                    let url = api::events_url(&ticket, &channel);
                    let Ok(es) = web_sys::EventSource::new(&url) else {
                        return;
                    };
                    let cb = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(
                        move |ev: web_sys::MessageEvent| {
                            if let Some(data) = ev.data().as_string() {
                                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&data) {
                                    if v["record_id"].as_str() == Some(my_id.as_str())
                                        && v["type"].as_str() == Some("record.updated")
                                    {
                                        if let Some(to_v) = v["payload"]["to_version"].as_i64() {
                                            set_conflict.set(Some(to_v));
                                        }
                                    }
                                }
                            }
                        },
                    );
                    es.set_onmessage(Some(cb.as_ref().unchecked_ref()));
                    *es_holder2.borrow_mut() = Some((es, cb));
                });
            }
        }
    }
    let es_holder_cleanup = es_holder.clone();
    on_cleanup(move || {
        if let Some((es, _cb)) = es_holder_cleanup.borrow_mut().take() {
            es.close();
            // `cb` drops here too, unlinking the JS callback.
        }
    });

    let entity_save = entity.clone();
    let id_save = id.clone();
    let on_save = move |_: leptos::ev::MouseEvent| {
        let tok = token.get().unwrap_or_default();
        let ent = entity_save.clone();
        let ver = version.get();
        // Field name → declared type, so typed fields are sent as JSON scalars
        // (bool/integer/decimal) rather than always as strings.
        let field_types: HashMap<String, String> = s
            .model
            .get()
            .and_then(|m| {
                m.entities
                    .into_iter()
                    .find(|e| e.name == ent)
                    .map(|e| e.fields)
            })
            .map(|fs| fs.into_iter().map(|f| (f.name, f.field_type)).collect())
            .unwrap_or_default();
        let body = serde_json::Value::Object(
            values
                .get()
                .iter()
                .filter_map(|(k, v)| {
                    if matches!(
                        k.as_str(),
                        "id" | "version" | "owner_id" | "state" | "created_at" | "updated_at"
                    ) {
                        None
                    } else {
                        Some((k.clone(), coerce_field(&field_types, k, v)))
                    }
                })
                .collect(),
        )
        .to_string();
        // clone before moving into the spawned task so the click handler is FnMut.
        let id_save = id_save.clone();
        let s_nav = s;
        spawn_local(async move {
            let res = match &id_save {
                Some(rid) => api::update_record(&tok, &ent, rid, ver.unwrap_or(0), body).await,
                None => api::create_record(&tok, &ent, body).await,
            };
            match res {
                Ok(_) => {
                    // Refresh the list so the change is visible on return.
                    s_nav.refresh.update(|n| *n += 1);
                    s_nav.page.set(Page::List(ent));
                }
                Err(e) => {
                    if e.contains("409") {
                        error.set(
                            "Conflict — the record was changed by someone else. Go back, reopen, and re-apply."
                                .into(),
                        );
                    } else {
                        error.set(e);
                    }
                }
            }
        });
    };

    let entity_cancel = entity.clone();
    let s_back = s;
    let on_cancel = move |_: leptos::ev::MouseEvent| {
        s.page.set(Page::List(entity_cancel.clone()));
    };

    let title = match &id {
        Some(_) => format!("Edit {entity}"),
        None => format!("New {entity}"),
    };

    view! {
        <div>
            <div style="display:flex; align-items:center; gap:1rem; margin:1rem 0;">
                <button on:click={
                    let entity = entity.clone();
                    move |_: leptos::ev::MouseEvent| s_back.page.set(Page::List(entity.clone()))
                } style="cursor:pointer;">"← Back"</button>
                <h2 style="margin:0;">{title}</h2>
            </div>

            {move || match conflict.get() {
                Some(v) => view! {
                    <div style="padding:8px; margin:6px 0; background:#fff8e1; border:1px solid #f0c000; border-radius:4px;">
                        {format!("Changed remotely (now v{v}). Your save may conflict — reopen to pick up the latest.")}
                    </div>
                }.into_view(),
                None => ().into_view(),
            }}

            {move || {
                if !loaded.get() {
                    return view! { <p>"Loading…"</p> }.into_view();
                }
                match form.get() {
                    Some(sections) => sections
                        .iter()
                        .map(|sec| view! {
                            <FormSectionView
                                section=sec.clone()
                                values=values
                                ref_options=ref_options />
                        })
                        .collect_view(),
                    // No form definition (or the API is unreachable): render from the model.
                    None => model_fields(&s, &entity, values).into_view(),
                }
            }.into_view()}

            <div style="margin-top:1rem;">
                <button on:click=on_save style="padding:6px 16px; cursor:pointer;">"Save"</button>
                <button on:click=on_cancel style="margin-left:6px; cursor:pointer;">"Cancel"</button>
            </div>
            {move || {
                let e = error.get();
                if e.is_empty() { ().into_view() } else { view!{ <p style="color:red;">{e}</p> }.into_view() }
            }}
        </div>
    }
}

/// One rendered form section (title + fields, widget-driven inputs).
#[component]
fn FormSectionView(
    section: FormSection,
    values: RwSignal<HashMap<String, String>>,
    ref_options: RwSignal<HashMap<String, Vec<(String, String)>>>,
) -> impl IntoView {
    let title = section.title.clone();
    view! {
        <fieldset style="border:1px solid #ddd; border-radius:4px; margin:8px 0; padding:8px 12px;">
            {move || match &title {
                Some(t) if !t.is_empty() => view! {
                    <legend style="font-weight:600; padding:0 4px;">{t.clone()}</legend>
                }.into_view(),
                _ => ().into_view(),
            }}
            <For each=move || section.fields.clone()
                 key=|f| f.name.clone()
                 children=move |f: FormField| {
                     view! { <FieldInput field=f values=values ref_options=ref_options /> }
                 }/>
        </fieldset>
    }
}

/// One form field: the input widget comes from the resolved definition
/// (authored override or inferred from the field type).
#[component]
fn FieldInput(
    field: FormField,
    values: RwSignal<HashMap<String, String>>,
    ref_options: RwSignal<HashMap<String, Vec<(String, String)>>>,
) -> impl IntoView {
    let fname = field.name.clone();
    let flabel = field.label.clone();
    let init = {
        let fname = fname.clone();
        move || {
            values
                .get_untracked()
                .get(&fname)
                .cloned()
                .unwrap_or_default()
        }
    };
    let opts: Vec<String> = field
        .options
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let input = match field.widget.as_str() {
        "checkbox" => {
            let k = fname.clone();
            view! {
                <input type="checkbox" prop:checked=init() == "true"
                    on:input=move |ev| {
                        let val = event_target_checked(&ev).to_string();
                        values.update(|m| { m.insert(k.clone(), val); });
                    }/>
            }
            .into_view()
        }
        "select" if !opts.is_empty() => {
            let k = fname.clone();
            let cur = init();
            let opts_view = opts
                .clone()
                .into_iter()
                .map(move |o| {
                    let selected = o == cur;
                    view! { <option value=o.clone() selected=selected>{o}</option> }
                })
                .collect_view();
            view! {
                <select style="width:100%; padding:4px;"
                    on:input=move |ev| {
                        values.update(|m| { m.insert(k.clone(), event_target_value(&ev)); });
                    }>
                    {opts_view}
                </select>
            }
            .into_view()
        }
        // reference picker: options resolved from the target entity by the server
        "reference" => {
            let k = fname.clone();
            let cur = init();
            let target = field.target_entity.clone().unwrap_or_default();
            let options = move || ref_options.get().get(&target).cloned().unwrap_or_default();
            view! {
                <select style="width:100%; padding:4px;"
                    on:input=move |ev| {
                        values.update(|m| { m.insert(k.clone(), event_target_value(&ev)); });
                    }>
                    <option value="" selected=cur.is_empty()>"— none —"</option>
                    <For each=options
                         key=|(label, id)| format!("{id}-{label}")
                         children=move |(label, oid)| {
                             let selected = oid == cur;
                             view! { <option value=oid.clone() selected=selected>{label}</option> }
                         }/>
                </select>
            }
            .into_view()
        }
        "textarea" => {
            let k = fname.clone();
            let v = init();
            view! {
                <textarea prop:value=v rows="3" style="width:100%; padding:4px;"
                    on:input=move |ev| {
                        values.update(|m| { m.insert(k.clone(), event_target_value(&ev)); });
                    }></textarea>
            }
            .into_view()
        }
        "number" => {
            let k = fname.clone();
            let v = init();
            view! {
                <input type="number" prop:value=v
                    on:input=move |ev| {
                        values.update(|m| { m.insert(k.clone(), event_target_value(&ev)); });
                    }
                    style="width:100%; padding:4px;"/>
            }
            .into_view()
        }
        "date" | "datetime" => {
            let k = fname.clone();
            let v = init();
            let t = if field.widget == "date" {
                "date"
            } else {
                "datetime-local"
            };
            view! {
                <input type=t prop:value=v
                    on:input=move |ev| {
                        values.update(|m| { m.insert(k.clone(), event_target_value(&ev)); });
                    }
                    style="width:100%; padding:4px;"/>
            }
            .into_view()
        }
        _ => {
            let k = fname.clone();
            let v = init();
            view! {
                <input prop:value=v
                    on:input=move |ev| {
                        values.update(|m| { m.insert(k.clone(), event_target_value(&ev)); });
                    }
                    style="width:100%; padding:4px;"/>
            }
            .into_view()
        }
    };
    view! {
        <p>
            <label>{flabel}</label>
            { if field.required { view!{ <span style="color:#c00;">"*"</span> }.into_view() } else { ().into_view() } }
            <br/>
            {input}
        </p>
    }
}

/// Fallback: render the form straight from the cached active model (used when
/// no form definition is stored server-side).
fn model_fields(
    s: &AppState,
    entity: &str,
    values: RwSignal<HashMap<String, String>>,
) -> impl IntoView {
    let model = s.model.get().unwrap_or(api::ModelInfo {
        entities: Vec::new(),
    });
    let entity_fields: Vec<EntityInfo> = model
        .entities
        .into_iter()
        .filter(|e| e.name == entity)
        .collect();
    let fields = entity_fields
        .first()
        .map(|e| e.fields.clone())
        .unwrap_or_default();
    fields
        .iter()
        .map(|f| {
            let field = FormField {
                name: f.name.clone(),
                label: f.label.clone().unwrap_or_else(|| f.name.clone()),
                field_type: f.field_type.clone(),
                required: f.required,
                widget: infer_widget(&f.field_type).to_string(),
                options: f
                    .config
                    .get("options")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
                target_entity: None,
            };
            let ref_options = create_rw_signal(HashMap::<String, Vec<(String, String)>>::new());
            view! { <FieldInput field=field values=values ref_options=ref_options /> }
        })
        .collect_view()
}

fn infer_widget(field_type: &str) -> &'static str {
    match field_type {
        "bool" => "checkbox",
        "enum" => "select",
        "text" => "textarea",
        "date" => "date",
        "datetime" => "datetime",
        "integer" | "decimal" | "money" | "auto_number" => "number",
        "reference" => "reference",
        "attachment" => "attachment",
        _ => "text",
    }
}

/// Coerce a form string value into the JSON value matching the field's declared
/// type (per the mda-meta field registry), so bool/integer/decimal fields are
/// sent as JSON scalars instead of always as strings. Empty → `Null` (clears
/// the field); an unparseable number falls back to a string rather than drop.
fn coerce_field(types: &HashMap<String, String>, key: &str, val: &str) -> serde_json::Value {
    if val.is_empty() {
        return serde_json::Value::Null;
    }
    match types.get(key).map(String::as_str).unwrap_or("string") {
        "bool" => serde_json::Value::Bool(matches!(val, "true" | "1")),
        "integer" => val
            .parse::<i64>()
            .map(serde_json::Value::from)
            .unwrap_or_else(|_| serde_json::Value::String(val.to_string())),
        "decimal" | "money" => val
            .parse::<f64>()
            .map(serde_json::Value::from)
            .unwrap_or_else(|_| serde_json::Value::String(val.to_string())),
        _ => serde_json::Value::String(val.to_string()),
    }
}

/// Tenant-authored nav URLs are untrusted: allow only http(s) and same-app
/// relative paths. Anything else (javascript:, data:, vbscript:, unknown
/// schemes) renders as a dead link instead of executing.
fn is_safe_nav_url(url: &str) -> bool {
    let u = url.trim();
    if u.is_empty() {
        return false;
    }
    let lower = u.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return true;
    }
    // same-app relative path (starts with / but not //, which is protocol-relative)
    u.starts_with('/') && !u.starts_with("//")
}
