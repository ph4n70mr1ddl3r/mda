import { useEffect, useState } from "react";
import { FormRenderer } from "./FormRenderer";
import { sampleForm, type FormDef } from "./form-def";

/**
 * Phase 0 spike (ADR-0009): evaluate React+TS for a metadata-driven form
 * renderer. Fetches a form definition and renders it. In v0 there is no
 * form API yet, so it falls back to a local stub after probing /health.
 */
export function App() {
  const [form] = useState<FormDef>(sampleForm);
  const [backend, setBackend] = useState<string>("(checking…)");

  useEffect(() => {
    fetch("/health")
      .then((r) => (r.ok ? r.json() : Promise.reject()))
      .then(() => setBackend("connected"))
      .catch(() => setBackend("not running (using stub form)"));
    // When /api/forms/:entity exists: fetch it and setForm.
  }, []);

  return (
    <main style={{ fontFamily: "system-ui, sans-serif", maxWidth: 520, margin: "2rem auto" }}>
      <h1>MDA — React spike</h1>
      <p>Backend: {backend}</p>
      <FormRenderer form={form} />
    </main>
  );
}
