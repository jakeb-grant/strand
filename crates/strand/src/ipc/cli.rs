//! The CLI's side of the socket: `strand reload`, `strand set` and
//! `strand watch`.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use serde_json::{Value as Json, json};

use super::{Request, VERSION, describe, encode, socket_path};

/// Connect to the running shell.
pub fn connect() -> Result<UnixStream, String> {
    let path = socket_path().ok_or("no socket: set XDG_RUNTIME_DIR or STRAND_SOCKET")?;
    UnixStream::connect(&path).map_err(|e| {
        format!(
            "no running strand on {} ({e}); start one with `strand run`",
            path.display()
        )
    })
}

/// Send `req` and read the answer line (`timeout` at most). The reader
/// is the connection's for its whole life: what the server sent after
/// the answer (a watcher's first events) stays buffered in it.
pub fn request(
    conn: &mut BufReader<UnixStream>,
    req: &Request,
    timeout: Duration,
) -> Result<Json, String> {
    let mut w = conn.get_ref();
    w.write_all(encode(req).as_bytes())
        .map_err(|e| format!("send: {e}"))?;
    conn.get_ref()
        .set_read_timeout(Some(timeout))
        .map_err(|e| e.to_string())?;
    let mut line = String::new();
    match conn.read_line(&mut line) {
        Ok(0) => return Err("no answer: the shell closed the connection".into()),
        Ok(_) => {}
        Err(e) => return Err(format!("no answer: {e}")),
    }
    serde_json::from_str(&line).map_err(|e| format!("bad answer {line:?}: {e}"))
}

/// `strand reload [--hard]`.
pub fn reload_cli(args: &[String]) -> Result<String, String> {
    let hard = match args {
        [] => false,
        [a] if a == "--hard" => true,
        _ => return Err("usage: strand reload [--hard]".into()),
    };
    let mut conn = BufReader::new(connect()?);
    let answer = request(
        &mut conn,
        &Request::Reload { hard },
        Duration::from_secs(30),
    )?;
    if answer.get("ok").and_then(Json::as_bool) != Some(true) {
        return Err(answer
            .get("error")
            .and_then(Json::as_str)
            .unwrap_or("failed")
            .to_string());
    }
    Ok(answer.get("event").map(describe).unwrap_or_default())
}

/// `strand set <path> <value>` (`strand set theme.look mocha`).
pub fn set_cli(args: &[String]) -> Result<String, String> {
    let [path, value] = args else {
        return Err("usage: strand set <file.name> <value>".into());
    };
    let mut conn = BufReader::new(connect()?);
    let answer = request(
        &mut conn,
        &Request::Set {
            path: path.clone(),
            value: value.clone(),
        },
        Duration::from_secs(10),
    )?;
    if answer.get("ok").and_then(Json::as_bool) != Some(true) {
        return Err(answer
            .get("error")
            .and_then(Json::as_str)
            .unwrap_or("failed")
            .to_string());
    }
    Ok(String::new())
}

/// `strand watch [--json]`: print every event until the shell ends.
pub fn watch_cli(args: &[String], out: &mut dyn Write) -> Result<(), String> {
    let json = match args {
        [] => false,
        [a] if a == "--json" => true,
        _ => return Err("usage: strand watch [--json]".into()),
    };
    let mut conn = BufReader::new(connect()?);
    let answer = request(&mut conn, &Request::Watch, Duration::from_secs(10))?;
    if answer.get("ok").and_then(Json::as_bool) != Some(true) {
        return Err(format!("refused: {answer}"));
    }
    conn.get_ref()
        .set_read_timeout(None)
        .map_err(|e| e.to_string())?;
    // Scripts know from this line on that no event is missed.
    if json
        && out
            .write_all(format!("{}\n", json!({"event": "watching", "v": VERSION})).as_bytes())
            .and_then(|_| out.flush())
            .is_err()
    {
        return Ok(());
    }
    for line in conn.lines() {
        let line = line.map_err(|e| e.to_string())?;
        let text = if json {
            format!("{line}\n")
        } else {
            match serde_json::from_str::<Json>(&line) {
                Ok(v) => describe(&v),
                Err(_) => format!("{line}\n"),
            }
        };
        if out
            .write_all(text.as_bytes())
            .and_then(|_| out.flush())
            .is_err()
        {
            // The reader went away (`strand watch | head`).
            return Ok(());
        }
    }
    Ok(())
}
