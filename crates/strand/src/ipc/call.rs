//! `strand call [--json] <service.action> [args…]` (S-ipc): a
//! service-level action (`strand call notifications.clear`; decisions.md
//! m5-owner Q13: item actions and exported `fn`s are out of scope).
//!
//! The target is checked against the schema the shell runs with: an
//! unknown service or action is refused with the closest name, a method
//! that is not an action is refused as one, and the arguments are text,
//! read by the parameters' types as `strand set` reads a value
//! (`Instance::set_text`, `parse_text`), a bad one refused naming the
//! parameter and its type (an enum's closest variant offered). Left-out
//! parameters with a default take the schema's default. The lock's
//! `auth` actions are refused: a password is typed into the lock, never
//! sent over the socket (decisions.md m5-ipc).
//!
//! Actions return nothing, so the answer is `{"ok": true}`, which
//! `--json` prints; without it a call that went ahead prints nothing.

use serde_json::Value as Json;
use strand_compiler::diagnostic::suggest;
use strand_compiler::instantiate::parse_text;
use strand_compiler::schema::Schema;
use strand_compiler::ty::{FnSig, Ty};
use strand_compiler::vm::Value;

use super::Request;
use super::query::{ask, json_flag};

/// A service action resolved against the schema, its arguments read.
#[derive(Debug, PartialEq)]
pub(crate) struct Call {
    pub service: String,
    pub action: String,
    pub args: Vec<Value>,
}

/// Services whose actions the socket does not run.
const NOT_FROM_IPC: &[&str] = &["auth"];

