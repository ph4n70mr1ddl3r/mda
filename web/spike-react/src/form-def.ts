// Minimal metadata shape the spike renders. Mirrors (a subset of) what
// /api/forms/:entity will eventually return from the runtime.

export type FieldType =
  | "string"
  | "text"
  | "integer"
  | "number"
  | "bool"
  | "enum"
  | "date";

export interface FieldDef {
  name: string;
  label: string;
  type: FieldType;
  required?: boolean;
  options?: string[]; // for enum
}

export interface FormDef {
  entity: string;
  label: string;
  fields: FieldDef[];
}

// Stub definition (no backend form API yet). Swap for a fetch to /api/forms/:entity.
export const sampleForm: FormDef = {
  entity: "Customer",
  label: "Customer",
  fields: [
    { name: "name", label: "Name", type: "string", required: true },
    { name: "email", label: "Email", type: "string", required: true },
    { name: "tier", label: "Tier", type: "enum", options: ["Bronze", "Silver", "Gold"] },
    { name: "credit_limit", label: "Credit Limit", type: "number" },
    { name: "active", label: "Active", type: "bool" },
  ],
};
