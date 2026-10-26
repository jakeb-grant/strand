//! `strand get [--json] <path>` and `strand toggle <path>` (S-ipc): the
//! CLI's side, and the values as an answer carries them.
//!
//! `get` reads what `set` writes (docs/m5-plan.md, "S-ipc"; decisions.md
//! m5-ipc): an exported `state` or `let` (or a field of one), a settings
//! field, or a token with the token spelling (`'$accent'`, m5-owner Q10),
//! the overlay included. The answer is `{"ok": true, "value": …, "text":
//! …, "layer": …}`: `value` as JSON (a bool, a plain number, text, a list,
//! a record as an object; every other value as the text `set` reads
//! back), `text` as `strand get` prints it, and `layer` where a settings
//! field's value comes from (`overlay`, `file`, `default`) or where a
//! token was defined (`TokenTable::origin`: `overlay` while overlaid,
//! `tokens base`, `palette:…`), else null. `--json` prints the answer.
//!
//! `toggle` flips an exported boolean (`Instance::toggle`): `{"ok": true,
//! "value": <new>}`; any other type is refused, naming it.

use std::io::BufReader;
use std::time::Duration;

use serde_json::{Value as Json, json};
use strand_compiler::ty::TypeTable;
use strand_compiler::vm::{Num, Value};
use strand_core::SettingsLayer;
use strand_scene::PropValue;

use super::{Request, cli};

/// Send `req` to the running shell and read its answer; a refusal is
/// the error.
pub(crate) fn ask(req: &Request) -> Result<Json, String> {
    let mut conn = BufReader::new(cli::connect()?);
    let answer = cli::request(&mut conn, req, Duration::from_secs(10))?;
    match strand_core::ipc::refusal(&answer) {
        Some(why) => Err(why),
        None => Ok(answer),
    }
}

/// `args` with a `--json` flag anywhere taken out: whether it was there.
pub(super) fn json_flag(args: &[String]) -> (bool, Vec<&str>) {
    let rest: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|a| *a != "--json")
        .collect();
    (rest.len() != args.len(), rest)
}

/// `strand get`'s usage.
pub(crate) const GET_USAGE: &str = "usage: strand get [--json] <file.name | '$token'>";
/// `strand toggle`'s usage.
pub(crate) const TOGGLE_USAGE: &str = "usage: strand toggle <file.name>";

/// `strand get [--json] <path>`: the value as text (`--json`: the
/// answer, one line).
pub fn get_cli(args: &[String]) -> Result<String, String> {
    let (json, rest) = json_flag(args);
    let [path] = rest[..] else {
        return Err(GET_USAGE.into());
    };
    let answer = ask(&Request::Get { path: path.into() })?;
    Ok(if json {
        format!("{answer}\n")
    } else {
        format!("{}\n", answer["text"].as_str().unwrap_or_default())
    })
}

/// `strand toggle <path>`.
pub fn toggle_cli(args: &[String]) -> Result<String, String> {
    let [path] = args else {
        return Err(TOGGLE_USAGE.into());
    };
    ask(&Request::Toggle { path: path.clone() })?;
    Ok(String::new())
}

/// A program value as `get`'s answer carries it.
pub(crate) fn value_json(v: &Value, types: &TypeTable) -> Json {
    match v {
        Value::Null | Value::Unit => Json::Null,
        Value::Bool(b) => json!(b),
        Value::Num(n, Num::Int) => json!(*n as i64),
        Value::Num(n, Num::Float) => json!(n),
        Value::Text(t) => json!(&**t),
        Value::List(items) => items.iter().map(|i| value_json(i, types)).collect(),
        Value::Record(r) => {
            // Field by field: collecting into the map sorts the pairs
            // first, a sort `strand run` carries for this alone.
            let mut map = serde_json::Map::new();
            for (f, v) in types.record(r.ty).fields.iter().zip(&r.fields) {
                map.insert(f.name.clone(), value_json(v, types));
            }
            Json::Object(map)
        }
        Value::Async(a) => a
            .value
            .as_ref()
            .map_or(Json::Null, |v| value_json(v, types)),
        v => json!(v.show(types)),
    }
}

/// A token's value as text, as `strand set '$x'` reads it back.
pub(crate) fn token_text(v: &PropValue) -> String {
    match v {
        PropValue::Text(t) => t.clone(),
        v => strand_compiler::instantiate::show(v),
    }
}

/// A token's value as `get`'s answer carries it.
pub(crate) fn token_json(v: &PropValue) -> Json {
    match v {
        PropValue::Bool(b) => json!(b),
        PropValue::Number(n) => json!(n),
        v => json!(token_text(v)),
    }
}

/// A settings layer's name in `get`'s answer.
pub(crate) fn layer_name(l: SettingsLayer) -> &'static str {
    match l {
        SettingsLayer::Overlay => "overlay",
        SettingsLayer::File => "file",
        SettingsLayer::Default => "default",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_json_where_json_has_them_and_set_text_elsewhere() {
        let types = TypeTable::default();
        assert_eq!(value_json(&Value::Bool(true), &types), json!(true));
        assert_eq!(value_json(&Value::int(3), &types), json!(3));
        assert_eq!(value_json(&Value::float(0.5), &types), json!(0.5));
        assert_eq!(value_json(&Value::text("hi"), &types), json!("hi"));
        assert_eq!(value_json(&Value::Null, &types), Json::Null);
        assert_eq!(
            value_json(&Value::Num(8.0, Num::Px), &types),
            json!("8px"),
            "a length keeps its unit, as `set` reads it"
        );
        assert_eq!(
            value_json(
                &Value::List(std::rc::Rc::new(vec![Value::int(1), Value::Bool(false)])),
                &types
            ),
            json!([1, false])
        );
        let accent = strand_scene::Color::from_hex("#ff8800").unwrap();
        assert_eq!(value_json(&Value::Color(accent), &types), json!("#ff8800"));
        assert_eq!(token_json(&PropValue::Color(accent)), json!("#ff8800"));
        assert_eq!(token_json(&PropValue::Number(8.0)), json!(8.0));
        assert_eq!(token_text(&PropValue::Text("Inter".into())), "Inter");
        assert_eq!(
            token_text(&PropValue::Duration(Duration::from_millis(150))),
            "150ms"
        );
    }

    #[test]
    fn the_json_flag_goes_anywhere() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(json_flag(&args(&["--json", "a.b"])), (true, vec!["a.b"]));
        assert_eq!(json_flag(&args(&["a.b", "--json"])), (true, vec!["a.b"]));
        assert_eq!(json_flag(&args(&["a.b"])), (false, vec!["a.b"]));
        assert!(
            get_cli(&args(&["a", "b"]))
                .unwrap_err()
                .starts_with("usage")
        );
        assert!(toggle_cli(&[]).unwrap_err().starts_with("usage"));
    }
}
