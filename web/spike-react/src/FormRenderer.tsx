import { useMemo, useState } from "react";
import type { FieldDef, FormDef } from "./form-def";

type Values = Record<string, unknown>;

/** A metadata-driven form renderer: maps a FieldDef to the right input. */
export function FormRenderer({ form }: { form: FormDef }) {
  const [values, setValues] = useState<Values>(() => initial(form));
  const [submitted, setSubmitted] = useState<Values | null>(null);

  const set = (name: string, v: unknown) =>
    setValues((prev) => ({ ...prev, [name]: v }));

  return (
    <form
      onSubmit={(e) => {
        e.preventDefault();
        setSubmitted(values);
      }}
    >
      <h2>{form.label}</h2>
      {form.fields.map((f) => (
        <p key={f.name}>
          <label style={{ display: "block", fontWeight: 600 }}>
            {f.label}
            {f.required ? " *" : ""}
          </label>
          {inputFor(f, values[f.name], (v) => set(f.name, v))}
        </p>
      ))}
      <button type="submit">Save</button>
      {submitted && (
        <pre style={{ marginTop: 16 }}>{JSON.stringify(submitted, null, 2)}</pre>
      )}
    </form>
  );
}

function inputFor(f: FieldDef, value: unknown, set: (v: unknown) => void) {
  switch (f.type) {
    case "text":
      return <textarea value={(value as string) ?? ""} onChange={(e) => set(e.target.value)} />;
    case "bool":
      return (
        <input
          type="checkbox"
          checked={Boolean(value)}
          onChange={(e) => set(e.target.checked)}
        />
      );
    case "enum":
      return (
        <select value={(value as string) ?? ""} onChange={(e) => set(e.target.value)}>
          <option value="" disabled>
            Select…
          </option>
          {f.options?.map((o) => (
            <option key={o} value={o}>
              {o}
            </option>
          ))}
        </select>
      );
    case "integer":
    case "number":
      return (
        <input
          type="number"
          step={f.type === "integer" ? 1 : "any"}
          value={(value as string) ?? ""}
          onChange={(e) => set(e.target.value)}
        />
      );
    case "date":
      return (
        <input type="date" value={(value as string) ?? ""} onChange={(e) => set(e.target.value)} />
      );
    case "string":
    default:
      return (
        <input
          type="text"
          value={(value as string) ?? ""}
          onChange={(e) => set(e.target.value)}
        />
      );
  }
}

function initial(form: FormDef): Values {
  return useMemo(() => {
    const v: Values = {};
    for (const f of form.fields) v[f.name] = f.type === "bool" ? false : "";
    return v;
  }, [form]);
}
