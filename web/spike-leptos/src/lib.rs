//! Phase 0 spike (ADR-0009): a metadata-driven form renderer in Leptos (CSR),
//! to evaluate Rust/WASM ergonomics vs the React spike in `web/spike-react`.

use std::collections::HashMap;
use std::rc::Rc;

use leptos::*;

#[derive(serde::Deserialize, Clone)]
pub struct FieldDef {
    pub name: String,
    pub label: String,
    #[serde(rename = "type")]
    pub field_type: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub options: Vec<String>,
}

#[derive(serde::Deserialize, Clone)]
pub struct FormDef {
    #[allow(dead_code)]
    pub entity: String,
    pub label: String,
    pub fields: Vec<FieldDef>,
}

/// Stub form (no backend form API yet). Swap for a fetch to `/api/forms/:entity`.
fn sample() -> FormDef {
    serde_json::from_str(
        r#"{
        "entity": "Customer",
        "label": "Customer",
        "fields": [
            {"name":"name","label":"Name","type":"string","required":true},
            {"name":"email","label":"Email","type":"string","required":true},
            {"name":"tier","label":"Tier","type":"enum","options":["Bronze","Silver","Gold"]},
            {"name":"credit_limit","label":"Credit Limit","type":"number"},
            {"name":"active","label":"Active","type":"bool"}
        ]
    }"#,
    )
    .unwrap()
}

/// Top-level app component.
#[component]
pub fn App() -> impl IntoView {
    view! {
        <main style="font-family:system-ui,sans-serif;max-width:520px;margin:2rem auto">
            <h1>"MDA — Leptos spike"</h1>
            <FormRenderer form=sample()/>
        </main>
    }
}

/// A metadata-driven form renderer.
#[component]
fn FormRenderer(#[prop(into)] form: FormDef) -> impl IntoView {
    let (values, set_values) = create_signal(HashMap::<String, String>::new());
    let (submitted, set_submitted) = create_signal(None::<HashMap<String, String>>);

    view! {
        <form on:submit=move |ev: leptos::ev::SubmitEvent| {
            ev.prevent_default();
            set_submitted.set(Some(values.get()));
        }>
            <h2>{form.label.clone()}</h2>
            <For each=move || form.fields.clone() key=|f| f.name.clone() children=move |f: FieldDef| {
                let name = f.name.clone();
                let setv = set_values;
                view! {
                    <p>
                        <label style="display:block;font-weight:600">
                            {format!("{}{}", f.label, if f.required { " *" } else { "" })}
                        </label>
                        {input_for(&f, move |v: String| {
                            setv.update(|m| {
                                m.insert(name.clone(), v);
                            });
                        })}
                    </p>
                }
            } />
            <button type="submit">"Save"</button>
            {move || {
                submitted.get().map(|s| {
                    view! {
                        <pre style="margin-top:16px">
                            {serde_json::to_string_pretty(&s).unwrap_or_default()}
                        </pre>
                    }
                })
            }}
        </form>
    }
}

/// Map a field definition to the right input element.
fn input_for(f: &FieldDef, on_change: impl Fn(String) + 'static) -> impl IntoView {
    let on_change = Rc::new(on_change);
    match f.field_type.as_str() {
        "bool" => {
            let oc = on_change.clone();
            view! {
                <input
                    type="checkbox"
                    on:change=move |e| oc(event_target_checked(&e).to_string())
                />
            }
            .into_view()
        }
        "enum" => {
            let options = f.options.clone();
            let oc = on_change.clone();
            view! {
                <select on:change=move |e| oc(event_target_value(&e))>
                    <option value="" disabled selected>
                        "Select…"
                    </option>
                    {options
                        .iter()
                        .map(|o| {
                            view! {
                                <option value=o.clone()>
                                    {o.clone()}
                                </option>
                            }
                        })
                        .collect::<Vec<_>>()}
                </select>
            }
            .into_view()
        }
        "number" => {
            let oc = on_change.clone();
            view! {
                <input type="number" on:input=move |e| oc(event_target_value(&e)) />
            }
            .into_view()
        }
        _ => {
            let oc = on_change.clone();
            view! {
                <input type="text" on:input=move |e| oc(event_target_value(&e)) />
            }
            .into_view()
        }
    }
}
