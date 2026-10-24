//! The JSON Schema subset every provider accepts, for tool parameters and structured output,
//! and validation of what the model returns against it (FR-AI-05).
//!
//! Only what Claude, Gemini, GPT and Grok can all be given is offered: types, enums, lengths,
//! ranges, arrays and objects; no `$ref`, `oneOf`, patterns or formats. An object never
//! allows properties it doesn't list. Not every provider constrains its answers by every rule
//! (Claude by no length or range, OpenAI by no length), so adapters translate
//! [`Schema::to_json`] for theirs, and [`Schema::validate`] still enforces the rest.

use std::fmt;

use serde_json::{Map, Value, json};

/// What the mock puts in a string it makes up (FR-AIP-05)
const EXAMPLE_TEXT: &str = "ตัวอย่าง";

/// The shape a value must have. Built like
/// `Schema::object().property("qty", Schema::integer().minimum(1))`; a builder method that
/// doesn't apply to the type panics, as that is a bug in the definition.
#[derive(Clone, Debug, PartialEq)]
pub struct Schema {
    kind: Kind,
    description: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
enum Kind {
    String {
        /// `None` allows any string
        one_of: Option<Vec<String>>,
        /// In characters
        max_length: Option<usize>,
    },
    Integer {
        minimum: Option<i64>,
        maximum: Option<i64>,
    },
    Number {
        minimum: Option<i64>,
        maximum: Option<i64>,
    },
    Boolean,
    Array {
        items: Box<Schema>,
        max_items: Option<usize>,
    },
    Object {
        properties: Vec<Property>,
    },
}

#[derive(Clone, Debug, PartialEq)]
struct Property {
    name: String,
    schema: Schema,
    required: bool,
}

/// Where a value breaks its schema, and how. Never quotes the value, so it can be logged and
/// sent back to the model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    /// `items[2].name`; empty for the value itself
    pub path: String,
    /// `must be an integer`
    pub rule: String,
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path.is_empty() {
            f.write_str(&self.rule)
        } else {
            write!(f, "`{}` {}", self.path, self.rule)
        }
    }
}

impl Schema {
    pub fn string() -> Self {
        Self::new(Kind::String {
            one_of: None,
            max_length: None,
        })
    }

    pub fn integer() -> Self {
        Self::new(Kind::Integer {
            minimum: None,
            maximum: None,
        })
    }

    pub fn number() -> Self {
        Self::new(Kind::Number {
            minimum: None,
            maximum: None,
        })
    }

    pub fn boolean() -> Self {
        Self::new(Kind::Boolean)
    }

    pub fn array(items: Schema) -> Self {
        Self::new(Kind::Array {
            items: Box::new(items),
            max_items: None,
        })
    }

    /// An object with no properties yet
    pub fn object() -> Self {
        Self::new(Kind::Object {
            properties: Vec::new(),
        })
    }

    fn new(kind: Kind) -> Self {
        Self {
            kind,
            description: None,
        }
    }