/// `target` (`notifications.clear`) and its text `args`, checked and
/// read against `schema`.
pub(crate) fn resolve(schema: &Schema, target: &str, args: &[String]) -> Result<Call, String> {
    let Some((service, action)) = target.split_once('.') else {
        return Err(format!(
            "`{target}`: name a service action, `<service>.<action>` (`notifications.clear`)"
        ));
    };
    if action.contains('.') {
        return Err(format!(
            "`{target}`: `call` runs service-level actions only (`notifications.clear`)"
        ));
    }
    let Some(&record) = schema.services.get(service) else {
        // `suggest` over a `Vec<&str>` here and below: one instantiation
        // the binary already has, not one per iterator.
        let names: Vec<&str> = schema.services.keys().map(String::as_str).collect();
        let hint = suggest(service, names)
            .map(|s| format!("; did you mean `{s}`?"))
            .unwrap_or_default();
        return Err(format!("no service `{service}`{hint}"));
    };
    if NOT_FROM_IPC.contains(&service) {
        return Err(format!(
            "`{target}`: the lock's `{service}` actions are not run over IPC"
        ));
    }
    let methods = &schema.types.record(record).methods;
    let actions: Vec<(&str, &FnSig)> = methods
        .iter()
        .flat_map(|m| {
            m.sigs
                .iter()
                .filter(|s| s.action)
                .map(move |s| (m.name.as_str(), &**s))
        })
        .collect();
    let Some(&(_, sig)) = actions.iter().find(|(n, _)| *n == action) else {
        if methods.iter().any(|m| m.name == action) {
            return Err(format!(
                "`{target}` is a method, not an action: `call` runs actions"
            ));
        }
        let mut names: Vec<&str> = actions.iter().map(|(n, _)| *n).collect();
        names.dedup();
        let hint = match suggest(action, names.clone()) {
            Some(s) => format!("; did you mean `{s}`?"),
            None if actions.is_empty() => format!("; `{service}` has no actions"),
            None => format!("; its actions: {}", names.join(", ")),
        };
        return Err(format!("`{service}` has no action `{action}`{hint}"));
    };
    let types = &schema.types;
    let shown = || {
        sig.params
            .iter()
            .map(|p| {
                let dots = if p.variadic { "..." } else { "" };
                format!("{dots}{}: {}", p.name, types.show(&p.ty))
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    let variadic = sig.params.last().is_some_and(|p| p.variadic);
    let required = sig
        .params
        .iter()
        .filter(|p| !p.has_default && !p.variadic)
        .count();
    if args.len() < required || (!variadic && args.len() > sig.params.len()) {
        return Err(format!(
            "`{target}({})` takes {} argument{}, not {}",
            shown(),
            if variadic {
                format!("{required} or more")
            } else if required == sig.params.len() {
                required.to_string()
            } else {
                format!("{required} to {}", sig.params.len())
            },
            if sig.params.len() == 1 && !variadic {
                ""
            } else {
                "s"
            },
            args.len()
        ));
    }
    let mut values = Vec::with_capacity(sig.params.len().max(args.len()));
    for (i, text) in args.iter().enumerate() {
        let p = &sig.params[i.min(sig.params.len() - 1)];
        let v = parse_text(types, &p.ty, text.trim()).ok_or_else(|| {
            let mut why = format!(
                "`{text}` is not a {} (`{}` of `{target}`)",
                types.show(&p.ty),
                p.name
            );
            if let Ty::Enum(e) = p.ty.non_null()
                && let Some(s) = suggest(
                    text.trim(),
                    types.enum_(*e).variants.iter().map(String::as_str),
                )
            {
                why.push_str(&format!("; did you mean `{s}`?"));
            }
            why
        })?;
        values.push(v);
    }
    // The schema's defaults for the rest, as written there (`"%H:%M"`).
    for p in sig.params.iter().skip(args.len()) {
        let Some(d) = p.default.as_deref().filter(|_| !p.variadic) else {
            break;
        };
        let d = d
            .strip_prefix('"')
            .and_then(|d| d.strip_suffix('"'))
            .unwrap_or(d);
        match parse_text(types, &p.ty, d) {
            Some(v) => values.push(v),
            None => break,
        }
    }
    Ok(Call {
        service: service.to_string(),
        action: action.to_string(),
        args: values,
    })
}

/// `strand call [--json] <service.action> [args…]`.
pub fn call_cli(args: &[String]) -> Result<String, String> {
    let (json, rest) = json_flag(args);
    let Some((target, args)) = rest.split_first() else {
        return Err("usage: strand call [--json] <service.action> [args…]".into());
    };
    let answer: Json = ask(&Request::Call {
        target: target.to_string(),
        args: args.iter().map(|a| a.to_string()).collect(),
    })?;
    Ok(if json {
        format!("{answer}\n")
    } else {
        String::new()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// The builtin schema with a test service whose actions take typed
    /// parameters (the builtin services' service-level actions take none
    /// but `auth.submit`'s text).
    fn schema() -> Schema {
        let mut s = crate::services::schema().clone();
        s.extend(
            "enum Mode { quiet, loud }\n\
             service probe {\n\
               level: float\n\
               action set_mode(mode: Mode)\n\
               action nudge(by: float, times: int = 2)\n\
               action say(...words: text)\n\
               fn twice(x: float) -> float\n\
             }\n",
        )
        .unwrap();
        s
    }

    #[test]
    fn a_service_action_resolves_with_its_arguments_read_by_type() {
        let s = crate::services::schema();
        assert_eq!(
            resolve(s, "notifications.clear", &[]),
            Ok(Call {
                service: "notifications".into(),
                action: "clear".into(),
                args: vec![],
            })
        );
        let s = schema();
        let mode = |v: &str| {
            let Ty::Enum(e) = s
                .types
                .record(s.services["probe"])
                .methods
                .iter()
                .find(|m| m.name == "set_mode")
                .unwrap()
                .sigs[0]
                .params[0]
                .ty
            else {
                panic!("an enum parameter");
            };
            Value::Enum(e, s.types.enum_(e).variant(v).unwrap())
        };
        assert_eq!(
            resolve(&s, "probe.set_mode", &args(&["loud"]))
                .unwrap()
                .args,
            vec![mode("loud")]
        );
        // A left-out parameter takes the schema's default.
        assert_eq!(
            resolve(&s, "probe.nudge", &args(&["0.5"])).unwrap().args,
            vec![Value::float(0.5), Value::int(2)]
        );
        assert_eq!(
            resolve(&s, "probe.say", &args(&["a", "b c"])).unwrap().args,
            vec![Value::text("a"), Value::text("b c")]
        );
    }

    #[test]
    fn a_bad_call_is_refused_with_what_was_meant() {
        let s = schema();
        let why = |t: &str, a: &[&str]| resolve(&s, t, &args(a)).unwrap_err();
        assert_eq!(
            why("probe.set_mode", &["laud"]),
            "`laud` is not a Mode (`mode` of `probe.set_mode`); did you mean `loud`?"
        );
        assert_eq!(
            why("probe.nudge", &["lots"]),
            "`lots` is not a float (`by` of `probe.nudge`)"
        );
        assert_eq!(
            why("notifications.claer", &[]),
            "`notifications` has no action `claer`; did you mean `clear`?"
        );
        assert_eq!(
            why("notificatons.clear", &[]),
            "no service `notificatons`; did you mean `notifications`?"
        );
        assert_eq!(
            why("notifications.clear", &["now"]),
            "`notifications.clear()` takes 0 arguments, not 1"
        );
        assert_eq!(
            why("probe.nudge", &[]),
            "`probe.nudge(by: float, times: int)` takes 1 to 2 arguments, not 0"
        );
        assert_eq!(
            why("probe.twice", &["1"]),
            "`probe.twice` is a method, not an action: `call` runs actions"
        );
        assert!(why("probe.zzz", &[]).ends_with("its actions: set_mode, nudge, say"));
        assert!(why("notifications", &[]).contains("<service>.<action>"));
        assert!(why("notifications.popups.clear", &[]).contains("service-level actions only"));
        assert!(why("auth.submit", &["hunter2"]).contains("not run over IPC"));
    }

    #[test]
    fn the_cli_needs_a_target() {
        assert!(
            call_cli(&args(&["--json"]))
                .unwrap_err()
                .starts_with("usage")
        );
    }
}
