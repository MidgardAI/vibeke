//! A small shape language for the control-API schema registry (`api_schema`), its JSON Schema
//! 2020-12 emitter and a validator used by tests.
//!
//! ```text
//! {pane: Target, direction: right|down|left|up, ratio?: number = 0.5, env?: {*: string}}
//! ```
//!
//! - object: `{field, field?, field: Type, field?: Type = default, ...}`; a field without a type
//!   is `any`; `{*: Type}` is a map with arbitrary string keys; keys may be quoted.
//! - optional vs nullable: `field?: T` may be **absent** but, when present, is a `T`; a field
//!   the server can send as JSON `null` says so in its type: `field: T|null` (always present,
//!   maybe null) or `field?: T|null` (absent or null). The emitter writes `null` into the
//!   schema only when the type says so (`anyOf: [T, {type: null}]`) and the validator below
//!   checks exactly what the emitted JSON Schema means.
//! - array: `[Type]`; union: `A | B`; bare lowercase words that are not builtin type names are
//!   string literals (`allow|deny`), quoted `'x'` too. Bare `true` and `false` are JSON boolean
//!   literals (`ask|bool`, `requires_confirmation: true`); quote them (`'true'`) for the string.
//! - builtins: `string bool int number any object null`; CamelCase names are references to
//!   shared definitions.
//! - Objects are open (unknown fields are allowed): within `vibeke/1` results only grow
//!   (07 §1.5).

