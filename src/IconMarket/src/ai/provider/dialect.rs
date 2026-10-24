//! Rewrites of [`Schema::to_json`](crate::ai::Schema::to_json) for providers that take less, or
//! take it differently. Whatever is dropped is still enforced: the gateway validates every
//! answer against the full schema (FR-AI-05).

use serde_json::{Value, json};

/// `schema` without `keywords`, at every level.
pub fn without(mut schema: Value, keywords: &[&str]) -> Value {
    strip(&mut schema, keywords);
    schema
}

fn strip(schema: &mut Value, keywords: &[&str]) {
    let Some(fields) = schema.as_object_mut() else {
        return;
    };
    for keyword in keywords {
        fields.remove(*keyword);
    }
    for_each_subschema(fields, |subschema| strip(subschema, keywords));
}

/// `schema` in the form strict modes require: every object lists all its properties as
/// required, and an optional one may be `null` instead, which validation treats as missing.
pub fn strict(mut schema: Value) -> Value {
    make_strict(&mut schema);
    schema
}

fn make_strict(schema: &mut Value) {
    let Some(fields) = schema.as_object_mut() else {
        return;
    };
    for_each_subschema(fields, make_strict);
    let required: Vec<Value> = match fields.get("required") {
        Some(Value::Array(names)) => names.clone(),
        _ => Vec::new(),
    };
    let Some(Value::Object(properties)) = fields.get_mut("properties") else {
        return;
    };
    for (name, property) in properties.iter_mut() {
        if !required.contains(&json!(name)) {
            *property = json!({ "anyOf": [property.take(), { "type": "null" }] });
        }
    }
    let names: Vec<Value> = properties.keys().map(|name| json!(name)).collect();
    fields.insert("required".to_owned(), Value::Array(names));
}

/// Calls `f` on each schema directly inside an object's, array's or `anyOf`'s schema.
fn for_each_subschema(fields: &mut serde_json::Map<String, Value>, mut f: impl FnMut(&mut Value)) {
    if let Some(Value::Object(properties)) = fields.get_mut("properties") {
        properties.values_mut().for_each(&mut f);
    }
    if let Some(items) = fields.get_mut("items") {
        f(items);
    }
    if let Some(Value::Array(choices)) = fields.get_mut("anyOf") {
        choices.iter_mut().for_each(f);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::Schema;

    fn filters() -> Value {
        Schema::object()
            .property("keywords", Schema::string().max_length(50))
            .optional("maximum", Schema::number().minimum(0).maximum(9))
            .optional(
                "tags",
                Schema::array(Schema::string().max_length(5)).max_items(3),
            )
            .to_json()
    }

    #[test]
    fn keywords_are_dropped_at_every_level_but_not_property_names() {
        assert_eq!(
            without(filters(), &["maxLength", "minimum", "maximum"]),
            json!({
                "type": "object",
                "properties": {
                    "keywords": { "type": "string" },
                    // A property named like a keyword stays
                    "maximum": { "type": "number" },
                    "tags": { "type": "array", "items": { "type": "string" }, "maxItems": 3 },
                },
                "required": ["keywords"],
                "additionalProperties": false,
            })
        );
    }

    #[test]
    fn strict_schemas_require_everything_and_allow_null_for_optional_properties() {
        let nested = Schema::object()
            .optional("delivery", Schema::boolean())
            .optional(
                "options",
                Schema::array(Schema::object().optional("extra", Schema::integer())),
            )
            .to_json();
        assert_eq!(
            strict(nested),
            json!({
                "type": "object",
                "properties": {
                    "delivery": { "anyOf": [{ "type": "boolean" }, { "type": "null" }] },
                    "options": { "anyOf": [
                        {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "extra": { "anyOf": [{ "type": "integer" }, { "type": "null" }] },
                                },
                                "required": ["extra"],
                                "additionalProperties": false,
                            },
                        },
                        { "type": "null" },
                    ] },
                },
                "required": ["delivery", "options"],
                "additionalProperties": false,
            })
        );
    }

    #[test]
    fn what_a_strict_schema_allows_still_validates() {
        let schema = Schema::object()
            .property("name", Schema::string())
            .optional("stock", Schema::integer());
        assert_eq!(schema.validate(&json!({ "name": "ไข่", "stock": null })), []);
        let strict = strict(schema.to_json());
        assert_eq!(strict["required"], json!(["name", "stock"]));
    }
}
