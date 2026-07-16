//! MDA Runtime UI — metadata-driven Leptos CSR app (Phase 6).
//! Login → entity menu → list. (Form create/edit is the next increment.)

mod api;

use leptos::*;

use api::{local_get, local_remove, local_set, EntityInfo, ModelInfo};

// ===== global app state =====

#[derive(Clone, Copy)]
pub struct AppState {
    pub token: RwSignal<Option<String>>,
    pub page: RwSignal<Page>,
    pub model: RwSignal<Option<ModelInfo>>,
}

#[derive(Clone, PartialEq)]
pub enum Page {
    Dashboard,
    List(String),
}

// ===== root =====

#[component]
pub fn App() -> impl IntoView {
    let token = create_rw_signal(local_get("mda_token"));
    let page = create_rw_signal(Page::Dashboard);
    let model = create_rw_signal(None);
    let state = AppState { token, page, model };
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
    let (email, set_email) = create_signal("admin@mda.local".to_string());
    let (password, set_password) = create_signal("admin123".to_string());
    let (error, set_error) = create_signal(String::new());
    let (loading, set_loading) = create_signal(false);

    let s = state;
    view! {
        <div style="max-width: 300px; margin: 2rem auto;">
            <h2>"Login"</h2>
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
                    let (e, p) = (email.get(), password.get());
                    spawn_local(async move {
                        match api::login(&e, &p).await {
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
                (if e.is_empty() { ().into_view() }
                 else { view!{ <p style="color:red;">{e}</p> }.into_view() })
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
    let resource = create_resource(
        move || (),
        move |_| {
            let token = token.get().unwrap_or_default();
            let entity = entity_fetch.clone();
            async move { api::list_records(&token, &entity).await }
        },
    );
    let s = state;
    let entity_back = entity.clone();

    view! {
        <div>
            <div style="display:flex; align-items:center; gap:1rem; margin:1rem 0;">
                <button on:click=move |_: leptos::ev::MouseEvent| s.page.set(Page::Dashboard)
                    style="cursor:pointer;">"← Back"</button>
                <h2 style="margin:0;">{entity_back.clone()}</h2>
            </div>
            <Suspense fallback=move || view!{ <p>"Loading…"</p> }>
                {move || match resource.get() {
                    Some(Ok(data)) => {
                        if data.items.is_empty() {
                            view! { <p style="color:#888;">"No records."</p> }.into_view()
                        } else {
                            data.items.into_iter().map(|row| {
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
                                view! {
                                    <div style="padding:6px; margin:4px 0; border:1px solid #eee; border-radius:4px;">
                                        {display}
                                        <span style="color:#aaa; margin-left:8px; font-size:12px;">{format!("{:.8}", id)}</span>
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