use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq)]
pub enum Shape {
    Any,
    Null,
    Bool,
    Int,
    Number,
    Str,
    /// An open JSON object with no declared fields.
    Object,
    Lit(String),
    /// A JSON boolean literal (bare `true` / `false`).
    BoolLit(bool),
    Array(Box<Shape>),
    Map(Box<Shape>),
    Obj(Vec<Field>),
    Union(Vec<Shape>),
    Ref(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub name: String,
    pub optional: bool,
    pub shape: Shape,
    pub default: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Sym(&'static str),
    Ident(String),
    Quoted(String),
    Num(f64),
}

fn lex(src: &str) -> Result<Vec<Tok>, String> {
    let cs: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut out = vec![];
    while i < cs.len() {
        let c = cs[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '.' && cs[i..].starts_with(&['.', '.', '.']) {
            out.push(Tok::Sym("..."));
            i += 3;
        } else if let Some(sym) = ["{", "}", "[", "]", ",", ":", "?", "=", "|", "*"]
            .iter()
            .find(|s| s.starts_with(c))
        {
            out.push(Tok::Sym(sym));
            i += 1;
        } else if c == '\'' || c == '"' {
            let mut j = i + 1;
            let mut s = String::new();
            while j < cs.len() && cs[j] != c {
                if cs[j] == '\\' && j + 1 < cs.len() {
                    j += 1;
                }
                s.push(cs[j]);
                j += 1;
            }
            if j >= cs.len() {
                return Err("unterminated quote".into());
            }
            out.push(Tok::Quoted(s));
            i = j + 1;
        } else if c.is_ascii_digit()
            || (c == '-' && cs.get(i + 1).is_some_and(char::is_ascii_digit))
        {
            let mut j = i + 1;
            while j < cs.len() && (cs[j].is_ascii_digit() || cs[j] == '.') {
                j += 1;
            }
            let t: String = cs[i..j].iter().collect();
            out.push(Tok::Num(t.parse().map_err(|_| format!("bad number {t}"))?));
            i = j;
        } else if c.is_alphanumeric() || c == '_' {
            let mut j = i;
            while j < cs.len() && (cs[j].is_alphanumeric() || cs[j] == '_' || cs[j] == '-') {
                j += 1;
            }
            out.push(Tok::Ident(cs[i..j].iter().collect()));
            i = j;
        } else {
            return Err(format!("unexpected character {c:?}"));
        }
    }
    Ok(out)
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }
    fn eat(&mut self, s: &str) -> bool {
        if matches!(self.peek(), Some(Tok::Sym(x)) if *x == s) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    fn expect(&mut self, s: &str) -> Result<(), String> {
        if self.eat(s) {
            Ok(())
        } else {
            Err(format!("expected `{s}`, found {:?}", self.peek()))
        }
    }

    fn union(&mut self) -> Result<Shape, String> {
        let mut arms = vec![self.atom()?];
        while self.eat("|") {
            arms.push(self.atom()?);
        }
        Ok(if arms.len() == 1 {
            arms.pop().unwrap()
        } else {
            Shape::Union(arms)
        })
    }

    fn atom(&mut self) -> Result<Shape, String> {
        match self.peek().cloned() {
            Some(Tok::Sym("{")) => {
                self.pos += 1;
                self.object()
            }
            Some(Tok::Sym("[")) => {
                self.pos += 1;
                let inner = self.union()?;
                self.expect("]")?;
                Ok(Shape::Array(Box::new(inner)))
            }
            Some(Tok::Quoted(s)) => {
                self.pos += 1;
                Ok(Shape::Lit(s))
            }
            Some(Tok::Ident(name)) => {
                self.pos += 1;
                Ok(match name.as_str() {
                    "string" => Shape::Str,
                    "bool" => Shape::Bool,
                    "int" => Shape::Int,
                    "number" => Shape::Number,
                    "any" => Shape::Any,
                    "object" => Shape::Object,
                    "null" => Shape::Null,
                    "true" => Shape::BoolLit(true),
                    "false" => Shape::BoolLit(false),
                    n if n.starts_with(|c: char| c.is_ascii_uppercase()) => Shape::Ref(n.into()),
                    n => Shape::Lit(n.into()),
                })
            }
            t => Err(format!("expected a type, found {t:?}")),
        }
    }

    fn object(&mut self) -> Result<Shape, String> {
        let mut fields = vec![];
        let mut map: Option<Shape> = None;
        loop {
            if self.eat("}") {
                break;
            }
            if self.eat("...") {
                // Objects are open already.
            } else if self.eat("*") {
                self.expect(":")?;
                map = Some(self.union()?);
            } else {
                let name = match self.peek().cloned() {
                    Some(Tok::Ident(n) | Tok::Quoted(n)) => {
                        self.pos += 1;
                        n
                    }
                    t => return Err(format!("expected a field name, found {t:?}")),
                };
                let optional = self.eat("?");
                let shape = if self.eat(":") {
                    self.union()?
                } else {
                    Shape::Any
                };
                let default = if self.eat("=") {
                    Some(self.default_value()?)
                } else {
                    None
                };
                fields.push(Field {
                    name,
                    optional,
                    shape,
                    default,
                });
            }
            if !self.eat(",") {
                self.expect("}")?;
                break;
            }
        }
        match (map, fields.is_empty()) {
            (Some(m), true) => Ok(Shape::Map(Box::new(m))),
            (Some(_), false) => Err("`*` cannot be mixed with named fields".into()),
            (None, true) => Ok(Shape::Object),
            (None, false) => Ok(Shape::Obj(fields)),
        }
    }

    fn default_value(&mut self) -> Result<Value, String> {
        match self.peek().cloned() {
            Some(Tok::Num(n)) => {
                self.pos += 1;
                Ok(if n.fract() == 0.0 && n.abs() < 9e15 {
                    json!(n as i64)
                } else {
                    json!(n)
                })
            }
            Some(Tok::Quoted(s)) => {
                self.pos += 1;
                Ok(json!(s))
            }
            Some(Tok::Ident(s)) => {
                self.pos += 1;
                Ok(match s.as_str() {
                    "true" => json!(true),
                    "false" => json!(false),
                    "null" => Value::Null,
                    _ => json!(s),
                })
            }
            Some(Tok::Sym("[")) => {
                self.pos += 1;
                let mut v = vec![];
                while !self.eat("]") {
                    v.push(self.default_value()?);
                    if !self.eat(",") {
                        self.expect("]")?;
                        break;
                    }
                }
                Ok(Value::Array(v))
            }
            Some(Tok::Sym("{")) => {
                self.pos += 1;
                self.expect("}")?;
                Ok(json!({}))
            }
            t => Err(format!("expected a default value, found {t:?}")),
        }
    }
}

/// Parse one shape.
pub fn parse(src: &str) -> Result<Shape, String> {
    let mut p = Parser {
        toks: lex(src)?,
        pos: 0,
    };
    let s = p.union()?;
    if p.pos != p.toks.len() {
        return Err(format!("trailing input at token {:?}", p.peek()));
    }
    Ok(s)
}

impl Shape {
    /// Names of every `Ref` inside the shape.
    pub fn refs(&self, out: &mut Vec<String>) {
        match self {
            Shape::Ref(n) => out.push(n.clone()),
            Shape::Array(s) | Shape::Map(s) => s.refs(out),
            Shape::Obj(fs) => fs.iter().for_each(|f| f.shape.refs(out)),
            Shape::Union(a) => a.iter().for_each(|s| s.refs(out)),
            _ => {}
        }
    }

    /// JSON Schema 2020-12 for this shape. `$ref`s point at `#/$defs/<Name>`.
    pub fn to_schema(&self) -> Value {
        match self {
            Shape::Any => json!({}),
            Shape::Null => json!({"type": "null"}),
            Shape::Bool => json!({"type": "boolean"}),
            Shape::Int => json!({"type": "integer"}),
            Shape::Number => json!({"type": "number"}),
            Shape::Str => json!({"type": "string"}),
            Shape::Object => json!({"type": "object"}),
            Shape::Lit(s) => json!({"const": s}),
            Shape::BoolLit(b) => json!({"const": b}),
            Shape::Array(s) => json!({"type": "array", "items": s.to_schema()}),
            Shape::Map(s) => json!({"type": "object", "additionalProperties": s.to_schema()}),
            Shape::Ref(n) => json!({"$ref": format!("#/$defs/{n}")}),
            Shape::Obj(fs) => {
                let mut props = Map::new();
                let mut required = vec![];
                for f in fs {
                    let mut s = f.shape.to_schema();
                    if let (Some(d), Some(o)) = (&f.default, s.as_object_mut()) {
                        o.insert("default".into(), d.clone());
                    } else if let Some(d) = &f.default {
                        // `true`/`false` schemas cannot carry annotations; wrap.
                        s = json!({"allOf": [s], "default": d});
                    }
                    props.insert(f.name.clone(), s);
                    if !f.optional {
                        required.push(json!(f.name));
                    }
                }
                let mut o = Map::new();
                o.insert("type".into(), json!("object"));
                o.insert("properties".into(), Value::Object(props));
                if !required.is_empty() {
                    o.insert("required".into(), Value::Array(required));
                }
                Value::Object(o)
            }
            Shape::Union(arms) => {
                if arms.iter().all(|a| matches!(a, Shape::Lit(_))) {
                    let vals: Vec<Value> = arms
                        .iter()
                        .map(|a| match a {
                            Shape::Lit(s) => json!(s),
                            _ => unreachable!(),
                        })
                        .collect();
                    return json!({"type": "string", "enum": vals});
                }
                json!({"anyOf": arms.iter().map(Shape::to_schema).collect::<Vec<_>>()})
            }
        }
    }

    /// Check `v` against the shape; every problem is reported as `path: message`.
    pub fn validate(&self, v: &Value, defs: &BTreeMap<String, Shape>) -> Vec<String> {
        let mut out = vec![];
        self.check(v, defs, "$", &mut out);
        out
    }

    fn matches(&self, v: &Value, defs: &BTreeMap<String, Shape>) -> bool {
        let mut o = vec![];
        self.check(v, defs, "$", &mut o);
        o.is_empty()
    }

    fn check(&self, v: &Value, defs: &BTreeMap<String, Shape>, path: &str, out: &mut Vec<String>) {
        let bad = |out: &mut Vec<String>, want: &str| {
            out.push(format!("{path}: expected {want}, got {}", brief(v)));
        };
        match self {
            Shape::Any => {}
            Shape::Null => {
                if !v.is_null() {
                    bad(out, "null")
                }
            }
            Shape::Bool => {
                if !v.is_boolean() {
                    bad(out, "bool")
                }
            }
            Shape::Int => {
                if !(v.is_i64() || v.is_u64()) {
                    bad(out, "int")
                }
            }
            Shape::Number => {
                if !v.is_number() {
                    bad(out, "number")
                }
            }
            Shape::Str => {
                if !v.is_string() {
                    bad(out, "string")
                }
            }
            Shape::Object => {
                if !v.is_object() {
                    bad(out, "object")
                }
            }
            Shape::Lit(s) => {
                if v.as_str() != Some(s) {
                    bad(out, &format!("'{s}'"))
                }
            }
            Shape::BoolLit(b) => {
                if v.as_bool() != Some(*b) {
                    bad(out, &b.to_string())
                }
            }
            Shape::Array(s) => match v.as_array() {
                Some(a) => {
                    for (i, e) in a.iter().enumerate() {
                        s.check(e, defs, &format!("{path}[{i}]"), out);
                    }
                }
                None => bad(out, "array"),
            },
            Shape::Map(s) => match v.as_object() {
                Some(o) => {
                    for (k, e) in o {
                        s.check(e, defs, &format!("{path}.{k}"), out);
                    }
                }
                None => bad(out, "object"),
            },
            Shape::Ref(n) => match defs.get(n) {
                Some(d) => d.check(v, defs, path, out),
                None => out.push(format!("{path}: unknown definition {n}")),
            },
            Shape::Obj(fs) => match v.as_object() {
                Some(o) => {
                    for f in fs {
                        // A present field is checked against its type whether it is optional
                        // or not: `null` passes only where the type includes `null`.
                        match o.get(&f.name) {
                            Some(e) => f.shape.check(e, defs, &format!("{path}.{}", f.name), out),
                            None if f.optional => {}
                            None => out.push(format!("{path}: missing required `{}`", f.name)),
                        }
                    }
                }
                None => bad(out, "object"),
            },
            Shape::Union(arms) => {
                if !arms.iter().any(|a| a.matches(v, defs)) {
                    out.push(format!("{path}: {} matches no alternative", brief(v)));
                }
            }
        }
    }
}

fn brief(v: &Value) -> String {
    let s = v.to_string();
    if s.len() > 60 {
        format!("{}…", s.chars().take(60).collect::<String>())
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Shape {
        parse(s).unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    #[test]
    fn parses_fields_unions_defaults_and_maps() {
        let s = p(
            "{pane: Target, direction: right|down, ratio?: number = 0.5, env?: {*: string}, keys?: [string], mode?: tui|headless = tui, ...}",
        );
        let schema = s.to_schema();
        assert_eq!(schema["required"], json!(["pane", "direction"]));
        assert_eq!(
            schema["properties"]["direction"],
            json!({"type": "string", "enum": ["right", "down"]})
        );
        assert_eq!(schema["properties"]["ratio"]["default"], json!(0.5));
        assert_eq!(schema["properties"]["mode"]["default"], json!("tui"));
        assert_eq!(
            schema["properties"]["env"],
            json!({"type": "object", "additionalProperties": {"type": "string"}})
        );
        assert_eq!(
            schema["properties"]["pane"],
            json!({"$ref": "#/$defs/Target"})
        );
    }

    #[test]
    fn untyped_fields_are_any_and_unions_mix() {
        let s = p("{title: string|null, x}");
        assert_eq!(s.to_schema()["properties"]["x"], json!({}));
        let s = p("{a: [string|int]} | {b}");
        assert!(matches!(s, Shape::Union(_)));
        assert_eq!(p("{}"), Shape::Object);
        assert_eq!(p("{*: int}"), Shape::Map(Box::new(Shape::Int)));
    }

    #[test]
    fn validates_values() {
        let defs = BTreeMap::from([("Target".to_string(), Shape::Str)]);
        let s = p("{pane: Target, n?: int, mode?: a|b, list: [{id: string}]}");
        assert!(
            s.validate(
                &json!({"pane": "w1", "list": [{"id": "x"}], "extra": 1}),
                &defs
            )
            .is_empty()
        );
        let errs = s.validate(
            &json!({"pane": 3, "n": "x", "mode": "c", "list": [{}]}),
            &defs,
        );
        assert_eq!(errs.len(), 4, "{errs:?}");
        assert!(
            p("{a} | {b}")
                .validate(&json!({"c": 1}), &defs)
                .iter()
                .any(|e| e.contains("missing required"))
                || !p("{a} | {b}").validate(&json!({"c": 1}), &defs).is_empty()
        );
        // Optional means "may be absent", not "may be null": null needs `|null`.
        assert!(p("{a?: int}").validate(&json!({}), &defs).is_empty());
        assert_eq!(p("{a?: int}").validate(&json!({"a": null}), &defs).len(), 1);
        assert!(
            p("{a?: int|null}")
                .validate(&json!({"a": null}), &defs)
                .is_empty()
        );
        assert!(
            p("{a: int|null}")
                .validate(&json!({"a": null}), &defs)
                .is_empty()
        );
        assert_eq!(p("{a: int|null}").validate(&json!({}), &defs).len(), 1);
    }

    #[test]
    fn bare_true_and_false_are_boolean_literals() {
        let defs = BTreeMap::new();
        assert_eq!(p("true"), Shape::BoolLit(true));
        assert_eq!(p("false"), Shape::BoolLit(false));
        assert_eq!(p("'true'"), Shape::Lit("true".into()));
        // `task.finish remove_worktree`: "ask" or a JSON boolean, never the string "true".
        let s = p("{remove_worktree?: ask|true|false}");
        let rw = &s.to_schema()["properties"]["remove_worktree"];
        assert_eq!(
            *rw,
            json!({"anyOf": [{"const": "ask"}, {"const": true}, {"const": false}]})
        );
        for ok in [
            json!({"remove_worktree": true}),
            json!({"remove_worktree": false}),
            json!({"remove_worktree": "ask"}),
        ] {
            assert!(s.validate(&ok, &defs).is_empty(), "{ok}");
        }
        for bad in [
            json!({"remove_worktree": "true"}),
            json!({"remove_worktree": "false"}),
            json!({"remove_worktree": 1}),
        ] {
            assert!(!s.validate(&bad, &defs).is_empty(), "{bad}");
        }
        // A boolean-literal result field.
        let r = p("{requires_confirmation: true}");
        assert_eq!(
            r.to_schema()["properties"]["requires_confirmation"],
            json!({"const": true})
        );
        assert!(
            r.validate(&json!({"requires_confirmation": true}), &defs)
                .is_empty()
        );
        assert!(
            !r.validate(&json!({"requires_confirmation": "true"}), &defs)
                .is_empty()
        );
        assert!(
            !r.validate(&json!({"requires_confirmation": false}), &defs)
                .is_empty()
        );
    }

    #[test]
    fn nullable_fields_are_emitted_as_nullable() {
        let s = p("{a?: string, b: string|null, c?: int|null}");
        let sc = s.to_schema();
        assert_eq!(sc["properties"]["a"], json!({"type": "string"}));
        assert_eq!(
            sc["properties"]["b"],
            json!({"anyOf": [{"type": "string"}, {"type": "null"}]})
        );
        assert_eq!(
            sc["properties"]["c"],
            json!({"anyOf": [{"type": "integer"}, {"type": "null"}]})
        );
        assert_eq!(sc["required"], json!(["b"]));
    }

    #[test]
    fn rejects_malformed_shapes() {
        for bad in [
            "{a:",
            "{a b}",
            "[string",
            "{*: int, x}",
            "string |",
            "{a = }",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }
}
