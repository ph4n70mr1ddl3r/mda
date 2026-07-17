//! MDA Runtime UI — metadata-driven Leptos CSR app (Phase 6).
//! Login → entity menu → list → form (create/edit) with a real-time conflict
//! banner over the SSE event channel.

mod api;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use leptos::*;
use wasm_bindgen::{closure::Closure, JsCast};

use api::{local_get, local_remove, local_set, EntityInfo, ModelInfo};

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
    /// Bumped after a create/update/delete so list views refetch on return
    /// (doesn't rely on SPA component remount to pick up changes).
    pub refresh: RwSignal<u64>,
}

#[derive(Clone, PartialEq)]
pub enum Page {
    Dashboard,
    List(String),
    /// Edit an existing record (Some id) or create one (None).
    Form {
        entity: String,
        id: Option<String>,
    },
}

// ===== root =====

#[component]
pub fn App() -> impl IntoView {
    let token = create_rw_signal(local_get("mda_token"));
    let page = create_rw_signal(Page::Dashboard);
    let model = create_rw_signal(None);
    let refresh = create_rw_signal(0u64);
    let state = AppState {
        token,
        page,
        model,
        refresh,
    };
    provide_context(state);

    let token_for_effect = token;
    let model_for_effect = model;
    create_effect(move |_| {
        if let Some(t) = token_for_effect.get() {
            spawn_local(async move {
                if let Ok(m) = api::get_model(&t).await {
                    model_for_effect.set(Some(m));
                }
            });
        }
    });

    let state = use_context::<AppState>().unwrap();
    view! {
        <div style="font-family: system-ui, sans-serif; max-width: 900px; margin: 1rem auto;">
            <header style="display:flex; justify-content:space-between; align-items:center; padding:0.5rem 0; border-bottom:1px solid #ccc;">
                <strong style="font-size:1.2rem;">"MDA"</strong>
                {move || if state.token.get().is_some() {
                    let s = state;
                    view! {
                        <button style="cursor:pointer;"
                            on:click=move |_: leptos::ev::MouseEvent| {
                                local_remove("mda_token");
                                s.token.set(None);
                                s.page.set(Page::Dashboard);
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
                            Page::Dashboard => view! { <Dashboard/> }.into_view(),
                            Page::List(name) => view! { <EntityList entity=name/> }.into_view(),
                            Page::Form { entity, id } => {
                                view! { <RecordForm entity id/> }.into_view()
                            }
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
                                s.page.set(Page::Dashboard);
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

// ===== dashboard =====

#[component]
fn Dashboard() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let s = state;
    view! {
        <div>
            <h2>"Entities"</h2>
            <For each=move || s.model.get().map(|m| m.entities).unwrap_or_default()
                 key=|e| e.name.clone()
                 children=move |e: EntityInfo| {
                     let s2 = s;
                     let name = e.name.clone();
                     let label = e.label.clone().unwrap_or_else(|| name.clone());
                     let count = e.fields.len();
                     view! {
                         <div style="padding:8px; margin:4px 0; border:1px solid #ddd; border-radius:4px; cursor:pointer;"
                             on:click=move |_: leptos::ev::MouseEvent| {
                                 s2.page.set(Page::List(name.clone()));
                             }>
                             <strong>{label}</strong>
                             <span style="color:#888; margin-left:8px;">
                                 {format!("{} fields", count)}
                             </span>
                         </div>
                     }
                 }/>
            {move || {
                let empty = s.model.get().map(|m| m.entities.is_empty()).unwrap_or(true);
                if empty { view!{ <p style="color:#888;">"No entities published yet."</p> }.into_view() }
                else { ().into_view() }
            }}
        </div>
    }
}

// ===== entity list =====

#[component]
fn EntityList(entity: String) -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let token = state.token;
    let entity_fetch = entity.clone();
    let refresh = state.refresh;
    let resource = create_resource(
        move || refresh.get(),
        move |_| {
            let token = token.get().unwrap_or_default();
            let entity = entity_fetch.clone();
            async move { api::list_records(&token, &entity).await }
        },
    );
    let s = state;
    let entity_new = entity.clone();
    let ent_sig = create_rw_signal(entity.clone());

    view! {
        <div>
            <div style="display:flex; align-items:center; gap:1rem; margin:1rem 0;">
                <button on:click=move |_: leptos::ev::MouseEvent| s.page.set(Page::Dashboard)
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
                    Some(Ok(data)) => {
                        if data.items.is_empty() {
                            view! { <p style="color:#888;">"No records."</p> }.into_view()
                        } else {
                            data.items.into_iter().map(move |row| {
                                let id = row["id"].as_str().unwrap_or("").to_string();
                                let display = row.as_object()
                                    .and_then(|o| o.iter()
                                        .find(|(k,_)| k.as_str() != "id" && k.as_str() != "version"
                                            && k.as_str() != "owner_id" && k.as_str() != "state"
                                            && k.as_str() != "created_at" && k.as_str() != "updated_at")
                                        .map(|(_,v)| match v {
                                            serde_json::Value::String(s) => s.clone(),
                                            o => o.to_string(),
                                        }))
                                    .unwrap_or_else(|| id.clone());
                                let s_edit = s;
                                let id_edit = id.clone();
                                let id_del = id.clone();
                                let token_del = token;
                                let ent_sig_edit = ent_sig;
                                let ent_sig_del = ent_sig;
                                let res_ref = resource;
                                view! {
                                    <div style="padding:6px; margin:4px 0; border:1px solid #eee; border-radius:4px; display:flex; align-items:center;">
                                        <span style="flex:1;">{display}</span>
                                        <span style="color:#aaa; margin:0 8px; font-size:12px;">{format!("{:.8}", id)}</span>
                                        <button style="cursor:pointer; margin-left:4px;"
                                            on:click=move |_: leptos::ev::MouseEvent| {
                                                s_edit.page.set(Page::Form {
                                                    entity: ent_sig_edit.get(),
                                                    id: Some(id_edit.clone()),
                                                });
                                            }>"Edit"</button>
                                        <button style="cursor:pointer; margin-left:4px; color:#a00;"
                                            on:click=move |_: leptos::ev::MouseEvent| {
                                                let tok = token_del.get().unwrap_or_default();
                                                let ent = ent_sig_del.get();
                                                let idd = id_del.clone();
                                                let res = res_ref;
                                                spawn_local(async move {
                                                    let _ = api::delete_record(&tok, &ent, &idd).await;
                                                    res.refetch();
                                                });
                                            }>"Delete"</button>
                                    </div>
                                }
                            }).collect_view()
                        }
                    }
                    Some(Err(e)) => view!{ <p style="color:red;">{format!("Error: {e}")}</p> }.into_view(),
                    None => ().into_view(),
                }}
            </Suspense>
        </div>
    }
}

// ===== record form (create / edit) + real-time conflict banner =====

#[component]
fn RecordForm(entity: String, id: Option<String>) -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let token = state.token;
    let s = state;

    // field definitions from the cached active model
    let entity_for_fields = entity.clone();
    let fields_of = move || {
        s.model
            .get()
            .and_then(|m| {
                m.entities
                    .into_iter()
                    .find(|e| e.name == entity_for_fields)
                    .map(|e| e.fields)
            })
            .unwrap_or_default()
    };

    let values: RwSignal<HashMap<String, String>> = create_rw_signal(HashMap::new());
    let version = create_rw_signal::<Option<i64>>(None);
    let loaded = create_rw_signal(false);
    let error = create_rw_signal(String::new());
    // remote new version from the SSE channel (None = no concurrent change).
    let conflict = create_rw_signal::<Option<i64>>(None);

    // load the record on edit
    let id_load = id.clone();
    let entity_load = entity.clone();
    create_effect(move |_| {
        if loaded.get() {
            return;
        }
        let tok = token.get().unwrap_or_default();
        if let Some(rid) = id_load.clone() {
            let ent = entity_load.clone();
            spawn_local(async move {
                match api::get_record(&tok, &ent, &rid).await {
                    Ok(rec) => {
                        if let Some(obj) = rec.as_object() {
                            let mut m = HashMap::new();
                            for (k, v) in obj {
                                let txt = match v {
                                    serde_json::Value::Null => String::new(),
                                    serde_json::Value::String(x) => x.to_string(),
                                    other => other.to_string(),
                                };
                                m.insert(k.clone(), txt);
                            }
                            values.set(m);
                        }
                        version.set(rec["version"].as_i64());
                    }
                    Err(e) => error.set(e),
                }
                loaded.set(true);
            });
        } else {
            loaded.set(true);
        }
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
    let entity_cancel2 = entity.clone();
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
                <button on:click=move |_: leptos::ev::MouseEvent| s_back.page.set(Page::List(entity_cancel2.clone())) style="cursor:pointer;">"← Back"</button>
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
                let vals = values.get();
                fields_of().iter().map(|f| {
                    let fname = f.name.clone();
                    let flabel = f.label.clone().unwrap_or_else(|| f.name.clone());
                    let init = vals.get(&f.name).cloned().unwrap_or_default();
                    let ftype = f.field_type.clone();
                    let opts: Vec<String> = f.config.get("options")
                        .and_then(|o| o.as_array())
                        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                        .unwrap_or_default();
                    let values_ref = values;
                    let key_for_input = fname.clone();
                    let input = match ftype.as_str() {
                        "bool" => {
                            let k = fname.clone();
                            view! {
                                <input type="checkbox" prop:checked=init == "true"
                                    on:input=move |ev| {
                                        let val = event_target_checked(&ev).to_string();
                                        values_ref.update(|m| { m.insert(k.clone(), val); });
                                    }/>
                            }.into_view()
                        }
                        "enum" if !opts.is_empty() => {
                            let k = fname.clone();
                            let opts_view = opts.clone().into_iter().map(|o| {
                                view! { <option value=o.clone() selected=o == init>{o}</option> }
                            }).collect_view();
                            view! {
                                <select style="width:100%; padding:4px;"
                                    on:input=move |ev| {
                                        values_ref.update(|m| { m.insert(k.clone(), event_target_value(&ev)); });
                                    }>
                                    {opts_view}
                                </select>
                            }.into_view()
                        }
                        "text" => {
                            let k = fname.clone();
                            view! {
                                <textarea prop:value=init rows="3" style="width:100%; padding:4px;"
                                    on:input=move |ev| {
                                        values_ref.update(|m| { m.insert(k.clone(), event_target_value(&ev)); });
                                    }></textarea>
                            }.into_view()
                        }
                        _ => {
                            let k = fname.clone();
                            view! {
                                <input prop:value=init
                                    on:input=move |ev| {
                                        values_ref.update(|m| { m.insert(k.clone(), event_target_value(&ev)); });
                                    }
                                    style="width:100%; padding:4px;"/>
                            }.into_view()
                        }
                    };
                    let _ = key_for_input;
                    view! {
                        <p>
                            <label>{flabel}</label>
                            { if f.required { view!{ <span style="color:#c00;">"*"</span> }.into_view() } else { ().into_view() } }
                            <br/>
                            {input}
                        </p>
                    }
                }).collect_view()
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