    /// What the value means, for the model
    pub fn describe(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// A string that is one of `values`
    pub fn one_of<S: Into<String>>(mut self, values: impl IntoIterator<Item = S>) -> Self {
        let Kind::String { one_of, .. } = &mut self.kind else {
            panic!("one_of applies to strings");
        };
        *one_of = Some(values.into_iter().map(Into::into).collect());
        self
    }

    /// A string of at most `chars` characters
    pub fn max_length(mut self, chars: usize) -> Self {
        let Kind::String { max_length, .. } = &mut self.kind else {
            panic!("max_length applies to strings");
        };
        *max_length = Some(chars);
        self
    }

    /// An integer or number of at least `value`
    pub fn minimum(mut self, value: i64) -> Self {
        let (Kind::Integer { minimum, .. } | Kind::Number { minimum, .. }) = &mut self.kind else {
            panic!("minimum applies to integers and numbers");
        };
        *minimum = Some(value);
        self
    }

    /// An integer or number of at most `value`
    pub fn maximum(mut self, value: i64) -> Self {
        let (Kind::Integer { maximum, .. } | Kind::Number { maximum, .. }) = &mut self.kind else {
            panic!("maximum applies to integers and numbers");
        };
        *maximum = Some(value);
        self
    }

    /// An array of at most `count` items
    pub fn max_items(mut self, count: usize) -> Self {
        let Kind::Array { max_items, .. } = &mut self.kind else {
            panic!("max_items applies to arrays");
        };
        *max_items = Some(count);
        self
    }

    /// A property the object must have
    pub fn property(self, name: impl Into<String>, schema: Schema) -> Self {
        self.with_property(name.into(), schema, true)
    }

    /// A property the object may have, or leave `null`
    pub fn optional(self, name: impl Into<String>, schema: Schema) -> Self {
        self.with_property(name.into(), schema, false)
    }

    fn with_property(mut self, name: String, schema: Schema, required: bool) -> Self {
        let Kind::Object { properties } = &mut self.kind else {
            panic!("properties apply to objects");
        };
        properties.push(Property {
            name,
            schema,
            required,
        });
        self
    }

    /// As JSON Schema
    pub fn to_json(&self) -> Value {
        let mut json = match &self.kind {
            Kind::String { one_of, max_length } => {
                let mut json = json!({ "type": "string" });
                if let Some(values) = one_of {
                    json["enum"] = json!(values);
                }
                if let Some(chars) = max_length {
                    json["maxLength"] = json!(chars);
                }
                json
            }
            Kind::Integer { minimum, maximum } => range("integer", *minimum, *maximum),
            Kind::Number { minimum, maximum } => range("number", *minimum, *maximum),
            Kind::Boolean => json!({ "type": "boolean" }),
            Kind::Array { items, max_items } => {
                let mut json = json!({ "type": "array", "items": items.to_json() });
                if let Some(count) = max_items {
                    json["maxItems"] = json!(count);
                }
                json
            }
            Kind::Object { properties } => {
                let schemas: Map<String, Value> = properties
                    .iter()
                    .map(|property| (property.name.clone(), property.schema.to_json()))
                    .collect();
                let required: Vec<&str> = properties
                    .iter()
                    .filter(|property| property.required)
                    .map(|property| property.name.as_str())
                    .collect();
                json!({
                    "type": "object",
                    "properties": schemas,
                    "required": required,
                    "additionalProperties": false,
                })
            }
        };
        if let Some(description) = &self.description {
            json["description"] = json!(description);
        }
        json
    }

    /// Every way `value` breaks the schema; empty when it matches. An optional property may
    /// be missing or `null`.
    pub fn validate(&self, value: &Value) -> Vec<Problem> {
        let mut problems = Vec::new();
        self.check(value, "", &mut problems);
        problems
    }

    fn check(&self, value: &Value, path: &str, problems: &mut Vec<Problem>) {
        let mut problem = |rule: String| {
            problems.push(Problem {
                path: path.to_owned(),
                rule,
            });
        };
        match &self.kind {
            Kind::String { one_of, max_length } => {
                let Some(text) = value.as_str() else {
                    return problem("must be a string".to_owned());
                };
                if let Some(values) = one_of
                    && !values.iter().any(|allowed| allowed == text)
                {
                    problem(format!("must be one of: {}", values.join(", ")));
                }
                if let Some(chars) = max_length
                    && text.chars().count() > *chars
                {
                    problem(format!("must be at most {chars} characters"));
                }
            }
            Kind::Integer { minimum, maximum } => match value.as_i64() {
                Some(n) => check_range(n as f64, *minimum, *maximum, problem),
                None => problem("must be an integer".to_owned()),
            },
            Kind::Number { minimum, maximum } => match value.as_f64() {
                Some(n) => check_range(n, *minimum, *maximum, problem),
                None => problem("must be a number".to_owned()),
            },
            Kind::Boolean => {
                if !value.is_boolean() {
                    problem("must be true or false".to_owned());
                }
            }
            Kind::Array { items, max_items } => {
                let Some(values) = value.as_array() else {
                    return problem("must be a list".to_owned());
                };
                if let Some(count) = max_items
                    && values.len() > *count
                {
                    problem(format!("must have at most {count} items"));
                }
                for (index, item) in values.iter().enumerate() {
                    items.check(item, &format!("{path}[{index}]"), problems);
                }
            }
            Kind::Object { properties } => {
                let Some(fields) = value.as_object() else {
                    return problem("must be an object".to_owned());
                };
                for property in properties {
                    let path = join(path, &property.name);
                    match fields.get(&property.name) {
                        None | Some(Value::Null) if property.required => problems.push(Problem {
                            path,
                            rule: "is required".to_owned(),
                        }),
                        None | Some(Value::Null) => {}
                        Some(value) => property.schema.check(value, &path, problems),
                    }
                }
                for name in fields.keys() {
                    if !properties.iter().any(|property| property.name == *name) {
                        problems.push(Problem {
                            path: join(path, name),
                            rule: "is not allowed".to_owned(),
                        });
                    }
                }
            }
        }
    }

    /// The smallest value the schema allows, which the mock answers with (FR-AIP-05): the
    /// first allowed string, the lowest allowed number, empty arrays and only the required
    /// properties.
    pub(super) fn example(&self) -> Value {
        match &self.kind {
            Kind::String { one_of, max_length } => match one_of {
                // An empty list allows nothing; this is as close as it gets
                Some(values) => values.first().map_or(Value::Null, |value| json!(value)),
                None => {
                    let chars = max_length.unwrap_or(usize::MAX);
                    json!(EXAMPLE_TEXT.chars().take(chars).collect::<String>())
                }
            },
            Kind::Integer { minimum, maximum } | Kind::Number { minimum, maximum } => {
                let lowest = minimum.unwrap_or(0);
                json!(maximum.map_or(lowest, |highest| lowest.min(highest)))
            }
            Kind::Boolean => json!(false),
            Kind::Array { .. } => json!([]),
            Kind::Object { properties } => {
                let fields: Map<String, Value> = properties
                    .iter()
                    .filter(|property| property.required)
                    .map(|property| (property.name.clone(), property.schema.example()))
                    .collect();
                Value::Object(fields)
            }
        }
    }
}

fn range(type_name: &str, minimum: Option<i64>, maximum: Option<i64>) -> Value {
    let mut json = json!({ "type": type_name });
    if let Some(value) = minimum {
        json["minimum"] = json!(value);
    }
    if let Some(value) = maximum {
        json["maximum"] = json!(value);
    }
    json
}

fn check_range(
    n: f64,
    minimum: Option<i64>,
    maximum: Option<i64>,
    mut problem: impl FnMut(String),
) {
    if let Some(min) = minimum
        && n < min as f64
    {
        problem(format!("must be at least {min}"));
    }
    if let Some(max) = maximum
        && n > max as f64
    {
        problem(format!("must be at most {max}"));
    }
}

/// `parent.name`, or `name` at the top
fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_owned()
    } else {
        format!("{parent}.{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A listing draft, roughly as #30 will ask for one
    fn draft() -> Schema {
        Schema::object()
            .property("name", Schema::string().max_length(10))
            .property("price", Schema::number().minimum(0).maximum(100_000))
            .property("category", Schema::string().one_of(["อาหาร", "ขนม"]))
            .optional("stock", Schema::integer().minimum(1))
            .optional("delivery", Schema::boolean())
            .optional(
                "options",
                Schema::array(
                    Schema::object()
                        .property("name", Schema::string())
                        .optional("extra", Schema::integer()),
                )
                .max_items(2),
            )
    }

    fn rules(schema: &Schema, value: Value) -> Vec<String> {
        schema
            .validate(&value)
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    #[test]
    fn a_matching_value_has_no_problems() {
        let value = json!({
            "name": "ขนมครก",
            "price": 20.5,
            "category": "ขนม",
            "stock": 30,
            "delivery": true,
            "options": [{ "name": "ใบเตย", "extra": 5 }],
        });
        assert_eq!(rules(&draft(), value), Vec::<String>::new());
    }

    #[test]
    fn optional_properties_may_be_missing_or_null() {
        let value = json!({ "name": "ขนมครก", "price": 20, "category": "ขนม", "stock": null });
        assert_eq!(rules(&draft(), value), Vec::<String>::new());
    }

    #[test]
    fn every_problem_is_named_with_its_path() {
        let value = json!({
            "name": "ขนมครกใบเตยสูตรโบราณ",
            "price": -1,
            "category": "ของเล่น",
            "stock": 2.5,
            "delivery": "yes",
            "options": [{ "name": 3 }, { "extra": 1 }, { "name": "x" }],
            "colour": "red",
        });
        assert_eq!(
            rules(&draft(), value),
            [
                "`name` must be at most 10 characters",
                "`price` must be at least 0",
                "`category` must be one of: อาหาร, ขนม",
                "`stock` must be an integer",
                "`delivery` must be true or false",
                "`options` must have at most 2 items",
                "`options[0].name` must be a string",
                "`options[1].name` is required",
                "`colour` is not allowed",
            ]
        );
    }

    #[test]
    fn required_properties_may_not_be_missing_or_null() {
        let value = json!({ "name": null, "price": 1 });
        assert_eq!(
            rules(&draft(), value),
            ["`name` is required", "`category` is required"]
        );
    }

    #[test]
    fn problems_with_the_value_itself_have_no_path() {
        assert_eq!(rules(&draft(), json!([])), ["must be an object"]);
        assert_eq!(
            rules(&Schema::array(Schema::boolean()), json!({})),
            ["must be a list"]
        );
        assert_eq!(
            rules(&Schema::integer().maximum(9), json!(10)),
            ["must be at most 9"]
        );
    }

    #[test]
    fn problems_never_quote_the_value() {
        let value = json!({ "name": "secret-ish text", "price": "secret-ish price" });
        let schema = Schema::object()
            .property("name", Schema::integer())
            .property("price", Schema::string().max_length(3));
        for rule in rules(&schema, value) {
            assert!(!rule.contains("secret-ish"), "{rule}");
        }
    }

    #[test]
    fn json_schema_lists_every_rule() {
        let schema = Schema::object()
            .property(
                "keywords",
                Schema::string().max_length(50).describe("What to look for"),
            )
            .optional("maxPrice", Schema::number().minimum(0))
            .optional("category", Schema::string().one_of(["อาหาร"]))
            .optional("tags", Schema::array(Schema::boolean()).max_items(3))
            .optional("qty", Schema::integer().minimum(1).maximum(99))
            .describe("Search filters");
        assert_eq!(
            schema.to_json(),
            json!({
                "type": "object",
                "description": "Search filters",
                "properties": {
                    "keywords": { "type": "string", "maxLength": 50, "description": "What to look for" },
                    "maxPrice": { "type": "number", "minimum": 0 },
                    "category": { "type": "string", "enum": ["อาหาร"] },
                    "tags": { "type": "array", "items": { "type": "boolean" }, "maxItems": 3 },
                    "qty": { "type": "integer", "minimum": 1, "maximum": 99 },
                },
                "required": ["keywords"],
                "additionalProperties": false,
            })
        );
    }

    #[test]
    fn the_example_matches_its_schema() {
        let example = draft().example();
        assert_eq!(
            example,
            json!({ "name": "ตัวอย่าง", "price": 0, "category": "อาหาร" })
        );
        assert_eq!(rules(&draft(), example), Vec::<String>::new());

        let short = Schema::string().max_length(3);
        assert_eq!(rules(&short, short.example()), Vec::<String>::new());
        let negative = Schema::integer().maximum(-5);
        assert_eq!(rules(&negative, negative.example()), Vec::<String>::new());
    }

    #[test]
    #[should_panic(expected = "one_of applies to strings")]
    fn a_rule_for_another_type_is_a_bug() {
        let _ = Schema::integer().one_of(["1"]);
    }
}
